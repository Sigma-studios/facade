//! The figures of `facade_core::resources` in a few words: CPU throttling,
//! disk activity, processes and health checks of an app; strain and disk
//! activity of a node.

use facade_core::api::{AppMetric, AppView, NodeMetric};
use facade_core::proto::AppStats;
use facade_core::resources::{ResourcePoint, failing_streak};

use crate::output::bytes;

/// Seconds the summaries cover.
const WINDOW: u64 = 300;

fn rate(v: f64) -> String {
    format!("{}/s", bytes(v.max(0.0) as u64))
}

/// The first and last of readings (oldest first) at most `WINDOW` apart.
fn span<T>(readings: &[T], at: impl Fn(&T) -> u64) -> Option<(&T, &T)> {
    let last = readings.last()?;
    let first = readings.iter().find(|r| at(r) + WINDOW >= at(last))?;
    (at(first) < at(last)).then_some((first, last))
}

/// Throttling and disk activity over a period, then what the last reading says
/// of processes and health checks.
fn app_parts(point: Option<&ResourcePoint>, stats: Option<&AppStats>) -> Vec<String> {
    let mut parts = Vec::new();
    if let Some(p) = point {
        if let Some(t) = p.throttled.filter(|t| *t >= 1.0) {
            parts.push(format!("CPU held back {t:.0}% of the time"));
        }
        // No activity at all: nothing worth a word.
        if let (Some(r), Some(w)) = (p.disk_read, p.disk_write)
            && r + w >= 1.0
        {
            parts.push(format!("disk read {}, write {}", rate(r), rate(w)));
        }
    }
    if let Some(r) = stats.map(|s| &s.resources) {
        if let Some((now, limit)) = r.pids_warning() {
            parts.push(format!("{now} of {limit} processes"));
        }
        if let Some(last) = r.health.last() {
            let streak = failing_streak(&r.health);
            parts.push(if streak > 0 {
                format!("health check failing ({streak} in a row)")
            } else {
                format!("health check passing ({} ms)", last.ms)
            });
        }
    }
    parts
}

/// What `facade metrics <app>` adds to its traffic: the last minutes' figures.
pub fn app_summary(metrics: &[AppMetric]) -> Option<String> {
    let point = span(metrics, |m| m.at).map(|(a, b)| {
        ResourcePoint::app_between((a.at, &a.stats.resources), (b.at, &b.stats.resources))
    });
    let parts = app_parts(point.as_ref(), metrics.last().map(|m| &m.stats));
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// What `facade app show` adds: memory against its limit, and the rest.
pub fn app_detail(view: &AppView) -> Option<String> {
    let mut parts = Vec::new();
    if let (Some(s), Some(mb)) = (
        view.stats.as_ref().filter(|s| s.mem_used > 0),
        view.spec.limits.memory_mb,
    ) {
        let limit = mb * 1024 * 1024;
        parts.push(format!(
            "memory {} of {} ({:.0}%)",
            bytes(s.mem_used),
            bytes(limit),
            s.mem_used as f64 * 100.0 / limit as f64
        ));
    }
    parts.extend(app_parts(view.recent.as_ref(), view.stats.as_ref()));
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// What `facade node metrics` prints above its table: strain over the last
/// minute, disk activity and time waiting on disk over the last minutes.
pub fn node_summary(metrics: &[NodeMetric]) -> Vec<String> {
    let mut lines = Vec::new();
    let last = metrics.last().map(|m| &m.stats.resources);
    if let Some(p) = last.and_then(|r| r.pressure)
        && let Some(strain) = p.strain()
    {
        let part = |name: &str, psi: Option<facade_core::resources::Psi>| {
            psi.map(|psi| format!("{name} {:.1}%", psi.avg60))
        };
        let parts: Vec<String> = [
            part("CPU", p.cpu),
            part("memory", p.memory),
            part("disk", p.io),
        ]
        .into_iter()
        .flatten()
        .collect();
        lines.push(format!(
            "strain: {strain} (tasks waiting over the last minute: {})",
            parts.join(", ")
        ));
    }
    if let Some((a, b)) = span(metrics, |m| m.at) {
        let p = ResourcePoint::node_between((a.at, &a.stats.resources), (b.at, &b.stats.resources));
        if let (Some(r), Some(w)) = (p.disk_read, p.disk_write) {
            let mut line = format!("disk:   read {}, write {}", rate(r), rate(w));
            if let Some(wait) = p.pressure_io.or(p.io_wait) {
                line += &format!(", waiting {wait:.1}% of the time");
            }
            line += &format!(" (last {} minutes)", WINDOW / 60);
            lines.push(line);
        }
    }
    lines
}
