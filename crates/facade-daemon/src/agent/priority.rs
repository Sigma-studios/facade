//! Builds yield to apps. A build may use every core and plenty of memory, which is
//! right on an idle node but should not slow down or kill the apps it serves.
//!
//! Rather than keeping cores away from builds (wasted when the apps are idle, and
//! not enough when they are busy), build steps run in a systemd slice of their own,
//! [`SLICE`], with a low CPU and IO weight: when apps want the CPU they get it
//! (builds then share about a tenth of it), and the rest of the time builds take
//! every core. BuildKit puts its steps in the slice through `--cgroup-parent`.
//!
//! The slice is next to `system.slice`, where Docker's containers are, so the weight
//! counts against all of them; a slice deeper down would only compete with its
//! siblings.
//!
//! Memory: BuildKit's steps inherit dockerd's OOM score adjustment (-500) while
//! apps have 0, so when memory runs out the kernel would kill an app before a
//! build. The agent raises the steps' adjustment to [`OOM_SCORE_ADJ`], so a build
//! fails first.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use tracing::{info, warn};

/// The slice build steps run in. A name without `-`: systemd reads dashes as
/// nesting (`facade-build.slice` lives in `facade.slice`), BuildKit does not.
pub const SLICE: &str = "facade.slice";
/// The slice's CPU and IO weight; systemd's default, that of `system.slice`, is 100.
const WEIGHT: u32 = 10;
/// Where a build step stands in line for the OOM killer: apps have 0, dockerd -500.
const OOM_SCORE_ADJ: i32 = 500;

static READY: OnceLock<bool> = OnceLock::new();

/// The `--cgroup-parent` for builds, once the slice is set up.
pub fn cgroup_parent() -> Option<&'static str> {
    READY.get().copied().unwrap_or(false).then_some(SLICE)
}

/// Sets the slice up (its settings last until the node reboots, and are set again
/// at each start), then keeps the build steps first in line for the OOM killer.
pub async fn run() {
    let ready = match setup().await {
        Ok(()) => {
            info!("builds run in {SLICE}, with CPU and IO weight {WEIGHT}");
            true
        }
        Err(e) => {
            warn!("builds share the CPU with apps evenly: {e:#}");
            false
        }
    };
    let _ = READY.set(ready);
    if !ready {
        return;
    }
    loop {
        demote_build_steps();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn setup() -> anyhow::Result<()> {
    if !Path::new("/run/systemd/system").exists() {
        anyhow::bail!("the node does not run systemd");
    }
    let systemctl = crate::platform::find_tool("systemctl")
        .ok_or_else(|| anyhow::anyhow!("systemctl is not on the node"))?;
    let weight = WEIGHT.to_string();
    let cpu = format!("CPUWeight={weight}");
    let io = format!("IOWeight={weight}");
    for args in [
        &["set-property", "--runtime", SLICE, &cpu, &io][..],
        &["start", SLICE][..],
    ] {
        let out = tokio::process::Command::new(&systemctl)
            .args(args)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("running systemctl: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "systemctl {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(())
}

/// Raises the OOM score adjustment of every process in the slice. Children inherit
/// it, so a step's later processes start with it.
fn demote_build_steps() {
    // cgroup v2, then v1's memory hierarchy.
    for root in ["/sys/fs/cgroup", "/sys/fs/cgroup/memory"] {
        let dir = Path::new(root).join(SLICE);
        if dir.is_dir() {
            let mut pids = Vec::new();
            collect_pids(&dir, &mut pids);
            for pid in pids {
                let file = format!("/proc/{pid}/oom_score_adj");
                let current = std::fs::read_to_string(&file).ok();
                if current.and_then(|s| s.trim().parse::<i32>().ok()) < Some(OOM_SCORE_ADJ) {
                    // The process may be gone already.
                    let _ = std::fs::write(&file, OOM_SCORE_ADJ.to_string());
                }
            }
            return;
        }
    }
}

fn collect_pids(dir: &Path, pids: &mut Vec<u32>) {
    if let Ok(procs) = std::fs::read_to_string(dir.join("cgroup.procs")) {
        pids.extend(procs.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            collect_pids(&entry.path(), pids);
        }
    }
}
