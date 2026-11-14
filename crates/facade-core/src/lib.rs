//! Types shared by the daemon (server and agent) and the CLI.

pub mod api;
pub mod env;
pub mod events;
pub mod exec;
pub mod git;
pub mod insights;
pub mod manifest;
#[cfg(feature = "pinned-tls")]
pub mod pinned;
pub mod platform;
pub mod proto;
pub mod reboot_report;
pub mod resources;
pub mod spec;
pub mod stack;
pub mod templates;
pub mod vulns;

pub use spec::*;

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A Unix time as RFC 3339, in UTC: `2026-11-14T03:12:45Z`.
pub fn rfc3339(at: u64) -> String {
    let (y, m, d) = insights::civil(insights::day_of(at));
    let secs = at % insights::DAY;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn rfc3339() {
        assert_eq!(super::rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(super::rfc3339(1_794_625_965), "2026-11-14T03:12:45Z");
    }
}
