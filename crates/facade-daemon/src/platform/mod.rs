//! Everything facade does that depends on the node's distribution goes through
//! here: how facade is installed, where runtime units go, where host tools are,
//! pending updates, whether a reboot is needed.
//!
//! At start-up the daemon reads `/etc/os-release` and picks one backend; the
//! rest only talks to [`Platform`]. A new distribution is one file implementing
//! it plus one line in [`detect`]. Backends share helpers rather than inherit
//! from each other: package managers in [`pkg`], reboot rules in [`reboot`].

mod generic;
pub mod pkg;
pub mod reboot;
mod ubuntu;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use facade_core::events::{Reboot, Updates};
use facade_core::platform::{Checked, InstallKind, OsId, PlatformInfo, Support};
use facade_core::vulns::Inventory;

/// One supported (or best-effort) distribution.
#[async_trait::async_trait]
pub trait Platform: Send + Sync {
    /// The distribution, from `/etc/os-release`.
    fn os(&self) -> &OsId;
    fn support(&self) -> Support;

    /// How facade itself is installed, so how it updates.
    fn install_kind(&self) -> InstallKind {
        install_kind_from_exe()
    }

    /// Where the units facade writes at run time go (sized volumes).
    fn unit_dir(&self) -> PathBuf {
        PathBuf::from("/etc/systemd/system")
    }

    /// A host tool (`mkfs.ext4`, `losetup`, …), or how to get it.
    fn tool(&self, name: &str) -> Result<PathBuf, MissingTool> {
        find_tool(name).ok_or_else(|| MissingTool {
            tool: name.to_string(),
            hint: self.tool_hint().to_string(),
        })
    }

    /// How to install the host tools facade needs, for error messages.
    fn tool_hint(&self) -> &str {
        "install e2fsprogs, mount and util-linux"
    }

    /// Pending system updates, or why this node cannot tell.
    async fn updates(&self) -> Checked<Updates>;

    /// Whether a reboot is needed, and why.
    async fn reboot(&self) -> Checked<Reboot>;

    /// What is installed and running, for the vulnerability check.
    async fn inventory(&self) -> Checked<Inventory> {
        Checked::Unsupported(format!(
            "facade has no vulnerability data for {}",
            self.os().name()
        ))
    }

    /// A path whose modification time changes with the package set.
    fn inventory_marker(&self) -> Option<PathBuf> {
        None
    }
}

/// A host tool that is not on this node.
#[derive(Debug, Clone)]
pub struct MissingTool {
    pub tool: String,
    pub hint: String,
}

impl std::fmt::Display for MissingTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} on the node ({})", self.tool, self.hint)
    }
}

impl std::error::Error for MissingTool {}

/// The node's platform, detected once.
pub fn current() -> &'static dyn Platform {
    static CURRENT: OnceLock<Box<dyn Platform>> = OnceLock::new();
    CURRENT
        .get_or_init(|| {
            let text = ["/etc/os-release", "/usr/lib/os-release"]
                .iter()
                .find_map(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_default();
            detect(OsId::parse(&text))
        })
        .as_ref()
}

/// The backend for a distribution.
pub fn detect(os: OsId) -> Box<dyn Platform> {
    match os.id.as_str() {
        "ubuntu" => Box::new(ubuntu::Ubuntu::new(os)),
        _ => Box::new(generic::Generic::new(os)),
    }
}

/// What the agent tells the server about its system.
pub fn info() -> PlatformInfo {
    let p = current();
    PlatformInfo {
        os: p.os().clone(),
        support: p.support(),
        install: p.install_kind(),
        kernel: running_kernel(),
    }
}

/// The running kernel's release (`uname -r`).
pub fn running_kernel() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A binary in the Nix store is Nix's to replace; anything else is facade's.
fn install_kind_from_exe() -> InstallKind {
    let exe = crate::selfbin::installed_path().unwrap_or_default();
    if exe.starts_with("/nix/store") {
        InstallKind::Managed {
            by: "nix".into(),
            hint: "bump the facade flake input and rebuild (nixos-rebuild switch)".into(),
        }
    } else {
        InstallKind::SelfManaged
    }
}

/// Where host tools may be besides `PATH`: services often get a short one.
const SBIN_DIRS: [&str; 5] = [
    "/usr/local/sbin",
    "/usr/sbin",
    "/sbin",
    "/run/current-system/sw/bin",
    "/run/current-system/sw/sbin",
];

/// `name` on `PATH`, else in the usual system directories.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(SBIN_DIRS.iter().map(PathBuf::from))
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `PATH` for host tools facade runs: its own, then the system directories.
pub fn tool_path() -> std::ffi::OsString {
    let own = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::env::split_paths(&own)
        .chain(
            ["/usr/local/bin", "/usr/bin", "/bin"]
                .iter()
                .chain(SBIN_DIRS.iter())
                .map(PathBuf::from),
        )
        .fold(Vec::<PathBuf>::new(), |mut acc, d| {
            if !acc.contains(&d) {
                acc.push(d);
            }
            acc
        });
    std::env::join_paths(dirs).unwrap_or(own)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_a_backend_by_id() {
        let os = |text: &str| OsId::parse(text);
        let ubuntu = detect(os("ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"26.04\"\n"));
        assert_eq!(ubuntu.os().id, "ubuntu");
        assert_eq!(ubuntu.support(), Support::Supported);
        // Debian is like Ubuntu's parent, not supported (yet).
        let debian = detect(os("ID=debian\nVERSION_ID=\"13\"\n"));
        assert_eq!(debian.support(), Support::BestEffort);
        let mint = detect(os("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n"));
        assert_eq!(mint.support(), Support::BestEffort);
    }

    #[test]
    fn tool_path_keeps_its_own_first_without_repeats() {
        let path = tool_path();
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        let mut seen = std::collections::HashSet::new();
        assert!(dirs.iter().all(|d| seen.insert(d.clone())));
        assert!(dirs.contains(&PathBuf::from("/usr/sbin")));
    }
}
