//! The weekly summary: once a week (Monday 07:00 UTC unless `[alerts]` in
//! `server.toml` says otherwise) one Discord message about the seven days
//! before: each app's availability, requests and their change, 5xx, visitors,
//! deploys, restarts and crashes; each node's disk, what it served, its
//! pending updates.
//!
//! Facts are gathered from what the server already keeps (availability
//! minutes, usage buckets, visitor sketches, daily bytes, Activity entries),
//! then composed into lines by [`compose`], which knows nothing of the server
//! so it can be tested on fixture data.
//!
//! When it was last sent is kept in the database: a restart on the day does
//! not send it twice, and a server that was down at the hour sends it when it
//! is back, as long as it is the same day.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use facade_core::api::WeeklyPreview;
use facade_core::events::EventKind;
use facade_core::insights::{DAY, civil, day_of, uptime_percent};
use tracing::{info, warn};

use super::Server;
use super::monitor::Note;
use super::store::{USAGE_BUCKET, UsageOf};
use crate::config::AlertsConfig;
use crate::hll;

const WEEK: u64 = 7 * DAY;
/// Discord takes 4096 characters per embed description; some room is left.
const PART_LIMIT: usize = 3800;
/// A failed delivery is tried again this much later (within the day).
const RETRY_AFTER: u64 = 600;
/// A change in requests is only worth saying from this many the week before.
const CHANGE_FROM: u64 = 50;

/// When the summary goes out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// Days after Monday.
    weekday: u64,
    hour: u64,
    name: &'static str,
}

impl Schedule {
    pub fn new(cfg: &AlertsConfig) -> Self {
        Self {
            weekday: cfg.weekly_day.index(),
            hour: u64::from(cfg.weekly_hour.min(23)),
            name: cfg.weekly_day.name(),
        }
    }

    /// The last time it was due, at or before `now`.
    pub fn last_due(&self, now: u64) -> u64 {
        let today = day_of(now);
        // 1970-01-01 was a Thursday, three days after a Monday.
        let back = (weekday(today) + 7 - self.weekday) % 7;
        let due = (today - back) * DAY + self.hour * 3600;
        if due > now { due - WEEK } else { due }
    }

    /// The next time it is due, after `now`.
    pub fn next_due(&self, now: u64) -> u64 {
        self.last_due(now) + WEEK
    }

    /// `Monday 07:00 UTC`.
    pub fn describe(&self) -> String {
        format!("{} {:02}:00 UTC", self.name, self.hour)
    }
}

/// Days after Monday of a day since the epoch.
fn weekday(day: u64) -> u64 {
    (day + 3) % 7
}

/// Whether the summary due at `due` should go out at `now`, given when one
/// last went out: not yet sent, and still the day it was due.
pub fn should_send(due: u64, now: u64, sent: Option<u64>) -> bool {
    now >= due && day_of(now) == day_of(due) && sent.is_none_or(|s| s < due)
}

/// The days a summary covers, `[from, to)`, and the time figures stop at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Week {
    pub from_day: u64,
    pub to_day: u64,
    pub until: u64,
}

impl Week {
    /// The seven whole days before the one `due` falls in.
    pub fn before(due: u64) -> Self {
        let to_day = day_of(due);
        Self {
            from_day: to_day - 7,
            to_day,
            until: to_day * DAY,
        }
    }

    /// The last seven days, today so far included.
    pub fn to_now(now: u64) -> Self {
        let to_day = day_of(now) + 1;
        Self {
            from_day: to_day - 7,
            to_day,
            until: now,
        }
    }

    fn start(&self) -> u64 {
        self.from_day * DAY
    }
}

/// What a week was for one app.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AppWeek {
    pub name: String,
    /// Minutes up and down (stopped and unknown ones are not counted).
    pub up: u64,
    pub down: u64,
    /// Requests through the entry point, `None` for apps it does not route.
    pub requests: Option<u64>,
    pub errors_5xx: u64,
    pub requests_before: Option<u64>,
    /// Average daily visitors over the days that had any.
    pub visitors_per_day: Option<u64>,
    pub deploys: u64,
    pub restarts: u64,
    pub crashes: u64,
    pub memory_kills: u64,
}

