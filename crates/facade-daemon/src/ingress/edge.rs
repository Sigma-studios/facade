//! What the entry point gathers for each app besides its counters: requests
//! that failed, grouped by status, method and path, and a sketch of who
//! visited each day.
//!
//! Both stay off the path of requests that succeed: a failed request takes a
//! lock and a few allocations (its path, normalised), a visitor is one keyed
//! hash and an atomic maximum per connection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use facade_core::proto::{ErrorTally, VisitorSketch};
use parking_lot::Mutex;

use crate::hll;

/// Failed requests grouped per app, at most this many groups: past it, the one
/// seen least recently makes room.
pub const MAX_ERROR_KEYS: usize = 200;
/// Paths are cut to this many bytes.
const MAX_PATH: usize = 160;
/// Requests from facade's outside check (see `server::reach`), which loads every
/// domain each minute: neither a visitor nor an error of the app's.
pub const CHECK_AGENT: &str = "facade-check";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ErrorKey {
    status: u16,
    method: hyper::Method,
    path: String,
}

#[derive(Debug, Clone, Copy)]
struct ErrorStat {
    count: u64,
    first_seen: u64,
    last_seen: u64,
}

/// One app's failed requests and visitors.
pub struct Edge {
    errors: Mutex<HashMap<ErrorKey, ErrorStat>>,
    visitors: Visitors,
}

impl Edge {
    pub fn new() -> Self {
        Self {
            errors: Mutex::new(HashMap::new()),
            visitors: Visitors::new(),
        }
    }

    /// A request answered 4xx or 5xx. `target` is its path and query as sent.
    pub fn record_error(&self, status: u16, method: &hyper::Method, target: &str) {
        let key = ErrorKey {
            status,
            method: method.clone(),
            path: normalize_path(target),
        };
        let now = facade_core::now();
        let mut errors = self.errors.lock();
        if let Some(stat) = errors.get_mut(&key) {
            stat.count += 1;
            stat.last_seen = now;
            return;
        }
        if errors.len() >= MAX_ERROR_KEYS
            && let Some(oldest) = errors
                .iter()
                .min_by_key(|(_, s)| s.last_seen)
                .map(|(k, _)| k.clone())
        {
            errors.remove(&oldest);
        }
        errors.insert(
            key,
            ErrorStat {
                count: 1,
                first_seen: now,
                last_seen: now,
            },
        );
    }

    pub fn visit(&self, hash: u64) {
        self.visitors
            .insert(hash, facade_core::insights::day_of(facade_core::now()));
    }

    pub fn errors(&self) -> Vec<ErrorTally> {
        let mut out: Vec<ErrorTally> = self
            .errors
            .lock()
            .iter()
            .map(|(k, s)| ErrorTally {
                status: k.status,
                method: k.method.to_string(),
                path: k.path.clone(),
                count: s.count,
                first_seen: s.first_seen,
                last_seen: s.last_seen,
            })
            .collect();
        out.sort_by(|a, b| (a.status, &a.method, &a.path).cmp(&(b.status, &b.method, &b.path)));
        out
    }

    pub fn visitors(&self) -> Vec<VisitorSketch> {
        self.visitors
            .snapshot(facade_core::insights::day_of(facade_core::now()))
    }
}

/// Sketches of today's visitors and yesterday's (until the agent has read
/// it): a day's slot is cleared and reused two days later.
struct Visitors {
    days: [Day; 2],
    /// Taken only to clear a slot for a new day.
    roll: Mutex<()>,
}

struct Day {
    /// The day the slot is for (`u64::MAX`: none yet).
    day: AtomicU64,
    registers: Box<[AtomicU8]>,
}

impl Visitors {
    fn new() -> Self {
        let day = || Day {
            day: AtomicU64::new(u64::MAX),
            registers: (0..hll::REGISTERS).map(|_| AtomicU8::new(0)).collect(),
        };
        Self {
            days: [day(), day()],
            roll: Mutex::new(()),
        }
    }

    fn insert(&self, hash: u64, today: u64) {
        let slot = &self.days[(today % 2) as usize];
        if slot.day.load(Ordering::Acquire) != today {
            let _rolling = self.roll.lock();
            if slot.day.load(Ordering::Acquire) != today {
                // The slot held the day before yesterday: nobody writes there
                // any more. Cleared before it says today, so that writers who
                // see today write into a clean slot.
                for r in slot.registers.iter() {
                    r.store(0, Ordering::Relaxed);
                }
                slot.day.store(today, Ordering::Release);
            }
        }
        let (index, rank) = hll::place(hash);
        slot.registers[index].fetch_max(rank, Ordering::Relaxed);
    }

