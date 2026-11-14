use std::io::Read;

use super::target::{Local, Target};
use super::*;

/// Bytes that do not compress or repeat, like the files of a real volume, with
/// a phrase in clear at the start, to look for on the target.
fn volume(len: usize, seed: u64) -> Vec<u8> {
    let mut out = b"THE-PLAINTEXT-OF-A-VOLUME ".to_vec();
    let mut counter = 0u64;
    while out.len() < len {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&seed.to_le_bytes());
        hasher.update(&counter.to_le_bytes());
        out.extend_from_slice(hasher.finalize().as_bytes());
        counter += 1;
    }
    out.truncate(len);
    out
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("facade-offsite-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn restored(store: &Store, app: &str, volume: &str, id: u64) -> Vec<u8> {
    let (_, chunks) = backup_store::read_manifest(&store.snapshot_path(app, volume, id)).unwrap();
    let mut out = Vec::new();
    store.reader(chunks).read_to_end(&mut out).unwrap();
    out
}

/// Every file of a directory, for the local target.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut names = Vec::new();
    walk(dir, "", &mut names);
    names.into_iter().map(|n| dir.join(n)).collect()
}

/// A server with three backups of a volume and one of itself, copied with
/// `keep = 2` to `target`: what is copied, kept, deleted, checked, healed and
/// brought back. `local_dir` is the target's directory when it is a local one,
/// to look at its files.
async fn scenario(target: &dyn Target, local_dir: Option<&Path>) {
    let dir = temp(&format!("scenario-{}", uuid::Uuid::new_v4()));
    let root = dir.join("backups");
    let store = Store::new(&root);
    let keys = Keys::from_secret(&[42; 32]);
    let progress = Progress::default();
    let job = Job {
        store: &store,
        server_dir: root.join(super::super::backups::SERVER_DIR),
        cache: root.join(CACHE_DIR),
        keys: &keys,
        target,
        keep: 2,
        progress: &progress,
    };

    let mut data = volume(6 << 20, 1);
    store
        .write_snapshot("blog", "data", 100, &data[..])
        .unwrap();
    data[4 << 20] ^= 0xff;
    store
        .write_snapshot("blog", "data", 200, &data[..])
        .unwrap();
    data.extend_from_slice(&volume(1 << 20, 2));
    store
        .write_snapshot("blog", "data", 300, &data[..])
        .unwrap();
    let state = root.join("_server/250");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("facade.db"), volume(3 << 20, 3)).unwrap();
    std::fs::write(state.join("master.key"), b"THE-PLAINTEXT-OF-A-KEY").unwrap();

    // The newest two of the volume, and the server's.
    let first = job.copy().await.unwrap();
    assert_eq!(
        first.copied,
        ["blog/data 300", "server 250", "blog/data 200"]
    );
    assert!(first.removed.is_empty());
    assert_eq!(first.backups, 3);
    assert!(first.uploaded_chunks > 0);
    assert_eq!(first.chunks, first.uploaded_chunks);
    let mut volumes: Vec<String> = target
        .list(VOLUMES)
        .await
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    volumes.sort();
    assert_eq!(
        volumes,
        [
            "volumes/blog/data/200.snapshot",
            "volumes/blog/data/300.snapshot"
        ]
    );
    // Nothing in clear on the target.
    if let Some(dir) = local_dir {
        for file in files(dir) {
            let bytes = std::fs::read(&file).unwrap();
            for phrase in [&b"THE-PLAINTEXT-OF-A-"[..], b"blog", b"tar_size"] {
                assert!(
                    !bytes.windows(phrase.len()).any(|w| w == phrase),
                    "{} holds {:?}",
                    file.display(),
                    String::from_utf8_lossy(phrase)
                );
            }
        }
    }

    // Nothing new: nothing sent.
    let again = job.copy().await.unwrap();
    assert_eq!(again.uploaded_chunks, 0);
    assert!(again.copied.is_empty());
    assert_eq!(again.size, first.size);

    // A new backup pushes the oldest out, and the chunks only it had.
    data.truncate(5 << 20);
    store
        .write_snapshot("blog", "data", 400, &data[..])
        .unwrap();
    let third = job.copy().await.unwrap();
    assert_eq!(third.copied, ["blog/data 400"]);
    assert_eq!(third.removed, ["blog/data 200"]);
    assert!(third.chunks_deleted > 0, "200 had chunks of its own");
    assert_eq!(
        list_ids(target).await,
        [300, 400],
        "the target keeps the newest two"
    );
    // The listing comes from the cache, without the target.
    assert!(
        job.cached(&Manifest::Volume {
            app: "blog".into(),
            volume: "data".into(),
            id: 400
        })
        .is_ok()
    );

    // Everything checks out, read back in full.
    let check = job.verify(None).await.unwrap();
    assert!(check.problems.is_empty(), "{:?}", check.problems);
    assert_eq!(check.verified, check.chunks);
    assert_eq!(check.chunks, third.chunks);

    // A damaged chunk is found, deleted, and uploaded again by the next copy.
    let (victim, _) = target.list(CHUNKS).await.unwrap()[0].clone();
    target
        .put(
            &victim,
            b"FOFF\x01garbage that is long enough to be read".repeat(2),
        )
        .await
        .unwrap();
    let damaged = job.verify(None).await.unwrap();
    assert_eq!(damaged.problems.len(), 1, "{:?}", damaged.problems);
    assert!(
        damaged.problems[0].contains("deleted"),
        "{}",
        damaged.problems[0]
    );
    let missing = job.verify(Some(1)).await.unwrap();
    assert!(
        missing.problems[0].contains("missing"),
        "{:?}",
        missing.problems
    );
    let healed = job.copy().await.unwrap();
    assert_eq!(healed.uploaded_chunks, 1);
    assert!(job.verify(None).await.unwrap().problems.is_empty());

    // A rebuilt server: an empty store and cache, the same key.
    let other = temp(&format!("rebuilt-{}", uuid::Uuid::new_v4()));
    let fresh = Store::new(other.join("backups"));
    let rebuilt = Job {
        store: &fresh,
        server_dir: other.join("backups/_server"),
        cache: other.join("backups").join(CACHE_DIR),
        keys: &keys,
        target,
        keep: 2,
        progress: &progress,
    };
    assert_eq!(rebuilt.server_backups().await.unwrap(), [250]);
    let files = rebuilt
        .fetch_server(250, &other.join("state"))
        .await
        .unwrap();
    assert_eq!(files, ["facade.db", "master.key"]);
    assert_eq!(
        std::fs::read(other.join("state/facade.db")).unwrap(),
        std::fs::read(state.join("facade.db")).unwrap()
    );
    assert_eq!(
        std::fs::read(other.join("state/master.key")).unwrap(),
        b"THE-PLAINTEXT-OF-A-KEY"
    );
    let header = rebuilt.fetch_volume("blog", "data", 400).await.unwrap();
    assert_eq!(header.id, 400);
    assert_eq!(restored(&fresh, "blog", "data", 400), data);
    assert!(fresh.check().problems.is_empty());
    // Fetching 300 now only needs the chunks 400 did not have.
    let before = progress.chunks_done.load(Ordering::Relaxed);
    rebuilt.fetch_volume("blog", "data", 300).await.unwrap();
    let fetched = progress.chunks_done.load(Ordering::Relaxed) - before;
    assert!(fetched > 0 && (fetched as usize) < first.chunks as usize);
    assert!(rebuilt.fetch_volume("blog", "data", 200).await.is_err());
    // Its first copy reads the target's manifests into its cache and keeps
    // what it has: the rebuilt server's backups are not newer.
    let checked = rebuilt.verify(Some(3)).await.unwrap();
    assert!(checked.problems.is_empty(), "{:?}", checked.problems);
    assert_eq!(checked.verified, 3);
    assert_eq!(
        super::list_cached(&other.join("backups").join(CACHE_DIR)).len(),
        3
    );

    // Another key is refused before anything is touched.
    let wrong = Keys::from_secret(&[43; 32]);
    let stranger = Job {
        keys: &wrong,
        ..rebuilt
    };
    let err = stranger.copy().await.unwrap_err();
    assert!(format!("{err:#}").contains("another key"), "{err:#}");
    assert_eq!(list_ids(target).await, [300, 400]);

    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&other).unwrap();
}