impl AppWeek {
    /// Nothing to say: not running all week and nothing happened to it.
    fn idle(&self) -> bool {
        self.up + self.down == 0
            && self.requests.unwrap_or(0) == 0
            && self.deploys + self.restarts + self.crashes + self.memory_kills == 0
    }
}

/// What a week was for one node.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeWeek {
    pub name: String,
    /// Since when it is offline, if it is.
    pub offline_since: Option<u64>,
    /// Disk used at the end of the week and its total, and how much more (or
    /// less) than at its start.
    pub disk_used: Option<u64>,
    pub disk_total: Option<u64>,
    pub disk_change: Option<i64>,
    /// Bytes its entry point sent to and received from visitors.
    pub served_out: u64,
    pub served_in: u64,
    /// `7 updates (2 security) · reboot required`.
    pub upkeep: Option<String>,
    /// `vulnerabilities: 4 critical, 58 high (62 without a fix)`.
    pub vulns: Option<String>,
    pub disk_errors: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    pub from_day: u64,
    pub to_day: u64,
    pub apps: Vec<AppWeek>,
    pub nodes: Vec<NodeWeek>,
}

// ---------------------------------------------------------------- composing

/// The message: its title and parts, each short enough for one embed.
pub fn compose(facts: &Facts) -> WeeklyPreview {
    let mut lines = Vec::new();
    let (shown, idle): (Vec<&AppWeek>, Vec<&AppWeek>) = facts.apps.iter().partition(|a| !a.idle());
    if !shown.is_empty() {
        lines.push("**Apps**".to_string());
        lines.extend(shown.iter().map(|a| app_line(a)));
    }
    if !idle.is_empty() {
        let names: Vec<&str> = idle.iter().map(|a| a.name.as_str()).collect();
        lines.push(format!("Not running: {}", names.join(", ")));
    }
    if !facts.nodes.is_empty() {
        lines.push("**Nodes**".to_string());
        lines.extend(facts.nodes.iter().map(node_line));
    }
    if lines.is_empty() {
        lines.push("No apps deployed.".into());
    }
    WeeklyPreview {
        title: format!(
            "📊 Week of {}",
            span(facts.from_day, facts.to_day.saturating_sub(1))
        ),
        parts: split(&lines, PART_LIMIT),
        from_day: facts.from_day,
        to_day: facts.to_day,
    }
}

fn app_line(a: &AppWeek) -> String {
    let mut parts = vec![format!("**{}**", a.name)];
    if let Some(percent) = uptime_percent(a.up, a.down) {
        parts.push(if a.down == 0 {
            "up 100 %".to_string()
        } else {
            format!(
                "up {} % ({} down)",
                // Floored: 99.999 % with a minute down is not "100.00 %".
                trim((percent * 100.0).floor() / 100.0, 2),
                minutes(a.down)
            )
        });
    }
    if let Some(requests) = a.requests {
        let mut s = format!("{} requests", count(requests));
        if let Some(before) = a.requests_before.filter(|b| *b >= CHANGE_FROM) {
            let change = (requests as f64 - before as f64) * 100.0 / before as f64;
            let _ = match change.round() as i64 {
                0 => write!(s, " (=)"),
                c if c > 0 => write!(s, " (↑ {c} %)"),
                c => write!(s, " (↓ {} %)", -c),
            };
        }
        parts.push(s);
        if requests > 0 {
            parts.push(match a.errors_5xx {
                0 => "no 5xx".to_string(),
                e => {
                    let share = e as f64 * 100.0 / requests as f64;
                    if share < 0.1 {
                        format!("{} 5xx (<0.1 %)", count(e))
                    } else {
                        format!("{} 5xx ({} %)", count(e), trim(share, 1))
                    }
                }
            });
        }
    }
    if let Some(v) = a.visitors_per_day.filter(|v| *v > 0) {
        parts.push(format!("~{} visitors/day", count(v)));
    }
    let mut events = Vec::new();
    for (n, one, many) in [
        (a.deploys, "deploy", "deploys"),
        (a.restarts, "restart", "restarts"),
        (a.crashes, "crash", "crashes"),
        (a.memory_kills, "memory kill", "memory kills"),
    ] {
        if n > 0 {
            events.push(format!("{n} {}", if n == 1 { one } else { many }));
        }
    }
    if !events.is_empty() {
        parts.push(events.join(", "));
    }
    parts.join(" · ")
}

