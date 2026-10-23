//! Updating facade itself from the builds CI publishes: every commit on the main
//! branch replaces the assets of one GitHub release (`[update]` in the config,
//! `.github/workflows/build.yml`) with a tarball per CPU architecture and a
//! `manifest.json` describing them.
//!
//! * **check** reads the manifest, and asks GitHub which commits it brings.
//! * **prepare** downloads the tarballs, verifies them against the manifest,
//!   unpacks the server's into `<data_dir>/update/stage` and asks its binary what
//!   installing it does to the running apps (`facaded upgrade-check`). Binaries
//!   for the other architectures are kept for the nodes running on them.
//! * **install** starts `facaded apply-update` from the stage in a systemd unit of
//!   its own, which restarts the server and rolls back if it does not come up.
//!
//! Other nodes follow by themselves: an agent running another build is sent the
//! server's binary when it connects ([`binary_for`] for another architecture).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use axum::Json;
use axum::extract::State;
use facade_core::api::{
    CommitView, NodeBuild, NodeBuildState, PlannedChange, PreparedUpdate, PublishedBuild,
    RunningBuild, ServerUpdate, UpdateRun, UpdateState, UpdateTarget,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::info;

use super::Server;
use super::api::{ApiError, ApiResult, User, conflict};
use crate::selfbin;

type S = State<Arc<Server>>;

/// `manifest.json` of a published build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub commit: String,
    pub subject: String,
    /// Unix time of the commit.
    pub time: u64,
    /// By CPU architecture (`x86_64`, `aarch64`).
    pub builds: BTreeMap<String, ManifestBuild>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestBuild {
    /// Asset name of the tarball: a `facade/` folder with `facaded`, `facade`,
    /// the installer and the systemd units.
    pub file: String,
    pub sha256: String,
    /// The `facaded` inside, which is how agents tell builds apart.
    pub facaded_sha256: String,
}

#[derive(Default)]
pub struct Releases(Mutex<Known>);

#[derive(Default)]
struct Known {
    latest: Option<Manifest>,
    checked_at: Option<u64>,
    check_error: Option<String>,
    commits: Vec<CommitView>,
    /// The running build is newer than the published one.
    ahead: bool,
    prepared: Option<PreparedUpdate>,
    /// A prepare is downloading.
    busy: bool,
}

fn update_dir(server: &Server) -> PathBuf {
    server.cfg.data_dir.join("update")
}

fn stage_dir(server: &Server) -> PathBuf {
    update_dir(server).join("stage")
}

/// Binaries of a published build for other architectures, by commit.
fn binaries_dir(server: &Server, commit: &str) -> PathBuf {
    update_dir(server).join("binaries").join(commit)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(format!("facade/{}", selfbin::version_line()))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(600))
        .build()
        .expect("client")
}

fn asset_url(server: &Server, file: &str) -> String {
    let update = &server.cfg.update;
    match &update.url {
        Some(url) => format!("{}/{file}", url.trim_end_matches('/')),
        None => format!(
            "https://github.com/{}/releases/download/{}/{file}",
            update.repository, update.release
        ),
    }
}

// ---------------------------------------------------------------- views

pub fn view(server: &Server) -> ServerUpdate {
    let current = selfbin::commit();
    let known = server.releases.0.lock();
    let latest = known.latest.as_ref().map(|m| PublishedBuild {
        commit: m.commit.clone(),
        subject: m.subject.clone(),
        time: m.time,
        arches: m.builds.keys().cloned().collect(),
    });
    let available = latest
        .as_ref()
        .is_some_and(|l| Some(l.commit.as_str()) != current && !known.ahead);
    ServerUpdate {
        repository: server.cfg.update.repository.clone(),
        current: RunningBuild {
            commit: current.map(str::to_string),
            version: env!("CARGO_PKG_VERSION").to_string(),
            arch: std::env::consts::ARCH.to_string(),
        },
        latest,
        checked_at: known.checked_at,
        check_error: known.check_error.clone(),
        available,
        commits: known.commits.clone(),
        prepared: known.prepared.clone(),
        run: crate::upgrade::read_run(&server.cfg.data_dir),
        nodes: node_builds(server),
    }
}

