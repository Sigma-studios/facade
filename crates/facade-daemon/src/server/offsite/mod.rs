//! Offsite copies of the backups: the volume backups and the server's own
//! (database and master key), copied after each backup run to a target
//! elsewhere (`[backups.offsite]` in `server.toml`), encrypted on the way out
//! (see `crypt`), so that losing the server does not lose them.
//!
//! ```text
//! <target>/facade-offsite                          a sealed marker: which key the copy uses
//! <target>/chunks/<2 hex>/<64 hex>                 a chunk, sealed, named by a keyed hash
//! <target>/volumes/<app>/<volume>/<time>.snapshot  a backup's manifest, sealed
//! <target>/server/<time>.snapshot                  a server backup's manifest, sealed: the
//!                                                  tar of its database and master key, in
//!                                                  chunks like a volume's
//! <data_dir>/backups/_offsite/…                    what the target's manifests say, in clear,
//!                                                  so that listing and cleaning up the target
//!                                                  need no download
//! ```
//!
//! Chunks and manifests never change once written, so a copy sends what the
//! target lacks: the chunks of new backups that no older backup had, then their
//! manifests, last, so that a manifest on the target always has its chunks.
//! The same number of backups is kept as locally (or more, `keep`), oldest
//! deleted first; then the chunks no manifest left lists. A copy also uploads
//! again the chunks of local backups the target lost (a check found them
//! damaged), so a target heals by itself while the data is still local.
//!
//! One server writes to one target: two would delete each other's chunks.

pub mod crypt;
pub mod replique;
pub mod target;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use facade_core::api::{BackupView, OffsiteCopyView, OffsiteVerifyView, OffsiteView};
use futures_util::{StreamExt, TryStreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tracing::{info, warn};

use super::Server;
use super::backup_store::{self, ChunkId, ChunkRef, Header, Store};
use crate::config::{BackupConfig, OffsiteConfig, OffsiteProvider};
use crypt::Keys;
use target::{Provider, S3Settings, SftpSettings, TEMP_PREFIX, Target};

/// The marker naming which key the copy is sealed with.
const MARKER: &str = "facade-offsite";
const CHUNKS: &str = "chunks";
const VOLUMES: &str = "volumes";
const SERVER: &str = "server";
const SUFFIX: &str = ".snapshot";
/// Under the backups directory: the target's manifests, in clear.
pub const CACHE_DIR: &str = "_offsite";
/// The server's backups as they appear in a manifest's header.
const SERVER_APP: &str = "_server";
const SERVER_VOLUME: &str = "state";
/// Objects moving at once: enough to hide a far target's latency.
const PARALLEL: usize = 8;
/// A failed copy is tried again this long after, rather than a day later.
const RETRY_AFTER: u64 = 3600;
const TICK: Duration = Duration::from_secs(600);

const KV_STATE: &str = "backups.offsite.state";
const KV_HOST_KEY: &str = "backups.offsite.host_key";
const KV_KEY_SAVED: &str = "backups.offsite.key_saved";
/// Set when the server was rebuilt from its offsite copy: copies wait for the
/// volumes to be restored (see [`restore_server`]).
pub const KV_PAUSED: &str = "backups.offsite.paused";

// ---------------------------------------------------------------- settings

/// `[backups.offsite]`, checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub provider: Provider,
    /// Backups kept offsite, per volume and of the server.
    pub keep: usize,
    pub verify_every: u64,
    pub verify_sample: usize,
    pub stale_after: u64,
}

impl Settings {
    pub fn from_config(cfg: &OffsiteConfig, backups: &BackupConfig) -> anyhow::Result<Self> {
        let Some(kind) = cfg.provider else {
            bail!("provider is required: \"sftp\", \"s3\" or \"local\"");
        };
        let name = match kind {
            OffsiteProvider::Sftp => "sftp",
            OffsiteProvider::S3 => "s3",
            OffsiteProvider::Local => "local",
        };
        let sftp_only = [
            ("host", cfg.host.is_some()),
            ("port", cfg.port.is_some()),
            ("user", cfg.user.is_some()),
            ("key_file", cfg.key_file.is_some()),
            ("host_key", cfg.host_key.is_some()),
        ];
        let s3_only = [
            ("endpoint", cfg.endpoint.is_some()),
            ("bucket", cfg.bucket.is_some()),
            ("region", cfg.region.is_some()),
            ("access_key_file", cfg.access_key_file.is_some()),
            ("secret_key_file", cfg.secret_key_file.is_some()),
        ];
        let not_for = |settings: &[(&str, bool)]| -> anyhow::Result<()> {
            match settings.iter().find(|(_, set)| *set) {
                Some((setting, _)) => bail!("{setting} is not a setting of the {name} provider"),
                None => Ok(()),
            }
        };
        let required = |setting: &str, value: &Option<String>| -> anyhow::Result<String> {
            value
                .clone()
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| anyhow!("{setting} is required with the {name} provider"))
        };
        let provider = match kind {
            OffsiteProvider::Sftp => {
                not_for(&s3_only)?;
                Provider::Sftp(SftpSettings {
                    host: required("host", &cfg.host)?,
                    port: cfg.port.unwrap_or(22),
                    user: required("user", &cfg.user)?,
                    key_file: cfg.key_file.clone().ok_or_else(|| {
                        anyhow!("key_file is required with the sftp provider (facade creates it if missing)")
                    })?,
                    host_key: cfg
                        .host_key
                        .as_deref()
                        .map(target::host_key_fingerprint)
                        .transpose()?,
                    path: cfg.path.clone().unwrap_or_else(|| "facade".into()),
                })
            }
            OffsiteProvider::S3 => {
                not_for(&sftp_only)?;
                let endpoint = required("endpoint", &cfg.endpoint)?;
                if !endpoint.starts_with("https://") && !endpoint.starts_with("http://") {
                    bail!("endpoint must start with https:// (or http:// for a local test server)");
                }
                Provider::S3(S3Settings {
                    endpoint,
                    bucket: required("bucket", &cfg.bucket)?,
                    region: cfg.region.clone().unwrap_or_else(|| "us-east-1".into()),
                    access_key_file: cfg.access_key_file.clone().ok_or_else(|| {
                        anyhow!("access_key_file is required with the s3 provider")
                    })?,
                    secret_key_file: cfg.secret_key_file.clone().ok_or_else(|| {
                        anyhow!("secret_key_file is required with the s3 provider")
                    })?,
                    prefix: cfg
                        .path
                        .as_deref()
                        .unwrap_or("")
                        .trim_matches('/')
                        .to_string(),
                })
            }
            OffsiteProvider::Local => {
                not_for(&sftp_only)?;
                not_for(&s3_only)?;
                let path = PathBuf::from(required("path", &cfg.path)?);
                if !path.is_absolute() {
                    bail!("path must be absolute with the local provider");
                }
                Provider::Local(path)
            }
        };
        let local = backups.keep.max(1);
        let keep = cfg.keep.unwrap_or(local);
        if keep < local {
            bail!(
                "keep = {keep} is fewer than [backups] keep = {local}: offsite keeps at least as many backups"
            );
        }
        Ok(Self {
            provider,
            keep,
            verify_every: cfg.verify_every_hours.max(1) * 3600,
            verify_sample: cfg.verify_sample,
            stale_after: cfg
                .stale_after_hours
                .unwrap_or(2 * backups.every_hours.max(1))
                .max(1)
                * 3600,
        })
    }
}