async fn list_ids(target: &dyn Target) -> Vec<u64> {
    let mut ids: Vec<u64> = target
        .list(VOLUMES)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|(n, _)| Manifest::parse(&n).map(|m| m.id()))
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn copies_to_a_directory() {
    let dir = temp("local-target");
    let target = Local::new(dir.clone());
    scenario(&target, Some(&dir)).await;
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn reads_manifest_names() {
    assert_eq!(
        Manifest::parse("volumes/blog/data/100.snapshot"),
        Some(Manifest::Volume {
            app: "blog".into(),
            volume: "data".into(),
            id: 100
        })
    );
    assert_eq!(
        Manifest::parse("server/7.snapshot"),
        Some(Manifest::Server { id: 7 })
    );
    for bad in [
        "volumes/../data/1.snapshot",
        "volumes/blog/data/.tmp-1",
        "volumes/blog/1.snapshot",
        "server/x.snapshot",
        "chunks/ab/cd",
    ] {
        assert_eq!(Manifest::parse(bad), None, "{bad}");
    }
}

#[test]
fn settings_name_what_is_wrong() {
    use crate::config::ServerConfig;
    let load = |toml: &str| -> anyhow::Result<Settings> {
        let cfg: ServerConfig = toml::from_str(toml)?;
        Settings::from_config(cfg.backups.offsite.as_ref().unwrap(), &cfg.backups)
    };
    let box_ = load(
        r#"
        [backups.offsite]
        provider = "sftp"
        host = "u1.your-storagebox.de"
        port = 23
        user = "u1-sub2"
        key_file = "/etc/facade/offsite_ed25519"
        "#,
    )
    .unwrap();
    assert_eq!(box_.keep, 7);
    assert_eq!(box_.stale_after, 48 * 3600);
    assert_eq!(
        box_.provider.describe(),
        "sftp://u1-sub2@u1.your-storagebox.de:23/facade"
    );

    let s3 = load(
        r#"
        [backups]
        keep = 3
        every_hours = 6
        [backups.offsite]
        provider = "s3"
        endpoint = "https://fsn1.your-objectstorage.com"
        bucket = "backups"
        access_key_file = "/a"
        secret_key_file = "/b"
        keep = 30
        "#,
    )
    .unwrap();
    assert_eq!((s3.keep, s3.stale_after), (30, 12 * 3600));

    for (toml, expected) in [
        (
            "provider = \"sftp\"\nhost = \"h\"\nuser = \"u\"",
            "key_file is required",
        ),
        (
            "provider = \"local\"\npath = \"/mnt/x\"\nbucket = \"b\"",
            "bucket is not a setting of the local provider",
        ),
        ("provider = \"local\"\npath = \"backups\"", "absolute"),
        (
            "provider = \"local\"\npath = \"/mnt/x\"\nkeep = 2",
            "fewer than [backups] keep = 7",
        ),
        ("host = \"h\"", "provider is required"),
        (
            "provider = \"s3\"\nendpoint = \"s3.example.com\"\nbucket = \"b\"",
            "https://",
        ),
    ] {
        let err = load(&format!("[backups.offsite]\n{toml}")).unwrap_err();
        assert!(format!("{err:#}").contains(expected), "{toml}: {err:#}");
    }
    // A setting nobody knows is refused by the configuration itself.
    assert!(toml::from_str::<ServerConfig>("[backups.offsite]\nprovidr = \"s3\"").is_err());
}

/// `FACADE_TEST_SFTP=user@host:port:/path` and `FACADE_TEST_SFTP_KEY=<key file>`,
/// e.g. against `docker run -p 2225:22 atmoz/sftp` with a key installed.
#[tokio::test]
#[ignore]
async fn copies_over_sftp() {
    let spec = std::env::var("FACADE_TEST_SFTP").expect("FACADE_TEST_SFTP");
    let key = std::env::var("FACADE_TEST_SFTP_KEY").expect("FACADE_TEST_SFTP_KEY");
    let (user, rest) = spec.split_once('@').unwrap();
    let mut parts = rest.splitn(3, ':');
    let host = parts.next().unwrap();
    let port: u16 = parts.next().unwrap().parse().unwrap();
    let path = parts.next().unwrap();
    let settings = target::SftpSettings {
        host: host.into(),
        port,
        user: user.into(),
        key_file: key.into(),
        host_key: None,
        path: format!("{path}/facade-test-{}", uuid::Uuid::new_v4()),
    };
    let provider = Provider::Sftp(settings.clone());
    let (target, seen) = target::connect(&provider, None).await.unwrap();
    let seen = seen.unwrap();
    target::tests::exercise(target.as_ref()).await;
    scenario(target.as_ref(), None).await;

    // The host key seen first is required from then on.
    assert!(target::connect(&provider, Some(seen.clone())).await.is_ok());
    let err = match target::connect(&provider, Some("SHA256:somethingelse".into())).await {
        Ok(_) => panic!("another host key was accepted"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains(&seen), "{err}");
}

/// `FACADE_TEST_S3=<endpoint> <bucket>` with `FACADE_TEST_S3_ACCESS_KEY_FILE` and
/// `FACADE_TEST_S3_SECRET_KEY_FILE`, e.g. against MinIO.
#[tokio::test]
#[ignore]
async fn copies_to_s3() {
    let spec = std::env::var("FACADE_TEST_S3").expect("FACADE_TEST_S3");
    let (endpoint, bucket) = spec.split_once(' ').unwrap();
    let provider = Provider::S3(target::S3Settings {
        endpoint: endpoint.into(),
        bucket: bucket.into(),
        region: "us-east-1".into(),
        access_key_file: std::env::var("FACADE_TEST_S3_ACCESS_KEY_FILE")
            .unwrap()
            .into(),
        secret_key_file: std::env::var("FACADE_TEST_S3_SECRET_KEY_FILE")
            .unwrap()
            .into(),
        prefix: format!("facade-test-{}", uuid::Uuid::new_v4()),
    });
    let (target, _) = target::connect(&provider, None).await.unwrap();
    target::tests::exercise(target.as_ref()).await;
    scenario(target.as_ref(), None).await;
}