fn node_builds(server: &Server) -> Vec<NodeBuild> {
    let own = selfbin::current().map(|b| b.sha256.as_str());
    let nodes = server.store.list_nodes().unwrap_or_default();
    let live = server.live.read();
    nodes
        .into_iter()
        .map(|n| {
            let reported = live.nodes.get(&n.name);
            let binary = reported.and_then(|l| l.binary.clone());
            let arch = reported.and_then(|l| l.arch.clone());
            let state = if n.name == server.cfg.local_node {
                // Its agent runs inside the server.
                NodeBuildState::Current
            } else {
                match (&binary, &arch) {
                    (None, _) => NodeBuildState::Unknown,
                    (Some(b), _) if Some(b.as_str()) == own => NodeBuildState::Current,
                    (Some(_), Some(a)) if a == std::env::consts::ARCH => NodeBuildState::Updating,
                    (Some(b), Some(a)) => match binary_for(server, a) {
                        Some((sha, _)) if sha == *b => NodeBuildState::Current,
                        Some(_) => NodeBuildState::Updating,
                        None => NodeBuildState::Manual,
                    },
                    (Some(_), None) => NodeBuildState::Manual,
                }
            };
            NodeBuild {
                connected: server.hub.is_connected(&n.name),
                name: n.name,
                arch,
                state,
            }
        })
        .collect()
}

/// The published `facaded` of the running commit for another architecture, when
/// the server downloaded it: its SHA-256 and where it is.
pub fn binary_for(server: &Server, arch: &str) -> Option<(String, PathBuf)> {
    let commit = selfbin::commit()?;
    let dir = binaries_dir(server, commit);
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).ok()?).ok()?;
    let build = manifest.builds.get(arch)?;
    let path = dir.join(format!("facaded-{arch}"));
    path.is_file().then(|| (build.facaded_sha256.clone(), path))
}

/// The downloaded binary with this SHA-256, for an agent fetching it.
pub fn binary_with_sha(server: &Server, sha256: &str) -> Option<PathBuf> {
    let commit = selfbin::commit()?;
    let dir = binaries_dir(server, commit);
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).ok()?).ok()?;
    let (arch, _) = manifest
        .builds
        .iter()
        .find(|(_, b)| b.facaded_sha256 == sha256)?;
    let path = dir.join(format!("facaded-{arch}"));
    path.is_file().then_some(path)
}

// ---------------------------------------------------------------- check

async fn fetch_manifest(server: &Server) -> anyhow::Result<Manifest> {
    let url = asset_url(server, "manifest.json");
    let response = client()
        .get(&url)
        .header("Cache-Control", "no-cache")
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!(
            "no published build at {url}: does {} publish its builds to the release `{}`?",
            server.cfg.update.repository,
            server.cfg.update.release
        );
    }
    let response = response.error_for_status()?;
    response
        .json()
        .await
        .context("reading the release manifest")
}

#[derive(Deserialize)]
struct Compare {
    status: String,
    commits: Vec<CompareCommit>,
}

#[derive(Deserialize)]
struct CompareCommit {
    sha: String,
    commit: CompareDetail,
}

#[derive(Deserialize)]
struct CompareDetail {
    message: String,
    author: CompareAuthor,
}

#[derive(Deserialize)]
struct CompareAuthor {
    name: String,
    date: String,
}

/// The commits from `base` to `head` on GitHub, newest first (at most 50), and
/// whether `base` is ahead of `head`.
async fn compare(
    server: &Server,
    base: &str,
    head: &str,
) -> anyhow::Result<(Vec<CommitView>, bool)> {
    let url = format!(
        "https://api.github.com/repos/{}/compare/{base}...{head}",
        server.cfg.update.repository
    );
    let compare: Compare = client()
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let commits = compare
        .commits
        .into_iter()
        .rev()
        .take(50)
        .map(|c| CommitView {
            hash: c.sha,
            subject: c
                .commit
                .message
                .lines()
                .next()
                .unwrap_or_default()
                .to_string(),
            author: c.commit.author.name,
            time: parse_rfc3339(&c.commit.author.date).unwrap_or(0),
        })
        .collect();
    Ok((commits, compare.status == "behind"))
}

/// `2026-10-20T19:21:40Z` → Unix time (GitHub's dates are always UTC).
fn parse_rfc3339(s: &str) -> Option<u64> {
    let (date, time) = s.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.split(':').map(|p| p.parse::<i64>());
    let (h, min, sec) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + min * 60 + sec).ok()
}

/// At start and every six hours, so the web UI can say a new build is out.
pub async fn check_regularly(server: Arc<Server>) {
    loop {
        check(&server).await;
        tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
    }
}

pub async fn check(server: &Server) {
    let result = fetch_manifest(server).await;
    let now = facade_core::now();
    let manifest = match result {
        Ok(m) => m,
        Err(e) => {
            let mut known = server.releases.0.lock();
            known.checked_at = Some(now);
            known.check_error = Some(format!("{e:#}"));
            return;
        }
    };
    let (commits, ahead) = match selfbin::commit() {
        Some(current) if current != manifest.commit => {
            compare(server, current, &manifest.commit)
                .await
                .unwrap_or_else(|e| {
                    // Only the list of commits is missing.
                    tracing::warn!("comparing commits on GitHub: {e:#}");
                    (vec![], false)
                })
        }
        _ => (vec![], false),
    };
    let mut known = server.releases.0.lock();
    if known
        .prepared
        .as_ref()
        .is_some_and(|p| p.commit != manifest.commit)
    {
        known.prepared = None;
    }
    known.latest = Some(manifest);
    known.checked_at = Some(now);
    known.check_error = None;
    known.commits = commits;
    known.ahead = ahead;
}