fn settings(server: &Server) -> Option<Settings> {
    let cfg = server.cfg.backups.offsite.as_ref()?;
    // Checked when the configuration was loaded.
    Settings::from_config(cfg, &server.cfg.backups).ok()
}

// ---------------------------------------------------------------- state

/// What the server knows of its offsite copy, kept across restarts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    /// When offsite copies were first configured: a copy that never worked is
    /// stale from then.
    since: Option<u64>,
    last_copy: Option<OffsiteCopyView>,
    last_success: Option<u64>,
    last_verify: Option<OffsiteVerifyView>,
}

#[derive(Default)]
pub struct Offsite {
    state: Mutex<State>,
    /// One copy or check at a time.
    busy: tokio::sync::Mutex<()>,
    copying: AtomicBool,
    /// A backup run finished since the last copy started.
    wanted: AtomicBool,
    wake: Notify,
    /// What the copy in progress has done, for its report.
    pub progress: Progress,
    /// Reports to Réplique not delivered yet.
    pub replique: replique::Reporter,
}

impl Offsite {
    /// Asks for a copy, after backups were taken.
    pub fn backups_taken(&self) {
        self.wanted.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }
}

#[derive(Default)]
pub struct Progress {
    pub chunks_total: AtomicU64,
    pub chunks_done: AtomicU64,
    pub bytes_total: AtomicU64,
    pub bytes_done: AtomicU64,
    pub phase: Mutex<&'static str>,
}

impl Progress {
    fn reset(&self) {
        for counter in [
            &self.chunks_total,
            &self.chunks_done,
            &self.bytes_total,
            &self.bytes_done,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
        *self.phase.lock() = "listing the target";
    }

    fn planned(&self, chunks: u64, bytes: u64) {
        self.chunks_total.fetch_add(chunks, Ordering::Relaxed);
        self.bytes_total.fetch_add(bytes, Ordering::Relaxed);
    }

    fn sent(&self, bytes: u64) {
        self.chunks_done.fetch_add(1, Ordering::Relaxed);
        self.bytes_done.fetch_add(bytes, Ordering::Relaxed);
    }
}

fn save_state(server: &Server) {
    let json = serde_json::to_string(&*server.offsite.state.lock()).unwrap_or_default();
    if let Err(e) = server.store.kv_put(KV_STATE, &json) {
        warn!("saving the offsite copy's state: {e:#}");
    }
}

// ---------------------------------------------------------------- names

fn chunk_object(name: &str) -> String {
    format!("{CHUNKS}/{}/{name}", &name[..2])
}

fn volume_object(app: &str, volume: &str, id: u64) -> String {
    format!("{VOLUMES}/{app}/{volume}/{id}{SUFFIX}")
}

fn server_object(id: u64) -> String {
    format!("{SERVER}/{id}{SUFFIX}")
}

/// What a manifest's name on the target says: `None` for anything else (an
/// interrupted upload's temporary file).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Manifest {
    Volume {
        app: String,
        volume: String,
        id: u64,
    },
    Server {
        id: u64,
    },
}

impl Manifest {
    fn parse(name: &str) -> Option<Self> {
        let parts: Vec<&str> = name.split('/').collect();
        match parts.as_slice() {
            [VOLUMES, app, volume, file] => {
                let id = file.strip_suffix(SUFFIX)?.parse().ok()?;
                // Names become paths in the cache: nothing like `..` gets there.
                facade_core::validate_app_name(app).ok()?;
                facade_core::validate_name("volume name", volume).ok()?;
                Some(Manifest::Volume {
                    app: app.to_string(),
                    volume: volume.to_string(),
                    id,
                })
            }
            [SERVER, file] => Some(Manifest::Server {
                id: file.strip_suffix(SUFFIX)?.parse().ok()?,
            }),
            _ => None,
        }
    }

    fn object(&self) -> String {
        match self {
            Manifest::Volume { app, volume, id } => volume_object(app, volume, *id),
            Manifest::Server { id } => server_object(*id),
        }
    }

    /// Backups are kept per volume, and the server's together.
    fn group(&self) -> (String, String) {
        match self {
            Manifest::Volume { app, volume, .. } => (app.clone(), volume.clone()),
            Manifest::Server { .. } => (SERVER_APP.into(), SERVER_VOLUME.into()),
        }
    }

    fn id(&self) -> u64 {
        match self {
            Manifest::Volume { id, .. } | Manifest::Server { id } => *id,
        }
    }

    fn label(&self) -> String {
        match self {
            Manifest::Volume { app, volume, id } => format!("{app}/{volume} {id}"),
            Manifest::Server { id } => format!("server {id}"),
        }
    }
}

// ---------------------------------------------------------------- the engine

/// What a copy, check or restore works with. Free of [`Server`], so that tests
/// run it on directories.
pub struct Job<'a> {
    /// The local backup store.
    pub store: &'a Store,
    /// `backups/_server`: the server's own backups, one directory each.
    pub server_dir: PathBuf,
    /// `backups/_offsite`.
    pub cache: PathBuf,
    pub keys: &'a Keys,
    pub target: &'a dyn Target,
    pub keep: usize,
    pub progress: &'a Progress,
}

