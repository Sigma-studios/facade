//! Commands for updating a server (`deploy/upgrade.sh`, or the server updating
//! itself), run on it with the binary about to be installed:
//!
//! * `facaded upgrade-check`: what installing this binary would do to the apps
//!   running on the server's node. A new build can change how container
//!   fingerprints or image names are computed; then containers are recreated,
//!   or rebuilt at their deployed commit, at the next convergence.
//! * `facaded backup-state`: a copy of the database and master key, next to the
//!   scheduled ones.
//! * `facaded apply-update`: installs a release folder and restarts the server,
//!   going back to the previous binary if the new one is not healthy. Also run by
//!   the server itself when updating from a published build (`server::release`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use bollard::Docker;
use bollard::query_parameters::ListContainersOptions;
use facade_core::Source;
use facade_core::api::{AppChange as Change, PlannedChange, UpdateRun, UpdateState};

use crate::config::ServerConfig;
use crate::server::{Catalog, secrets::SecretBox, store::Store};

pub async fn check(cfg: &ServerConfig, json: bool) -> anyhow::Result<()> {
    // The running server's database may predate this binary's schema: read a
    // copy, brought up to date the way this binary would, and never touch the
    // real one.
    let copy = cfg
        .data_dir
        .join("tmp")
        .join(format!("upgrade-check-{}.db", uuid::Uuid::new_v4()));
    crate::tls::create_private_dir(copy.parent().expect("has a parent"))?;
    Store::open_read_only(&cfg.data_dir.join("facade.db"))?.snapshot(&copy)?;
    let result = check_copy(cfg, &copy, json).await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", copy.display()));
    }
    result
}

async fn check_copy(cfg: &ServerConfig, copy: &std::path::Path, json: bool) -> anyhow::Result<()> {
    let store = Store::open(copy)?;
    let secrets = SecretBox::load_or_create(&cfg.data_dir.join("master.key"))?;
    let docker = Docker::connect_with_local_defaults().context("connecting to Docker")?;

    // What runs now: app → (run key, image) from container labels.
    let filters = HashMap::from([(
        "label".to_string(),
        vec![format!("facade.node={}", cfg.local_node)],
    )]);
    let containers = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some(filters),
            ..Default::default()
        }))
        .await?;
    let running: HashMap<String, String> = containers
        .into_iter()
        .filter_map(|c| {
            let labels = c.labels?;
            Some((
                labels.get("facade.app")?.clone(),
                labels.get("facade.run")?.clone(),
            ))
        })
        .collect();

    let mut planned = Vec::new();
    for row in store.list_apps()? {
        let spec = &row.spec;
        if spec.node != cfg.local_node || row.revision == 0 || !spec.enabled || spec.is_static() {
            continue;
        }
        let w = Catalog {
            store: &store,
            secrets: &secrets,
        }
        .workload(&spec.name)?;
        let image = crate::agent::planned_image(&w).expect("only static sites have no image");
        let change = match running.get(&spec.name) {
            None => Change::NotRunning,
            Some(_) if docker.inspect_image(&image).await.is_err() => Change::Rebuild,
            Some(key) if !crate::agent::run_key_matches(&w, &image, key) => Change::Restart,
            Some(_) => Change::Unchanged,
        };
        planned.push(PlannedChange {
            app: spec.name.clone(),
            change,
        });
        if json {
            continue;
        }
        let what = match change {
            Change::Unchanged => "unchanged".to_string(),
            Change::Restart => "RESTARTS: its container is recreated (a few seconds down)".into(),
            Change::Rebuild => match &spec.source {
                Source::Git { build, .. } if !build.is_static() => match &row.commit {
                    Some(sha) => format!(
                        "REBUILDS at its deployed commit {}, then restarts",
                        &sha[..sha.len().min(7)]
                    ),
                    None => "REBUILDS from the latest commit of its branch, then restarts".into(),
                },
                Source::Image { .. } => "PULLS its image again, then restarts".into(),
                _ => "REBUILDS its image, then restarts".into(),
            },
            Change::NotRunning => "not running now".into(),
        };
        println!("  {:<24} {what}", spec.name);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&planned)?);
    } else if planned.is_empty() {
        println!("  (no running apps on {})", cfg.local_node);
    }
    Ok(())
}