fn node_line(n: &NodeWeek) -> String {
    let mut parts = vec![format!("**{}**", n.name)];
    if let Some(since) = n.offline_since {
        parts.push(format!("⚠ offline since {}", date(day_of(since))));
    }
    if let Some(used) = n.disk_used {
        let mut s = match n.disk_total.filter(|t| *t > 0) {
            Some(total) => format!(
                "disk {} % ({} of {})",
                used * 100 / total,
                bytes(used),
                bytes(total)
            ),
            None => format!("disk {}", bytes(used)),
        };
        if let Some(change) = n.disk_change {
            let sign = if change < 0 { "−" } else { "+" };
            let _ = write!(s, ", {sign}{} this week", bytes(change.unsigned_abs()));
        }
        parts.push(s);
    }
    if n.served_out + n.served_in > 0 {
        parts.push(format!(
            "served {} (received {})",
            bytes(n.served_out),
            bytes(n.served_in)
        ));
    }
    if let Some(upkeep) = &n.upkeep {
        parts.push(upkeep.clone());
    }
    if let Some(vulns) = &n.vulns {
        parts.push(vulns.clone());
    }
    if n.disk_errors {
        parts.push("⚠ disk errors".into());
    }
    parts.join(" · ")
}

/// `7 updates · reboot required`, else what could not be checked: a node is
/// never all clear only because nothing was known.
fn upkeep_line(
    upkeep: Option<&facade_core::events::Upkeep>,
    platform: Option<&facade_core::platform::PlatformInfo>,
) -> Option<String> {
    use facade_core::platform::Support;
    let mut parts: Vec<String> = upkeep.and_then(|u| u.badge()).into_iter().collect();
    match platform {
        Some(p) if p.support == Support::BestEffort => parts.push(p.label()),
        _ => {
            let unknown = upkeep.map(|u| u.unknown()).unwrap_or_default();
            if !unknown.is_empty() {
                let what: Vec<&str> = unknown.iter().filter_map(|u| u.split(':').next()).collect();
                parts.push(format!("couldn't check {}", what.join(" or ")));
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Lines joined into parts of at most `limit` characters, cut between lines
/// (a single longer line is cut too).
fn split(lines: &[String], limit: usize) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in lines {
        let mut line = line.as_str();
        while line.chars().count() > limit {
            let cut = line
                .char_indices()
                .nth(limit)
                .map_or(line.len(), |(i, _)| i);
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            parts.push(line[..cut].to_string());
            line = &line[cut..];
        }
        if !current.is_empty() && current.chars().count() + 1 + line.chars().count() > limit {
            parts.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Compact counts: `950`, `12.4k`, `3.1M`.
fn count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", trim(n as f64 / 1e3, usize::from(n < 100_000))),
        _ => format!("{}M", trim(n as f64 / 1e6, 1)),
    }
}

/// `x` with at most `decimals`, trailing zeros dropped.
fn trim(x: f64, decimals: usize) -> String {
    let s = format!("{x:.decimals$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

pub(super) fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    match unit {
        0 => format!("{n} B"),
        _ => format!("{} {}", trim(v, if v < 10.0 { 1 } else { 0 }), UNITS[unit]),
    }
}

pub(super) fn minutes(m: u64) -> String {
    match m {
        0..60 => format!("{m} min"),
        60..1440 => format!("{} h {} min", m / 60, m % 60).replace(" 0 min", ""),
        _ => format!("{} d {} h", m / 1440, m % 1440 / 60).replace(" 0 h", ""),
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn date(day: u64) -> String {
    let (_, m, d) = civil(day);
    format!("{d} {}", MONTHS[m as usize - 1])
}

/// `26 Oct – 1 Nov 2026`, both days included.
fn span(first: u64, last: u64) -> String {
    let (y0, _, _) = civil(first);
    let (y1, _, _) = civil(last);
    if y0 == y1 {
        format!("{} – {} {y1}", date(first), date(last))
    } else {
        format!("{} {y0} – {} {y1}", date(first), date(last))
    }
}

// ---------------------------------------------------------------- gathering

/// Everything the summary of `week` says, from what the server keeps.
pub fn gather(server: &Server, week: Week) -> anyhow::Result<Facts> {
    let start = week.start();
    let before = start - WEEK;
    let today = day_of(facade_core::now());
    let mut events: HashMap<(String, EventKind), u64> = HashMap::new();
    for (app, kind, n) in server.store.app_event_counts(
        &[
            EventKind::Deployed,
            EventKind::Restart,
            EventKind::Crash,
            EventKind::OomKill,
        ],
        start,
        week.until,
    )? {
        events.insert((app, kind), n);
    }

    let mut apps = Vec::new();
    for row in server.store.list_apps()? {
        let spec = &row.spec;
        // Never deployed: only configuration.
        if row.revision == 0 {
            continue;
        }
        let name = spec.name.clone();
        let mut a = AppWeek {
            name: name.clone(),
            ..Default::default()
        };
        let stored: HashMap<u64, Vec<u8>> = server
            .store
            .uptime_days(&name, week.from_day)?
            .into_iter()
            .collect();
        for day in week.from_day..week.to_day {
            let len = if day == today {
                ((week.until % DAY) / 60) as usize
            } else {
                1440
            };
            let d = super::reach::summarize(day, stored.get(&day).map(Vec::as_slice), len);
            a.up += u64::from(d.up);
            a.down += u64::from(d.down);
        }
        if !spec.domains.is_empty() || !spec.tls_ports.is_empty() {
            let (this, last, errors) = match daily_requests(server, &name, week)? {
                Some(counts) => counts,
                // Days from before requests were counted per day: from the
                // usage history instead, which leaves out the 5 minutes
                // around each restart of the entry point.
                None => requests(server, &name, before, start, week.until)?,
            };
            a.requests = Some(this);
            a.requests_before = last;
            a.errors_5xx = errors;
            let days: Vec<u64> = server
                .store
                .visitor_sketches(&name, week.from_day)?
                .into_iter()
                .filter(|(day, _)| *day < week.to_day)
                .map(|(_, r)| hll::estimate(&r).round() as u64)
                .filter(|v| *v > 0)
                .collect();
            if !days.is_empty() {
                a.visitors_per_day = Some(days.iter().sum::<u64>() / days.len() as u64);
            }
        }
        let n = |kind| events.get(&(name.clone(), kind)).copied().unwrap_or(0);
        a.deploys = n(EventKind::Deployed);
        a.restarts = n(EventKind::Restart);
        a.crashes = n(EventKind::Crash);
        a.memory_kills = n(EventKind::OomKill);
        apps.push(a);
    }
    apps.sort_by(|a, b| a.name.cmp(&b.name));

    let mut nodes = Vec::new();
    for row in server.store.list_nodes()? {
        let name = row.name;
        let mut n = NodeWeek {
            name: name.clone(),
            ..Default::default()
        };
        {
            let live = server.live.read();
            if let Some(l) = live.nodes.get(&name) {
                n.offline_since = l.offline_since;
                n.disk_total = l.stats.as_ref().map(|s| s.disk_total).filter(|t| *t > 0);
            }
        }
        let disks: Vec<u64> = server
            .store
            .usage(UsageOf::Node, &name, start, 3600)?
            .into_iter()
            .filter(|p| p.at < week.until)
            .filter_map(|p| p.disk.filter(|d| *d > 0))
            .collect();
        if let (Some(first), Some(last)) = (disks.first(), disks.last()) {
            n.disk_used = Some(*last);
            if disks.len() > 1 {
                n.disk_change = Some(*last as i64 - *first as i64);
            }
        }
        for (day, received, sent) in server.store.bytes_by_day(&name, true, week.from_day)? {
            if day < week.to_day {
                n.served_in += received;
                n.served_out += sent;
            }
        }
        let platform = server
            .live
            .read()
            .nodes
            .get(&name)
            .and_then(|l| l.platform.clone());
        n.upkeep = upkeep_line(server.activity.upkeep(&name).as_ref(), platform.as_ref());
        n.vulns = server
            .vulns
            .summary(&name)
            .and_then(|v| match (&v.unknown, v.badge()) {
                (_, Some(badge)) => Some(format!("vulnerabilities: {badge}")),
                (Some(_), None) => Some("vulnerabilities not checked".into()),
                (None, None) => None,
            });
        n.disk_errors = server.activity.disk_trouble(&name).is_some();
        nodes.push(n);
    }
    nodes.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Facts {
        from_day: week.from_day,
        to_day: week.to_day,
        apps,
        nodes,
    })
}

/// An app's requests in `week` and the week before, and its 5xx in the first,
/// from what the entry point counted each day; `None` when a day of `week`
/// is from before that (the week before is then `None` alone).
fn daily_requests(
    server: &Server,
    app: &str,
    week: Week,
) -> anyhow::Result<Option<(u64, Option<u64>, u64)>> {
    let (mut this, mut last, mut errors) = (0, Some(0u64), 0);
    for (day, requests, errors_5xx) in server.store.requests_by_day(app, week.from_day - 7)? {
        if day >= week.to_day {
            continue;
        }
        if day >= week.from_day {
            let Some(requests) = requests else {
                return Ok(None);
            };
            this += requests;
            errors += errors_5xx.unwrap_or(0);
        } else {
            last = last.zip(requests).map(|(a, b)| a + b);
        }
    }
    Ok(Some((this, last, errors)))
}

/// An app's requests in `[start, until)` and in `[before, start)`, and its 5xx
/// in the first, from its usage buckets: each one's rate times the time since
/// the bucket before it (what the counters grew by).
fn requests(
    server: &Server,
    app: &str,
    before: u64,
    start: u64,
    until: u64,
) -> anyhow::Result<(u64, Option<u64>, u64)> {
    let points = server
        .store
        .usage(UsageOf::App, app, before, USAGE_BUCKET)?;
    let (mut this, mut last, mut errors) = (0.0, 0.0, 0.0);
    let mut previous: Option<u64> = None;
    for p in points {
        if let (Some(t0), Some(rate)) = (previous, p.requests) {
            let secs = p.at.saturating_sub(t0) as f64;
            if p.at >= until {
                break;
            } else if p.at >= start {
                this += rate * secs;
                errors += p.errors_5xx.unwrap_or(0.0) * secs;
            } else {
                last += rate * secs;
            }
        }
        previous = Some(p.at);
    }
    Ok((
        this.round() as u64,
        Some(last.round() as u64),
        errors.round() as u64,
    ))
}

/// The summary of `week`, as it would be posted.
pub fn preview(server: &Server, week: Week) -> anyhow::Result<WeeklyPreview> {
    Ok(compose(&gather(server, week)?))
}

// ---------------------------------------------------------------- sending

pub async fn run(server: Arc<Server>) {
    let schedule = Schedule::new(&server.cfg.alerts);
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    let mut retry_at = 0;
    loop {
        tick.tick().await;
        let now = facade_core::now();
        let due = schedule.last_due(now);
        if now < retry_at
            || !server.notifier.weekly_on()
            || !should_send(due, now, server.notifier.weekly_sent())
        {
            continue;
        }
        let week = Week::before(due);
        let message = match preview(&server, week) {
            Ok(m) => m,
            Err(e) => {
                warn!("composing the weekly summary: {e:#}");
                retry_at = now + RETRY_AFTER;
                continue;
            }
        };
        let notes: Vec<Note> = message
            .parts
            .iter()
            .enumerate()
            .map(|(i, part)| {
                let title = if i == 0 {
                    message.title.clone()
                } else {
                    format!("{} (continued)", message.title)
                };
                Note::summary(&title, part)
            })
            .collect();
        match server.notifier.send(&server.store, &notes).await {
            Ok(()) => {
                info!("weekly summary sent ({})", message.title);
                server.notifier.weekly_was_sent(&server.store, now);
            }
            Err(e) => {
                warn!("sending the weekly summary to Discord: {e:#}");
                retry_at = now + RETRY_AFTER;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Weekday;

    // 2026-11-02 is a Monday.
    const MONDAY: u64 = 20_759;

    fn schedule() -> Schedule {
        Schedule::new(&AlertsConfig::default())
    }

    #[test]
    fn days_of_the_week() {
        assert_eq!(civil(MONDAY), (2026, 11, 2));
        assert_eq!(weekday(MONDAY), 0);
        assert_eq!(weekday(0), 3); // a Thursday
    }

    #[test]
    fn due_on_monday_mornings() {
        let s = schedule();
        let seven = MONDAY * DAY + 7 * 3600;
        assert_eq!(s.last_due(seven), seven);
        assert_eq!(s.last_due(seven - 1), seven - WEEK);
        // Thursday: last Monday's, next Monday's.
        assert_eq!(s.last_due(seven + 3 * DAY), seven);
        assert_eq!(s.next_due(seven + 3 * DAY), seven + WEEK);
        assert_eq!(s.describe(), "Monday 07:00 UTC");
        let friday = Schedule::new(&AlertsConfig {
            weekly_day: Weekday::Friday,
            weekly_hour: 18,
        });
        assert_eq!(friday.last_due(seven), (MONDAY - 3) * DAY + 18 * 3600);
        assert_eq!(friday.describe(), "Friday 18:00 UTC");
    }

    #[test]
    fn sent_once_and_caught_up_the_same_day() {
        let due = MONDAY * DAY + 7 * 3600;
        // Not before the hour, at the hour, later that day if it was missed.
        assert!(!should_send(due, due - 60, None));
        assert!(should_send(due, due, None));
        assert!(should_send(due, due + 5 * 3600, Some(due - WEEK)));
        // Already sent (a restart on Monday does not send it again).
        assert!(!should_send(due, due + 3600, Some(due + 10)));
        // The next day is too late: the next one comes next week.
        assert!(!should_send(due, (MONDAY + 1) * DAY + 60, Some(due - WEEK)));
    }

    #[test]
    fn weeks() {
        let due = MONDAY * DAY + 7 * 3600;
        let w = Week::before(due);
        assert_eq!(
            (w.from_day, w.to_day, w.until),
            (MONDAY - 7, MONDAY, MONDAY * DAY)
        );
        let now = MONDAY * DAY + 600;
        let w = Week::to_now(now);
        assert_eq!(
            (w.from_day, w.to_day, w.until),
            (MONDAY - 6, MONDAY + 1, now)
        );
    }

    fn fixture() -> Facts {
        Facts {
            from_day: MONDAY - 7,
            to_day: MONDAY,
            apps: vec![
                AppWeek {
                    name: "blog".into(),
                    up: 10_078,
                    down: 2,
                    requests: Some(12_431),
                    errors_5xx: 37,
                    requests_before: Some(10_900),
                    visitors_per_day: Some(212),
                    deploys: 2,
                    crashes: 1,
                    ..Default::default()
                },
                AppWeek {
                    name: "old".into(),
                    ..Default::default()
                },
                AppWeek {
                    name: "shop".into(),
                    up: 10_080,
                    requests: Some(940),
                    requests_before: Some(1_000),
                    ..Default::default()
                },
                AppWeek {
                    name: "worker".into(),
                    up: 9_000,
                    down: 1_080,
                    restarts: 3,
                    memory_kills: 2,
                    ..Default::default()
                },
            ],
            nodes: vec![
                NodeWeek {
                    name: "eu-2".into(),
                    offline_since: Some((MONDAY - 1) * DAY + 3600),
                    ..Default::default()
                },
                NodeWeek {
                    name: "main".into(),
                    disk_used: Some(18_200_000_000),
                    disk_total: Some(40_000_000_000),
                    disk_change: Some(1_300_000_000),
                    served_out: 4_200_000_000,
                    served_in: 310_000_000,
                    upkeep: Some("7 updates (2 security) · reboot required".into()),
                    ..Default::default()
                },
            ],
        }
    }

    #[test]
    fn composes_one_line_per_app_and_node() {
        let m = compose(&fixture());
        assert_eq!(m.title, "📊 Week of 26 Oct – 1 Nov 2026");
        assert_eq!(m.parts.len(), 1);
        let lines: Vec<&str> = m.parts[0].lines().collect();
        assert_eq!(
            lines,
            [
                "**Apps**",
                "**blog** · up 99.98 % (2 min down) · 12.4k requests (↑ 14 %) · 37 5xx (0.3 %) \
                 · ~212 visitors/day · 2 deploys, 1 crash",
                "**shop** · up 100 % · 940 requests (↓ 6 %) · no 5xx",
                "**worker** · up 89.28 % (18 h down) · 3 restarts, 2 memory kills",
                "Not running: old",
                "**Nodes**",
                "**eu-2** · ⚠ offline since 1 Nov",
                "**main** · disk 45 % (18 GB of 40 GB), +1.3 GB this week \
                 · served 4.2 GB (received 310 MB) · 7 updates (2 security) · reboot required",
            ]
        );
    }

    #[test]
    fn many_apps_are_split_into_parts() {
        let mut facts = fixture();
        facts.apps = (0..200)
            .map(|i| AppWeek {
                name: format!("app-{i:03}"),
                up: 10_080,
                requests: Some(1_000 + i),
                ..Default::default()
            })
            .collect();
        let m = compose(&facts);
        assert!(m.parts.len() > 1);
        for part in &m.parts {
            assert!(part.chars().count() <= PART_LIMIT);
        }
        // Nothing lost or cut in the middle of a line.
        let all: Vec<&str> = m.parts.iter().flat_map(|p| p.lines()).collect();
        assert_eq!(all.iter().filter(|l| l.starts_with("**app-")).count(), 200);
    }

    #[test]
    fn a_week_without_apps() {
        let m = compose(&Facts {
            from_day: MONDAY - 7,
            to_day: MONDAY,
            ..Default::default()
        });
        assert_eq!(m.parts, ["No apps deployed."]);
    }

    #[test]
    fn figures() {
        assert_eq!(count(950), "950");
        assert_eq!(count(1_000), "1k");
        assert_eq!(count(12_431), "12.4k");
        assert_eq!(count(124_310), "124k");
        assert_eq!(count(3_456), "3.5k");
        assert_eq!(count(3_100_000), "3.1M");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1_300_000_000), "1.3 GB");
        assert_eq!(minutes(2), "2 min");
        assert_eq!(minutes(60), "1 h");
        assert_eq!(minutes(1_500), "1 d 1 h");
        assert_eq!(span(MONDAY - 7, MONDAY - 1), "26 Oct – 1 Nov 2026");
        assert_eq!(span(MONDAY - 308, MONDAY - 302), "29 Dec 2025 – 4 Jan 2026");
    }

    #[test]
    fn long_lines_are_cut() {
        let lines = vec!["a".repeat(10), "b".repeat(25), "c".repeat(3)];
        let parts = split(&lines, 12);
        assert_eq!(
            parts,
            ["aaaaaaaaaa", "bbbbbbbbbbbb", "bbbbbbbbbbbb", "b\nccc"]
        );
    }

    #[test]
    fn upkeep_lines_say_what_was_not_checked() {
        use facade_core::events::{Checked, Reboot, Upkeep};
        use facade_core::platform::{InstallKind, OsId, PlatformInfo, Support};
        let mut u = Upkeep {
            at: 1,
            updates: Checked::Failed("apt list: locked".into()),
            reboot: Checked::Known(Reboot {
                required: true,
                reasons: vec![],
            }),
        };
        assert_eq!(
            upkeep_line(Some(&u), None).as_deref(),
            Some("reboot required · couldn't check updates")
        );
        let debian = PlatformInfo {
            os: OsId::parse("ID=debian\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n"),
            support: Support::BestEffort,
            install: InstallKind::SelfManaged,
            kernel: None,
        };
        u.reboot = Checked::Unsupported("no".into());
        assert_eq!(
            upkeep_line(Some(&u), Some(&debian)).as_deref(),
            Some("best effort: Debian GNU/Linux 13 (trixie)")
        );
        assert_eq!(upkeep_line(None, None), None);
    }
}
