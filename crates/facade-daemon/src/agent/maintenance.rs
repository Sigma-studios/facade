//! Keeping a node's disk in check. Builds leave cache behind (cargo's target
//! directory alone grows by gigabytes), and every deploy leaves an image: left
//! alone, a small VPS fills up in weeks.
//!
//! * every 6 hours: dangling images, and build cache unused for a week;
//! * when the disk is 85 % full: build cache unused for a day, and the images kept
//!   for rollbacks; at 90 %, the whole build cache.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bollard::query_parameters::PruneImagesOptions;
use facade_core::proto::{Cleanup, StorageReport};
use tokio::process::Command;
use tracing::{info, warn};

use super::Agent;
use super::runtime::dir_size;

const CHECK_EVERY: Duration = Duration::from_secs(5 * 60);
const ROUTINE_EVERY: Duration = Duration::from_secs(6 * 3600);
const REPORT_EVERY: Duration = Duration::from_secs(15 * 60);
/// Between two cleanups forced by a full disk, so one that frees nothing does not
/// run in a loop.
const PRESSURE_PAUSE: Duration = Duration::from_secs(30 * 60);
/// Measuring Docker's folders walks every layer: not for every report a deploy
/// triggers, but for each periodic one.
const DOCKER_MEASURE_EVERY: Duration = Duration::from_secs(10 * 60);
const PRESSURE: f64 = 85.0;
const CRITICAL: f64 = 90.0;

