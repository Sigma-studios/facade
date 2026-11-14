//! Apps get their whole stop timeout when the node shuts down, too.
//!
//! At a reboot, systemd stops `docker.service`, and dockerd stops each
//! container with its stop signal and stop timeout. But systemd only waits
//! `TimeoutStopSec` for dockerd: Docker's own package (get.docker.com) leaves
//! systemd's default of 90 seconds, after which dockerd is killed and the
//! containers with it, mid-way through saving their state (Ubuntu's docker.io
//! waits as long as it takes). So the agent makes sure that limit covers the
//! longest stop timeout of its containers, plus [`MARGIN`], with a drop-in in
//! `/run` (written again at each start, so nothing outlives facade, and
//! writable on NixOS). It only ever raises the limit.

use std::sync::atomic::{AtomicU64, Ordering};

use bollard::query_parameters::{InspectContainerOptions, ListContainersOptions};
use tracing::{info, warn};

use super::runtime::Runtime;

/// Time for dockerd itself around its containers' stops.
const MARGIN: u64 = 30;
const DROP_IN_DIR: &str = "/run/systemd/system/docker.service.d";
/// Last in name order, so it wins over other drop-ins.
const DROP_IN: &str = "zz-facade-stop-timeout.conf";

/// The longest stop (seconds) known to be covered already.
static COVERED: AtomicU64 = AtomicU64::new(0);

/// At start: covers the stop timeouts of the containers already there.
pub async fn run(runtime: Runtime) {
    let docker = runtime.docker();
    let Ok(containers) = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some([("label".to_string(), vec!["facade.app".to_string()])].into()),
            ..Default::default()
        }))
        .await
    else {
        return;
    };
    let mut longest = u64::from(facade_core::DEFAULT_STOP_TIMEOUT);
    for id in containers.into_iter().filter_map(|c| c.id) {
        if let Ok(c) = docker
            .inspect_container(&id, None::<InspectContainerOptions>)
            .await
            && let Some(t) = c.config.and_then(|c| c.stop_timeout)
            && t > 0
        {
            longest = longest.max(t as u64);
        }
    }
    cover(longest).await;
}

/// Makes sure systemd gives dockerd `stop_timeout` seconds and the margin.
pub async fn cover(stop_timeout: u64) {
    if stop_timeout <= COVERED.load(Ordering::Relaxed) {
        return;
    }
    match raise(stop_timeout + MARGIN).await {
        Ok(()) => {
            COVERED.fetch_max(stop_timeout, Ordering::Relaxed);
        }
        Err(e) => warn!(
            "apps may be killed before their {stop_timeout} s stop timeout when the node shuts down: {e:#}"
        ),
    }
}

async fn raise(secs: u64) -> anyhow::Result<()> {
    if !std::path::Path::new("/run/systemd/system").exists() {
        anyhow::bail!("the node does not run systemd");
    }
    let current = current_limit().await?;
    if current >= secs * 1_000_000 {
        return Ok(());
    }
    std::fs::create_dir_all(DROP_IN_DIR)?;
    let file = std::path::Path::new(DROP_IN_DIR).join(DROP_IN);
    std::fs::write(
        &file,
        format!(
            "# Written by facade: Docker gets the time its containers' stop timeouts\n\
             # ask for when the node shuts down.\n\
             [Service]\nTimeoutStopSec={secs}s\n"
        ),
    )?;
    let systemctl = crate::platform::find_tool("systemctl")
        .ok_or_else(|| anyhow::anyhow!("systemctl is not on the node"))?;
    let status = tokio::process::Command::new(systemctl)
        .arg("daemon-reload")
        .status()
        .await?;
    anyhow::ensure!(status.success(), "systemctl daemon-reload failed");
    info!(
        "docker.service may take {secs} s to stop now (it had {}), for the apps' stop timeouts",
        if current == u64::MAX {
            "no limit".to_string()
        } else {
            format!("{} s", current / 1_000_000)
        }
    );
    Ok(())
}

/// docker.service's `TimeoutStopUSec`, in microseconds (`u64::MAX`: none).
async fn current_limit() -> anyhow::Result<u64> {
    let busctl = crate::platform::find_tool("busctl")
        .ok_or_else(|| anyhow::anyhow!("busctl is not on the node"))?;
    let out = tokio::process::Command::new(busctl)
        .args([
            "get-property",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1/unit/docker_2eservice",
            "org.freedesktop.systemd1.Service",
            "TimeoutStopUSec",
        ])
        .output()
        .await?;
    anyhow::ensure!(
        out.status.success(),
        "no docker.service: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    parse_usec(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| anyhow::anyhow!("unexpected TimeoutStopUSec"))
}

/// `t 90000000` (busctl's way of saying a u64).
fn parse_usec(s: &str) -> Option<u64> {
    s.trim().strip_prefix("t ")?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_busctls_answer() {
        assert_eq!(parse_usec("t 90000000\n"), Some(90_000_000));
        assert_eq!(parse_usec("t 18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_usec("s \"x\""), None);
    }
}
