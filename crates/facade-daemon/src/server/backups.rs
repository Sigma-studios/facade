//! Scheduled copies of app volumes and of the server's own state.
//!
//! ```text
//! <data_dir>/backups/<app>/<volume>/<unix time>.snapshot   (see `backup_store`)
//! <data_dir>/backups/<app>/<volume>/<unix time>.tar.gz     full copies, from before
//! <data_dir>/backups/_chunks/…
//! <data_dir>/backups/_server/<unix time>/{facade.db,master.key}
//! ```
//!
//! `_` cannot start an app name, so the server's own directories never collide.
//!
//! Agents send a volume as a gzipped tar (what every agent version does); the
//! server unzips it as it arrives and chunks the tar, so nothing is buffered and
//! no protocol change was needed. Full copies stay listable, downloadable and
//! restorable, and count with incremental backups towards `keep`, so they are
//! deleted one by one as incremental ones arrive.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use bytes::Bytes;
use facade_core::api::{BackupCheckReport, BackupReport, BackupStoreView, BackupView};
use facade_core::proto::{AgentRequest, AppState};
use futures_util::Stream;
use tokio::sync::OwnedRwLockReadGuard;
use tracing::{debug, info, warn};

use super::Server;
use super::backup_store::{self, ChunkRef, FULL_SUFFIX, SNAPSHOT_SUFFIX, Store};
use crate::tls;

const CHECK_EVERY: Duration = Duration::from_secs(600);
const LAST_RUN_KEY: &str = "backups.last_run";
const SERVER_DIR: &str = "_server";

/// Under the data directory; the local agent reports what it takes.
pub const DIR: &str = "backups";

pub fn root(server: &Server) -> PathBuf {
    server.cfg.data_dir.join(DIR)
}

fn store(server: &Server) -> Store {
    Store::new(root(server))
}