    fn snapshot(&self, today: u64) -> Vec<VisitorSketch> {
        let mut out: Vec<VisitorSketch> = self
            .days
            .iter()
            .filter_map(|slot| {
                let day = slot.day.load(Ordering::Acquire);
                if day != today && day.checked_add(1) != Some(today) {
                    return None;
                }
                let registers: Vec<u8> = slot
                    .registers
                    .iter()
                    .map(|r| r.load(Ordering::Relaxed))
                    .collect();
                Some(VisitorSketch {
                    day,
                    registers: hll::encode(&registers),
                })
            })
            .collect();
        out.sort_by_key(|s| s.day);
        out
    }
}

/// The key a visitor is counted under: its address and browser, hashed with
/// the entry point's secret, so the sketches say nothing about who came.
pub struct VisitorKey(blake3::Hasher);

impl VisitorKey {
    /// The key kept in `dir`, made on first use; stable across restarts and
    /// handovers, so a visitor coming back the same day counts once.
    pub fn load(dir: &std::path::Path) -> Self {
        let file = dir.join("visitor-key");
        let key = std::fs::read(&file)
            .ok()
            .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
            .unwrap_or_else(|| {
                let key: [u8; 32] = rand::random();
                if let Err(e) = crate::tls::write_private(&file, &key) {
                    tracing::warn!("saving the visitor key: {e}");
                }
                key
            });
        Self(blake3::Hasher::new_keyed(&key))
    }

    pub fn hash(&self, ip: std::net::IpAddr, user_agent: &[u8]) -> u64 {
        let mut hasher = self.0.clone();
        match ip {
            std::net::IpAddr::V4(v4) => hasher.update(&v4.octets()),
            // An IPv6 client may use a new address for every connection within its
            // /64 (privacy extensions): the network is what stays.
            std::net::IpAddr::V6(v6) => hasher.update(&v6.octets()[..8]),
        };
        hasher.update(&user_agent[..user_agent.len().min(256)]);
        let mut first = [0u8; 8];
        first.copy_from_slice(&hasher.finalize().as_bytes()[..8]);
        u64::from_le_bytes(first)
    }
}