pub fn backup_state(cfg: &ServerConfig) -> anyhow::Result<()> {
    let store = Store::open_read_only(&cfg.data_dir.join("facade.db"))?;
    let dir = crate::server::backups::snapshot_state(&cfg.data_dir, &store, cfg.backups.keep)?;
    println!("{}", dir.display());
    Ok(())
}

/// Where `apply-update` records its progress and outcome, which the server shows
/// (Settings → Updates) once it is back.
pub fn run_file(data_dir: &Path) -> PathBuf {
    data_dir.join("update").join("run.json")
}

pub fn read_run(data_dir: &Path) -> Option<UpdateRun> {
    let raw = std::fs::read(run_file(data_dir)).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Progress of `apply-update`, printed and kept in [`run_file`].
struct Progress<'a> {
    data_dir: &'a Path,
    run: UpdateRun,
}

impl Progress<'_> {
    fn step(&mut self, message: impl Into<String>) {
        self.run.message = message.into();
        println!("{}", self.run.message);
        self.save();
    }

    fn end(&mut self, state: UpdateState, message: impl Into<String>) {
        self.run.state = state;
        self.run.finished = Some(facade_core::now());
        self.step(message);
    }

    fn save(&self) {
        if let Err(e) = write_run(self.data_dir, &self.run) {
            eprintln!("recording progress: {e:#}");
        }
    }
}

pub fn write_run(data_dir: &Path, run: &UpdateRun) -> std::io::Result<()> {
    let path = run_file(data_dir);
    let tmp = path.with_extension("json.tmp");
    crate::tls::create_private_dir(path.parent().expect("has a parent"))?;
    std::fs::write(&tmp, serde_json::to_vec_pretty(run).expect("serializable"))?;
    std::fs::rename(&tmp, &path)
}

pub struct ApplyOptions {
    /// A release folder: `facaded`, `facade` and the systemd units.
    pub stage: PathBuf,
    /// Where the binaries are installed.
    pub bin_dir: PathBuf,
    /// The commit being replaced, when known (for the record).
    pub from: Option<String>,
}

/// `facaded apply-update`, run as root with the binary being installed: backs up
/// the database and key, installs the binaries (keeping `.prev` copies) and the
/// units already in use, restarts `facaded` and waits until it is healthy, going
/// back to the previous binary if it is not. Last, hands the entry point over.
/// The server starts it in a unit of its own (so restarting `facaded` does not
/// stop it); `deploy/upgrade.sh` over SSH.
pub async fn apply(cfg: &ServerConfig, opts: ApplyOptions) -> anyhow::Result<()> {
    let mut progress = Progress {
        data_dir: &cfg.data_dir,
        run: UpdateRun {
            to: crate::selfbin::commit()
                .unwrap_or("a build without a commit")
                .to_string(),
            from: opts.from.clone(),
            state: UpdateState::Running,
            message: String::new(),
            started: facade_core::now(),
            finished: None,
        },
    };
    match install(cfg, &opts, &mut progress).await {
        Ok(()) => Ok(()),
        Err(e) => {
            if progress.run.state == UpdateState::Running {
                progress.end(UpdateState::Failed, format!("{e:#}"));
            }
            Err(e)
        }
    }
}

const UNIT_DIR: &str = "/etc/systemd/system";

