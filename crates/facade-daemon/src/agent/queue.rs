//! The node's build slot: one build at a time (a small VPS would run out of memory
//! with more), the others waiting their turn in order. The queue is published so
//! the server can show it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use facade_core::proto::{BuildEntry, BuildState};
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use uuid::Uuid;

pub struct BuildQueue {
    slots: Arc<Semaphore>,
    entries: Mutex<Vec<(u64, BuildEntry)>>,
    next: AtomicU64,
    published: watch::Sender<Vec<BuildEntry>>,
}

/// Held while building; leaving the queue when dropped (also on cancellation).
pub struct Slot {
    _permit: OwnedSemaphorePermit,
    _ticket: Ticket,
}

struct Ticket {
    queue: Arc<BuildQueue>,
    id: u64,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.queue.entries.lock().retain(|(id, _)| *id != self.id);
        self.queue.publish();
    }
}

impl BuildQueue {
    pub fn new(parallel: usize) -> Arc<Self> {
        Arc::new(Self {
            slots: Arc::new(Semaphore::new(parallel.max(1))),
            entries: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
            published: watch::channel(Vec::new()).0,
        })
    }

    fn publish(&self) {
        let snapshot: Vec<BuildEntry> =
            self.entries.lock().iter().map(|(_, e)| e.clone()).collect();
        self.published.send_replace(snapshot);
    }

    pub fn subscribe(&self) -> watch::Receiver<Vec<BuildEntry>> {
        self.published.subscribe()
    }

    /// Waits for the build slot. Builds get it in the order they asked.
    pub async fn acquire(
        self: &Arc<Self>,
        app: &str,
        deployment: Option<Uuid>,
        commit: Option<String>,
    ) -> Slot {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.entries.lock().push((
            id,
            BuildEntry {
                app: app.to_string(),
                deployment,
                commit,
                state: BuildState::Queued,
                since: facade_core::now(),
            },
        ));
        self.publish();
        let ticket = Ticket {
            queue: self.clone(),
            id,
        };
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        if let Some((_, entry)) = self.entries.lock().iter_mut().find(|(i, _)| *i == id) {
            entry.state = BuildState::Building;
            entry.since = facade_core::now();
        }
        self.publish();
        Slot {
            _permit: permit,
            _ticket: ticket,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn builds_wait_their_turn() {
        let queue = BuildQueue::new(1);
        let rx = queue.subscribe();
        let first = queue.acquire("a", None, None).await;
        let q = queue.clone();
        let waiting = tokio::spawn(async move { q.acquire("b", None, None).await });
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let states: Vec<_> = rx
            .borrow()
            .iter()
            .map(|e| (e.app.clone(), e.state))
            .collect();
        assert_eq!(
            states,
            [
                ("a".to_string(), BuildState::Building),
                ("b".to_string(), BuildState::Queued)
            ]
        );
        drop(first);
        let second = waiting.await.unwrap();
        assert_eq!(rx.borrow()[0].state, BuildState::Building);
        drop(second);
        assert!(rx.borrow().is_empty());
        // A cancelled waiter leaves the queue too.
        let held = queue.acquire("c", None, None).await;
        let q = queue.clone();
        let cancelled = tokio::spawn(async move { q.acquire("d", None, None).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancelled.abort();
        let _ = cancelled.await;
        assert_eq!(rx.borrow().len(), 1);
        drop(held);
    }
}
