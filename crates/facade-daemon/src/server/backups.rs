//! Scheduled copies of app volumes and of the server's own state.
//!
//! ```text
//! <data_dir>/backups/<app>/<volume>/<unix time>.tar.gz
//! <data_dir>/backups/_server/<unix time>/{facade.db,master.key}
//! ```
//!
//! `_` cannot start an app name, so the server's own directory never collides.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use facade_core::api::{BackupReport, BackupView};
use tracing::{info, warn};

use super::Server;
use crate::tls;

const CHECK_EVERY: Duration = Duration::from_secs(600);
const LAST_RUN_KEY: &str = "backups.last_run";
const SERVER_DIR: &str = "_server";

pub fn root(server: &Server) -> PathBuf {
    server.cfg.data_dir.join("backups")
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
        for volume in &row.spec.volumes {
            let key = format!("{}/{}", row.spec.name, volume.name);
            match backup_volume(server, &row.spec.name, &volume.name).await {
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
    }
    report
}

pub async fn backup_volume(server: &Server, app: &str, volume: &str) -> anyhow::Result<BackupView> {
    let row = server
        .store
        .get_app(app)?
        .ok_or_else(|| anyhow!("no app named {app}"))?;
    if !row.spec.volumes.iter().any(|v| v.name == volume) {
        bail!("{app} has no volume named {volume}");
    }
    let dir = root(server).join(app).join(volume);
    create_dirs(&dir)?;
    let id = facade_core::now();
    let path = dir.join(format!("{id}.tar.gz"));
    let partial = dir.join(format!(".{id}.partial"));
    let result = async {
        let mut stream = server
            .hub
            .open_volume_export(&row.spec.node, app, volume)
            .await?;
        let mut file = tls::private_file(&partial).await?;
        let size = tokio::io::copy(&mut stream, &mut file).await?;
        file.sync_all().await?;
        if size == 0 {
            bail!("the node sent an empty archive (see its log)");
        }
        tokio::fs::rename(&partial, &path).await?;
        anyhow::Ok(size)
    }
    .await;
    let size = match result {
        Ok(size) => size,
        Err(e) => {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(e);
        }
    };
    prune(&dir, server.cfg.backups.keep.max(1));
    Ok(BackupView {
        app: app.into(),
        volume: volume.into(),
        id,
        size,
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
    let base = data_dir.join("backups").join(SERVER_DIR);
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
        if name == SERVER_DIR || app.is_some_and(|a| a != name) {
            continue;
        }
        for vol_dir in std::fs::read_dir(app_dir.path())?.flatten() {
            let volume = vol_dir.file_name().to_string_lossy().to_string();
            for file in std::fs::read_dir(vol_dir.path())?.flatten() {
                let Some(id) = file
                    .file_name()
                    .to_str()
                    .and_then(|f| f.strip_suffix(".tar.gz"))
                    .and_then(|f| f.parse::<u64>().ok())
                else {
                    continue;
                };
                out.push(BackupView {
                    app: name.clone(),
                    volume: volume.clone(),
                    id,
                    size: file.metadata().map(|m| m.len()).unwrap_or(0),
                });
            }
        }
    }
    out.sort_by(|a, b| b.id.cmp(&a.id).then(a.app.cmp(&b.app)));
    Ok(out)
}

/// The file of one backup, if it exists.
pub fn path(server: &Server, app: &str, volume: &str, id: u64) -> anyhow::Result<PathBuf> {
    facade_core::validate_name("app name", app)?;
    facade_core::validate_name("volume name", volume)?;
    let path = root(server)
        .join(app)
        .join(volume)
        .join(format!("{id}.tar.gz"));
    if !path.is_file() {
        bail!("no backup {id} of {app}/{volume} (see `facade backup ls {app}`)");
    }
    Ok(path)
}

pub async fn restore(server: &Server, app: &str, volume: &str, id: u64) -> anyhow::Result<()> {
    let path = path(server, app, volume, id)?;
    let row = server
        .store
        .get_app(app)?
        .ok_or_else(|| anyhow!("no app named {app}"))?;
    if !row.spec.volumes.iter().any(|v| v.name == volume) {
        bail!("{app} has no volume named {volume}");
    }
    server
        .hub
        .import_volume(&row.spec.node, app, volume, &path)
        .await
        .with_context(|| format!("restoring {app}/{volume} from backup {id}"))
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

/// Deletes all but the `keep` newest entries named by a Unix time.
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dated: Vec<(u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let id = name.strip_suffix(".tar.gz").unwrap_or(&name).parse().ok()?;
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
        std::fs::write(dir.join(".500.partial"), b"x").unwrap();
        prune(&dir, 2);
        let mut left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(left, [".500.partial", "300.tar.gz", "400.tar.gz"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
