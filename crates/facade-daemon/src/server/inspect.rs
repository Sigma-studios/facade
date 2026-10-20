//! `POST /api/inspect`: clone a repository and say what could be deployed from it.

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
    let repo = req.repo.trim().to_string();
    if repo.is_empty() || repo.starts_with('-') {
        return Err(ApiError::bad_request("enter a git repository URL"));
    }
    let branch = req.branch.filter(|b| !b.trim().is_empty());
    if branch.as_deref().is_some_and(|b| b.starts_with('-')) {
        return Err(ApiError::bad_request("invalid branch"));
    }
    let dir = server
        .cfg
        .data_dir
        .join("inspect")
        .join(uuid::Uuid::new_v4().to_string());
    let result = scan(&repo, branch.as_deref(), &dir).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    result
        .map(Json)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))
}

async fn scan(
    repo: &str,
    branch: Option<&str>,
    dir: &std::path::Path,
) -> anyhow::Result<InspectReport> {
    tokio::fs::create_dir_all(dir.parent().expect("has parent")).await?;
    let mut clone = Command::new("git");
    clone.args(["clone", "--depth", "1", "--single-branch"]);
    if let Some(branch) = branch {
        clone.args(["--branch", branch]);
    }
    clone
        .arg("--")
        .arg(repo)
        .arg(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(90), clone.output())
        .await
        .context("cloning took longer than 90 seconds")?
        .context("running git (is it installed on the server?)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("git clone failed");
        bail!(
            "cannot clone {repo}: {}",
            reason.trim_start_matches("fatal: ")
        );
    }
    let commit = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["log", "-1", "--format=%h %s"])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let root = dir.to_path_buf();
    let candidates = tokio::task::spawn_blocking(move || lang::detect_all(&root)).await?;
    Ok(InspectReport { commit, candidates })
}