/// What the target holds.
#[derive(Default)]
struct Remote {
    /// Chunk name → size.
    chunks: HashMap<String, u64>,
    /// Manifests (their chunks are in the cache) → size.
    manifests: BTreeMap<Manifest, u64>,
    /// Interrupted uploads.
    stray: Vec<String>,
    /// Manifests that would not open: their chunks would look unused, so
    /// nothing is deleted while there are any.
    unreadable: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct CopyReport {
    pub uploaded_chunks: u64,
    pub uploaded_bytes: u64,
    /// Backups copied in this run.
    pub copied: Vec<String>,
    /// Backups deleted from the target (retention).
    pub removed: Vec<String>,
    pub chunks_deleted: u64,
    pub bytes_deleted: u64,
    /// What the target holds afterwards.
    pub backups: u64,
    pub chunks: u64,
    pub size: u64,
    /// The manifests that would not open.
    pub unreadable: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct VerifyReport {
    pub verified: u64,
    /// Chunks listed by the target's manifests.
    pub chunks: u64,
    pub manifests: Vec<String>,
    pub problems: Vec<String>,
}

impl Job<'_> {
    fn cache_path(&self, manifest: &Manifest) -> PathBuf {
        self.cache.join(manifest.object())
    }

    /// Writes the marker on a new target, or checks the target's opens with
    /// this key: a copy never mixes two keys on one target.
    pub async fn check_marker(&self) -> anyhow::Result<()> {
        match self.target.get(MARKER).await? {
            Some(sealed) => {
                self.keys.open(MARKER, &sealed).map_err(|_| {
                    anyhow!(
                        "the target holds an offsite copy sealed with another key: \
                         put that key in {} or point [backups.offsite] somewhere else",
                        crypt::KEY_FILE
                    )
                })?;
            }
            None => {
                let marker = serde_json::json!({ "format": 1, "created": facade_core::now() });
                let sealed = self.keys.seal(MARKER, marker.to_string().as_bytes());
                self.target.put(MARKER, sealed).await?;
            }
        }
        Ok(())
    }

    /// Lists the target, and brings the cache in line with its manifests.
    async fn remote(&self) -> anyhow::Result<Remote> {
        let mut remote = Remote::default();
        for (name, size) in self.target.list(CHUNKS).await? {
            let file = name.rsplit('/').next().unwrap_or_default();
            if backup_store::parse_id(file).is_some() {
                remote.chunks.insert(file.to_string(), size);
            } else if file.starts_with(TEMP_PREFIX) {
                remote.stray.push(name);
            }
        }
        let mut listed = self.target.list(VOLUMES).await?;
        listed.extend(self.target.list(SERVER).await?);
        for (name, size) in listed {
            match Manifest::parse(&name) {
                Some(m) => {
                    remote.manifests.insert(m, size);
                }
                None if name
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .starts_with(TEMP_PREFIX) =>
                {
                    remote.stray.push(name)
                }
                None => {}
            }
        }
        // Manifests the cache lacks (all of them on a rebuilt server).
        let missing: Vec<Manifest> = remote
            .manifests
            .keys()
            .filter(|m| self.cached(m).is_err())
            .cloned()
            .collect();
        let fetched: Vec<(Manifest, anyhow::Result<()>)> = futures_util::stream::iter(missing)
            .map(|m| async move {
                let result = self.fetch_manifest(&m).await;
                (m, result)
            })
            .buffer_unordered(PARALLEL)
            .collect()
            .await;
        for (m, result) in fetched {
            if let Err(e) = result {
                remote.unreadable.push(format!("{}: {e:#}", m.label()));
            }
        }
        // And what the target does not have any more.
        for cached in self.cached_manifests() {
            if !remote.manifests.contains_key(&cached) {
                let _ = std::fs::remove_file(self.cache_path(&cached));
            }
        }
        Ok(remote)
    }

    async fn fetch_manifest(&self, m: &Manifest) -> anyhow::Result<()> {
        let object = m.object();
        let sealed = self
            .target
            .get(&object)
            .await?
            .ok_or_else(|| anyhow!("{object} disappeared"))?;
        let plain = self.keys.open(&object, &sealed)?;
        let (header, chunks) = backup_store::parse_manifest(&plain[..])?;
        backup_store::write_manifest_file(&self.cache_path(m), &header, &chunks)
    }

    fn cached(&self, m: &Manifest) -> anyhow::Result<(Header, Vec<ChunkRef>)> {
        backup_store::read_manifest(&self.cache_path(m))
    }

    fn cached_manifests(&self) -> Vec<Manifest> {
        list_cached(&self.cache)
    }

    /// Local backups: volume manifests, and the server's directories.
    fn local(&self) -> BTreeMap<Manifest, PathBuf> {
        let mut out = BTreeMap::new();
        for (app, volume, path) in self.store.snapshots() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if let Some(id) = name
                .strip_suffix(backup_store::SNAPSHOT_SUFFIX)
                .and_then(|id| id.parse().ok())
            {
                out.insert(Manifest::Volume { app, volume, id }, path);
            }
        }
        if let Ok(dirs) = std::fs::read_dir(&self.server_dir) {
            for dir in dirs.flatten() {
                if let Some(id) = dir.file_name().to_str().and_then(|n| n.parse().ok()) {
                    out.insert(Manifest::Server { id }, dir.path());
                }
            }
        }
        out
    }

