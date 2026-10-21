//! `POST /api/inspect`: clone a repository and say what could be deployed from it.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use axum::Json;
use axum::extract::State;
use facade_core::api::{InspectReport, InspectRequest};
use tokio::process::Command;

use super::Server;
use super::api::{ApiError, User};
use crate::lang;

pub async fn inspect(
    State(server): State<Arc<Server>>,
    _: User,
    Json(req): Json<InspectRequest>,
) -> Result<Json<InspectReport>, ApiError> {
    let dir = server
        .cfg
        .data_dir
        .join("inspect")
        .join(uuid::Uuid::new_v4().to_string());
    let result = async {
        let checkout = checkout(&server, &req.repo, req.branch.as_deref(), &dir).await?;
        let root = dir.clone();
        let (candidates, compose_files) = tokio::task::spawn_blocking(move || {
            (lang::detect_all(&root), super::compose::find(&root))
        })
        .await?;
        Ok::<_, anyhow::Error>(InspectReport {
            commit: checkout.commit,
            candidates,
            credential: checkout.credential,
            compose_files,
        })
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    result
        .map(Json)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))
}

pub struct Checkout {
    /// Short hash and subject.
    pub commit: String,
    /// Name of the git credential used, if any.
    pub credential: Option<String>,
}

/// A shallow clone of `repo` into `dir`, with the credential covering it.
pub async fn checkout(
    server: &Server,
    repo: &str,
    branch: Option<&str>,
    dir: &Path,
) -> anyhow::Result<Checkout> {
    let repo = repo.trim().to_string();
    if repo.is_empty() || repo.starts_with('-') {
        bail!("enter a git repository URL");
    }
    if facade_core::git::has_embedded_credentials(&repo) {
        bail!(
            "do not put credentials in the URL: add a git credential instead (Settings → Git access)"
        );
    }
    let branch = branch.map(str::trim).filter(|b| !b.is_empty());
    if branch.is_some_and(|b| b.starts_with('-')) {
        bail!("invalid branch");
    }
    tokio::fs::create_dir_all(dir.parent().expect("has parent")).await?;
    let (access, credential) = super::git::access(server, &repo)?;
    let mut clone = access.git();
    clone.args(["clone", "--depth", "1", "--single-branch"]);
    if let Some(branch) = branch {
        clone.args(["--branch", branch]);
    }
    clone.arg("--").arg(&access.url).arg(dir);
    crate::gitauth::run(clone, Duration::from_secs(90))
        .await
        .with_context(|| format!("cannot clone {repo}"))?;
    let commit = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["log", "-1", "--format=%h %s"])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    Ok(Checkout { commit, credential })
}
