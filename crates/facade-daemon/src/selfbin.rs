//! The running `facaded` executable: a fingerprint to tell builds apart, the
//! commit it was built from, and the path it is installed at (where an update
//! puts the next one).

use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct Binary {
    /// SHA-256 of the executable, hex.
    pub sha256: String,
    pub size: u64,
}

/// The commit this binary was built from: `FACADE_COMMIT` at build time, set by
/// CI and by `deploy/build-linux.sh` for a clean checkout.
pub fn commit() -> Option<&'static str> {
    option_env!("FACADE_COMMIT").filter(|c| !c.is_empty())
}

/// `facaded --version`: the crate version, and the commit when known.
pub fn version_line() -> &'static str {
    static LINE: OnceLock<String> = OnceLock::new();
    LINE.get_or_init(|| match commit() {
        Some(c) => format!("{} ({})", env!("CARGO_PKG_VERSION"), short_commit(c)),
        None => env!("CARGO_PKG_VERSION").to_string(),
    })
}

pub fn short_commit(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// The running executable, read once. `/proc/self/exe` still reads the running
/// file after it was replaced on disk.
pub fn current() -> Option<&'static Binary> {
    static CURRENT: OnceLock<Option<Binary>> = OnceLock::new();
    CURRENT
        .get_or_init(|| {
            use sha2::{Digest, Sha256};
            let bytes = std::fs::read("/proc/self/exe").ok()?;
            Some(Binary {
                sha256: hex::encode(Sha256::digest(&bytes)),
                size: bytes.len() as u64,
            })
        })
        .as_ref()
}

/// Where this executable was started from, even if it has been replaced since
/// (the running copy then shows as "… (deleted)").
pub fn installed_path() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let path = exe.to_string_lossy();
    Ok(PathBuf::from(
        path.strip_suffix(" (deleted)").unwrap_or(&path).to_string(),
    ))
}

/// The running executable's contents.
pub async fn open() -> std::io::Result<tokio::fs::File> {
    tokio::fs::File::open("/proc/self/exe").await
}

/// The first characters of a fingerprint, for logs.
pub fn short(sha256: &str) -> &str {
    sha256.get(..12).unwrap_or(sha256)
}
