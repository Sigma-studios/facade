//! What entry points report beyond rates: bytes served, failed requests and
//! visitors, turned into what each report adds and kept per day (bytes,
//! visitors) or per 10 minutes (failed requests).
//!
//! Readings are cumulative (since the entry point started counting for the
//! app), so the server keeps the last one of each app and adds the difference.
//! Its first report from a node after the server starts only sets where counts
//! stand: what came in while the server was away is not known. After that, an
//! app or error group it has not heard of is new, and counts whole.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};

use facade_core::insights::{
    AppInsights, Bandwidth, DayVisitors, ErrorPeriod, ErrorsView, NodeInsights, day_of, month_start,
};
use facade_core::proto::NodeReport;
use parking_lot::Mutex;
use tracing::warn;

use super::Server;
use super::api::{ApiError, ApiResult, User};
use super::store::Store;
use super::store::edge::{ERROR_BUCKET, ErrorAdd};
use super::users::load_app_for;
use crate::hll;

/// Error groups an API answer lists at most.
const ERROR_ROWS: usize = 500;
/// Days the visitors chart covers.
pub const VISITOR_DAYS: u64 = 30;

#[derive(Default)]
pub struct Edge {
    marks: Mutex<Marks>,
}

#[derive(Default)]
struct Marks {
    /// Nodes heard from since the server started.
    nodes: HashSet<String>,
    /// App → its last byte counters: `(since, received, sent)`.
    bytes: HashMap<String, (u64, u64, u64)>,
    errors: HashMap<String, ErrorMarks>,
}

struct ErrorMarks {
    since: u64,
    /// `(status, method, path)` → `(first_seen, count, last_seen)`.
    groups: HashMap<(u16, String, String), (u64, u64, u64)>,
}

/// What a group's new reading adds to the one before: `(count, first_seen)`
/// of the new requests. `known`: the group's last reading, if any; `whole`:
/// whether a group not heard of before is new (rather than unknown).
fn added(
    known: Option<(u64, u64, u64)>,
    first_seen: u64,
    count: u64,
    last_seen: u64,
    whole: bool,
) -> Option<(u64, u64)> {
    match known {
        Some((first, before, last)) if first == first_seen && count >= before => {
            (count > before).then(|| (count - before, (last + 1).min(last_seen)))
        }
        // Forgotten by the entry point to make room, and seen again.
        Some(_) => Some((count, first_seen)),
        None if whole => Some((count, first_seen)),
        None => None,
    }
}

impl Edge {
    /// Takes what a node's report adds: bytes, failed requests, visitors.
    pub fn record(&self, store: &Store, node: &str, at: u64, report: &NodeReport) {
        let mut marks = self.marks.lock();
        let heard_before = !marks.nodes.insert(node.to_string());
        let day = day_of(at);

        for stats in &report.apps {
            let Some(t) = &stats.traffic else { continue };
            let now = (t.since, t.bytes_in, t.bytes_out);
            let delta = match marks.bytes.insert(stats.app.clone(), now) {
                Some((since, i, o)) if since == t.since && t.bytes_in >= i && t.bytes_out >= o => {
                    Some((t.bytes_in - i, t.bytes_out - o))
                }
                // A new entry point counts from zero: all of it is new.
                Some(_) => Some((t.bytes_in, t.bytes_out)),
                None if heard_before => Some((t.bytes_in, t.bytes_out)),
                None => None,
            };
            if let Some((received, sent)) = delta.filter(|d| *d != (0, 0))
                && let Err(e) = store.add_bytes(&stats.app, node, day, received, sent)
            {
                warn!("recording bytes of {}: {e:#}", stats.app);
            }
        }

        let bucket = at / ERROR_BUCKET * ERROR_BUCKET;
        for (app, reading) in &report.edge {
            let fresh = !marks.errors.contains_key(app);
            let mine = marks
                .errors
                .entry(app.clone())
                .or_insert_with(|| ErrorMarks {
                    since: reading.since,
                    groups: HashMap::new(),
                });
            let restarted = mine.since != reading.since;
            if restarted {
                *mine = ErrorMarks {
                    since: reading.since,
                    groups: HashMap::new(),
                };
            }
            let whole = !fresh || heard_before || restarted;
            let mut adds = Vec::new();
            for e in &reading.errors {
                let key = (e.status, e.method.clone(), e.path.clone());
                let known = mine
                    .groups
                    .insert(key, (e.first_seen, e.count, e.last_seen));
                if let Some((count, first_seen)) =
                    added(known, e.first_seen, e.count, e.last_seen, whole)
                {
                    adds.push(ErrorAdd {
                        status: e.status,
                        method: &e.method,
                        path: &e.path,
                        count,
                        first_seen,
                        last_seen: e.last_seen,
                    });
                }
            }
            if let Err(e) = store.add_errors(app, bucket, &adds) {
                warn!("recording failed requests of {app}: {e:#}");
            }
            for sketch in &reading.visitors {
                match hll::decode(&sketch.registers) {
                    Some(registers) => {
                        if let Err(e) = store.merge_visitors(app, sketch.day, &registers) {
                            warn!("recording visitors of {app}: {e:#}");
                        }
                    }
                    None => warn!("{node} sent a visitor sketch that is not one"),
                }
            }
        }
    }
}

