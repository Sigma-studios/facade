//! Opens TLS ports in the host's firewall (ufw) while the entry point serves
//! them, and closes them when no app asks for them any more.
//!
//! Docker publishes an app's host ports past ufw by itself, but TLS ports are
//! listened on by the entry point, a plain process, so ufw drops their traffic
//! unless told otherwise. Without ufw, or with it inactive, nothing is done.

use tokio::process::Command;
use tracing::{info, warn};

const COMMENT: &str = "facade tls port";

/// ufw rewrites its rule files on each call: two at once lose one's rule.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn active() -> bool {
    match Command::new("ufw").arg("status").output().await {
        Ok(out) => {
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("Status: active")
        }
        Err(_) => false,
    }
}

async fn ufw(args: &[&str]) {
    match Command::new("ufw").args(args).output().await {
        Ok(out) if out.status.success() => {
            let said = String::from_utf8_lossy(&out.stdout);
            if !said.contains("Skipping") {
                info!("ufw {}: {}", args.join(" "), said.trim());
            }
        }
        Ok(out) => warn!(
            "ufw {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => warn!("ufw {}: {e}", args.join(" ")),
    }
}

/// Lets `port/tcp` in (does nothing if it already is).
pub async fn open(port: u16) {
    let _turn = ONE_AT_A_TIME.lock().await;
    if active().await {
        ufw(&["allow", &format!("{port}/tcp"), "comment", COMMENT]).await;
    }
}

/// Closes `port/tcp` again, once no app serves on it.
pub async fn close(port: u16) {
    let _turn = ONE_AT_A_TIME.lock().await;
    if active().await {
        ufw(&["delete", "allow", &format!("{port}/tcp")]).await;
    }
}