    /// Uploads the chunks in `todo` that the target lacks. `read` gives a
    /// chunk's file contents (compressed).
    async fn upload_chunks<F, Fut>(
        &self,
        todo: Vec<ChunkRef>,
        remote: &mut Remote,
        report: &mut CopyReport,
        read: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(ChunkRef) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<Vec<u8>>>,
    {
        let mut seen = HashSet::new();
        let todo: Vec<(ChunkRef, String)> = todo
            .into_iter()
            .map(|c| (c, self.keys.chunk_name(&c.id)))
            .filter(|(_, name)| !remote.chunks.contains_key(name) && seen.insert(name.clone()))
            .collect();
        self.progress.planned(
            todo.len() as u64,
            todo.iter().map(|(c, _)| c.stored as u64).sum(),
        );
        let read = &read;
        let sent: Vec<(String, u64)> = futures_util::stream::iter(todo)
            .map(|(chunk, name)| async move {
                let object = chunk_object(&name);
                let data = read(chunk).await?;
                let sealed = self.keys.seal(&object, &data);
                let size = sealed.len() as u64;
                self.target.put(&object, sealed).await?;
                self.progress.sent(chunk.stored as u64);
                anyhow::Ok((name, size))
            })
            .buffer_unordered(PARALLEL)
            .try_collect()
            .await?;
        for (name, size) in sent {
            report.uploaded_chunks += 1;
            report.uploaded_bytes += size;
            remote.chunks.insert(name, size);
        }
        Ok(())
    }

    async fn put_manifest(
        &self,
        m: &Manifest,
        header: &Header,
        chunks: &[ChunkRef],
        remote: &mut Remote,
    ) -> anyhow::Result<()> {
        let object = m.object();
        let sealed = self
            .keys
            .seal(&object, &backup_store::manifest_bytes(header, chunks));
        let size = sealed.len() as u64;
        self.target.put(&object, sealed).await?;
        backup_store::write_manifest_file(&self.cache_path(m), header, chunks)?;
        remote.manifests.insert(m.clone(), size);
        Ok(())
    }

    /// Copies a volume backup: the chunks the target lacks, then the manifest.
    async fn copy_volume(
        &self,
        m: &Manifest,
        path: &Path,
        remote: &mut Remote,
        report: &mut CopyReport,
    ) -> anyhow::Result<()> {
        let (header, chunks) = backup_store::read_manifest(path)?;
        self.upload_chunks(chunks.clone(), remote, report, |chunk| {
            let store = self.store.clone();
            async move { tokio::task::spawn_blocking(move || store.read_chunk_file(&chunk)).await? }
        })
        .await?;
        if !remote.manifests.contains_key(m) {
            self.put_manifest(m, &header, &chunks, remote).await?;
            report.copied.push(m.label());
        }
        Ok(())
    }

    /// Whether some chunks of a manifest on the target are missing there (a
    /// check found them damaged).
    fn lacks_chunks(&self, m: &Manifest, remote: &Remote) -> bool {
        self.cached(m).is_ok_and(|(_, chunks)| {
            chunks
                .iter()
                .any(|c| !remote.chunks.contains_key(&self.keys.chunk_name(&c.id)))
        })
    }

    /// Copies a server backup: its database and key as one tar, cut into chunks
    /// like a volume's (the database changes little from one day to the next,
    /// so most of its chunks are on the target already).
    async fn copy_server(
        &self,
        id: u64,
        dir: &Path,
        remote: &mut Remote,
        report: &mut CopyReport,
    ) -> anyhow::Result<()> {
        let dir = dir.to_path_buf();
        // In memory: the database of one server is tens of megabytes.
        let (header, chunks, data) =
            tokio::task::spawn_blocking(move || server_chunks(id, &dir)).await??;
        let data = Arc::new(data);
        self.upload_chunks(chunks.clone(), remote, report, |chunk| {
            let data = data.clone();
            async move {
                data.get(&chunk.id)
                    .cloned()
                    .ok_or_else(|| anyhow!("chunk not cut"))
            }
        })
        .await?;
        let m = Manifest::Server { id };
        if !remote.manifests.contains_key(&m) {
            self.put_manifest(&m, &header, &chunks, remote).await?;
            report.copied.push(m.label());
        }
        Ok(())
    }

    /// Brings the target up to date with the local backups.
    pub async fn copy(&self) -> anyhow::Result<CopyReport> {
        self.progress.reset();
        self.check_marker().await?;
        let mut remote = self.remote().await?;
        let local = self.local();
        let mut report = CopyReport::default();

        // The newest `keep` of each volume (and of the server), wherever they are.
        let mut groups: BTreeMap<(String, String), BTreeSet<u64>> = BTreeMap::new();
        for m in local.keys().chain(remote.manifests.keys()) {
            groups.entry(m.group()).or_default().insert(m.id());
        }
        let wanted = |m: &Manifest| {
            groups
                .get(&m.group())
                .is_some_and(|ids| ids.iter().rev().take(self.keep).any(|id| *id == m.id()))
        };

        *self.progress.phase.lock() = "uploading chunks";
        // Newest first: if the copy is cut short, the latest backups made it.
        let mut todo: Vec<(&Manifest, &PathBuf)> =
            local.iter().filter(|(m, _)| wanted(m)).collect();
        todo.sort_by_key(|(m, _)| std::cmp::Reverse(m.id()));
        for (m, path) in todo {
            let copied = match m {
                Manifest::Volume { .. } => {
                    self.copy_volume(m, path, &mut remote, &mut report).await
                }
                Manifest::Server { id }
                    if !remote.manifests.contains_key(m) || self.lacks_chunks(m, &remote) =>
                {
                    self.copy_server(*id, path, &mut remote, &mut report).await
                }
                Manifest::Server { .. } => Ok(()),
            };
            copied.with_context(|| format!("copying {}", m.label()))?;
        }

        *self.progress.phase.lock() = "deleting old backups";
        let old: Vec<Manifest> = remote
            .manifests
            .keys()
            .filter(|m| !wanted(m))
            .cloned()
            .collect();
        for m in old {
            self.target.delete(&m.object()).await?;
            let _ = std::fs::remove_file(self.cache_path(&m));
            remote.manifests.remove(&m);
            report.removed.push(m.label());
        }

        report.unreadable = remote.unreadable.clone();
        if remote.unreadable.is_empty() {
            let mut used = HashSet::new();
            for m in remote.manifests.keys() {
                let (_, chunks) = self.cached(m)?;
                used.extend(chunks.iter().map(|c| self.keys.chunk_name(&c.id)));
            }
            let unused: Vec<(String, u64)> = remote
                .chunks
                .iter()
                .filter(|(name, _)| !used.contains(*name))
                .map(|(n, s)| (n.clone(), *s))
                .collect();
            for (name, size) in unused {
                self.target.delete(&chunk_object(&name)).await?;
                remote.chunks.remove(&name);
                report.chunks_deleted += 1;
                report.bytes_deleted += size;
            }
            for name in &remote.stray {
                self.target.delete(name).await?;
            }
        }
        report.backups = remote.manifests.len() as u64;
        report.chunks = remote.chunks.len() as u64;
        report.size = remote.chunks.values().sum::<u64>() + remote.manifests.values().sum::<u64>();
        Ok(report)
    }

    /// Checks every manifest's chunks are on the target, and reads back
    /// `sample` of them (all when `None`), checking each against its hash. A
    /// damaged chunk is deleted, so that the next copy uploads it again while the
    /// data is still local.
    pub async fn verify(&self, sample: Option<usize>) -> anyhow::Result<VerifyReport> {
        self.check_marker().await?;
        let remote = self.remote().await?;
        let mut report = VerifyReport {
            problems: remote.unreadable.clone(),
            ..Default::default()
        };
        // Each chunk once, with the backups holding it.
        let mut holders: BTreeMap<String, (ChunkRef, Vec<String>)> = BTreeMap::new();
        for m in remote.manifests.keys() {
            report.manifests.push(m.label());
            let (_, chunks) = self.cached(m)?;
            for chunk in chunks {
                let entry = holders
                    .entry(self.keys.chunk_name(&chunk.id))
                    .or_insert((chunk, Vec::new()));
                if entry.1.last() != Some(&m.label()) {
                    entry.1.push(m.label());
                }
            }
        }
        report.chunks = holders.len() as u64;
        let mut present = Vec::new();
        for (name, (chunk, held_by)) in &holders {
            if remote.chunks.contains_key(name) {
                present.push((name.clone(), *chunk, held_by.clone()));
            } else {
                report.problems.push(format!(
                    "chunk {} is missing, in {}",
                    hex::encode(chunk.id),
                    held_by.join(", ")
                ));
            }
        }
        if let Some(n) = sample {
            use rand::seq::SliceRandom;
            present.shuffle(&mut rand::rng());
            present.truncate(n);
        }
        let results: Vec<(String, ChunkRef, Vec<String>, anyhow::Result<()>)> =
            futures_util::stream::iter(present)
                .map(|(name, chunk, held_by)| async move {
                    let object = chunk_object(&name);
                    let checked = async {
                        let sealed =
                            self.target.get(&object).await?.ok_or_else(|| {
                                anyhow!("chunk {} is missing", hex::encode(chunk.id))
                            })?;
                        let plain = self.keys.open(&object, &sealed)?;
                        backup_store::check_chunk(&chunk, &plain)?;
                        anyhow::Ok(())
                    }
                    .await;
                    (name, chunk, held_by, checked)
                })
                .buffer_unordered(PARALLEL)
                .collect()
                .await;
        for (name, _, held_by, checked) in results {
            match checked {
                Ok(()) => report.verified += 1,
                Err(e) => {
                    let mut problem = format!("{e:#}, in {}", held_by.join(", "));
                    if self.target.delete(&chunk_object(&name)).await.is_ok() {
                        problem += " (deleted: the next copy uploads it again if the backup is still on the server)";
                    }
                    report.problems.push(problem);
                }
            }
        }
        Ok(report)
    }

    /// Brings backup `id` of `app`/`volume` back into the local store from the
    /// target: its manifest, and the chunks the store lacks, each checked.
    pub async fn fetch_volume(&self, app: &str, volume: &str, id: u64) -> anyhow::Result<Header> {
        let object = volume_object(app, volume, id);
        let sealed = self
            .target
            .get(&object)
            .await?
            .ok_or_else(|| anyhow!("no backup {id} of {app}/{volume} offsite"))?;
        let plain = self.keys.open(&object, &sealed)?;
        let (header, chunks) = backup_store::parse_manifest(&plain[..])?;
        if header.app != app || header.volume != volume || header.id != id {
            bail!(
                "{object} holds another backup ({}/{} {})",
                header.app,
                header.volume,
                header.id
            );
        }
        let missing: Vec<ChunkRef> = {
            let mut seen = HashSet::new();
            chunks
                .iter()
                .filter(|c| !self.store.chunk_path(&c.id).is_file() && seen.insert(c.id))
                .copied()
                .collect()
        };
        self.progress.planned(
            missing.len() as u64,
            missing.iter().map(|c| c.stored as u64).sum(),
        );
        let to_sync = Arc::new(Mutex::new(BTreeSet::new()));
        futures_util::stream::iter(missing)
            .map(|chunk| {
                let to_sync = to_sync.clone();
                async move {
                    let object = chunk_object(&self.keys.chunk_name(&chunk.id));
                    let sealed = self.target.get(&object).await?.ok_or_else(|| {
                        anyhow!("chunk {} is missing offsite", hex::encode(chunk.id))
                    })?;
                    let plain = self.keys.open(&object, &sealed)?;
                    let store = self.store.clone();
                    tokio::task::spawn_blocking(move || {
                        let mut dirs = BTreeSet::new();
                        store.put_compressed_chunk(&chunk, &plain, &mut dirs)?;
                        to_sync.lock().extend(dirs);
                        anyhow::Ok(())
                    })
                    .await??;
                    self.progress.sent(chunk.stored as u64);
                    anyhow::Ok(())
                }
            })
            .buffer_unordered(PARALLEL)
            .try_collect::<Vec<()>>()
            .await?;
        let to_sync = to_sync.lock().clone();
        let store = self.store.clone();
        let written = header.clone();
        tokio::task::spawn_blocking(move || store.write_manifest(&written, &chunks, &to_sync))
            .await??;
        Ok(header)
    }

    /// The server backups on the target, newest last.
    pub async fn server_backups(&self) -> anyhow::Result<Vec<u64>> {
        let mut ids: Vec<u64> = self
            .target
            .list(SERVER)
            .await?
            .into_iter()
            .filter_map(|(name, _)| match Manifest::parse(&name) {
                Some(Manifest::Server { id }) => Some(id),
                _ => None,
            })
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// The files of server backup `id` (`facade.db`, `master.key`), from the
    /// target, written into `into`.
    pub async fn fetch_server(&self, id: u64, into: &Path) -> anyhow::Result<Vec<String>> {
        let object = server_object(id);
        let sealed = self
            .target
            .get(&object)
            .await?
            .ok_or_else(|| anyhow!("no server backup {id} offsite"))?;
        let plain = self.keys.open(&object, &sealed)?;
        let (_, chunks) = backup_store::parse_manifest(&plain[..])?;
        let parts: Vec<Vec<u8>> = futures_util::stream::iter(chunks)
            .map(|chunk| async move {
                let object = chunk_object(&self.keys.chunk_name(&chunk.id));
                let sealed =
                    self.target.get(&object).await?.ok_or_else(|| {
                        anyhow!("chunk {} is missing offsite", hex::encode(chunk.id))
                    })?;
                backup_store::check_chunk(&chunk, &self.keys.open(&object, &sealed)?)
            })
            .buffered(PARALLEL)
            .try_collect()
            .await?;
        let into = into.to_path_buf();
        tokio::task::spawn_blocking(move || unpack_server(&parts.concat(), &into)).await?
    }
}

/// The names a server backup's tar may hold, and nothing else is unpacked.
const SERVER_FILES: [&str; 2] = ["facade.db", "master.key"];

/// A server backup cut into chunks: its manifest's header and chunks, and each
/// chunk's file contents by id.
type ServerChunks = (Header, Vec<ChunkRef>, HashMap<ChunkId, Vec<u8>>);

/// The tar of a server backup's directory, cut into chunks.
fn server_chunks(id: u64, dir: &Path) -> anyhow::Result<ServerChunks> {
    let mut tar = tar::Builder::new(Vec::new());
    tar.mode(tar::HeaderMode::Deterministic);
    for name in SERVER_FILES {
        tar.append_path_with_name(dir.join(name), name)
            .with_context(|| format!("reading {}", dir.join(name).display()))?;
    }
    let tar = tar.into_inner()?;
    let mut header = Header {
        format: backup_store::FORMAT,
        app: SERVER_APP.into(),
        volume: SERVER_VOLUME.into(),
        id,
        tar_size: tar.len() as u64,
        stored: 0,
        added: 0,
        chunks: 0,
    };
    let mut chunks = Vec::new();
    let mut data = HashMap::new();
    backup_store::cut(&tar[..], |plain| {
        let id: ChunkId = blake3::hash(&plain).into();
        let compressed = backup_store::compress(&plain)?;
        let chunk = ChunkRef {
            id,
            len: plain.len() as u32,
            stored: compressed.len() as u32,
        };
        header.stored += chunk.stored as u64;
        chunks.push(chunk);
        data.insert(id, compressed);
        Ok(())
    })?;
    header.chunks = chunks.len();
    Ok((header, chunks, data))
}

fn unpack_server(tar: &[u8], into: &Path) -> anyhow::Result<Vec<String>> {
    crate::tls::create_private_dir(into)?;
    let mut archive = tar::Archive::new(tar);
    let mut out = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().to_string();
        if !SERVER_FILES.contains(&name.as_str()) {
            bail!("the server backup holds {name:?}, which is not one of its files");
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        crate::tls::write_private(&into.join(&name), &data)?;
        out.push(name);
    }
    Ok(out)
}

/// The manifests in a cache directory.
fn list_cached(cache: &Path) -> Vec<Manifest> {
    let mut files = Vec::new();
    walk(cache, "", &mut files);
    files.iter().filter_map(|n| Manifest::parse(n)).collect()
}

/// Files under `dir`, as names relative to it (`a/b/c`).
fn walk(dir: &Path, name: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().to_string();
        let child = if name.is_empty() {
            file
        } else {
            format!("{name}/{file}")
        };
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            walk(&entry.path(), &child, out);
        } else {
            out.push(child);
        }
    }
}