pub async fn run(agent: Arc<Agent>) {
    // Volumes and sizes right away; the first cleanup waits a little, as the agent
    // has just started converging apps.
    refresh_report(&agent).await;
    let mut last_report = Some(Instant::now());
    tokio::time::sleep(Duration::from_secs(60)).await;
    let mut last_routine: Option<Instant> = None;
    let mut last_pressure: Option<Instant> = None;
    loop {
        let full = disk_percent();
        let mut cleaned = false;
        if full >= PRESSURE && last_pressure.is_none_or(|t| t.elapsed() >= PRESSURE_PAUSE) {
            info!("disk {full:.0} % full: cleaning up");
            cleanup(&agent, true).await;
            last_pressure = Some(Instant::now());
            cleaned = true;
        } else if last_routine.is_none_or(|t| t.elapsed() >= ROUTINE_EVERY) {
            cleanup(&agent, false).await;
            last_routine = Some(Instant::now());
            cleaned = true;
        }
        if cleaned || last_report.is_none_or(|t| t.elapsed() >= REPORT_EVERY) {
            refresh_report(&agent).await;
            last_report = Some(Instant::now());
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

/// Used space of the root filesystem, where Docker keeps everything.
fn disk_usage() -> (u64, u64) {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .find(|d| d.mount_point() == std::path::Path::new("/"))
        .or_else(|| disks.list().first())
        .map(|d| (d.total_space() - d.available_space(), d.total_space()))
        .unwrap_or((0, 0))
}

fn disk_percent() -> f64 {
    let (used, total) = disk_usage();
    if total == 0 {
        0.0
    } else {
        used as f64 * 100.0 / total as f64
    }
}

async fn builder_prune(args: &[&str]) {
    let result = Command::new("docker")
        .args(["builder", "prune", "-f"])
        .args(args)
        .kill_on_drop(true)
        .output()
        .await;
    match result {
        Ok(out) if !out.status.success() => warn!(
            "docker builder prune: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => warn!("docker builder prune: {e}"),
        Ok(_) => {}
    }
}

/// Frees what can be freed without touching running apps, and records it.
pub async fn cleanup(agent: &Agent, aggressive: bool) {
    let (before, _) = disk_usage();
    let docker = agent.runtime.docker();
    let dangling = HashMap::from([("dangling".to_string(), vec!["true".to_string()])]);
    if let Err(e) = docker
        .prune_images(Some(PruneImagesOptions {
            filters: Some(dangling),
        }))
        .await
    {
        warn!("pruning images: {e}");
    }
    if aggressive {
        // Rollback images go first: only what runs now is kept.
        for (app, image) in agent.runtime.running_images().await {
            agent.runtime.prune_images(&app, &image, 0).await;
        }
        builder_prune(&["-a", "--filter", "until=24h"]).await;
        if disk_percent() >= CRITICAL {
            builder_prune(&["-a"]).await;
        }
    } else {
        builder_prune(&["-a", "--filter", "until=168h"]).await;
    }
    let (after, _) = disk_usage();
    let freed = before.saturating_sub(after);
    info!(
        "cleanup{} freed {} MB",
        if aggressive { " (aggressive)" } else { "" },
        freed / 1_000_000
    );
    *agent.docker_size.lock() = None;
    *agent.last_cleanup.lock() = Some(Cleanup {
        at: facade_core::now(),
        freed,
        aggressive,
    });
}

/// `12.76GB`, `121.6MB (66%)`, `0B` (Docker's decimal units) → bytes.
fn parse_size(s: &str) -> u64 {
    let s = s.split_whitespace().next().unwrap_or("");
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().unwrap_or(0.0);
    let factor = match unit.to_ascii_uppercase().as_str() {
        "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => 1.0,
    };
    (n * factor) as u64
}

/// What Docker's folders take on disk, volumes aside (reported apart): images,
/// build cache, containers and their logs. Docker's own figures for images and
/// build cache share layers (with containerd's image store, most of them), so
/// they cannot be added up. `None` where the folders are out of reach (Docker
/// Desktop keeps them in a VM).
async fn docker_on_disk(agent: &Agent) -> Option<u64> {
    let cached = *agent.docker_size.lock();
    if let Some((at, size)) = cached
        && at.elapsed() < DOCKER_MEASURE_EVERY
    {
        return Some(size);
    }
    let root = PathBuf::from(agent.runtime.docker().info().await.ok()?.docker_root_dir?);
    if !root.is_dir() {
        return None;
    }
    // The volumes are reported apart (`docker system df -v` walks them).
    let mut size = du(&root)
        .await?
        .saturating_sub(du(&root.join("volumes")).await.unwrap_or(0));
    let containerd = Path::new("/var/lib/containerd");
    if containerd.is_dir() {
        size += du(containerd).await.unwrap_or(0);
    }
    *agent.docker_size.lock() = Some((Instant::now(), size));
    Some(size)
}

/// What `dir` takes on disk, on its own filesystem (`du -skx`), at idle priority.
async fn du(dir: &Path) -> Option<u64> {
    let du = ["du", "-skx"];
    let low = Command::new("ionice")
        .args(["-c3", "nice", "-n19"])
        .args(du)
        .arg(dir)
        .kill_on_drop(true)
        .output()
        .await;
    let out = match low {
        Ok(out) => out,
        // No ionice: du at normal priority.
        Err(_) => Command::new(du[0])
            .args(&du[1..])
            .arg(dir)
            .kill_on_drop(true)
            .output()
            .await
            .ok()?,
    };
    // Files of running containers come and go during the walk; du then exits
    // with an error but still prints its total.
    let text = String::from_utf8_lossy(&out.stdout);
    let kib: u64 = text.split_whitespace().next()?.parse().ok()?;
    Some(kib * 1024)
}

/// Sizes from `docker system df`, this node's volumes, each app's share, and
/// facade's own files.
pub async fn refresh_report(agent: &Agent) {
    let (disk_used, disk_total) = disk_usage();
    let facade_files = dir_size(
        agent.files_dir.clone(),
        vec![agent.runtime.sized_volumes_dir()],
    )
    .await;
    let docker_on_disk = docker_on_disk(agent).await;
    let mut report = StorageReport {
        at: facade_core::now(),
        last_cleanup: agent.last_cleanup.lock().clone(),
        disk_used,
        disk_total,
        facade_files,
        docker_on_disk,
        ..Default::default()
    };
    if let Ok(out) = Command::new("docker")
        .args(["system", "df", "--format", "{{json .}}"])
        .kill_on_drop(true)
        .output()
        .await
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let field = |k: &str| row.get(k).and_then(|v| v.as_str()).unwrap_or("");
            match field("Type") {
                "Images" => report.images = parse_size(field("Size")),
                "Containers" => report.containers = parse_size(field("Size")),
                "Local Volumes" => report.volumes_size = parse_size(field("Size")),
                "Build Cache" => {
                    report.build_cache = parse_size(field("Size"));
                    report.build_cache_reclaimable = parse_size(field("Reclaimable"));
                }
                _ => {}
            }
        }
    }
    match agent.runtime.volumes().await {
        Ok(mut volumes) => {
            // Volume sizes need a walk of their files; `df -v` does it.
            if !volumes.is_empty()
                && let Ok(out) = Command::new("docker")
                    .args(["system", "df", "-v", "--format", "{{json .Volumes}}"])
                    .kill_on_drop(true)
                    .output()
                    .await
                && let Ok(rows) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout)
            {
                for v in &mut volumes {
                    v.size = rows
                        .iter()
                        .find(|r| r.get("Name").and_then(|n| n.as_str()) == Some(&v.name))
                        .and_then(|r| r.get("Size")?.as_str().map(parse_size));
                }
            }
            report.volumes = volumes;
        }
        Err(e) => warn!("listing volumes: {e:#}"),
    }
    match agent.runtime.app_disk().await {
        Ok(apps) => report.apps = apps,
        Err(e) => warn!("measuring apps: {e:#}"),
    }
    agent.set_storage(report);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_sizes() {
        assert_eq!(parse_size("12.76GB"), 12_760_000_000);
        assert_eq!(parse_size("121.6MB (66%)"), 121_600_000);
        assert_eq!(parse_size("0B"), 0);
        assert_eq!(parse_size("512kB"), 512_000);
    }
}
