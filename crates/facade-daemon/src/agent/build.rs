//! Fetching and building sources. Shells out to `git` and `docker build`: the
//! Docker CLI brings BuildKit (cache mounts) which the raw API does not.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use facade_core::proto::GitAuth;

use super::DeployLog;
use crate::gitauth::Access;

/// Fetches `commit` (or, without one, the tip of `branch` or the default branch)
/// into `dest`, with submodules. Returns the full hash and subject checked out.
pub async fn git_checkout(
    access: &Access,
    branch: Option<&str>,
    commit: Option<&str>,
    dest: &Path,
    log: &DeployLog,
) -> anyhow::Result<(String, String)> {
    let target = commit.or(branch).unwrap_or("HEAD");
    log.line(format!("fetching {} at {target}", access.url));
    tokio::fs::create_dir_all(dest).await?;
    let git = |args: &[&str]| {
        let mut cmd = access.git();
        cmd.arg("-C").arg(dest).args(args);
        cmd
    };
    run_logged(git(&["init", "-q"]), log).await?;
    run_logged(git(&["remote", "add", "--", "origin", &access.url]), log).await?;
    let fetched = run_logged(git(&["fetch", "--depth", "1", "origin", target]), log).await;
    if let Err(e) = fetched {
        // Some servers only hand out commits by name when they are a branch tip.
        let Some(commit) = commit else {
            return Err(e.context("git fetch failed"));
        };
        log.line("fetching the full history to find the commit");
        run_logged(git(&["fetch", "origin"]), log)
            .await
            .context("git fetch failed")?;
        run_logged(git(&["checkout", "-q", "--detach", commit]), log)
            .await
            .with_context(|| format!("commit {commit} is not in the repository"))?;
    } else {
        run_logged(git(&["checkout", "-q", "--detach", "FETCH_HEAD"]), log).await?;
    }
    if dest.join(".gitmodules").is_file() {
        log.line("fetching submodules");
        let shallow = run_logged(
            git(&[
                "submodule",
                "update",
                "--init",
                "--recursive",
                "--depth",
                "1",
            ]),
            log,
        )
        .await;
        if shallow.is_err() {
            run_logged(git(&["submodule", "update", "--init", "--recursive"]), log)
                .await
                .context("fetching submodules failed")?;
        }
    }
    let out = git(&["log", "-1", "--format=%H%x00%s"]).output().await?;
    let out = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (sha, title) = out.split_once('\0').unwrap_or((&out, ""));
    log.line(format!("at commit {} {title}", &sha[..sha.len().min(12)]));
    Ok((sha.to_string(), title.to_string()))
}

/// Extra `docker build` arguments. The app's git credential is handed over as
/// build secrets, so Dockerfiles can fetch private dependencies with
/// `RUN --mount=type=secret,id=gitconfig,target=/root/.gitconfig` (tokens) or
/// `RUN --mount=type=ssh` (keys); neither ends up in the image.
pub struct BuildOptions {
    args: Vec<String>,
}

impl BuildOptions {
    /// Writes what the credential needs into `dir`, which must be outside the
    /// build context and is deleted with the build. `no_cache` rebuilds every layer.
    pub fn new(auth: Option<&GitAuth>, dir: &Path, no_cache: bool) -> anyhow::Result<Self> {
        let mut args = vec![];
        if no_cache {
            args.push("--no-cache".to_string());
        }
        match auth {
            Some(GitAuth::Token {
                scope,
                username,
                token,
            }) => {
                let host = scope.split('/').next().unwrap_or(scope);
                let path = dir.join("gitconfig");
                let config = format!(
                    "[url \"https://{username}:{token}@{host}/\"]\n\tinsteadOf = https://{host}/\n\tinsteadOf = git@{host}:\n\tinsteadOf = ssh://git@{host}/\n"
                );
                crate::tls::write_private(&path, config.as_bytes())?;
                args.push("--secret".into());
                args.push(format!("id=gitconfig,src={}", path.display()));
            }
            Some(GitAuth::SshKey { private_key }) => {
                let path = dir.join("ssh-key");
                crate::tls::write_private(&path, format!("{}\n", private_key.trim()).as_bytes())?;
                args.push("--ssh".into());
                args.push(format!("default={}", path.display()));
                let hosts = dir.join("known_hosts");
                std::fs::write(&hosts, facade_core::git::KNOWN_HOSTS)?;
                args.push("--secret".into());
                args.push(format!("id=known_hosts,src={}", hosts.display()));
            }
            None => {}
        }
        Ok(Self { args })
    }
}

pub async fn docker_build(
    app: &str,
    tag: &str,
    dockerfile: &Path,
    context: &Path,
    options: &BuildOptions,
    log: &DeployLog,
) -> anyhow::Result<()> {
    if !dockerfile.is_file() {
        bail!(
            "no Dockerfile at {} (set build.dockerfile, or use the rust/static build kinds)",
            dockerfile.display()
        );
    }
    log.line(format!("building {tag}"));
    let mut cmd = Command::new("docker");
    cmd.args(["build", "--progress=plain", "--pull", "-t", tag])
        .args(["--label", &format!("facade.app={app}")])
        .args(&options.args)
        .arg("-f")
        .arg(dockerfile)
        .arg(context)
        .env("DOCKER_BUILDKIT", "1");
    run_logged(cmd, log).await.context("docker build failed")
}

/// Runs `cmd`, forwarding stdout and stderr line by line to the deploy log.
pub async fn run_logged(mut cmd: Command, log: &DeployLog) -> anyhow::Result<()> {
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
        let last = last.unwrap_or_else(|| status.to_string());
        if cmd.as_std().get_program() == "git" {
            bail!("{}", crate::gitauth::explain(&last));
        }
        bail!("{last}");
    }
    Ok(())
}
