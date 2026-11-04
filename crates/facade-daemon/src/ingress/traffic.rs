//! What the entry point counts for each app it routes to: requests by how they
//! were answered, how long the app took to answer, and the connections open.
//!
//! Counters only grow, from the moment the entry point first routed to the
//! app; the agent reads them with each report and the server turns the
//! difference between two readings into rates and percentiles. On the request
//! path that is a few relaxed atomic additions, nothing allocated or locked.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use facade_core::proto::{EdgeReading, LATENCY_BOUNDS_MS, LATENCY_BUCKETS, Traffic};

use super::edge::Edge;

/// One app's counters.
pub struct AppTraffic {
    since: u64,
    requests: AtomicU64,
    client_errors: AtomicU64,
    server_errors: AtomicU64,
    latency: [AtomicU64; LATENCY_BUCKETS],
    open: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    /// Failed requests and visitors.
    pub edge: Edge,
}

impl AppTraffic {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            since: facade_core::now(),
            requests: AtomicU64::new(0),
            client_errors: AtomicU64::new(0),
            server_errors: AtomicU64::new(0),
            latency: std::array::from_fn(|_| AtomicU64::new(0)),
            open: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            edge: Edge::new(),
        })
    }

    /// Bytes a client connection received and sent (see [`super::meter`]).
    pub fn add_bytes(&self, received: u64, sent: u64) {
        if received > 0 {
            self.bytes_in.fetch_add(received, Ordering::Relaxed);
        }
        if sent > 0 {
            self.bytes_out.fetch_add(sent, Ordering::Relaxed);
        }
    }

    /// A request answered with `status`, the app's answer taking `took` to its
    /// headers (`None` for the entry point's own answers: an app that is down
    /// would otherwise read as very fast).
    pub fn record(&self, status: u16, took: Option<Duration>) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        match status {
            400..=499 => self.client_errors.fetch_add(1, Ordering::Relaxed),
            500.. => self.server_errors.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
        if let Some(took) = took {
            self.latency[bucket(took)].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn open(&self) -> u64 {
        self.open.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> Traffic {
        Traffic {
            since: self.since,
            requests: self.requests.load(Ordering::Relaxed),
            client_errors: self.client_errors.load(Ordering::Relaxed),
            server_errors: self.server_errors.load(Ordering::Relaxed),
            latency: self
                .latency
                .iter()
                .map(|c| c.load(Ordering::Relaxed))
                .collect(),
            open: self.open(),
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            bytes_out: self.bytes_out.load(Ordering::Relaxed),
        }
    }

    pub fn edge_snapshot(&self) -> EdgeReading {
        EdgeReading {
            since: self.since,
            errors: self.edge.errors(),
            visitors: self.edge.visitors(),
        }
    }
}

/// The histogram bucket of a response time: the first whose bound it does not
/// exceed, or the last.
fn bucket(took: Duration) -> usize {
    let ms = took.as_secs_f64() * 1000.0;
    LATENCY_BOUNDS_MS.partition_point(|bound| *bound < ms)
}

/// Counts a connection open for an app while alive.
pub struct Open(Arc<AppTraffic>);

impl Open {
    pub fn new(app: Arc<AppTraffic>) -> Self {
        app.open.fetch_add(1, Ordering::Relaxed);
        Self(app)
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_by_class_and_time() {
        let app = AppTraffic::new();
        app.record(200, Some(Duration::from_micros(400)));
        app.record(101, Some(Duration::from_millis(60)));
        app.record(404, Some(Duration::from_millis(1)));
        app.record(502, None);
        app.record(503, Some(Duration::from_secs(60)));
        let open = Open::new(app.clone());
        let t = app.snapshot();
        assert_eq!(
            (t.requests, t.client_errors, t.server_errors, t.open),
            (5, 1, 2, 1)
        );
        assert_eq!(t.latency.len(), LATENCY_BUCKETS);
        assert_eq!(
            t.latency.iter().sum::<u64>(),
            4,
            "not the entry point's own"
        );
        assert_eq!(t.latency[0], 2, "up to 1 ms, inclusive");
        assert_eq!(t.latency[11], 1, "50 to 75 ms");
        assert_eq!(t.latency[LATENCY_BUCKETS - 1], 1, "past the last bound");
        drop(open);
        assert_eq!(app.open(), 0);
    }
}