/// Bytes of this calendar month and the one before, from days of
/// `(day, received, sent)`.
pub fn bandwidth(days: &[(u64, u64, u64)], today: u64) -> Bandwidth {
    let start = month_start(today);
    let last_start = month_start(start.saturating_sub(1));
    let mut b = Bandwidth {
        month_start: start,
        ..Default::default()
    };
    for &(day, received, sent) in days {
        if day >= start {
            b.month_in += received;
            b.month_out += sent;
            if day == today {
                b.today_in += received;
                b.today_out += sent;
            }
        } else if day >= last_start {
            b.last_month_in += received;
            b.last_month_out += sent;
        }
    }
    b
}

fn bandwidth_of(server: &Server, name: &str, node: bool, today: u64) -> Bandwidth {
    let from = month_start(month_start(today).saturating_sub(1));
    match server.store.bytes_by_day(name, node, from) {
        Ok(days) => bandwidth(&days, today),
        Err(e) => {
            warn!("reading bytes of {name}: {e:#}");
            Bandwidth::default()
        }
    }
}

/// The last `VISITOR_DAYS` days of visitors, today last.
pub fn visitors(server: &Server, app: &str, today: u64) -> Vec<DayVisitors> {
    let from = today + 1 - VISITOR_DAYS;
    let sketches: HashMap<u64, Vec<u8>> = server
        .store
        .visitor_sketches(app, from)
        .unwrap_or_default()
        .into_iter()
        .collect();
    (from..=today)
        .map(|day| DayVisitors {
            day,
            visitors: sketches
                .get(&day)
                .map_or(0, |r| hll::estimate(r).round() as u64),
        })
        .collect()
}

pub fn app_insights(server: &Server, app: &facade_core::AppSpec) -> AppInsights {
    let now = facade_core::now();
    let today = day_of(now);
    AppInsights {
        availability: super::reach::availability(server, app, now),
        visitors: visitors(server, &app.name, today),
        bandwidth: bandwidth_of(server, &app.name, false, today),
        reach: server
            .reach
            .of_domains(app.domains.iter().chain(&app.redirects)),
        routed: !app.domains.is_empty() || !app.tls_ports.is_empty(),
    }
}

pub fn node_insights(server: &Server, node: &str) -> NodeInsights {
    NodeInsights {
        bandwidth: bandwidth_of(server, node, true, day_of(facade_core::now())),
    }
}

pub fn errors(server: &Server, app: &str, period: ErrorPeriod) -> anyhow::Result<ErrorsView> {
    let since = facade_core::now().saturating_sub(period.secs());
    let (rows, total) = server.store.errors_since(app, since, ERROR_ROWS)?;
    Ok(ErrorsView {
        period,
        more: total.saturating_sub(rows.len()),
        rows,
    })
}

// ---------------------------------------------------------------- API

type S = State<Arc<Server>>;