// ---------------------------------------------------------------- the server's side

fn key_path(data_dir: &Path) -> PathBuf {
    data_dir.join(crypt::KEY_FILE)
}

fn cache_dir(server: &Server) -> PathBuf {
    super::backups::root(server).join(CACHE_DIR)
}

/// Connects to the configured target, keeping its SSH host key the first time.
async fn connect(server: &Server, settings: &Settings) -> anyhow::Result<Arc<dyn Target>> {
    let known = server.store.kv_get(KV_HOST_KEY)?;
    let (target, seen) = target::connect(&settings.provider, known.clone()).await?;
    if let (None, Some(seen)) = (known, seen) {
        info!(
            "offsite copy: {} has host key {seen}, kept from now on",
            settings.provider.describe()
        );
        server.store.kv_put(KV_HOST_KEY, &seen)?;
    }
    Ok(target)
}

/// At start: the state, the key (created the first time), the SSH key of an
/// sftp target; then copies after backups, retries, and checks, until the end.
pub async fn run(server: Arc<Server>) {
    let Some(settings) = settings(&server) else {
        return;
    };
    if let Ok(Some(json)) = server.store.kv_get(KV_STATE) {
        *server.offsite.state.lock() = serde_json::from_str(&json).unwrap_or_default();
    }
    if server.offsite.state.lock().since.is_none() {
        server.offsite.state.lock().since = Some(facade_core::now());
        save_state(&server);
    }
    match Keys::load_or_create(&key_path(&server.cfg.data_dir)) {
        Ok((_, true)) => warn!(
            "offsite copy: a key was generated in {}; without it the copy cannot be read: \
             see it with `facade backup offsite key` and keep it somewhere else",
            key_path(&server.cfg.data_dir).display()
        ),
        Ok(_) => {}
        Err(e) => warn!("offsite copy: {e:#}"),
    }
    if let Provider::Sftp(s) = &settings.provider
        && let Err(e) = ensure_ssh_key(&s.key_file)
    {
        warn!("offsite copy: {e:#}");
    }
    info!("offsite copies go to {}", settings.provider.describe());
    loop {
        tokio::select! {
            _ = server.offsite.wake.notified() => {}
            _ = tokio::time::sleep(TICK) => {}
        }
        if paused(&server).is_some() {
            continue;
        }
        let now = facade_core::now();
        let (last_copy, last_success, last_verify) = {
            let state = server.offsite.state.lock();
            (
                state.last_copy.clone(),
                state.last_success,
                state.last_verify.as_ref().map(|v| v.at),
            )
        };
        let retry = last_copy
            .as_ref()
            .is_some_and(|c| c.error.is_some() && now.saturating_sub(c.at) >= RETRY_AFTER);
        if server.offsite.wanted.load(Ordering::SeqCst) || retry || last_copy.is_none() {
            let _ = copy(&server).await;
        } else if last_success.is_some()
            && now.saturating_sub(last_verify.unwrap_or(0)) >= settings.verify_every
        {
            let _ = verify(&server, false).await;
        }
    }
}

