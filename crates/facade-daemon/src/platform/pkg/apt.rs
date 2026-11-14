//! apt: pending updates as the system already knows them. Nothing is downloaded
//! (no `apt update`): the lists are those the system refreshes.

use std::collections::HashSet;
use std::path::Path;

use facade_core::events::Updates;
use facade_core::platform::Checked;
use tokio::process::Command;

/// At most this many package names are reported.
const NAMES: usize = 100;

/// Ubuntu's `apt-check` (counts, and the security ones), else `apt list
/// --upgradable`.
pub async fn updates() -> Checked<Updates> {
    let apt_check = Path::new("/usr/lib/update-notifier/apt-check");
    if apt_check.exists() {
        // Counts on stderr (`7;2`), then names with --package-names.
        if let Ok(out) = Command::new(apt_check).output().await
            && let Some((count, security)) = parse_apt_check(&String::from_utf8_lossy(&out.stderr))
        {
            let mut names = vec![];
            if count > 0
                && let Ok(out) = Command::new(apt_check)
                    .arg("--package-names")
                    .output()
                    .await
            {
                names = unique_lines(&String::from_utf8_lossy(&out.stderr));
                names.truncate(NAMES);
            }
            return Checked::Known(Updates::Packages {
                count,
                security,
                names,
            });
        }
    }
    match Command::new("apt")
        .args(["list", "--upgradable"])
        .env("LANG", "C")
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            let (names, security) = parse_apt_list(&String::from_utf8_lossy(&out.stdout));
            Checked::Known(Updates::Packages {
                count: names.len() as u32,
                security,
                names: names.into_iter().take(NAMES).collect(),
            })
        }
        Ok(out) => Checked::Failed(format!(
            "apt list --upgradable: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Checked::Failed(format!("running apt: {e}")),
    }
}

pub fn unique_lines(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && seen.insert(l.to_string()))
        .map(str::to_string)
        .collect()
}

/// `apt-check`'s `updates;security`.
fn parse_apt_check(text: &str) -> Option<(u32, u32)> {
    let line = text.lines().map(str::trim).find(|l| l.contains(';'))?;
    let (a, b) = line.split_once(';')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// Package names of `apt list --upgradable`, and how many come from a
/// security suite.
fn parse_apt_list(text: &str) -> (Vec<String>, u32) {
    let mut names = Vec::new();
    let mut security = 0;
    for line in text.lines() {
        let Some((name, rest)) = line.split_once('/') else {
            continue;
        };
        if !line.contains("[upgradable from") {
            continue;
        }
        let suites = rest.split_whitespace().next().unwrap_or("");
        if suites.split(',').any(|s| s.ends_with("-security")) {
            security += 1;
        }
        names.push(name.to_string());
    }
    (names, security)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_apt_outputs() {
        assert_eq!(parse_apt_check("7;2"), Some((7, 2)));
        assert_eq!(parse_apt_check("0;0\n"), Some((0, 0)));
        assert_eq!(parse_apt_check("nonsense"), None);
        let list = "Listing...\n\
            libssl3t64/noble-updates,noble-security 3.0.13-0ubuntu3.4 amd64 [upgradable from: 3.0.13-0ubuntu3.1]\n\
            snapd/noble-updates 2.65.3+24.04 amd64 [upgradable from: 2.63+24.04]\n";
        assert_eq!(
            parse_apt_list(list),
            (vec!["libssl3t64".to_string(), "snapd".to_string()], 1)
        );
        assert_eq!(
            unique_lines("linux-image-6.8.0-48-generic\nlinux-base\nlinux-base\n"),
            vec!["linux-image-6.8.0-48-generic", "linux-base"]
        );
    }
}

/// Where the last upgrade's output goes (a tmpfs: one run's worth).
const UPGRADE_LOG: &str = "/run/facade-updates.log";

/// Refreshes the package lists and installs every pending update, new
/// dependencies included (a new kernel comes as one), keeping changed
/// configuration files. Returns the output's last lines.
///
/// apt runs as a unit of its own (`systemd-run --wait`), out of facade's: an
/// upgrade restarting a service must never stop dpkg halfway, and
/// `NEEDRESTART_SUSPEND` keeps needrestart from restarting facade meanwhile
/// (services left on old libraries are restarted afterwards, or by the reboot).
pub async fn upgrade() -> anyhow::Result<Vec<String>> {
    let systemd_run = crate::platform::find_tool("systemd-run")
        .ok_or_else(|| anyhow::anyhow!("systemd-run is not on the node"))?;
    let lock = "-o DPkg::Lock::Timeout=900";
    let script = format!(
        "exec >{UPGRADE_LOG} 2>&1; apt-get -q {lock} update && apt-get -y -q {lock} \
         -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold \
         --with-new-pkgs upgrade && apt-get -y -q {lock} autoremove"
    );
    let out = Command::new(systemd_run)
        .args([
            "--unit=facade-updates",
            "--description=System updates installed by facade",
            "--wait",
            "--collect",
            "--quiet",
            "--setenv=DEBIAN_FRONTEND=noninteractive",
            "--setenv=NEEDRESTART_SUSPEND=1",
            "--setenv=LANG=C",
            "/bin/sh",
            "-c",
            &script,
        ])
        .env("PATH", crate::platform::tool_path())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("running systemd-run: {e}"))?;
    let log = std::fs::read_to_string(UPGRADE_LOG).unwrap_or_default();
    let tail: Vec<String> = {
        let lines: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
        lines[lines.len().saturating_sub(40)..]
            .iter()
            .map(|l| l.to_string())
            .collect()
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = tail.last().cloned().unwrap_or_default();
        if err.contains("already") && err.contains("facade-updates") {
            anyhow::bail!("updates are already being installed");
        }
        anyhow::bail!(
            "installing updates failed: {}",
            if last.is_empty() {
                err.trim().to_string()
            } else {
                last
            }
        );
    }
    Ok(tail)
}

/// Restarts the services still running replaced libraries, a few seconds from
/// now (facade may be one), when needrestart is there.
pub async fn restart_outdated_services() {
    let (Some(systemd_run), Some(needrestart)) = (
        crate::platform::find_tool("systemd-run"),
        crate::platform::find_tool("needrestart"),
    ) else {
        return;
    };
    let _ = Command::new(systemd_run)
        .args([
            "--unit=facade-needrestart",
            "--on-active=10",
            "--quiet",
            "--description=Restart services after updates installed by facade",
        ])
        .arg(needrestart)
        .args(["-r", "a", "-q"])
        .env("PATH", crate::platform::tool_path())
        .output()
        .await;
}