pub async fn get_app_insights(
    State(server): S,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<Json<AppInsights>> {
    let row = load_app_for(&server, &user, &name)?;
    Ok(Json(app_insights(&server, &row.spec)))
}

#[derive(serde::Deserialize)]
pub struct ErrorsQuery {
    #[serde(default)]
    period: Option<String>,
}

pub async fn get_app_errors(
    State(server): S,
    user: User,
    Path(name): Path<String>,
    Query(q): Query<ErrorsQuery>,
) -> ApiResult<Json<ErrorsView>> {
    load_app_for(&server, &user, &name)?;
    let period = match q.period.as_deref() {
        None => ErrorPeriod::Day,
        Some(p) => {
            ErrorPeriod::parse(p).ok_or_else(|| ApiError::bad_request("period: 1h, 24h or 7d"))?
        }
    };
    Ok(Json(errors(&server, &name, period)?))
}

pub async fn get_node_insights(
    State(server): S,
    _: User,
    Path(name): Path<String>,
) -> Json<NodeInsights> {
    Json(node_insights(&server, &name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use facade_core::proto::{AppState, AppStats, EdgeReading, ErrorTally, NodeStats, Traffic};
    use std::path::Path;

    fn report(apps: &[(&str, u64, u64, u64)], edge: &[(&str, u64, Vec<ErrorTally>)]) -> NodeReport {
        NodeReport {
            node: NodeStats::default(),
            apps: apps
                .iter()
                .map(|&(app, since, received, sent)| AppStats {
                    app: app.into(),
                    state: AppState::Running,
                    cpu_percent: 0.0,
                    mem_used: 0,
                    mem_limit: 0,
                    net_rx: 0,
                    net_tx: 0,
                    restarts: 0,
                    traffic: Some(Traffic {
                        since,
                        bytes_in: received,
                        bytes_out: sent,
                        ..Default::default()
                    }),
                })
                .collect(),
            storage: None,
            listeners: vec![],
            ingress_errors: vec![],
            edge: edge
                .iter()
                .map(|(app, since, errors)| {
                    (
                        app.to_string(),
                        EdgeReading {
                            since: *since,
                            errors: errors.clone(),
                            visitors: vec![],
                        },
                    )
                })
                .collect(),
        }
    }

    fn tally(path: &str, count: u64, first_seen: u64) -> ErrorTally {
        ErrorTally {
            status: 404,
            method: "GET".into(),
            path: path.into(),
            count,
            first_seen,
            last_seen: first_seen + count,
        }
    }

    #[test]
    fn readings_add_what_they_grew_by() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let edge = Edge::default();
        let at = 86_400 * 10 + 60;
        // The first report after the server started: where counts stand.
        edge.record(
            &store,
            "main",
            at,
            &report(
                &[("a", 1, 100, 1000)],
                &[("a", 1, vec![tally("/x", 5, 10)])],
            ),
        );
        assert!(store.bytes_by_day("a", false, 0).unwrap().is_empty());
        assert_eq!(store.errors_since("a", 0, 10).unwrap().1, 0);
        // Then what each adds.
        edge.record(
            &store,
            "main",
            at + 15,
            &report(
                &[("a", 1, 150, 1500), ("new", 7, 10, 20)],
                &[
                    ("a", 1, vec![tally("/x", 8, 10), tally("/y", 2, 20)]),
                    ("new", 7, vec![tally("/z", 4, 30)]),
                ],
            ),
        );
        assert_eq!(store.bytes_by_day("a", false, 0).unwrap(), [(10, 50, 500)]);
        assert_eq!(store.bytes_by_day("new", false, 0).unwrap(), [(10, 10, 20)]);
        assert_eq!(
            store.bytes_by_day("main", true, 0).unwrap(),
            [(10, 60, 520)]
        );
        let counts = |app| {
            let (rows, _) = store.errors_since(app, 0, 10).unwrap();
            rows.into_iter()
                .map(|r| (r.path, r.count))
                .collect::<Vec<_>>()
        };
        assert_eq!(counts("a"), [("/x".into(), 3), ("/y".into(), 2)]);
        assert_eq!(counts("new"), [("/z".to_string(), 4)], "new: counts whole");
        // A new entry point: its counts are all new.
        edge.record(
            &store,
            "main",
            at + 30,
            &report(&[("a", 9, 5, 7)], &[("a", 9, vec![tally("/x", 1, 40)])]),
        );
        assert_eq!(store.bytes_by_day("a", false, 0).unwrap(), [(10, 55, 507)]);
        assert_eq!(counts("a"), [("/x".into(), 4), ("/y".into(), 2)]);
        // A group forgotten and seen again (another first_seen) counts whole.
        edge.record(
            &store,
            "main",
            at + 45,
            &report(&[("a", 9, 5, 7)], &[("a", 9, vec![tally("/y", 1, 50)])]),
        );
        assert_eq!(counts("a"), [("/x".into(), 4), ("/y".into(), 3)]);
    }

    #[test]
    fn months() {
        // 2026-11-04, with days in October and November.
        let today = 20_761;
        let start = month_start(today);
        let b = bandwidth(&[(start - 1, 1, 10), (start, 2, 20), (today, 3, 30)], today);
        assert_eq!((b.month_in, b.month_out), (5, 50));
        assert_eq!((b.today_in, b.today_out), (3, 30));
        assert_eq!((b.last_month_in, b.last_month_out), (1, 10));
    }
}
