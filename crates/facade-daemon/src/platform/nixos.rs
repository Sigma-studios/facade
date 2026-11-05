//! NixOS: Nix owns facade's binaries and units, so facade never writes to
//! `/usr` or `/etc`; it reports, the system's configuration changes.
//!
//! Updates are where the running system's nixpkgs stands against its branch
//! (the server, which can reach the channel, says how far behind it is); a
//! reboot is needed when the booted system's kernel, initrd or modules are not
//! the current system's; the inventory is the current system's store paths.

use std::path::{Path, PathBuf};

use facade_core::events::{ChannelState, Reboot, Updates};
use facade_core::platform::{Checked, OsId, Support};
use facade_core::vulns::{Inventory, Package};
use tokio::process::Command;

use super::Platform;

pub struct NixOs {
    os: OsId,
}

impl NixOs {
    pub fn new(os: OsId) -> Self {
        Self { os }
    }
}

#[async_trait::async_trait]
impl Platform for NixOs {
    fn os(&self) -> &OsId {
        &self.os
    }

    fn support(&self) -> Support {
        Support::Supported
    }

    fn tool_hint(&self) -> &str {
        "enable services.facade, or add e2fsprogs and util-linux to the service's path"
    }

    async fn updates(&self) -> Checked<Updates> {
        let out = match Command::new("nixos-version")
            .arg("--json")
            .env("PATH", super::tool_path())
            .output()
            .await
        {
            Ok(out) if out.status.success() => out,
            Ok(out) => {
                return Checked::Failed(format!(
                    "nixos-version: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Err(e) => return Checked::Failed(format!("running nixos-version: {e}")),
        };
        let branch = std::env::var("FACADE_NIXPKGS_BRANCH").ok();
        match channel(&String::from_utf8_lossy(&out.stdout), branch.as_deref()) {
            Some(c) => Checked::Known(Updates::Channel(c)),
            None => Checked::Failed("nixos-version --json gave no nixpkgs revision".into()),
        }
    }

    async fn reboot(&self) -> Checked<Reboot> {
        Checked::Known(booted_vs_current(
            Path::new("/run/booted-system"),
            Path::new("/run/current-system"),
        ))
    }

    async fn inventory(&self) -> Checked<Inventory> {
        let out = match Command::new("nix-store")
            .args(["--query", "--requisites", "/run/current-system"])
            .env("PATH", super::tool_path())
            .kill_on_drop(true)
            .output()
            .await
        {
            Ok(out) if out.status.success() => out,
            Ok(out) => {
                return Checked::Failed(format!(
                    "nix-store --query: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Err(e) => return Checked::Failed(format!("running nix-store: {e}")),
        };
        let mut packages = store_packages(&String::from_utf8_lossy(&out.stdout));
        // The kernel the next boot runs, apart: the running one is `kernel`.
        if let Some(v) = kernel_version(Path::new("/run/current-system/kernel")) {
            packages.push(Package {
                name: CURRENT_KERNEL.into(),
                version: v,
                ..Default::default()
            });
        }
        let kernel = super::running_kernel();
        Checked::Known(Inventory {
            hash: Inventory::hash_of("nix", &packages, kernel.as_deref()),
            kind: "nix".into(),
            packages,
            kernel,
        })
    }

    fn inventory_marker(&self) -> Option<PathBuf> {
        // Repointed by every switch.
        Some(PathBuf::from("/run/current-system"))
    }
}

/// In a NixOS inventory, the version of the current system's kernel (what the
/// next boot runs).
pub const CURRENT_KERNEL: &str = "@current-kernel";

/// `nixos-version --json`: the release, and the nixpkgs revision it was built from.
fn channel(json: &str, branch: Option<&str>) -> Option<ChannelState> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let version = v.get("nixosVersion")?.as_str()?;
    let revision = v.get("nixpkgsRevision")?.as_str()?.to_string();
    // `26.05.20261020.0a1b2c3 (Yarara)`, or `26.11pre…` on unstable.
    let release: String = version
        .split('.')
        .take(2)
        .collect::<Vec<_>>()
        .join(".")
        .split("pre")
        .next()
        .unwrap_or_default()
        .to_string();
    let unstable = version.contains("pre");
    let branch = match branch.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => b.to_string(),
        None if unstable => "nixos-unstable".into(),
        None => format!("nixos-{release}"),
    };
    Some(ChannelState {
        release,
        revision,
        branch,
        ..Default::default()
    })
}

/// A reboot is needed when the booted system's kernel, initrd or modules are
/// not the current system's.
fn booted_vs_current(booted: &Path, current: &Path) -> Reboot {
    let link = |root: &Path, what: &str| std::fs::read_link(root.join(what)).ok();
    let mut reasons = vec![];
    for what in ["kernel", "initrd", "kernel-modules"] {
        let (b, c) = (link(booted, what), link(current, what));
        if b.is_none() || c.is_none() || b == c {
            continue;
        }
        if what == "kernel" {
            match (
                kernel_version(&booted.join("kernel")),
                kernel_version(&current.join("kernel")),
            ) {
                (Some(from), Some(to)) if from != to => {
                    reasons.push(format!("kernel {from} → {to}"))
                }
                _ => reasons.push("kernel changed".into()),
            }
        } else if !(what == "kernel-modules" && reasons.iter().any(|r| r.starts_with("kernel"))) {
            reasons.push(format!("{what} changed"));
        }
    }
    Reboot {
        required: !reasons.is_empty(),
        reasons,
    }
}

/// `…/kernel` → `/nix/store/<hash>-linux-6.12.44/bzImage` → `6.12.44`.
fn kernel_version(link: &Path) -> Option<String> {
    let target = std::fs::read_link(link).ok()?;
    let dir = target.parent()?.file_name()?.to_string_lossy().to_string();
    let (name, version) = split_store_name(&dir)?;
    (name == "linux").then_some(version)
}

/// Store paths as packages: `/nix/store/<hash>-openssl-3.5.3` → openssl 3.5.3.
/// Paths without a version (scripts, configuration files) are left out.
fn store_packages(text: &str) -> Vec<Package> {
    let mut seen = std::collections::HashSet::new();
    let mut out = vec![];
    for line in text.lines() {
        let Some(base) = line.trim().rsplit('/').next() else {
            continue;
        };
        let Some((name, version)) = split_store_name(base) else {
            continue;
        };
        // Outputs other than the main one (`-dev`, `-lib`, `-man`) are the same package.
        if seen.insert((name.clone(), version.clone())) {
            out.push(Package {
                name,
                version,
                ..Default::default()
            });
        }
    }
    out.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    out
}

/// `<hash>-name-1.2.3[-output]` → (`name`, `1.2.3`): the version starts at the
/// first dash followed by a digit, as Nix's `parseDrvName` does.
fn split_store_name(base: &str) -> Option<(String, String)> {
    let (_hash, rest) = base.split_once('-')?;
    let bytes = rest.as_bytes();
    let at = (0..bytes.len())
        .find(|&i| bytes[i] == b'-' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))?;
    let name = &rest[..at];
    let mut version = &rest[at + 1..];
    for output in [
        "-bin", "-dev", "-lib", "-out", "-man", "-doc", "-info", "-modules", "-debug",
    ] {
        if let Some(v) = version.strip_suffix(output) {
            version = v;
            break;
        }
    }
    (!name.is_empty() && !version.is_empty()).then(|| (name.to_string(), version.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nixos_version() {
        let c = channel(
            r#"{"nixosVersion":"26.05.20261020.0a1b2c3","nixpkgsRevision":"0a1b2c3d4e5f60718293a4b5c6d7e8f901234567"}"#,
            None,
        )
        .unwrap();
        assert_eq!(c.release, "26.05");
        assert_eq!(c.branch, "nixos-26.05");
        assert_eq!(c.revision, "0a1b2c3d4e5f60718293a4b5c6d7e8f901234567");
        let unstable = channel(
            r#"{"nixosVersion":"26.11pre912345.0a1b2c3","nixpkgsRevision":"0a1b2c3"}"#,
            None,
        )
        .unwrap();
        assert_eq!(unstable.release, "26.11");
        assert_eq!(unstable.branch, "nixos-unstable");
        let pinned = channel(
            r#"{"nixosVersion":"26.05.20261020.0a1b2c3","nixpkgsRevision":"0a1b2c3"}"#,
            Some("nixos-26.05-small"),
        )
        .unwrap();
        assert_eq!(pinned.branch, "nixos-26.05-small");
        assert!(channel("{}", None).is_none());
    }

    #[test]
    fn reads_store_paths() {
        let text = "/nix/store/0a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p-openssl-3.5.3\n\
                    /nix/store/1a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p-openssl-3.5.3-bin\n\
                    /nix/store/2a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p-linux-6.12.44-modules\n\
                    /nix/store/3a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p-unit-docker.service\n\
                    /nix/store/4a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p-etc-os-release\n\
                    /nix/store/5a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p-python3.12-requests-2.32.3\n\
                    /nix/store/6a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p-nixos-system-facade-26.05.20261020.0a1b2c3\n";
        let p = store_packages(text);
        let names: Vec<(&str, &str)> = p
            .iter()
            .map(|p| (p.name.as_str(), p.version.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("linux", "6.12.44"),
                ("nixos-system-facade", "26.05.20261020.0a1b2c3"),
                ("openssl", "3.5.3"),
                ("python3.12-requests", "2.32.3"),
            ]
        );
    }

    #[test]
    fn a_new_kernel_asks_for_a_reboot() {
        let root = std::env::temp_dir().join(format!("facade-nixos-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let make = |name: &str, kernel: &str, initrd: &str| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(
                format!("/nix/store/aaaa-linux-{kernel}/bzImage"),
                dir.join("kernel"),
            )
            .unwrap();
            std::os::unix::fs::symlink(
                format!("/nix/store/{initrd}-initrd-linux-{kernel}/initrd"),
                dir.join("initrd"),
            )
            .unwrap();
            dir
        };
        let booted = make("booted", "6.12.40", "bbbb");
        let same = make("same", "6.12.40", "bbbb");
        let newer = make("newer", "6.12.44", "cccc");
        let initrd = make("initrd", "6.12.40", "dddd");
        assert_eq!(booted_vs_current(&booted, &same), Reboot::default());
        let r = booted_vs_current(&booted, &newer);
        assert!(r.required);
        assert_eq!(r.reasons, ["kernel 6.12.40 → 6.12.44", "initrd changed"]);
        assert_eq!(
            booted_vs_current(&booted, &initrd).reasons,
            ["initrd changed"]
        );
        // Not NixOS, or no booted system recorded: nothing to say.
        assert_eq!(
            booted_vs_current(&root.join("none"), &newer),
            Reboot::default()
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