/// A path as errors are grouped by: no query or fragment, and segments that
/// name one thing among many (numbers, UUIDs, hashes, long tokens) replaced by
/// a placeholder, so `/users/42/avatar?s=64` and `/users/7/avatar` are one
/// line. Cut to a bounded length.
pub fn normalize_path(target: &str) -> String {
    let path = target.split(['?', '#']).next().unwrap_or_default();
    let mut out = String::with_capacity(path.len().min(MAX_PATH) + 8);
    for (i, segment) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(placeholder(segment).unwrap_or(segment));
        if out.len() > MAX_PATH {
            let mut cut = MAX_PATH;
            while !out.is_char_boundary(cut) {
                cut -= 1;
            }
            out.truncate(cut);
            out.push('…');
            break;
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

fn placeholder(segment: &str) -> Option<&'static str> {
    if segment.is_empty() {
        return None;
    }
    // A file name keeps its extension's meaning: `/img/3f2a9c1d8e.png`.
    let (stem, ext) = match segment.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 5 => (stem, Some(ext)),
        _ => (segment, None),
    };
    let kind = if stem.bytes().all(|b| b.is_ascii_digit()) {
        "{n}"
    } else if is_uuid(stem) {
        "{uuid}"
    } else if stem.len() >= 8
        && stem.bytes().all(|b| b.is_ascii_hexdigit())
        && stem.bytes().any(|b| b.is_ascii_digit())
    {
        "{hex}"
    } else if stem.len() >= 20
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && stem.bytes().filter(u8::is_ascii_digit).count() >= 2
        && stem.bytes().any(|b| b.is_ascii_alphabetic())
    {
        "{token}"
    } else {
        return None;
    };
    Some(match (kind, ext) {
        (k, None) => k,
        ("{n}", Some(_)) => "{n}.*",
        ("{uuid}", Some(_)) => "{uuid}.*",
        ("{hex}", Some(_)) => "{hex}.*",
        (_, Some(_)) => "{token}.*",
    })
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_group_what_names_one_of_many() {
        let cases = [
            ("/", "/"),
            ("", "/"),
            ("/users/42/avatar?s=64", "/users/{n}/avatar"),
            ("/users/7/avatar", "/users/{n}/avatar"),
            ("/api/v1/items", "/api/v1/items"),
            (
                "/o/123e4567-e89b-12d3-a456-426614174000/edit",
                "/o/{uuid}/edit",
            ),
            ("/blob/3f2a9c1d8e7b", "/blob/{hex}"),
            ("/assets/app-3f2a9c1d.js", "/assets/app-3f2a9c1d.js"),
            ("/img/3f2a9c1d8e.png", "/img/{hex}.*"),
            ("/2024/10/post", "/{n}/{n}/post"),
            ("/reset/aB3dE5gH7jK9mN1pQ2rS4", "/reset/{token}"),
            ("/deadbeef", "/deadbeef"),
            ("/wp-login.php", "/wp-login.php"),
            ("/a#frag", "/a"),
            ("/search/", "/search/"),
        ];
        for (raw, grouped) in cases {
            assert_eq!(normalize_path(raw), grouped, "{raw}");
        }
        let long = format!("/{}", "segment/".repeat(60));
        let cut = normalize_path(&long);
        assert!(cut.len() <= MAX_PATH + '…'.len_utf8(), "{}", cut.len());
        assert!(cut.ends_with('…'));
        let accents = format!("/{}", "é".repeat(200));
        assert!(normalize_path(&accents).ends_with('…'));
    }

    #[test]
    fn errors_are_counted_per_group_and_bounded() {
        let edge = Edge::new();
        let get = hyper::Method::GET;
        edge.record_error(404, &get, "/users/1?x=2");
        edge.record_error(404, &get, "/users/2");
        edge.record_error(404, &hyper::Method::POST, "/users/2");
        edge.record_error(500, &get, "/users/3");
        let errors = edge.errors();
        let summary: Vec<_> = errors
            .iter()
            .map(|e| (e.status, e.method.as_str(), e.path.as_str(), e.count))
            .collect();
        assert_eq!(
            summary,
            [
                (404, "GET", "/users/{n}", 2),
                (404, "POST", "/users/{n}", 1),
                (500, "GET", "/users/{n}", 1)
            ]
        );
        // Past the bound, the least recently seen go.
        for i in 0..MAX_ERROR_KEYS * 2 {
            edge.record_error(404, &get, &format!("/page-{i}x"));
        }
        assert_eq!(edge.errors().len(), MAX_ERROR_KEYS);
    }

    #[test]
    fn visitors_are_sketched_per_day() {
        let visitors = Visitors::new();
        for i in 0..1000u64 {
            visitors.insert(
                blake3::hash(&i.to_le_bytes()).as_bytes()[..8]
                    .iter()
                    .fold(0, |h, b| h << 8 | u64::from(*b)),
                100,
            );
        }
        visitors.insert(1, 101);
        let days = visitors.snapshot(101);
        assert_eq!(days.iter().map(|d| d.day).collect::<Vec<_>>(), [100, 101]);
        let n = hll::estimate(&hll::decode(&days[0].registers).unwrap());
        assert!((n - 1000.0).abs() < 50.0, "{n}");
        // Two days later, day 100's slot is day 102's, cleared.
        visitors.insert(2, 102);
        let days = visitors.snapshot(102);
        assert_eq!(days.iter().map(|d| d.day).collect::<Vec<_>>(), [101, 102]);
        let n = hll::estimate(&hll::decode(&days[1].registers).unwrap());
        assert!(n < 1.5, "{n}");
        assert!(visitors.snapshot(110).is_empty(), "nothing recent");
    }

    #[test]
    fn visitor_keys_are_stable_and_private() {
        let dir = std::env::temp_dir().join(format!("facade-visitor-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ip: std::net::IpAddr = "203.0.113.7".parse().unwrap();
        let a = VisitorKey::load(&dir).hash(ip, b"Firefox");
        assert_eq!(VisitorKey::load(&dir).hash(ip, b"Firefox"), a, "kept");
        assert_ne!(VisitorKey::load(&dir).hash(ip, b"Chrome"), a);
        // Addresses of one IPv6 network are one visitor.
        let key = VisitorKey::load(&dir);
        assert_eq!(
            key.hash("2001:db8::1".parse().unwrap(), b""),
            key.hash("2001:db8::2:3".parse().unwrap(), b"")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