pub async fn run(server: Arc<Server>) -> anyhow::Result<()> {
    let cfg = server.cfg.backups.clone();
    if !cfg.enabled {
        info!("scheduled backups are off ([backups] enabled = false)");
        return std::future::pending().await;
    }
    let every = cfg.every_hours.max(1) * 3600;
    // Give agents time to connect after a restart before the first run.
    tokio::time::sleep(Duration::from_secs(120)).await;
    loop {
        let last = server
            .store
            .kv_get(LAST_RUN_KEY)?
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        if facade_core::now().saturating_sub(last) >= every {
            if let Err(e) = backup_server(&server).await {
                warn!("backing up the server's database: {e:#}");
                server
                    .backup_errors
                    .write()
                    .insert(SERVER_DIR.into(), format!("{e:#}"));
            } else {
                server.backup_errors.write().remove(SERVER_DIR);
            }
            let report = backup_all(&server, None).await;
            info!(
                "scheduled backup: {} volume(s) saved, {} failed",
                report.done.len(),
                report.failed.len()
            );
            server
                .store
                .kv_put(LAST_RUN_KEY, &facade_core::now().to_string())?;
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

/// Backs up every volume of `app` (or of every app). Volumes of apps that never
/// ran have nothing in them yet and are skipped.
pub async fn backup_all(server: &Server, app: Option<&str>) -> BackupReport {
    let mut report = BackupReport {
        done: vec![],
        failed: vec![],
    };
    let apps = match server.store.list_apps() {
        Ok(apps) => apps,
        Err(e) => {
            report.failed.push(format!("listing apps: {e:#}"));
            return report;
        }
    };
    for row in apps {
        if app.is_some_and(|a| a != row.spec.name) || row.revision == 0 {
            continue;
        }
        let hooks = row.spec.backup.clone().unwrap_or_default();
        let running = !row.spec.volumes.is_empty()
            && server
                .live
                .read()
                .apps
                .get(&row.spec.name)
                .is_some_and(|a| a.state == AppState::Running);
        // The app makes what is on disk whole first; if it cannot, nothing is
        // copied rather than a copy that may not open.
        let before = match &hooks.before {
            Some(command) if running => run_hook(server, &row.spec, command)
                .await
                .map_err(|e| format!("the before command failed: {e:#}")),
            _ => Ok(()),
        };
        for volume in &row.spec.volumes {
            let key = format!("{}/{}", row.spec.name, volume.name);
            let copied = match &before {
                Ok(()) => backup_volume(server, &row.spec.name, &volume.name).await,
                Err(e) => Err(anyhow!("{e}")),
            };
            match copied {
                Ok(view) => {
                    server.backup_errors.write().remove(&key);
                    report.done.push(view);
                }
                Err(e) => {
                    warn!("backing up {key}: {e:#}");
                    server
                        .backup_errors
                        .write()
                        .insert(key.clone(), format!("{e:#}"));
                    report.failed.push(format!("{key}: {e:#}"));
                }
            }
        }
        if let Some(command) = hooks.after.as_ref().filter(|_| running) {
            let key = format!("{}/after", row.spec.name);
            match run_hook(server, &row.spec, command).await {
                Ok(()) => {
                    server.backup_errors.write().remove(&key);
                }
                Err(e) => {
                    let message = format!("the after command failed: {e:#}");
                    warn!("backing up {}: {message}", row.spec.name);
                    server
                        .backup_errors
                        .write()
                        .insert(key.clone(), message.clone());
                    report.failed.push(format!("{key}: {message}"));
                }
            }
        }
    }
    collect_garbage(server).await;
    report
}

/// Deletes the chunks no backup holds any more, unless the store is in use
/// (backups being taken, downloaded or restored): it then waits for the next
/// run, rather than holding them up.
pub async fn collect_garbage(server: &Server) {
    let Ok(alone) = server.backup_store_lock.clone().try_write_owned() else {
        debug!("backup store in use: unused chunks are left for the next run");
        return;
    };
    let store = store(server);
    let collected = tokio::task::spawn_blocking(move || {
        let _alone = alone;
        store.collect_garbage()
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|r| r);
    match collected {
        Ok(c) => {
            server.backup_errors.write().remove(STORE_KEY);
            if c.files > 0 {
                info!(
                    "backup store: deleted {} unused file(s), {} bytes",
                    c.files, c.bytes
                );
            }
        }
        Err(e) => {
            warn!("cleaning the backup store: {e:#}");
            server
                .backup_errors
                .write()
                .insert(STORE_KEY.into(), format!("{e:#}"));
        }
    }
}

/// Where a failed garbage collection is reported among backup errors.
const STORE_KEY: &str = "store";

/// Longest a backup command may run.
const HOOK_TIMEOUT_SECS: u32 = 600;

/// Runs one of the app's backup commands in its container.
async fn run_hook(
    server: &Server,
    spec: &facade_core::AppSpec,
    command: &str,
) -> anyhow::Result<()> {
    server
        .hub
        .request(
            &spec.node,
            AgentRequest::Exec {
                app: spec.name.clone(),
                command: command.to_string(),
                timeout_secs: HOOK_TIMEOUT_SECS,
            },
        )
        .await
}

pub async fn backup_volume(server: &Server, app: &str, volume: &str) -> anyhow::Result<BackupView> {
    let row = server
        .store
        .get_app(app)?
        .ok_or_else(|| anyhow!("no app named {app}"))?;
    let Some(declared) = row.spec.volumes.iter().find(|v| v.name == volume) else {
        bail!("{app} has no volume named {volume}");
    };
    let docker_name = row.spec.volume_name(declared);
    let id = facade_core::now();
    let shared = server.backup_store_lock.clone().read_owned().await;
    let stream = server
        .hub
        .open_volume_export(&row.spec.node, app, &docker_name)
        .await?;
    // Read from a blocking thread, which needs the runtime's handle.
    let input = tokio_util::io::SyncIoBridge::new(stream);
    let store = store(server);
    let (a, v) = (app.to_string(), volume.to_string());
    let header = tokio::task::spawn_blocking(move || {
        use std::io::BufRead;
        let _shared = shared;
        let mut input = std::io::BufReader::with_capacity(256 * 1024, input);
        if input.fill_buf()?.is_empty() {
            bail!("the node sent an empty archive (see its log)");
        }
        store.write_snapshot(&a, &v, id, flate2::bufread::GzDecoder::new(input))
    })
    .await??;
    prune(
        &root(server).join(app).join(volume),
        server.cfg.backups.keep.max(1),
    );
    Ok(BackupView {
        app: app.into(),
        volume: volume.into(),
        id,
        size: header.stored,
        added: Some(header.added),
    })
}

/// Copies the database (consistently, while the server runs) and the key that
/// decrypts its secrets.
pub async fn backup_server(server: &Server) -> anyhow::Result<PathBuf> {
    snapshot_state(&server.cfg.data_dir, &server.store, server.cfg.backups.keep)
}

/// Copies the database and master key of the server in `data_dir` to
/// `backups/_server/<time>`, keeping the `keep` newest copies.
pub fn snapshot_state(
    data_dir: &Path,
    store: &super::store::Store,
    keep: usize,
) -> anyhow::Result<PathBuf> {
    let base = data_dir.join(DIR).join(SERVER_DIR);
    create_dirs(&base)?;
    let dir = base.join(facade_core::now().to_string());
    if dir.exists() {
        return Ok(dir);
    }
    let partial = base.join(".partial");
    let _ = std::fs::remove_dir_all(&partial);
    tls::create_private_dir(&partial)?;
    store.snapshot(&partial.join("facade.db"))?;
    let key = std::fs::read(data_dir.join("master.key"))?;
    tls::write_private(&partial.join("master.key"), &key)?;
    std::fs::rename(&partial, &dir)?;
    prune(&base, keep.max(1));
    Ok(dir)
}

/// A gzipped tar of a fresh server backup (database and master key).
pub async fn server_archive(server: &Server) -> anyhow::Result<Vec<u8>> {
    let dir = backup_server(server).await?;
    tokio::task::spawn_blocking(move || {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        tar.append_dir_all("facade-server", &dir)?;
        Ok(tar.into_inner()?.finish()?)
    })
    .await?
}

/// Backups of `app` (or all), newest first.
pub fn list(server: &Server, app: Option<&str>) -> anyhow::Result<Vec<BackupView>> {
    let root = root(server);
    let mut out = Vec::new();
    let Ok(apps) = std::fs::read_dir(&root) else {
        return Ok(out);
    };
    for app_dir in apps.flatten() {
        let name = app_dir.file_name().to_string_lossy().to_string();
        if name.starts_with(['_', '.']) || app.is_some_and(|a| a != name) {
            continue;
        }
        for vol_dir in std::fs::read_dir(app_dir.path())?.flatten() {
            let volume = vol_dir.file_name().to_string_lossy().to_string();
            for file in std::fs::read_dir(vol_dir.path())?.flatten() {
                let file_name = file.file_name().to_string_lossy().to_string();
                let Some((id, kind)) = backup_id(&file_name) else {
                    continue;
                };
                let (size, added) = match kind {
                    Kind::Full => (file.metadata().map(|m| m.len()).unwrap_or(0), None),
                    Kind::Snapshot => match backup_store::read_header(&file.path()) {
                        Ok(header) => (header.stored, Some(header.added)),
                        Err(e) => {
                            warn!("{}: {e:#}", file.path().display());
                            continue;
                        }
                    },
                };
                out.push(BackupView {
                    app: name.clone(),
                    volume: volume.clone(),
                    id,
                    size,
                    added,
                });
            }
        }
    }
    out.sort_by(|a, b| b.id.cmp(&a.id).then(a.app.cmp(&b.app)));
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Full,
    Snapshot,
}

/// The id and kind of the backup in the file named `name`.
fn backup_id(name: &str) -> Option<(u64, Kind)> {
    let (id, kind) = if let Some(id) = name.strip_suffix(SNAPSHOT_SUFFIX) {
        (id, Kind::Snapshot)
    } else {
        (name.strip_suffix(FULL_SUFFIX)?, Kind::Full)
    };
    Some((id.parse().ok()?, kind))
}

/// One stored backup.
pub enum Stored {
    /// A gzipped tar.
    Full(PathBuf),
    /// The manifest of an incremental backup.
    Snapshot(PathBuf),
}

pub fn find(server: &Server, app: &str, volume: &str, id: u64) -> anyhow::Result<Stored> {
    facade_core::validate_app_name(app)?;
    facade_core::validate_name("volume name", volume)?;
    let snapshot = store(server).snapshot_path(app, volume, id);
    if snapshot.is_file() {
        return Ok(Stored::Snapshot(snapshot));
    }
    let full = root(server)
        .join(app)
        .join(volume)
        .join(format!("{id}{FULL_SUFFIX}"));
    if full.is_file() {
        return Ok(Stored::Full(full));
    }
    bail!("no backup {id} of {app}/{volume} (see `facade backup ls {app}`)");
}

/// The tar of an incremental backup and its size, gzipped or not, read from its
/// chunks on a blocking thread as the stream is polled. The store stays shared
/// until the stream ends or is dropped, so its chunks cannot be collected
/// meanwhile.
pub async fn snapshot_stream(
    server: &Server,
    manifest: &Path,
    gzip: bool,
) -> anyhow::Result<(
    u64,
    impl Stream<Item = std::io::Result<Bytes>> + Send + 'static,
)> {
    let shared = server.backup_store_lock.clone().read_owned().await;
    let store = store(server);
    let (header, chunks) = backup_store::read_manifest(manifest)?;
    store.check_present(&chunks)?;
    Ok((header.tar_size, stream_chunks(store, chunks, gzip, shared)))
}

fn stream_chunks(
    store: Store,
    chunks: Vec<ChunkRef>,
    gzip: bool,
    shared: OwnedRwLockReadGuard<()>,
) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    use std::io::Write;
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(4);
    tokio::task::spawn_blocking(move || {
        let _shared = shared;
        let out = std::io::BufWriter::with_capacity(256 * 1024, ChannelWriter(tx.clone()));
        let mut tar = store.reader(chunks);
        let written = if gzip {
            // Fast: at the default level, compressing is what limits a download.
            let mut gz = flate2::write::GzEncoder::new(out, flate2::Compression::fast());
            std::io::copy(&mut tar, &mut gz).and_then(|_| gz.finish()?.flush())
        } else {
            let mut out = out;
            std::io::copy(&mut tar, &mut out).and_then(|_| out.flush())
        };
        if let Err(e) = written {
            let _ = tx.blocking_send(Err(e));
        }
    });
    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
}

/// Hands what is written to the receiving end of a channel.
struct ChannelWriter(tokio::sync::mpsc::Sender<std::io::Result<Bytes>>);

impl std::io::Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .blocking_send(Ok(Bytes::copy_from_slice(buf)))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "nobody reads"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub async fn restore(server: &Server, app: &str, volume: &str, id: u64) -> anyhow::Result<()> {
    let stored = find(server, app, volume, id)?;
    let row = server
        .store
        .get_app(app)?
        .ok_or_else(|| anyhow!("no app named {app}"))?;
    let Some(declared) = row.spec.volumes.iter().find(|v| v.name == volume) else {
        bail!("{app} has no volume named {volume}");
    };
    let create = facade_core::proto::NewVolume {
        name: declared.name.clone(),
        stack: row.spec.stack.as_ref().map(|s| s.stack.clone()),
    };
    let docker_name = row.spec.volume_name(declared);
    let restored = match stored {
        Stored::Full(path) => {
            server
                .hub
                .import_volume(&row.spec.node, app, &docker_name, Some(create), &path)
                .await
        }
        // Agents take a plain tar as well as a gzipped one, and a plain one's size
        // is known before it is read.
        Stored::Snapshot(path) => {
            let (size, tar) = snapshot_stream(server, &path, false).await?;
            let tar = tokio_util::io::StreamReader::new(Box::pin(tar));
            server
                .hub
                .import_volume_from(&row.spec.node, app, &docker_name, Some(create), tar, size)
                .await
        }
    };
    restored.with_context(|| format!("restoring {app}/{volume} from backup {id}"))
}

/// What volume backups take on the server's disk.
pub async fn store_view(server: &Server) -> anyhow::Result<BackupStoreView> {
    let store = store(server);
    Ok(tokio::task::spawn_blocking(move || store.view()).await?)
}

/// Reads back every chunk of every incremental backup.
pub async fn check(server: &Server) -> anyhow::Result<BackupCheckReport> {
    let shared = server.backup_store_lock.clone().read_owned().await;
    let store = store(server);
    Ok(tokio::task::spawn_blocking(move || {
        let _shared = shared;
        store.check()
    })
    .await?)
}

fn create_dirs(dir: &Path) -> anyhow::Result<()> {
    let mut path = PathBuf::new();
    for part in dir.components() {
        path.push(part);
        if !path.exists() {
            tls::create_private_dir(&path)?;
        }
    }
    Ok(())
}

/// Deletes all but the `keep` newest entries named by a Unix time (full copies
/// and incremental backups alike). The chunks only deleted backups held go at the
/// next garbage collection.
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dated: Vec<(u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let id = match backup_id(&name) {
                Some((id, _)) => id,
                None => name.parse().ok()?,
            };
            Some((id, e.path()))
        })
        .collect();
    dated.sort_by_key(|d| std::cmp::Reverse(d.0));
    for (_, path) in dated.into_iter().skip(keep) {
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = removed {
            warn!("removing old backup {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prunes_oldest() {
        let dir = std::env::temp_dir().join(format!("facade-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for id in [100, 300, 200, 400] {
            std::fs::write(dir.join(format!("{id}.tar.gz")), b"x").unwrap();
        }
        for id in [500, 250] {
            std::fs::write(dir.join(format!("{id}.snapshot")), b"x").unwrap();
        }
        std::fs::write(dir.join(".600.partial"), b"x").unwrap();
        std::fs::write(dir.join(".tmp-1234"), b"x").unwrap();
        prune(&dir, 3);
        let mut left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                ".600.partial",
                ".tmp-1234",
                "300.tar.gz",
                "400.tar.gz",
                "500.snapshot"
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
