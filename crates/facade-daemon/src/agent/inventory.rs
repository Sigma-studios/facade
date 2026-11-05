//! What is installed on the node, for the server's vulnerability check. The
//! server asks for it (every few hours, at connection, and when told it
//! changed); only the hash travels when nothing changed.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use facade_core::platform::Checked;
use facade_core::proto::AgentEvent;
use facade_core::vulns::Inventory;
use tracing::debug;

use super::Agent;

/// How often the package set is looked at for changes.
const LOOK_EVERY: Duration = Duration::from_secs(60);
/// An upgrade changes it many times in a row: wait for it to settle.
const SETTLE: Duration = Duration::from_secs(120);

/// The inventory, or `None` when its hash is still `known`.
pub async fn answer(known: Option<String>) -> Checked<Option<Inventory>> {
    crate::platform::current()
        .inventory()
        .await
        .map(|inv| (known.as_deref() != Some(inv.hash.as_str())).then_some(inv))
}

/// Tells the server when the package set changed.
pub async fn watch(agent: Arc<Agent>) {
    let Some(marker) = crate::platform::current().inventory_marker() else {
        return;
    };
    let modified = || {
        std::fs::symlink_metadata(&marker)
            .and_then(|m| m.modified())
            .ok()
    };
    let mut seen: Option<SystemTime> = modified();
    loop {
        tokio::time::sleep(LOOK_EVERY).await;
        let now = modified();
        if now == seen {
            continue;
        }
        // Settled: unchanged for a while.
        tokio::time::sleep(SETTLE).await;
        if modified() != now {
            continue;
        }
        seen = now;
        if let Checked::Known(inv) = crate::platform::current().inventory().await {
            debug!(hash = %inv.hash, "packages changed");
            agent.emit(AgentEvent::InventoryChanged { hash: inv.hash });
        }
    }
}
