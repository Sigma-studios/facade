//! Uploaded executables, stored by SHA-256 under `data_dir/artifacts` and fetched
//! by the agents that run them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Json;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use facade_core::Source;
use facade_core::api::ArtifactView;
use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use super::Server;
use super::api::{ApiError, User};
use crate::lang::executable::elf_arch;

/// Uploads bigger than this are refused.
pub const MAX_UPLOAD: usize = 1024 * 1024 * 1024;

pub fn dir(server: &Server) -> PathBuf {
    server.cfg.data_dir.join("artifacts")
}

pub fn path(server: &Server, sha256: &str) -> Option<PathBuf> {
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(dir(server).join(sha256))
}

pub fn exists(server: &Server, sha256: &str) -> bool {
    path(server, sha256).is_some_and(|p| p.is_file())
}

#[derive(Deserialize)]
pub struct UploadQuery {
    filename: String,
}

/// `POST /api/artifacts?filename=…` with the raw file as body.
pub async fn upload(
    State(server): State<Arc<Server>>,
    _: User,
    Query(query): Query<UploadQuery>,
    body: Body,
) -> Result<(StatusCode, Json<ArtifactView>), ApiError> {
    let filename = sanitize(&query.filename);
    let dir = dir(&server);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(anyhow::Error::from)?;
    let partial = dir.join(format!(".upload-{}", uuid::Uuid::new_v4()));
    let result = receive(body, &partial).await;
    let (sha256, size, header) = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(ApiError::bad_request(format!("{e:#}")));
        }
    };
    let arch = match elf_arch(&header) {
        Ok(arch) => arch,
        Err(e) => {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(ApiError::bad_request(e));
        }
    };
    tokio::fs::rename(&partial, dir.join(&sha256))
        .await
        .map_err(anyhow::Error::from)?;
    info!(%filename, %arch, size, "executable uploaded");
    Ok((
        StatusCode::CREATED,
        Json(ArtifactView {
            sha256,
            filename,
            arch: arch.to_string(),
            size,
        }),
    ))
}

/// Streams the body to `dest`, returning its hash, size and first bytes.
async fn receive(body: Body, dest: &Path) -> anyhow::Result<(String, u64, Vec<u8>)> {
    let mut file = tokio::fs::File::create(dest).await?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut header = Vec::with_capacity(64);
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("upload interrupted")?;
        size += chunk.len() as u64;
        if size > MAX_UPLOAD as u64 {
            anyhow::bail!("file is larger than {} MB", MAX_UPLOAD / 1024 / 1024);
        }
        if header.len() < 64 {
            let take = (64 - header.len()).min(chunk.len());
            header.extend_from_slice(&chunk[..take]);
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    if size == 0 {
        anyhow::bail!("the file is empty");
    }
    Ok((hex::encode(hasher.finalize()), size, header))
}

/// A file name usable as a path component and command name.
fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let clean: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let clean = clean.trim_start_matches('.').to_string();
    if clean.is_empty() {
        "app".into()
    } else {
        clean
    }
}

/// Executables used by apps on `node`.
pub fn used_by_node(server: &Server, node: &str) -> anyhow::Result<Vec<String>> {
    Ok(server
        .store
        .list_apps()?
        .into_iter()
        .filter(|a| a.spec.node == node)
        .filter_map(|a| match a.spec.source {
            Source::Executable { sha256, .. } => Some(sha256),
            _ => None,
        })
        .collect())
}

/// Deletes uploads no app uses, a day after they were uploaded (so a file uploaded
/// for an app being created is not removed under it).
pub async fn collect_garbage(server: Arc<Server>) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tick.tick().await;
        let used: Vec<String> = match server.store.list_apps() {
            Ok(apps) => apps
                .into_iter()
                .filter_map(|a| match a.spec.source {
                    Source::Executable { sha256, .. } => Some(sha256),
                    _ => None,
                })
                .collect(),
            Err(e) => {
                warn!("artifact cleanup: {e:#}");
                continue;
            }
        };
        let Ok(mut entries) = tokio::fs::read_dir(dir(&server)).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            let old = entry
                .metadata()
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(86_400));
            if old && !used.contains(&name) {
                let _ = tokio::fs::remove_file(entry.path()).await;
                info!("removed unused upload {name}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize("C:\\builds\\my server"), "my_server");
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize(".hidden"), "hidden");
        assert_eq!(sanitize(""), "app");
    }
}