async fn install(
    cfg: &ServerConfig,
    opts: &ApplyOptions,
    progress: &mut Progress<'_>,
) -> anyhow::Result<()> {
    for file in ["facaded", "facade"] {
        anyhow::ensure!(
            opts.stage.join(file).is_file(),
            "no {file} in {}",
            opts.stage.display()
        );
    }
    progress.step("backing up the database and key");
    let store = Store::open_read_only(&cfg.data_dir.join("facade.db"))?;
    let backup = crate::server::backups::snapshot_state(&cfg.data_dir, &store, cfg.backups.keep)?;
    drop(store);
    progress.step(format!("backed up to {}", backup.display()));

    let binaries = ["facaded", "facade"].map(|name| opts.bin_dir.join(name));
    for bin in &binaries {
        if bin.exists() {
            std::fs::copy(bin, prev(bin)).with_context(|| format!("keeping {}", bin.display()))?;
        }
    }
    // Units already installed get their new version; the old ones are kept to
    // go back to.
    let mut old_units = Vec::new();
    for entry in std::fs::read_dir(&opts.stage)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "service")
            && let Some(name) = path.file_name()
        {
            let installed = Path::new(UNIT_DIR).join(name);
            if let Ok(old) = std::fs::read(&installed) {
                old_units.push((installed.clone(), old));
                put(&path, &installed, 0o644)?;
            }
        }
    }
    for bin in &binaries {
        let new = opts.stage.join(bin.file_name().expect("has a name"));
        put(&new, bin, 0o755)?;
    }
    systemctl(&["daemon-reload"])?;

    progress.step("restarting facaded");
    systemctl(&["restart", "facaded"])?;
    if wait_healthy(cfg).await {
        progress.step("facaded is up");
    } else {
        let journal = journal_tail("facaded", 15);
        progress
            .step("facaded is not healthy after the update; going back to the previous version");
        for bin in &binaries {
            if prev(bin).exists() {
                std::fs::rename(prev(bin), bin)?;
            }
        }
        for (path, old) in &old_units {
            std::fs::write(path, old)?;
        }
        systemctl(&["daemon-reload"])?;
        systemctl(&["restart", "facaded"])?;
        let back = if wait_healthy(cfg).await {
            "the previous version is running again"
        } else {
            "the previous version does not come up either: see journalctl -u facaded"
        };
        progress.end(
            UpdateState::RolledBack,
            format!("the new version did not become healthy within {HEALTH_WAIT_SECS} s; {back}.\n{journal}"),
        );
        anyhow::bail!("rolled back");
    }

    let handover = hand_over_ingress(progress).await;
    progress.end(
        UpdateState::Done,
        format!(
            "updated; the previous binary is {}{handover}",
            prev(&binaries[0]).display()
        ),
    );
    Ok(())
}

const HEALTH_WAIT_SECS: u64 = 90;

fn prev(bin: &Path) -> PathBuf {
    let mut name = bin.file_name().expect("has a name").to_os_string();
    name.push(".prev");
    bin.with_file_name(name)
}

/// Atomic replacement: `dest` always holds a complete file.
fn put(src: &Path, dest: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut name = dest.file_name().expect("has a name").to_os_string();
    name.push(".new");
    let tmp = dest.with_file_name(name);
    std::fs::copy(src, &tmp).with_context(|| format!("copying to {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, dest).with_context(|| format!("installing {}", dest.display()))?;
    Ok(())
}

fn systemctl(args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(args)
        .status()
        .context("running systemctl")?;
    anyhow::ensure!(status.success(), "systemctl {} failed", args.join(" "));
    Ok(())
}

fn systemctl_output(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("systemctl")
        .args(args)
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn journal_tail(unit: &str, lines: usize) -> String {
    std::process::Command::new("journalctl")
        .args([
            "-u",
            unit,
            "-n",
            &lines.to_string(),
            "--no-pager",
            "-o",
            "cat",
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Whether the server answers its health check (its own agent connected) within
/// [`HEALTH_WAIT_SECS`], on the loopback API.
async fn wait_healthy(cfg: &ServerConfig) -> bool {
    let url = format!("http://{}/api/health", cfg.api_loopback);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .expect("client");
    for _ in 0..HEALTH_WAIT_SECS {
        if let Ok(r) = client.get(&url).send().await
            && r.status().is_success()
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    false
}

/// `systemctl reload facaded-ingress` when it runs: the entry point passes its
/// sockets to the binary now installed. Says how it went, for the final message.
async fn hand_over_ingress(progress: &mut Progress<'_>) -> String {
    if systemctl_output(&["is-active", "facaded-ingress"]).as_deref() != Some("active") {
        return String::new();
    }
    progress.step("handing the entry point over");
    let pid = || systemctl_output(&["show", "facaded-ingress", "-p", "MainPID", "--value"]);
    let before = pid();
    if let Err(e) = systemctl(&["reload", "facaded-ingress"]) {
        return format!("; the entry point kept the previous version ({e})");
    }
    for _ in 0..40 {
        let after = pid();
        if after != before {
            return format!(
                "; entry point handed over (pid {} → {})",
                before.unwrap_or_default(),
                after.unwrap_or_default()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    "; the entry point kept the previous version (it still serves): see journalctl -u facaded-ingress"
        .into()
}
