//! Fetching and building sources. Shells out to `git` and `docker build`: the
//! Docker CLI brings BuildKit (cache mounts) which the raw API does not.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, bail};
use facade_core::proto::Workload;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::DeployLog;
use crate::gitauth;

pub async fn git_checkout(
    repo: &str,
    branch: Option<&str>,
    dest: &Path,
    w: &Workload,
    log: &DeployLog,
) -> anyhow::Result<()> {
    log.line(format!(
        "cloning {repo} ({})",
        branch.unwrap_or("default branch")
    ));
    let mut cmd = Command::new("git");
    cmd.args(["clone", "--depth", "1", "--single-branch"]);
    if let Some(branch) = branch {
        cmd.args(["--branch", branch]);
    }
    cmd.arg("--")
        .arg(repo)
        .arg(dest)
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(gitauth::git_env(repo, &w.credentials));
    run_logged(cmd, log).await.context("git clone failed")?;

    let out = Command::new("git")
        .arg("-C")
        .arg(dest)
        .args(["log", "-1", "--format=%H %s"])
        .output()
        .await?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (hash, subject) = line.split_once(' ').unwrap_or((&line, ""));
    log.line(format!(
        "at commit {} {subject}",
        hash.get(..7).unwrap_or(hash)
    ));
    if !hash.is_empty() {
        log.set_commit(hash.to_string());
    }
    Ok(())
}

pub async fn docker_build(
    app: &str,
    tag: &str,
    dockerfile: &Path,
    context: &Path,
    w: &Workload,
    log: &DeployLog,
) -> anyhow::Result<()> {
    if !dockerfile.is_file() {
        bail!(
            "no Dockerfile at {} (set build.dockerfile, or use another build kind)",
            dockerfile.display()
        );
    }
    log.line(format!("building {tag}"));
    let mut cmd = Command::new("docker");
    cmd.args(["build", "--progress=plain", "--pull", "-t", tag])
        .args(["--label", &format!("facade.app={app}")])
        .arg("-f")
        .arg(dockerfile)
        .arg(context)
        .env("DOCKER_BUILDKIT", "1");
    // Registry logins go in a config directory of their own, outside the build
    // context so they can never be copied into an image.
    let config_dir = match gitauth::docker_config(&w.credentials) {
        Some(config) => {
            let dir = context
                .parent()
                .unwrap_or(context)
                .join(format!(".docker-{}", uuid::Uuid::new_v4()));
            crate::tls::create_private_dir(&dir)?;
            crate::tls::write_private(&dir.join("config.json"), config.as_bytes())?;
            // buildx and its plugins are found through the config directory too.
            if let Some(home) = std::env::var_os("HOME") {
                let plugins = Path::new(&home).join(".docker/cli-plugins");
                if plugins.is_dir() {
                    let _ = std::os::unix::fs::symlink(plugins, dir.join("cli-plugins"));
                }
            }
            cmd.env("DOCKER_CONFIG", &dir);
            Some(dir)
        }
        None => None,
    };
    let result = run_logged(cmd, log).await.context("docker build failed");
    if let Some(dir) = config_dir {
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
    result
}

/// Runs `cmd`, forwarding stdout and stderr line by line to the deploy log.
async fn run_logged(mut cmd: Command, log: &DeployLog) -> anyhow::Result<()> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running {:?}", cmd.as_std().get_program()))?;
    let mut stdout = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut stderr = BufReader::new(child.stderr.take().expect("piped")).lines();
    let (mut out_done, mut err_done) = (false, false);
    let mut last = None;
    while !(out_done && err_done) {
        let line = tokio::select! {
            line = stdout.next_line(), if !out_done => line?.or_else(|| { out_done = true; None }),
            line = stderr.next_line(), if !err_done => line?.or_else(|| { err_done = true; None }),
        };
        if let Some(line) = line {
            if !line.trim().is_empty() {
                last = Some(line.clone());
            }
            log.line(line);
        }
    }
    let status = child.wait().await?;
    if !status.success() {
        // The full output is in the deploy log; the error only needs its last word.
        bail!("{}", last.unwrap_or_else(|| status.to_string()));
    }
    Ok(())
}
