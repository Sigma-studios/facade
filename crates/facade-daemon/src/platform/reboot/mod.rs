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