// ---------------------------------------------------------------- prepare

/// Downloads `url` to `dest`, returning its SHA-256.
async fn download(url: &str, dest: &Path) -> anyhow::Result<String> {
    use tokio::io::AsyncWriteExt;
    let mut response = client()
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()?;
    let mut file = tokio::fs::File::create(dest).await?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = response.chunk().await? {
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(hex::encode(hasher.finalize()))
}

/// The regular files of a release tarball, by name (it holds one flat folder).
fn unpack(tarball: &Path, dest: &Path, only: Option<&str>) -> anyhow::Result<Vec<String>> {
    let gz = flate2::read::GzDecoder::new(std::fs::File::open(tarball)?);
    let mut archive = tar::Archive::new(gz);
    let mut names = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?;
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if only.is_some_and(|o| o != name) {
            continue;
        }
        let mode = entry.header().mode().unwrap_or(0o644) & 0o755;
        let out = dest.join(&name);
        let mut file = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut file)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode))?;
        names.push(name);
    }
    Ok(names)
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

async fn prepare(server: &Server, commit: &str) -> anyhow::Result<PreparedUpdate> {
    let manifest = {
        let known = server.releases.0.lock();
        match &known.latest {
            Some(m) if m.commit == commit => m.clone(),
            _ => bail!(PrepareError::Changed),
        }
    };
    let arch = std::env::consts::ARCH;
    if !manifest.builds.contains_key(arch) {
        bail!("the published build has no binary for this server's CPU ({arch})");
    }
    let dir = update_dir(server);
    let stage = stage_dir(server);
    let downloads = dir.join("downloads");
    let binaries = binaries_dir(server, commit);
    for d in [&stage, &downloads] {
        let _ = std::fs::remove_dir_all(d);
    }
    for d in [&stage, &downloads, &binaries] {
        crate::tls::create_private_dir(d)?;
    }

    for (build_arch, build) in &manifest.builds {
        let tarball = downloads.join(&build.file);
        let sha = download(&asset_url(server, &build.file), &tarball).await?;
        if sha != build.sha256 {
            bail!(
                "{} does not match the manifest: the release is being published, try again in a minute",
                build.file
            );
        }
        let ours = build_arch == arch;
        let (target, only) = if ours {
            (stage.clone(), None)
        } else {
            (binaries.clone(), Some("facaded"))
        };
        let tarball2 = tarball.clone();
        let names = tokio::task::spawn_blocking(move || unpack(&tarball2, &target, only)).await??;
        if !names.iter().any(|n| n == "facaded") {
            bail!("{} holds no facaded", build.file);
        }
        let binary = if ours {
            stage.join("facaded")
        } else {
            let path = binaries.join(format!("facaded-{build_arch}"));
            std::fs::rename(binaries.join("facaded"), &path)?;
            path
        };
        if sha256_file(&binary)? != build.facaded_sha256 {
            bail!("the facaded in {} does not match the manifest", build.file);
        }
    }
    let _ = std::fs::remove_dir_all(&downloads);
    std::fs::write(
        binaries.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    // Only the running build's and this one's binaries are worth keeping.
    if let Ok(entries) = std::fs::read_dir(dir.join("binaries")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name != commit && Some(name.as_str()) != selfbin::commit() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    let output = tokio::process::Command::new(stage.join("facaded"))
        .arg("upgrade-check")
        .arg("--json")
        .arg("--config")
        .arg(&server.cfg.path)
        .output()
        .await
        .context("running the new facaded")?;
    if !output.status.success() {
        bail!(
            "the new facaded cannot check this server: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let plan: Vec<PlannedChange> = serde_json::from_slice(&output.stdout)
        .context("reading what the new facaded would change")?;
    info!(commit = %selfbin::short_commit(commit), "update downloaded and checked");
    Ok(PreparedUpdate {
        commit: commit.to_string(),
        plan,
    })
}

#[derive(Debug)]
enum PrepareError {
    Changed,
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrepareError::Changed => write!(f, "the published build changed: check again"),
        }
    }
}

impl std::error::Error for PrepareError {}

// ---------------------------------------------------------------- install

/// Starts `apply-update` from the stage, outside the server's own unit so it
/// outlives the restart.
async fn install(server: &Server, commit: &str) -> anyhow::Result<()> {
    let prepared = server.releases.0.lock().prepared.clone();
    if prepared.as_ref().is_none_or(|p| p.commit != commit) {
        bail!(PrepareError::Changed);
    }
    if !Path::new("/run/systemd/system").exists() {
        bail!("updating from here needs systemd: use deploy/upgrade.sh");
    }
    if let Some(run) = crate::upgrade::read_run(&server.cfg.data_dir)
        && run.state == UpdateState::Running
        && facade_core::now().saturating_sub(run.started) < 900
    {
        bail!("an update is already running");
    }
    let stage = stage_dir(server);
    let bin_dir = selfbin::installed_path()?
        .parent()
        .context("facaded has no folder")?
        .to_path_buf();
    let mut command = tokio::process::Command::new("systemd-run");
    command
        .args(["--unit", "facade-update", "--collect", "--quiet"])
        .arg(format!(
            "--description=facade update to {}",
            selfbin::short_commit(commit)
        ))
        .arg(stage.join("facaded"))
        .arg("apply-update")
        .arg(&stage)
        .arg("--config")
        .arg(&server.cfg.path)
        .arg("--bin-dir")
        .arg(&bin_dir);
    if let Some(from) = selfbin::commit() {
        command.arg("--from").arg(from);
    }
    // Shown until apply-update records its own progress.
    let mut run = UpdateRun {
        to: commit.to_string(),
        from: selfbin::commit().map(str::to_string),
        state: UpdateState::Running,
        message: "starting".into(),
        started: facade_core::now(),
        finished: None,
    };
    crate::upgrade::write_run(&server.cfg.data_dir, &run)?;
    let output = command.output().await.context("running systemd-run")?;
    if !output.status.success() {
        run.state = UpdateState::Failed;
        run.finished = Some(facade_core::now());
        run.message = format!(
            "starting the update: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        crate::upgrade::write_run(&server.cfg.data_dir, &run)?;
        bail!(run.message);
    }
    info!(commit = %selfbin::short_commit(commit), "update started; facaded restarts shortly");
    server.releases.0.lock().prepared = None;
    Ok(())
}

// ---------------------------------------------------------------- API

pub async fn status(State(server): S, user: User) -> ApiResult<Json<ServerUpdate>> {
    user.require_admin()?;
    Ok(Json(view(&server)))
}

pub async fn check_now(State(server): S, user: User) -> ApiResult<Json<ServerUpdate>> {
    user.require_admin()?;
    check(&server).await;
    Ok(Json(view(&server)))
}

pub async fn prepare_now(
    State(server): S,
    user: User,
    Json(target): Json<UpdateTarget>,
) -> ApiResult<Json<ServerUpdate>> {
    user.require_admin()?;
    {
        let mut known = server.releases.0.lock();
        if known.busy {
            return Err(conflict("already downloading an update"));
        }
        known.busy = true;
    }
    let result = prepare(&server, &target.commit).await;
    let mut known = server.releases.0.lock();
    known.busy = false;
    match result {
        Ok(prepared) => known.prepared = Some(prepared),
        Err(e) => return Err(api_error(e)),
    }
    drop(known);
    Ok(Json(view(&server)))
}

pub async fn install_now(
    State(server): S,
    user: User,
    Json(target): Json<UpdateTarget>,
) -> ApiResult<Json<ServerUpdate>> {
    user.require_admin()?;
    install(&server, &target.commit).await.map_err(api_error)?;
    info!(user = %user.username, "server update started");
    Ok(Json(view(&server)))
}

fn api_error(e: anyhow::Error) -> ApiError {
    if e.downcast_ref::<PrepareError>().is_some() {
        conflict(e)
    } else {
        e.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_dates() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-10-20T19:21:40Z"), Some(1_792_524_100));
        assert_eq!(parse_rfc3339("2000-02-29T12:00:00Z"), Some(951_825_600));
        assert_eq!(parse_rfc3339("yesterday"), None);
    }

    #[test]
    fn manifest_format() {
        let raw = r#"{
            "commit": "8783c44eae86290a4863ecaabec0fa6514e9d280",
            "subject": "Updates without downtime",
            "time": 1792524100,
            "builds": {
                "x86_64": {"file": "facade-linux-x86_64.tar.gz", "sha256": "aa", "facaded_sha256": "bb"},
                "aarch64": {"file": "facade-linux-aarch64.tar.gz", "sha256": "cc", "facaded_sha256": "dd"}
            }
        }"#;
        let m: Manifest = serde_json::from_str(raw).unwrap();
        assert_eq!(m.builds["aarch64"].facaded_sha256, "dd");
    }
}
