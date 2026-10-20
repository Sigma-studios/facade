//! Fetching and building sources. Shells out to `git` and `docker build`: the
//! Docker CLI brings BuildKit (cache mounts) which the raw API does not.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::DeployLog;

pub async fn git_checkout(
    repo: &str,
    branch: Option<&str>,
    dest: &Path,
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
        .env("GIT_TERMINAL_PROMPT", "0");
    run_logged(cmd, log).await.context("git clone failed")?;

    let out = Command::new("git")
        .arg("-C")
        .arg(dest)
        .args(["log", "-1", "--format=%h %s"])
        .output()
        .await?;
    log.line(format!(
        "at commit {}",
        String::from_utf8_lossy(&out.stdout).trim()
    ));
    Ok(())
}

pub async fn docker_build(
    app: &str,
    tag: &str,
    dockerfile: &Path,
    context: &Path,
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
        .arg("-f")
        .arg(dockerfile)
        .arg(context)
        .env("DOCKER_BUILDKIT", "1");
    run_logged(cmd, log).await.context("docker build failed")
}

/// Multi-stage build of one cargo binary, with the registry and target directory in
/// BuildKit cache mounts so redeploys only rebuild what changed.
pub fn rust_dockerfile(
    app: &str,
    package: Option<&str>,
    bin: &str,
    features: &[String],
    no_default_features: bool,
) -> String {
    let mut args = String::new();
    if let Some(package) = package {
        args += &format!(" -p {package}");
    }
    args += &format!(" --bin {bin}");
    if no_default_features {
        args += " --no-default-features";
    }
    if !features.is_empty() {
        args += &format!(" --features {}", features.join(","));
    }
    format!(
        r#"FROM rust:1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=facade-cargo-registry \
    --mount=type=cache,target=/src/target,id=facade-{app}-target \
    cargo build --release{args} && cp target/release/{bin} /facade-app

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*
COPY --from=build /facade-app /usr/local/bin/{bin}
CMD ["/usr/local/bin/{bin}"]
"#
    )
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
