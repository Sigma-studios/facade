//! Types shared by the daemon (server and agent) and the CLI.

pub mod api;
pub mod manifest;
#[cfg(feature = "pinned-tls")]
pub mod pinned;
pub mod proto;
pub mod spec;

pub use spec::*;

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