/// Creates the SSH key an sftp target is logged in to with, when there is
/// none: its public half (next to it, `.pub`) goes on the target.
fn ensure_ssh_key(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut seed = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut seed);
    let keypair = ssh_key::private::Ed25519Keypair::from_seed(&seed);
    let key = ssh_key::PrivateKey::new(
        ssh_key::private::KeypairData::Ed25519(keypair),
        "facade offsite copy",
    )?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let private = key.to_openssh(ssh_key::LineEnding::LF)?;
    crate::tls::write_private(path, private.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    let public = key.public_key().to_openssh()?;
    std::fs::write(path.with_extension("pub"), format!("{public}\n"))?;
    info!(
        "offsite copy: created the SSH key {}; install its public half on the target: {public}",
        path.display()
    );
    Ok(())
}

/// The public half of an sftp target's key, to install there.
fn public_key(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let key = ssh_key::PrivateKey::from_openssh(text.trim()).ok()?;
    key.public_key().to_openssh().ok()
}

fn paused(server: &Server) -> Option<String> {
    server.store.kv_get(KV_PAUSED).ok().flatten()
}

/// Copies to the target now (unless a copy or check is running), recording how
/// it went, and reporting it to Réplique when configured.
pub async fn copy(server: &Arc<Server>) -> anyhow::Result<OffsiteCopyView> {
    let settings =
        settings(server).ok_or_else(|| anyhow!("no [backups.offsite] in server.toml"))?;
    let Ok(_busy) = server.offsite.busy.try_lock() else {
        bail!("an offsite copy or check is already running");
    };
    if let Some(why) = paused(server) {
        bail!("offsite copies are paused: {why} (facade backup offsite resume)");
    }
    server.offsite.wanted.store(false, Ordering::SeqCst);
    server.offsite.copying.store(true, Ordering::SeqCst);
    let started = Instant::now();
    let at = facade_core::now();
    let run = replique::Run::start(server, replique::RunKind::Backup, at);
    let progress_task = run.as_ref().map(|run| run.follow(server.clone()));

    let result = async {
        let keys = Keys::load(&key_path(&server.cfg.data_dir))?;
        let target = connect(server, &settings).await?;
        // Chunks a copy reads cannot be collected meanwhile.
        let _shared = server.backup_store_lock.clone().read_owned().await;
        let store = Store::new(super::backups::root(server));
        let job = Job {
            store: &store,
            server_dir: super::backups::root(server).join(super::backups::SERVER_DIR),
            cache: cache_dir(server),
            keys: &keys,
            target: target.as_ref(),
            keep: settings.keep,
            progress: &server.offsite.progress,
        };
        job.copy().await
    }
    .await;
    server.offsite.copying.store(false, Ordering::SeqCst);
    if let Some(task) = progress_task {
        task.abort();
    }

    let secs = started.elapsed().as_secs();
    let previous = server.offsite.state.lock().last_copy.clone();
    let view = match &result {
        Ok(r) => {
            let mut error = None;
            if !r.unreadable.is_empty() {
                error = Some(format!(
                    "{} manifest(s) on the target would not open, so nothing was cleaned up there: {}",
                    r.unreadable.len(),
                    r.unreadable[0]
                ));
            }
            OffsiteCopyView {
                at,
                secs,
                error,
                uploaded_chunks: r.uploaded_chunks,
                uploaded_bytes: r.uploaded_bytes,
                copied: r.copied.len() as u64,
                removed: r.removed.len() as u64,
                backups: r.backups,
                chunks: r.chunks,
                size: r.size,
            }
        }
        // What the target held stays what it was last seen to hold.
        Err(e) => OffsiteCopyView {
            at,
            secs,
            error: Some(format!("{e:#}")),
            backups: previous.as_ref().map_or(0, |p| p.backups),
            chunks: previous.as_ref().map_or(0, |p| p.chunks),
            size: previous.as_ref().map_or(0, |p| p.size),
            ..Default::default()
        },
    };
    match &result {
        Ok(r) => info!(
            "offsite copy: {} backup(s) copied, {} chunk(s) uploaded ({} bytes), {} backup(s) and {} chunk(s) deleted, in {secs} s",
            r.copied.len(),
            r.uploaded_chunks,
            r.uploaded_bytes,
            r.removed.len(),
            r.chunks_deleted
        ),
        Err(e) => warn!("offsite copy to {}: {e:#}", settings.provider.describe()),
    }
    {
        let mut state = server.offsite.state.lock();
        state.last_copy = Some(view.clone());
        if result.is_ok() {
            state.last_success = Some(at);
        }
    }
    save_state(server);
    if let Some(run) = run {
        run.finish_copy(server, &result, at, secs, settings.keep);
    }
    result.map(|_| view)
}

