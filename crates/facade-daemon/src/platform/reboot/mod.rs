//! Ways a system says it needs a reboot, for backends to pick from.

use facade_core::events::Reboot;

/// Debian's convention: `/run/reboot-required`, and the packages asking for it
/// in `/run/reboot-required.pkgs`.
pub fn required_file() -> Reboot {
    let exists = |p: &str| std::path::Path::new(p).exists();
    let required = exists("/var/run/reboot-required") || exists("/run/reboot-required");
    let reasons = ["/var/run/reboot-required.pkgs", "/run/reboot-required.pkgs"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .map(|t| crate::platform::pkg::apt::unique_lines(&t))
        .unwrap_or_default();
    Reboot { required, reasons }
}

/// The transient unit a reboot asked by facade runs as: its name tells a
/// second request that one is on its way.
const REBOOT_UNIT: &str = "facade-reboot";

/// Reboots the machine in `delay`, through a systemd timer: the reboot happens
/// even though facade itself is stopped by it, and a failure to arrange it is
/// known at once (rather than after answering).
pub async fn soon(delay: std::time::Duration) -> anyhow::Result<()> {
    if !std::path::Path::new("/run/systemd/system").exists() {
        anyhow::bail!("this node does not run systemd: reboot it by hand");
    }
    let systemd_run = crate::platform::find_tool("systemd-run")
        .ok_or_else(|| anyhow::anyhow!("systemd-run is not on the node"))?;
    let systemctl = crate::platform::find_tool("systemctl")
        .ok_or_else(|| anyhow::anyhow!("systemctl is not on the node"))?;
    let out = tokio::process::Command::new(systemd_run)
        .arg(format!("--unit={REBOOT_UNIT}"))
        .arg(format!("--on-active={}", delay.as_secs().max(1)))
        .args([
            "--timer-property=AccuracySec=1s",
            "--description=Reboot asked by facade",
            "--quiet",
        ])
        .arg(systemctl)
        .arg("reboot")
        .env("PATH", crate::platform::tool_path())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("running systemd-run: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("already") {
            anyhow::bail!("a reboot is already on its way");
        }
        anyhow::bail!("systemd-run: {}", err.trim());
    }
    Ok(())
}
