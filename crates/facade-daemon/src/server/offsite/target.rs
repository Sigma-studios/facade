//! Where offsite copies are written: a directory over SFTP, a bucket of an
//! S3-compatible object storage, or a mounted path. Each is a flat set of named
//! objects (`chunks/ab/abcd…`, `volumes/blog/data/1731550000.snapshot`), written
//! whole or not at all, which is all the copy needs from them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use parking_lot::Mutex;

/// Names of objects being written (SFTP, local): renamed once whole. Left over
/// by an interrupted copy, they go with the next one's clean-up.
pub const TEMP_PREFIX: &str = ".tmp-";

#[async_trait]
pub trait Target: Send + Sync {
    /// Writes the object `name`, replacing any there: whole, or not at all.
    async fn put(&self, name: &str, data: Vec<u8>) -> anyhow::Result<()>;
    /// The object `name`, or `None` when there is none.
    async fn get(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>>;
    /// Every object whose name starts with `dir/`, as its name and size.
    async fn list(&self, dir: &str) -> anyhow::Result<Vec<(String, u64)>>;
    /// Deletes the object `name`; one already gone is fine.
    async fn delete(&self, name: &str) -> anyhow::Result<()>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Provider {
    Sftp(SftpSettings),
    S3(S3Settings),
    Local(PathBuf),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SftpSettings {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_file: PathBuf,
    /// The host key's fingerprint (`SHA256:…`), when configured.
    pub host_key: Option<String>,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct S3Settings {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key_file: PathBuf,
    pub secret_key_file: PathBuf,
    /// Inside the bucket, without slashes around.
    pub prefix: String,
}

impl Provider {
    /// Where the copy goes, for people: `sftp://u1-sub1@u1.your-storagebox.de:23/facade`.
    pub fn describe(&self) -> String {
        match self {
            Provider::Sftp(s) => {
                let sep = if s.path.starts_with('/') { "" } else { "/" };
                format!("sftp://{}@{}:{}{sep}{}", s.user, s.host, s.port, s.path)
            }
            Provider::S3(s) => {
                let prefix = if s.prefix.is_empty() {
                    String::new()
                } else {
                    format!("/{}", s.prefix)
                };
                format!(
                    "s3://{}{prefix} at {}",
                    s.bucket,
                    s.endpoint.trim_end_matches('/')
                )
            }
            Provider::Local(path) => path.display().to_string(),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Provider::Sftp(_) => "sftp",
            Provider::S3(_) => "s3",
            Provider::Local(_) => "local",
        }
    }
}

/// Connects to the target. For SFTP, `known_host_key` is the fingerprint seen
/// at the first connection when none is configured; the one seen now comes back
/// with the target, to be kept the first time.
pub async fn connect(
    provider: &Provider,
    known_host_key: Option<String>,
) -> anyhow::Result<(Arc<dyn Target>, Option<String>)> {
    Ok(match provider {
        Provider::Local(path) => (Arc::new(Local::new(path.clone())), None),
        Provider::S3(s) => (Arc::new(S3::new(s)?), None),
        Provider::Sftp(s) => {
            let (sftp, seen) = Sftp::connect(s, known_host_key).await?;
            (Arc::new(sftp), Some(seen))
        }
    })
}

// ---------------------------------------------------------------- local

/// A directory on a disk of this machine (another disk, a network mount).
pub struct Local {
    root: PathBuf,
}

impl Local {
    /// The directory must exist: it is not created, so that a disk that is not
    /// mounted is an error rather than an empty target on the disk underneath.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

#[async_trait]
impl Target for Local {
    async fn put(&self, name: &str, data: Vec<u8>) -> anyhow::Result<()> {
        let path = self.path(name);
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            if !root.is_dir() {
                bail!(
                    "{} is not a directory (is the disk mounted?)",
                    root.display()
                );
            }
            let dir = path.parent().expect("objects are in a directory");
            std::fs::create_dir_all(dir)?;
            let temp = dir.join(format!("{TEMP_PREFIX}{}", uuid::Uuid::new_v4()));
            let written = (|| {
                let mut file = std::fs::File::create(&temp)?;
                file.write_all(&data)?;
                file.sync_all()?;
                std::fs::rename(&temp, &path)?;
                std::fs::File::open(dir)?.sync_all()
            })();
            if written.is_err() {
                let _ = std::fs::remove_file(&temp);
            }
            written.with_context(|| format!("writing {}", path.display()))
        })
        .await?
    }

    async fn get(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let path = self.path(name);
        match tokio::fs::read(&path).await {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    async fn list(&self, dir: &str) -> anyhow::Result<Vec<(String, u64)>> {
        let root = self.root.clone();
        let dir = dir.to_string();
        tokio::task::spawn_blocking(move || {
            if !root.is_dir() {
                bail!(
                    "{} is not a directory (is the disk mounted?)",
                    root.display()
                );
            }
            let mut out = Vec::new();
            walk_local(&root.join(&dir), &dir, &mut out)?;
            Ok(out)
        })
        .await?
    }

    async fn delete(&self, name: &str) -> anyhow::Result<()> {
        let path = self.path(name);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("deleting {}", path.display())),
        }
    }
}

fn walk_local(dir: &Path, name: &str, out: &mut Vec<(String, u64)>) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        let child = format!("{name}/{}", entry.file_name().to_string_lossy());
        let meta = entry.metadata()?;
        if meta.is_dir() {
            walk_local(&entry.path(), &child, out)?;
        } else {
            out.push((child, meta.len()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- s3

/// A bucket of an S3-compatible object storage. Objects are written in one
/// request each, which S3 makes atomic.
pub struct S3 {
    store: object_store::aws::AmazonS3,
    prefix: String,
}

impl S3 {
    fn new(s: &S3Settings) -> anyhow::Result<Self> {
        let read = |path: &Path| {
            std::fs::read_to_string(path)
                .map(|v| v.trim().to_string())
                .with_context(|| format!("reading {}", path.display()))
        };
        let store = object_store::aws::AmazonS3Builder::new()
            .with_endpoint(&s.endpoint)
            .with_bucket_name(&s.bucket)
            .with_region(&s.region)
            .with_access_key_id(read(&s.access_key_file)?)
            .with_secret_access_key(read(&s.secret_key_file)?)
            // Most S3-compatible storages (MinIO, Garage…) answer at the
            // endpoint's own name; buckets as host names are AWS's thing.
            .with_virtual_hosted_style_request(false)
            .with_allow_http(s.endpoint.starts_with("http://"))
            .build()
            .context("setting up the S3 client")?;
        Ok(Self {
            store,
            prefix: s.prefix.clone(),
        })
    }

    fn path(&self, name: &str) -> object_store::path::Path {
        if self.prefix.is_empty() {
            object_store::path::Path::from(name)
        } else {
            object_store::path::Path::from(format!("{}/{name}", self.prefix))
        }
    }
}

#[async_trait]
impl Target for S3 {
    async fn put(&self, name: &str, data: Vec<u8>) -> anyhow::Result<()> {
        use object_store::ObjectStore;
        self.store
            .put(&self.path(name), data.into())
            .await
            .with_context(|| format!("writing {name}"))?;
        Ok(())
    }

    async fn get(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        use object_store::ObjectStore;
        match self.store.get(&self.path(name)).await {
            Ok(result) => Ok(Some(
                result
                    .bytes()
                    .await
                    .with_context(|| format!("reading {name}"))?
                    .to_vec(),
            )),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {name}")),
        }
    }

    async fn list(&self, dir: &str) -> anyhow::Result<Vec<(String, u64)>> {
        use object_store::ObjectStore;
        let skip = if self.prefix.is_empty() {
            0
        } else {
            self.prefix.len() + 1
        };
        let mut listing = self.store.list(Some(&self.path(dir)));
        let mut out = Vec::new();
        while let Some(meta) = listing.next().await {
            let meta = meta.with_context(|| format!("listing {dir}"))?;
            let full = meta.location.to_string();
            out.push((full.get(skip..).unwrap_or_default().to_string(), meta.size));
        }
        Ok(out)
    }

    async fn delete(&self, name: &str) -> anyhow::Result<()> {
        use object_store::ObjectStore;
        match self.store.delete(&self.path(name)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e).with_context(|| format!("deleting {name}")),
        }
    }
}

// ---------------------------------------------------------------- sftp

/// A directory on an SFTP server (a Hetzner Storage Box, a NAS, any OpenSSH).
/// One SSH connection per copy; requests on it run concurrently.
pub struct Sftp {
    sftp: russh_sftp::client::SftpSession,
    root: String,
    // Kept for the connection to stay open as long as the session is used.
    _ssh: russh::client::Handle<HostKeyCheck>,
}

pub struct HostKeyCheck {
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl russh::client::Handler for HostKeyCheck {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            // Host certificates need a CA to check them against: not supported.
            return Ok(false);
        };
        let seen = key.fingerprint(russh::keys::HashAlg::Sha256).to_string();
        let trusted = self.expected.as_ref().is_none_or(|e| *e == seen);
        *self.seen.lock() = Some(seen);
        Ok(trusted)
    }
}

/// `ssh-ed25519 AAAA…` (as in `known_hosts` or `ssh-keyscan`'s output, host
/// name first or not) or `SHA256:…` (as `ssh-keygen -lf` prints it), as a
/// SHA-256 fingerprint.
pub fn host_key_fingerprint(text: &str) -> anyhow::Result<String> {
    let text = text.trim();
    if text.starts_with("SHA256:") {
        return Ok(text.to_string());
    }
    let parts: Vec<&str> = text.split_whitespace().collect();
    // `host ssh-ed25519 AAAA…` as well as `ssh-ed25519 AAAA… comment`.
    let start = parts
        .iter()
        .position(|p| p.starts_with("ssh-") || p.starts_with("ecdsa-"))
        .ok_or_else(|| anyhow!("host_key: expected `ssh-ed25519 AAAA…` or `SHA256:…`"))?;
    let line = parts[start..].join(" ");
    let key = russh::keys::PublicKey::from_openssh(&line)
        .map_err(|e| anyhow!("host_key: not an SSH public key ({e})"))?;
    Ok(key.fingerprint(russh::keys::HashAlg::Sha256).to_string())
}

impl Sftp {
    async fn connect(
        s: &SftpSettings,
        known_host_key: Option<String>,
    ) -> anyhow::Result<(Self, String)> {
        let key = russh::keys::load_secret_key(&s.key_file, None)
            .map_err(|e| anyhow!("reading the key {}: {e}", s.key_file.display()))?;
        let config = Arc::new(russh::client::Config {
            inactivity_timeout: Some(Duration::from_secs(120)),
            keepalive_interval: Some(Duration::from_secs(30)),
            ..Default::default()
        });
        let expected = s.host_key.clone().or(known_host_key);
        let seen = Arc::new(Mutex::new(None));
        let check = HostKeyCheck {
            expected: expected.clone(),
            seen: seen.clone(),
        };
        let address = (s.host.as_str(), s.port);
        let connected = tokio::time::timeout(
            Duration::from_secs(30),
            russh::client::connect(config, address, check),
        )
        .await
        .map_err(|_| anyhow!("{}:{} did not answer within 30 s", s.host, s.port))?;
        let mut ssh = match connected {
            Ok(ssh) => ssh,
            Err(russh::Error::UnknownKey) => {
                let seen = seen.lock().clone().unwrap_or_default();
                bail!(
                    "{}'s host key is {seen}, not {} as {}: if the server really changed its key, \
                     set host_key in [backups.offsite]",
                    s.host,
                    expected.unwrap_or_default(),
                    if s.host_key.is_some() {
                        "configured"
                    } else {
                        "at the first connection"
                    }
                );
            }
            Err(e) => bail!("connecting to {}:{}: {e}", s.host, s.port),
        };
        let hash = ssh.best_supported_rsa_hash().await?.flatten();
        let auth = ssh
            .authenticate_publickey(
                &s.user,
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await
            .with_context(|| format!("logging in to {} as {}", s.host, s.user))?;
        if !auth.success() {
            bail!(
                "{} refused the key {} for {} (is its public half installed there?)",
                s.host,
                s.key_file.display(),
                s.user
            );
        }
        let channel = ssh.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = russh_sftp::client::SftpSession::new(channel.into_stream())
            .await
            .context("starting SFTP")?;
        sftp.set_timeout(60);
        let root = s.path.trim_end_matches('/').to_string();
        let seen = seen.lock().clone().unwrap_or_default();
        Ok((
            Self {
                sftp,
                root: if root.is_empty() { ".".into() } else { root },
                _ssh: ssh,
            },
            seen,
        ))
    }

    fn path(&self, name: &str) -> String {
        format!("{}/{name}", self.root)
    }

    /// Creates `dir` and its parents, those already there being fine.
    async fn create_dirs(&self, dir: &str) -> anyhow::Result<()> {
        let mut path = String::new();
        for part in dir.split('/') {
            if path.is_empty() && part.is_empty() {
                path.push('/');
                continue;
            }
            if !path.is_empty() && !path.ends_with('/') {
                path.push('/');
            }
            path.push_str(part);
            if part == "." || part.is_empty() {
                continue;
            }
            if let Err(e) = self.sftp.create_dir(path.as_str()).await {
                // Servers say "failure" rather than "exists": check instead.
                let there = self
                    .sftp
                    .metadata(path.as_str())
                    .await
                    .is_ok_and(|m| m.is_dir());
                if !there {
                    return Err(anyhow!("creating {path}: {e}"));
                }
            }
        }
        Ok(())
    }

    async fn write_new(
        &self,
        path: &str,
        data: &[u8],
    ) -> Result<(), russh_sftp::client::error::Error> {
        use russh_sftp::protocol::OpenFlags;
        use tokio::io::AsyncWriteExt;
        let mut file = self
            .sftp
            .open_with_flags(
                path,
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            )
            .await?;
        file.write_all(data).await?;
        file.shutdown().await?;
        Ok(())
    }

    async fn walk(
        &self,
        dir: &str,
        name: &str,
        out: &mut Vec<(String, u64)>,
    ) -> anyhow::Result<()> {
        let entries = match self.sftp.read_dir(dir).await {
            Ok(entries) => entries,
            Err(e) if is_not_found(&e) => return Ok(()),
            Err(e) => return Err(anyhow!("listing {dir}: {e}")),
        };
        for entry in entries {
            let file = entry.file_name();
            if file == "." || file == ".." {
                continue;
            }
            let meta = entry.metadata();
            if meta.is_dir() {
                Box::pin(self.walk(&format!("{dir}/{file}"), &format!("{name}/{file}"), out))
                    .await?;
            } else {
                out.push((format!("{name}/{file}"), meta.size.unwrap_or(0)));
            }
        }
        Ok(())
    }
}

fn is_not_found(e: &russh_sftp::client::error::Error) -> bool {
    matches!(
        e,
        russh_sftp::client::error::Error::Status(s)
            if s.status_code == russh_sftp::protocol::StatusCode::NoSuchFile
    )
}

#[async_trait]
impl Target for Sftp {
    async fn put(&self, name: &str, data: Vec<u8>) -> anyhow::Result<()> {
        let path = self.path(name);
        let dir = path.rsplit_once('/').map_or(".", |(d, _)| d).to_string();
        let temp = format!("{dir}/{TEMP_PREFIX}{}", uuid::Uuid::new_v4());
        if let Err(e) = self.write_new(&temp, &data).await {
            if !is_not_found(&e) {
                return Err(anyhow!("writing {path}: {e}"));
            }
            // The directory is not there yet.
            self.create_dirs(&dir).await?;
            self.write_new(&temp, &data)
                .await
                .map_err(|e| anyhow!("writing {path}: {e}"))?;
        }
        // SFTP's rename does not replace (OpenSSH's links, then unlinks), so an
        // object written again goes in two steps. Objects are only ever
        // written again with the same contents (sealed anew), so the moment
        // without one loses nothing.
        if self
            .sftp
            .rename(temp.as_str(), path.as_str())
            .await
            .is_err()
        {
            let _ = self.sftp.remove_file(path.as_str()).await;
            if let Err(e) = self.sftp.rename(temp.as_str(), path.as_str()).await {
                let _ = self.sftp.remove_file(temp.as_str()).await;
                return Err(anyhow!("writing {path}: {e}"));
            }
        }
        Ok(())
    }

    async fn get(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let path = self.path(name);
        match self.sftp.read(path.as_str()).await {
            Ok(data) => Ok(Some(data)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(anyhow!("reading {path}: {e}")),
        }
    }

    async fn list(&self, dir: &str) -> anyhow::Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        self.walk(&self.path(dir), dir, &mut out).await?;
        Ok(out)
    }

    async fn delete(&self, name: &str) -> anyhow::Result<()> {
        let path = self.path(name);
        match self.sftp.remove_file(path.as_str()).await {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(anyhow!("deleting {path}: {e}")),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// What every target must do, checked on a directory, and on SFTP and S3
    /// servers in the ignored tests of `offsite`.
    pub async fn exercise(target: &dyn Target) {
        assert_eq!(target.get("chunks/00/missing").await.unwrap(), None);
        assert!(target.list("chunks").await.unwrap().is_empty());
        target.put("chunks/ab/one", b"1".to_vec()).await.unwrap();
        target.put("chunks/ab/two", b"22".to_vec()).await.unwrap();
        target
            .put("chunks/cd/three", b"333".to_vec())
            .await
            .unwrap();
        target
            .put("volumes/blog/data/100.snapshot", b"m".to_vec())
            .await
            .unwrap();
        // Writing the same name again keeps one object.
        target.put("chunks/ab/one", b"1".to_vec()).await.unwrap();
        assert_eq!(
            target.get("chunks/ab/two").await.unwrap().as_deref(),
            Some(&b"22"[..])
        );
        let mut chunks = target.list("chunks").await.unwrap();
        chunks.sort();
        assert_eq!(
            chunks,
            [
                ("chunks/ab/one".to_string(), 1),
                ("chunks/ab/two".to_string(), 2),
                ("chunks/cd/three".to_string(), 3),
            ]
        );
        assert_eq!(
            target.list("volumes").await.unwrap(),
            [("volumes/blog/data/100.snapshot".to_string(), 1)]
        );
        target.delete("chunks/ab/one").await.unwrap();
        target.delete("chunks/ab/one").await.unwrap();
        assert_eq!(target.list("chunks").await.unwrap().len(), 2);
        for (name, _) in target.list("chunks").await.unwrap() {
            target.delete(&name).await.unwrap();
        }
        target
            .delete("volumes/blog/data/100.snapshot")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_local_directory() {
        let dir = std::env::temp_dir().join(format!("facade-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        exercise(&Local::new(dir.clone())).await;
        // Not mounted: an error rather than an empty listing, which would make
        // the copy upload everything again (to the disk underneath).
        let gone = Local::new(dir.join("not-mounted"));
        assert!(gone.list("chunks").await.is_err());
        assert!(gone.put("facade-offsite", vec![1]).await.is_err());
        assert!(!dir.join("not-mounted").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn host_keys() {
        let line =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
        let fp = host_key_fingerprint(line).unwrap();
        assert!(fp.starts_with("SHA256:"), "{fp}");
        assert_eq!(
            host_key_fingerprint(&format!("[u1.your-storagebox.de]:23 {line}")).unwrap(),
            fp
        );
        assert_eq!(
            host_key_fingerprint(&format!("{line} root@box")).unwrap(),
            fp
        );
        assert_eq!(host_key_fingerprint(&fp).unwrap(), fp);
        assert!(host_key_fingerprint("nonsense").is_err());
    }

    #[test]
    fn describes_targets() {
        let sftp = Provider::Sftp(SftpSettings {
            host: "u1.your-storagebox.de".into(),
            port: 23,
            user: "u1-sub2".into(),
            key_file: "/etc/facade/offsite_ed25519".into(),
            host_key: None,
            path: "facade".into(),
        });
        assert_eq!(
            sftp.describe(),
            "sftp://u1-sub2@u1.your-storagebox.de:23/facade"
        );
        let s3 = Provider::S3(S3Settings {
            endpoint: "https://fsn1.your-objectstorage.com/".into(),
            bucket: "backups".into(),
            region: "fsn1".into(),
            access_key_file: "/a".into(),
            secret_key_file: "/b".into(),
            prefix: "facade".into(),
        });
        assert_eq!(
            s3.describe(),
            "s3://backups/facade at https://fsn1.your-objectstorage.com"
        );
    }
}