/// Reads back a sample of the offsite chunks (all of them with `all`).
pub async fn verify(server: &Arc<Server>, all: bool) -> anyhow::Result<OffsiteVerifyView> {
    let settings =
        settings(server).ok_or_else(|| anyhow!("no [backups.offsite] in server.toml"))?;
    let Ok(_busy) = server.offsite.busy.try_lock() else {
        bail!("an offsite copy or check is already running");
    };
    let at = facade_core::now();
    let run = replique::Run::start(server, replique::RunKind::Check, at);
    let sample = (!all).then_some(settings.verify_sample);
    let result = async {
        let keys = Keys::load(&key_path(&server.cfg.data_dir))?;
        let target = connect(server, &settings).await?;
        let store = Store::new(super::backups::root(server));
        let job = Job {
            store: &store,
            server_dir: super::backups::root(server).join(super::backups::SERVER_DIR),
            cache: cache_dir(server),
            keys: &keys,
            target: target.as_ref(),
            keep: settings.keep,
            progress: &server.offsite.progress,
        };
        job.verify(sample).await
    }
    .await;
    let view = match &result {
        Ok(r) => OffsiteVerifyView {
            at,
            verified: r.verified,
            chunks: r.chunks,
            problems: r.problems.clone(),
        },
        Err(e) => OffsiteVerifyView {
            at,
            problems: vec![format!("{e:#}")],
            ..Default::default()
        },
    };
    if view.problems.is_empty() {
        info!(
            "offsite check: {} of {} chunk(s) read back intact",
            view.verified, view.chunks
        );
    } else {
        warn!("offsite check: {:?}", view.problems);
    }
    server.offsite.state.lock().last_verify = Some(view.clone());
    save_state(server);
    if let Some(run) = run {
        run.finish_check(server, &result, at, sample);
    }
    Ok(view)
}

/// Brings an offsite backup back into the local store, for a restore or a
/// download.
pub async fn fetch(server: &Server, app: &str, volume: &str, id: u64) -> anyhow::Result<()> {
    let settings =
        settings(server).ok_or_else(|| anyhow!("no [backups.offsite] in server.toml"))?;
    let keys = Keys::load(&key_path(&server.cfg.data_dir))?;
    let target = connect(server, &settings).await?;
    let _shared = server.backup_store_lock.clone().read_owned().await;
    let store = Store::new(super::backups::root(server));
    let progress = Progress::default();
    let job = Job {
        store: &store,
        server_dir: super::backups::root(server).join(super::backups::SERVER_DIR),
        cache: cache_dir(server),
        keys: &keys,
        target: target.as_ref(),
        keep: settings.keep,
        progress: &progress,
    };
    job.fetch_volume(app, volume, id).await?;
    info!(
        "{app}/{volume} {id} fetched from the offsite copy ({} chunk(s))",
        progress.chunks_done.load(Ordering::Relaxed)
    );
    Ok(())
}

/// The offsite backups of `app` (or all), from what the last copy or check saw
/// on the target.
pub fn list(server: &Server, app: Option<&str>) -> Vec<BackupView> {
    if server.cfg.backups.offsite.is_none() {
        return Vec::new();
    }
    let cache = cache_dir(server);
    let mut files = Vec::new();
    walk(&cache.join(VOLUMES), VOLUMES, &mut files);
    let mut out = Vec::new();
    for name in files {
        let Some(Manifest::Volume {
            app: owner,
            volume,
            id,
        }) = Manifest::parse(&name)
        else {
            continue;
        };
        if app.is_some_and(|a| a != owner) {
            continue;
        }
        if let Ok(header) = backup_store::read_header(&cache.join(&name)) {
            out.push(BackupView {
                app: owner,
                volume,
                id,
                size: header.stored,
                added: Some(header.added),
                offsite: true,
                offsite_only: true,
            });
        }
    }
    out
}

pub fn view(server: &Server) -> Option<OffsiteView> {
    let settings = settings(server)?;
    let state = server.offsite.state.lock().clone();
    let now = facade_core::now();
    let stale = is_stale(&state, &settings, now);
    let (public_key, host_key) = match &settings.provider {
        Provider::Sftp(s) => (
            public_key(&s.key_file),
            s.host_key
                .clone()
                .or_else(|| server.store.kv_get(KV_HOST_KEY).ok().flatten()),
        ),
        _ => (None, None),
    };
    Some(OffsiteView {
        target: settings.provider.describe(),
        provider: settings.provider.name().into(),
        keep: settings.keep,
        verify_every_hours: settings.verify_every / 3600,
        verify_sample: settings.verify_sample,
        stale_after_hours: settings.stale_after / 3600,
        last_copy: state.last_copy,
        last_success: state.last_success,
        last_verify: state.last_verify,
        stale,
        copying: server.offsite.copying.load(Ordering::SeqCst),
        progress: server.offsite.copying.load(Ordering::SeqCst).then(|| {
            let p = &server.offsite.progress;
            (
                p.bytes_done.load(Ordering::Relaxed),
                p.bytes_total.load(Ordering::Relaxed),
            )
        }),
        paused: paused(server),
        key_saved: key_saved(server),
        public_key,
        host_key,
        reports_to: server.cfg.backups.replique.as_ref().map(|r| r.url.clone()),
    })
}

fn is_stale(state: &State, settings: &Settings, now: u64) -> bool {
    let since = state.last_success.or(state.since).unwrap_or(now);
    now.saturating_sub(since) > settings.stale_after
}

fn key_saved(server: &Server) -> bool {
    server.store.kv_get(KV_KEY_SAVED).ok().flatten().is_some()
}

/// The key, while the user has not said it is kept somewhere else: after that
/// it is never shown again (it stays readable by root on the server).
pub fn reveal_key(server: &Server) -> anyhow::Result<Option<String>> {
    if server.cfg.backups.offsite.is_none() {
        bail!("no [backups.offsite] in server.toml");
    }
    if key_saved(server) {
        return Ok(None);
    }
    let (keys, _) = Keys::load_or_create(&key_path(&server.cfg.data_dir))?;
    Ok(Some(keys.hex().to_string()))
}

pub fn key_is_saved(server: &Server) -> anyhow::Result<()> {
    server
        .store
        .kv_put(KV_KEY_SAVED, &facade_core::now().to_string())
}

pub fn resume(server: &Server) -> anyhow::Result<()> {
    server.store.kv_delete(KV_PAUSED)?;
    server.offsite.backups_taken();
    Ok(())
}

/// What is wrong with the offsite copy, as alerts (they reach Discord).
pub fn conditions(server: &Server, out: &mut HashMap<String, String>) {
    let Some(settings) = settings(server) else {
        return;
    };
    if paused(server).is_some() {
        return;
    }
    let state = server.offsite.state.lock().clone();
    let target = settings.provider.describe();
    let failed = state.last_copy.as_ref().and_then(|c| c.error.clone());
    if let Some(error) = &failed {
        out.insert(
            "offsite:copy".into(),
            format!("offsite copy to {target} failed: {error}"),
        );
    }
    // A failing copy is its own alert: one cause, one row.
    if failed.is_none() && is_stale(&state, &settings, facade_core::now()) {
        let message = match state.last_success {
            Some(at) => format!(
                "offsite copy to {target} is {} hours old",
                facade_core::now().saturating_sub(at) / 3600
            ),
            None => format!("no offsite copy to {target} has worked yet"),
        };
        out.insert("offsite:stale".into(), message);
    }
    if let Some(v) = &state.last_verify
        && let Some(first) = v.problems.first()
    {
        out.insert(
            "offsite:verify".into(),
            format!(
                "offsite check of {target} found {} problem(s): {first}",
                v.problems.len()
            ),
        );
    }
}

/// Standing problems no alert covers: the key not kept anywhere else yet,
/// copies paused after a rebuild.
pub fn notices(server: &Server) -> (bool, Option<String>) {
    if server.cfg.backups.offsite.is_none() {
        return (false, None);
    }
    (!key_saved(server), paused(server))
}

// ---------------------------------------------------------------- rebuilding a server

/// `facaded offsite-restore`: on a new machine, before facade first starts,
/// brings back the server's database and master key from the offsite copy
/// `[backups.offsite]` points at, sealed with `key`. Offsite copies are paused
/// in the restored database: the apps come back with empty volumes, and their
/// backups must not push the real ones out of the target before the volumes
/// are restored from it.
pub async fn restore_server(
    cfg: &crate::config::ServerConfig,
    key: &str,
    id: Option<u64>,
    list_only: bool,
    force: bool,
) -> anyhow::Result<()> {
    let offsite = cfg
        .backups
        .offsite
        .as_ref()
        .ok_or_else(|| anyhow!("no [backups.offsite] in {}", cfg.path.display()))?;
    let settings = Settings::from_config(offsite, &cfg.backups)?;
    let keys = Keys::parse(key)?;
    let (target, seen) = target::connect(&settings.provider, None).await?;
    if let Some(seen) = &seen {
        println!("{} has host key {seen}", settings.provider.describe());
    }
    let store = Store::new(cfg.data_dir.join(super::backups::DIR));
    let progress = Progress::default();
    let job = Job {
        store: &store,
        server_dir: cfg
            .data_dir
            .join(super::backups::DIR)
            .join(super::backups::SERVER_DIR),
        cache: cfg.data_dir.join(super::backups::DIR).join(CACHE_DIR),
        keys: &keys,
        target: target.as_ref(),
        keep: settings.keep,
        progress: &progress,
    };
    job.check_marker()
        .await
        .context("is this the key of this copy?")?;
    let ids = job.server_backups().await?;
    if list_only {
        if ids.is_empty() {
            println!("no server backups on the target");
        }
        for id in ids.iter().rev() {
            println!("{id}  {}", facade_core::rfc3339(*id));
        }
        return Ok(());
    }
    let id = match id {
        Some(id) if ids.contains(&id) => id,
        Some(id) => bail!("no server backup {id} on the target (--list shows them)"),
        None => *ids
            .last()
            .ok_or_else(|| anyhow!("no server backups on the target"))?,
    };
    let db = cfg.data_dir.join("facade.db");
    if db.exists() && !force {
        bail!(
            "{} exists: this server has a database already (stop facaded and add --force to replace it; it is kept as facade.db.before-restore)",
            db.display()
        );
    }
    crate::tls::create_private_dir(&cfg.data_dir)?;
    let staging = cfg.data_dir.join(".offsite-restore");
    let _ = std::fs::remove_dir_all(&staging);
    let files = job.fetch_server(id, &staging).await?;
    if files.len() != SERVER_FILES.len() {
        bail!("server backup {id} is incomplete ({files:?})");
    }
    for name in SERVER_FILES {
        let dest = cfg.data_dir.join(name);
        if dest.exists() {
            std::fs::rename(&dest, cfg.data_dir.join(format!("{name}.before-restore")))?;
        }
        // The database's write-ahead log belongs to the database replaced.
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(cfg.data_dir.join(format!("{name}{suffix}")));
        }
        std::fs::rename(staging.join(name), &dest)?;
    }
    let _ = std::fs::remove_dir_all(&staging);
    crate::tls::write_private(
        &key_path(&cfg.data_dir),
        format!("{}\n", keys.hex()).as_bytes(),
    )?;
    let restored = super::store::Store::open(&db)?;
    restored.kv_put(
        KV_PAUSED,
        &format!(
            "the server was rebuilt from server backup {id} on {}; restore the volumes first",
            facade_core::rfc3339(facade_core::now())
        ),
    )?;
    // The user just typed the key in: it is kept somewhere else.
    restored.kv_put(KV_KEY_SAVED, &facade_core::now().to_string())?;
    if let Some(seen) = seen {
        restored.kv_put(KV_HOST_KEY, &seen)?;
    }
    println!(
        "restored the database and master key of {} into {}; offsite copies are paused until \
         the volumes are restored (facade backup restore <app> <volume> latest --from offsite), \
         then `facade backup offsite resume`",
        facade_core::rfc3339(id),
        cfg.data_dir.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests;
