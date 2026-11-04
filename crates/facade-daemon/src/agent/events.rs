//! What the agent sees happen on its node, for the Activity: Docker's events
//! about its containers (an app stopping without facade stopping it, the
//! kernel killing it for memory, its health check turning), the kernel log
//! (OOM kills of anything, disk errors), successful SSH logins, and the
//! node's pending updates and reboot.
//!
//! facade's own stops (deploys, restarts, settings changes, moves, removals,
//! the two containers of an overlapping replacement) are marked in
//! [`Expected`] before they happen, so they never read as crashes. Entries wait
//! in a queue while the server is away, and go to servers that ask for them
//! only (see `AgentRequest::Sync`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bollard::query_parameters::{EventsOptions, InspectContainerOptions, LogsOptions};
use facade_core::events::{EventDetails, EventKind, NodeEvent, Severity, Upkeep};
use facade_core::proto::{AgentEvent, AgentMsg};
use futures_util::StreamExt;
use parking_lot::Mutex;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::Agent;

/// Entries kept while the server is away; the oldest go first.
const QUEUE: usize = 500;
/// Lines of a crashed app's output kept with its entry.
const SNAPSHOT_LINES: usize = 50;
/// At most one crash snapshot per app this often (a crash loop).
const SNAPSHOT_EVERY: Duration = Duration::from_secs(30);
/// How often pending updates are read.
const UPKEEP_EVERY: Duration = Duration::from_secs(3 * 3600);
/// Kernel lines about one disk within this long make one entry.
const DISK_BURST: Duration = Duration::from_secs(3);
/// One entry per disk this often at most.
const DISK_EVERY: Duration = Duration::from_secs(600);

/// Containers facade is stopping, by name or id: their exit is no crash.
/// Held while the stop runs, and a little after (Docker reports it async).
#[derive(Default)]
pub struct Expected(Mutex<HashMap<String, Option<Instant>>>);

pub struct ExpectGuard<'a> {
    expected: &'a Expected,
    key: String,
}

impl Expected {
    /// Marks `key` (a container's name or id) until the guard drops, plus a
    /// few seconds.
    pub fn hold(&self, key: &str) -> ExpectGuard<'_> {
        self.0.lock().insert(key.to_string(), None);
        ExpectGuard {
            expected: self,
            key: key.to_string(),
        }
    }

    /// Whether a container's exit was facade's doing; forgets it.
    pub fn take(&self, id: &str, name: &str) -> bool {
        let mut marks = self.0.lock();
        let now = Instant::now();
        marks.retain(|_, until| until.is_none_or(|t| t > now));
        marks.remove(id).is_some() | marks.remove(name).is_some()
    }
}

impl Drop for ExpectGuard<'_> {
    fn drop(&mut self) {
        if let Some(until) = self.expected.0.lock().get_mut(&self.key) {
            *until = Some(Instant::now() + Duration::from_secs(15));
        }
    }
}

/// The agent's side of the Activity.
pub struct Activity {
    queue: Mutex<VecDeque<NodeEvent>>,
    /// The connected server takes entries.
    accepted: AtomicBool,
    /// Latest upkeep, and whether the server still has to get it.
    upkeep: Mutex<(Option<Upkeep>, bool)>,
    watch: Mutex<Watch>,
    /// When this agent started, for the server to tell restarts apart.
    pub started: u64,
}

impl Default for Activity {
    fn default() -> Self {
        Self {
            queue: Default::default(),
            accepted: AtomicBool::new(false),
            upkeep: Default::default(),
            watch: Default::default(),
            started: facade_core::now(),
        }
    }
}

/// What Docker and the kernel said lately, by container id.
#[derive(Default)]
struct Watch {
    /// Apps whose health check failed last.
    unhealthy: HashSet<String>,
    /// Docker said the container hit an OOM kill.
    ooms: HashMap<String, u64>,
    /// The kernel named the program it killed in the container.
    kernel_ooms: HashMap<String, (u64, String)>,
    /// Someone sent the container a signal (`docker stop`, `docker kill`).
    kills: HashMap<String, (u64, String)>,
    deaths: HashMap<String, u64>,
    snapshots: HashMap<String, Instant>,
    /// Docker OOM events, for the kernel counter fallback.
    docker_ooms: u64,
    /// Last Docker event handled (ns), so a reconnection replays nothing.
    last_event: i64,
}

impl Watch {
    /// Forgets what is too old to matter.
    fn tidy(&mut self, now: u64) {
        let fresh = |at: &u64| now.saturating_sub(*at) < 600;
        self.ooms.retain(|_, at| fresh(at));
        self.kernel_ooms.retain(|_, (at, _)| fresh(at));
        self.kills.retain(|_, (at, _)| fresh(at));
        self.deaths.retain(|_, at| fresh(at));
        self.snapshots
            .retain(|_, at| at.elapsed() < Duration::from_secs(600));
    }
}

impl Activity {
    /// A new connection: the server gets the upkeep again with the first report.
    pub fn connected(&self) {
        self.upkeep.lock().1 = true;
    }

    pub fn disconnected(&self) {
        self.accepted.store(false, Ordering::Relaxed);
    }

    /// The upkeep for the next report, if the server has not got it.
    pub fn take_upkeep(&self) -> Option<Upkeep> {
        let mut upkeep = self.upkeep.lock();
        std::mem::take(&mut upkeep.1)
            .then(|| upkeep.0.clone())
            .flatten()
    }

    fn set_upkeep(&self, u: Upkeep) {
        let mut upkeep = self.upkeep.lock();
        let changed = upkeep.0.as_ref().is_none_or(|old| {
            Upkeep {
                at: u.at,
                ..old.clone()
            } != u
        });
        upkeep.0 = Some(u);
        if changed {
            upkeep.1 = true;
        }
    }
}

impl Agent {
    /// Queues an entry and sends what the server can take.
    pub(super) fn note(&self, event: NodeEvent) {
        debug!(summary = %event.summary, "activity");
        {
            let mut queue = self.activity.queue.lock();
            if queue.len() == QUEUE {
                queue.pop_front();
            }
            queue.push_back(event);
        }
        self.flush_activity();
    }

    /// The server says it takes entries.
    pub(super) fn accept_activity(&self) {
        self.activity.accepted.store(true, Ordering::Relaxed);
        self.flush_activity();
    }

    fn flush_activity(&self) {
        if !self.activity.accepted.load(Ordering::Relaxed) {
            return;
        }
        let Some(tx) = self.outbox.lock().clone() else {
            return;
        };
        let mut queue = self.activity.queue.lock();
        while let Some(event) = queue.pop_front() {
            let msg = AgentMsg::Event {
                event: AgentEvent::Activity {
                    event: Box::new(event),
                },
            };
            if let Err(e) = tx.try_send(msg) {
                // Full or gone: kept for later.
                let AgentMsg::Event {
                    event: AgentEvent::Activity { event },
                } = e.into_inner()
                else {
                    unreachable!()
                };
                queue.push_front(*event);
                break;
            }
        }
    }
}

/// Starts the watchers.
pub async fn run(agent: Arc<Agent>) {
    tokio::spawn(watch_docker(agent.clone()));
    tokio::spawn(watch_kernel(agent.clone()));
    tokio::spawn(watch_ssh(agent.clone()));
    tokio::spawn(upkeep_loop(agent));
}

/// When the machine booted (`btime` in /proc/stat).
pub fn boot_time() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    stat.lines()
        .find_map(|l| l.strip_prefix("btime "))
        .and_then(|v| v.trim().parse().ok())
}

// ---------------------------------------------------------------- Docker

async fn watch_docker(agent: Arc<Agent>) {
    let runtime = agent.runtime.clone();
    let mut since: Option<i64> = None;
    loop {
        let filters = HashMap::from([
            ("type".to_string(), vec!["container".to_string()]),
            ("label".to_string(), vec![runtime.node_filter()]),
        ]);
        let mut events = runtime.docker().events(Some(EventsOptions {
            since: since.map(|s| s.to_string()),
            filters: Some(filters),
            ..Default::default()
        }));
        while let Some(event) = events.next().await {
            let event = match event {
                Ok(e) => e,
                Err(e) => {
                    debug!("docker events: {e}");
                    break;
                }
            };
            if let Some(t) = event.time {
                since = Some(t);
            }
            on_docker_event(&agent, event);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// A container's name for an app: `facade-<app>`, and the other names it
/// takes during deploys.
enum Role {
    /// The app's container.
    Main,
    /// The new or old container of an overlapping replacement, or a release
    /// command: facade's business, its deploy log says how it went.
    Transient,
}

fn role(name: &str, app: &str) -> Option<Role> {
    let main = super::runtime::container(app);
    if name == main {
        return Some(Role::Main);
    }
    let rest = name.strip_prefix(&main)?;
    matches!(rest, "_next" | "_old" | "-release").then_some(Role::Transient)
}

fn on_docker_event(agent: &Arc<Agent>, event: bollard::models::EventMessage) {
    let (Some(action), Some(actor)) = (event.action, event.actor) else {
        return;
    };
    let Some(id) = actor.id else { return };
    let attrs = actor.attributes.unwrap_or_default();
    let (Some(name), Some(app)) = (attrs.get("name").cloned(), attrs.get("facade.app").cloned())
    else {
        return;
    };
    let nanos = event
        .time_nano
        .or(event.time.map(|t| t * 1_000_000_000))
        .unwrap_or(0);
    let now = facade_core::now();
    {
        let mut watch = agent.activity.watch.lock();
        if nanos != 0 && nanos <= watch.last_event {
            return;
        }
        watch.last_event = nanos;
        watch.tidy(now);
    }
    let Some(role) = role(&name, &app) else {
        return;
    };
    let at = if nanos > 0 {
        (nanos / 1_000_000_000) as u64
    } else {
        now
    };
    match action.as_str() {
        "kill" => {
            let signal = attrs.get("signal").cloned().unwrap_or_default();
            agent.activity.watch.lock().kills.insert(id, (now, signal));
        }
        "oom" => {
            {
                let mut watch = agent.activity.watch.lock();
                watch.ooms.insert(id.clone(), now);
                watch.docker_ooms += 1;
            }
            if matches!(role, Role::Main) {
                tokio::spawn(oom_followup(agent.clone(), id, app, at));
            }
        }
        "die" => {
            // A signal someone sent it just before (`docker stop`, `kill`).
            let signal = {
                let mut watch = agent.activity.watch.lock();
                watch.deaths.insert(id.clone(), now);
                watch
                    .kills
                    .remove(&id)
                    .filter(|(at, _)| now.saturating_sub(*at) <= 30)
                    .map(|(_, s)| s)
            };
            // Checked at once: the mark lasts only a little past the stop.
            if agent.runtime.expected().take(&id, &name) || matches!(role, Role::Transient) {
                return;
            }
            let exit: i64 = attrs
                .get("exitCode")
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            tokio::spawn(on_die(agent.clone(), id, app, exit, signal, nanos));
        }
        a if a.starts_with("health_status") => {
            if !matches!(role, Role::Main) {
                return;
            }
            let status = a
                .trim_start_matches("health_status")
                .trim_start_matches(':')
                .trim()
                .to_string();
            tokio::spawn(on_health(agent.clone(), id, app, status, at));
        }
        _ => {}
    }
}

/// What a container's exit says about why it stopped.
#[derive(Debug, PartialEq)]
struct Exit {
    kind: EventKind,
    severity: Severity,
    summary: String,
    reason: String,
}

fn megabytes(bytes: u64) -> String {
    format!("{} MB", bytes / (1024 * 1024))
}

/// `signal` is what someone else sent it (`docker stop` and the like), if any.
fn explain_exit(
    exit: i64,
    oom: bool,
    signal: Option<&str>,
    limit: Option<u64>,
    program: Option<&str>,
) -> Exit {
    if oom {
        let limit = limit.map(|l| format!(" ({} limit)", megabytes(l)));
        let summary = match program {
            Some(p) => format!(
                "Killed: out of memory{}, the kernel stopped {p}",
                limit.unwrap_or_default()
            ),
            None => format!("Killed: out of memory{}", limit.unwrap_or_default()),
        };
        return Exit {
            kind: EventKind::OomKill,
            severity: Severity::Error,
            summary,
            reason: "out of memory".into(),
        };
    }
    if let Some(signal) = signal {
        let name = signal
            .parse::<i64>()
            .ok()
            .and_then(signal_name)
            .map(str::to_string)
            .unwrap_or_else(|| signal.to_string());
        return Exit {
            kind: EventKind::Crash,
            severity: Severity::Warn,
            summary: format!("Stopped outside facade ({name})"),
            reason: "stopped outside facade".into(),
        };
    }
    match exit {
        0 => Exit {
            kind: EventKind::Crash,
            severity: Severity::Warn,
            summary: "Exited by itself (code 0)".into(),
            reason: "exited".into(),
        },
        129..=159 if signal_name(exit - 128).is_some() => {
            let sig = signal_name(exit - 128).unwrap_or_default();
            Exit {
                kind: EventKind::Crash,
                severity: Severity::Error,
                summary: format!("Crashed: killed by {sig} (exit code {exit})"),
                reason: sig.to_string(),
            }
        }
        _ => Exit {
            kind: EventKind::Crash,
            severity: Severity::Error,
            summary: format!("Crashed: exit code {exit}"),
            reason: format!("exit code {exit}"),
        },
    }
}

fn signal_name(n: i64) -> Option<&'static str> {
    Some(match n {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        10 => "SIGUSR1",
        11 => "SIGSEGV",
        12 => "SIGUSR2",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return None,
    })
}

async fn on_die(
    agent: Arc<Agent>,
    id: String,
    app: String,
    exit: i64,
    signal: Option<String>,
    nanos: i64,
) {
    // What the kernel and Docker say about memory lands around the exit.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let docker = agent.runtime.docker().clone();
    let info = docker
        .inspect_container(&id, None::<InspectContainerOptions>)
        .await
        .ok();
    let limit = info
        .as_ref()
        .and_then(|i| i.host_config.as_ref())
        .and_then(|h| h.memory)
        .filter(|m| *m > 0)
        .map(|m| m as u64);
    let oom_flag = info
        .as_ref()
        .and_then(|i| i.state.as_ref())
        .and_then(|s| s.oom_killed)
        .unwrap_or(false);
    let (oom_event, program) = {
        let mut watch = agent.activity.watch.lock();
        (
            watch.ooms.remove(&id).is_some(),
            watch.kernel_ooms.remove(&id).map(|(_, p)| p),
        )
    };
    // A memory kill shows as SIGKILL (137); an OOM event for a child process
    // before a crash of another cause does not make the crash an OOM kill.
    let oom = oom_flag || (oom_event && exit == 137);
    let e = explain_exit(exit, oom, signal.as_deref(), limit, program.as_deref());
    let logs = if snapshot_due(&agent, &app) {
        last_lines(&docker, &id, nanos).await
    } else {
        vec![]
    };
    let at = if nanos > 0 {
        (nanos / 1_000_000_000) as u64
    } else {
        facade_core::now()
    };
    info!(%app, "{}", e.summary);
    agent.note(NodeEvent {
        at,
        kind: e.kind,
        severity: e.severity,
        problem: true,
        app: Some(app),
        on_node: e.kind == EventKind::OomKill,
        summary: e.summary,
        details: EventDetails {
            logs,
            reason: Some(e.reason),
            exit_code: Some(exit),
            memory_limit: limit,
            program,
            ..Default::default()
        },
    });
}

fn snapshot_due(agent: &Agent, app: &str) -> bool {
    let mut watch = agent.activity.watch.lock();
    if watch
        .snapshots
        .get(app)
        .is_some_and(|at| at.elapsed() < SNAPSHOT_EVERY)
    {
        return false;
    }
    watch.snapshots.insert(app.to_string(), Instant::now());
    true
}

/// The container's last lines up to `nanos` (its exit): Docker may have
/// restarted it since, and what the new process wrote is not the crash's.
async fn last_lines(docker: &bollard::Docker, id: &str, nanos: i64) -> Vec<String> {
    let until = if nanos > 0 {
        (nanos / 1_000_000_000 + 1) as i32
    } else {
        0
    };
    let mut logs = docker.logs(
        id,
        Some(LogsOptions {
            stdout: true,
            stderr: true,
            timestamps: true,
            tail: (SNAPSHOT_LINES * 2).to_string(),
            until,
            ..Default::default()
        }),
    );
    let mut text = String::new();
    while let Some(Ok(chunk)) = logs.next().await {
        text.push_str(&String::from_utf8_lossy(&chunk.into_bytes()));
    }
    snapshot(&text, nanos)
}

/// The last `SNAPSHOT_LINES` lines of `text` written up to `nanos` (by their
/// Docker timestamps; lines without one are kept).
fn snapshot(text: &str, nanos: i64) -> Vec<String> {
    let lines: Vec<String> = text
        .lines()
        .filter(|l| {
            nanos <= 0
                || l.split_once(' ')
                    .and_then(|(ts, _)| docker_time(ts))
                    .is_none_or(|t| t <= nanos)
        })
        .map(|l| l.trim_end().to_string())
        .collect();
    let skip = lines.len().saturating_sub(SNAPSHOT_LINES);
    lines.into_iter().skip(skip).collect()
}

/// Nanoseconds since the epoch of a Docker log timestamp
/// (`2026-11-04T10:00:00.123456789Z`, RFC 3339 in UTC).
fn docker_time(ts: &str) -> Option<i64> {
    let ts = ts.strip_suffix('Z')?;
    let (date, time) = ts.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (h, min, s) = (t.next()??, t.next()??, t.next()??);
    let mut nanos: i64 = 0;
    for (i, c) in frac.chars().take(9).enumerate() {
        nanos += i64::from(c.to_digit(10)?) * 10i64.pow(8 - i as u32);
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86400 + h * 3600 + min * 60 + s) * 1_000_000_000) + nanos)
}

/// Docker said the app's container hit an OOM kill: if it did not stop, the
/// kernel killed one of its processes.
async fn oom_followup(agent: Arc<Agent>, id: String, app: String, at: u64) {
    tokio::time::sleep(Duration::from_secs(4)).await;
    let program = {
        let mut watch = agent.activity.watch.lock();
        // It stopped: `on_die` says it was killed for memory.
        if watch.deaths.get(&id).is_some_and(|d| *d + 2 >= at) {
            return;
        }
        watch.ooms.remove(&id);
        watch.kernel_ooms.remove(&id).map(|(_, p)| p)
    };
    let limit = agent
        .runtime
        .docker()
        .inspect_container(&id, None::<InspectContainerOptions>)
        .await
        .ok()
        .and_then(|i| i.host_config)
        .and_then(|h| h.memory)
        .filter(|m| *m > 0)
        .map(|m| m as u64);
    let limit_text = limit
        .map(|l| format!(" ({} limit)", megabytes(l)))
        .unwrap_or_default();
    let summary = match &program {
        Some(p) => format!("Out of memory: the kernel killed {p}{limit_text}"),
        None => format!("Out of memory: the kernel killed one of its processes{limit_text}"),
    };
    agent.note(NodeEvent {
        at,
        kind: EventKind::OomKill,
        severity: Severity::Error,
        problem: true,
        app: Some(app),
        on_node: true,
        summary,
        details: EventDetails {
            reason: Some("out of memory".into()),
            memory_limit: limit,
            program,
            ..Default::default()
        },
    });
}

async fn on_health(agent: Arc<Agent>, id: String, app: String, status: String, at: u64) {
    match status.as_str() {
        "unhealthy" => {
            if !agent.activity.watch.lock().unhealthy.insert(app.clone()) {
                return;
            }
            let output = agent
                .runtime
                .docker()
                .inspect_container(&id, None::<InspectContainerOptions>)
                .await
                .ok()
                .and_then(|i| i.state)
                .and_then(|s| s.health)
                .and_then(|h| h.log)
                .and_then(|l| l.last().cloned())
                .and_then(|r| r.output)
                .unwrap_or_default();
            let first = output.lines().map(str::trim).find(|l| !l.is_empty());
            let summary = match first {
                Some(line) => format!("Unhealthy: {}", truncate(line, 120)),
                None => "Unhealthy: its health check fails".to_string(),
            };
            agent.note(NodeEvent {
                at,
                kind: EventKind::Unhealthy,
                severity: Severity::Warn,
                problem: true,
                app: Some(app),
                on_node: false,
                summary,
                details: EventDetails {
                    logs: output.lines().take(20).map(str::to_string).collect(),
                    ..Default::default()
                },
            });
        }
        "healthy" => {
            if !agent.activity.watch.lock().unhealthy.remove(&app) {
                return;
            }
            agent.note(NodeEvent {
                at,
                kind: EventKind::Healthy,
                severity: Severity::Ok,
                problem: false,
                app: Some(app),
                on_node: false,
                summary: "Healthy again".into(),
                details: EventDetails::default(),
            });
        }
        _ => {}
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

// ---------------------------------------------------------------- the kernel log

/// What the kernel said that matters here.
#[derive(Debug, PartialEq)]
enum Kernel {
    /// It killed `program` for memory; `memcg` is the cgroup it was charged to.
    Oom {
        program: String,
        memcg: Option<String>,
    },
    Disk {
        device: String,
        problem: DiskProblem,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DiskProblem {
    /// Reads or writes failing (`write`, `read`, `timeout`…).
    Io(String),
    /// Its filesystem reported errors.
    Filesystem(String),
    ReadOnly,
}

/// Reads kernel lines one at a time, remembering what an OOM report says
/// before the line naming the killed process.
#[derive(Default)]
struct KernelParser {
    memcg: Option<String>,
}

/// The message of a `/dev/kmsg` record (`6,1052,105738466541,-;message`),
/// or the line as it is.
fn kmsg_message(record: &str) -> &str {
    let first = record.lines().next().unwrap_or("");
    match first.split_once(';') {
        Some((prefix, message))
            if prefix.split(',').count() >= 3
                && prefix.split(',').take(3).all(|p| p.parse::<u64>().is_ok()) =>
        {
            message
        }
        _ => first,
    }
}

impl KernelParser {
    fn line(&mut self, record: &str) -> Option<Kernel> {
        let msg = kmsg_message(record).trim();
        if let Some(rest) = msg.strip_prefix("oom-kill:") {
            self.memcg = rest.split(',').find_map(|kv| {
                kv.strip_prefix("task_memcg=")
                    .or_else(|| kv.strip_prefix("oom_memcg="))
                    .map(str::to_string)
            });
            return None;
        }
        if let Some(i) = msg.find("Killed process ") {
            let rest = &msg[i + "Killed process ".len()..];
            let open = rest.find('(')?;
            let close = rest[open..].find(')')? + open;
            let program = rest[open + 1..close].to_string();
            return Some(Kernel::Oom {
                program,
                memcg: self.memcg.take(),
            });
        }
        disk_problem(msg)
    }
}

/// The device named after `marker` up to one of `ends`.
fn device_after<'a>(msg: &'a str, marker: &str, ends: &[char]) -> Option<&'a str> {
    let i = msg.find(marker)? + marker.len();
    let rest = &msg[i..];
    let end = rest.find(|c| ends.contains(&c)).unwrap_or(rest.len());
    let dev = rest[..end].trim();
    (!dev.is_empty()).then_some(dev)
}

/// Block devices whose errors mean nothing about the node's disks.
fn ignored_device(dev: &str) -> bool {
    ["loop", "sr", "fd", "ram", "zram"]
        .iter()
        .any(|p| dev.starts_with(p))
}

fn disk_problem(msg: &str) -> Option<Kernel> {
    let disk = |device: &str, problem| {
        Some(Kernel::Disk {
            device: device.to_string(),
            problem,
        })
    };
    if msg.contains("Remounting filesystem read-only") {
        let dev = device_after(msg, "(", &[')'])?;
        return disk(dev, DiskProblem::ReadOnly);
    }
    if let Some(dev) = device_after(msg, "Aborting journal on device ", &[' ']) {
        let dev = dev.trim_end_matches('.');
        let dev = dev.rsplit_once('-').map_or(dev, |(d, _)| d);
        return disk(dev, DiskProblem::Filesystem("journal aborted".into()));
    }
    for fs in [
        "EXT4-fs error",
        "EXT3-fs error",
        "BTRFS error",
        "BTRFS critical",
    ] {
        if msg.starts_with(fs) {
            let dev = device_after(msg, "(device ", &[')'])?;
            let kind = fs
                .split('-')
                .next()
                .unwrap_or(fs)
                .split(' ')
                .next()
                .unwrap_or(fs);
            return disk(dev, DiskProblem::Filesystem(kind.to_lowercase()));
        }
    }
    if msg.starts_with("XFS (")
        && (msg.contains("I/O error")
            || msg.contains("Corruption")
            || msg.contains("Shutting down filesystem"))
    {
        let dev = device_after(msg, "XFS (", &[')'])?;
        return disk(dev, DiskProblem::Filesystem("xfs".into()));
    }
    if msg.contains("I/O error, dev ") {
        let dev = device_after(msg, "I/O error, dev ", &[',', ' '])?;
        if ignored_device(dev) {
            return None;
        }
        let op = msg
            .split_once(":(")
            .and_then(|(_, r)| r.split_once(')'))
            .map(|(op, _)| op.to_lowercase())
            .unwrap_or_else(|| "I/O error".into());
        let what = match op.as_str() {
            "read" | "write" => format!("{op} errors"),
            "I/O error" => op,
            other => format!("{other} errors"),
        };
        return disk(dev, DiskProblem::Io(what));
    }
    if let Some(dev) = device_after(msg, "Buffer I/O error on dev ", &[',', ' ']) {
        if ignored_device(dev) {
            return None;
        }
        return disk(dev, DiskProblem::Io("I/O error".into()));
    }
    for marker in ["critical medium error, dev ", "critical target error, dev "] {
        if let Some(dev) = device_after(msg, marker, &[',', ' ']) {
            return disk(dev, DiskProblem::Io("medium errors".into()));
        }
    }
    if msg.starts_with("nvme ") && msg.contains(" timeout") && msg.contains("QID") {
        let dev = device_after(msg, "nvme ", &[':'])?;
        return disk(dev, DiskProblem::Io("I/O timeouts".into()));
    }
    None
}

fn disk_summary(device: &str, problem: &DiskProblem) -> String {
    match problem {
        DiskProblem::Io(what) => format!("Disk errors on {device}: {what}"),
        DiskProblem::Filesystem(what) => format!("Filesystem errors on {device} ({what})"),
        DiskProblem::ReadOnly => format!("Filesystem on {device} remounted read-only"),
    }
}

/// The id of the Docker container a cgroup path belongs to: the last
/// `docker-<id>.scope` (systemd driver) or `docker/<id>` (cgroupfs) in it.
fn container_of(memcg: &str) -> Option<String> {
    memcg.split('/').rev().find_map(|part| {
        let id = part
            .strip_prefix("docker-")
            .and_then(|p| p.strip_suffix(".scope"))
            .unwrap_or(part);
        (id.len() == 64 && id.chars().all(|c| c.is_ascii_hexdigit())).then(|| id.to_string())
    })
}

/// Where kernel lines come from: `/dev/kmsg`, or `FACADE_KMSG` (paths
/// separated by `:`; a regular file is followed like `tail -f`, for tests).
fn kernel_sources() -> Vec<PathBuf> {
    match std::env::var("FACADE_KMSG") {
        Ok(paths) if !paths.is_empty() => paths.split(':').map(PathBuf::from).collect(),
        _ => vec![PathBuf::from("/dev/kmsg")],
    }
}

async fn watch_kernel(agent: Arc<Agent>) {
    let (tx, mut rx) = mpsc::channel::<String>(1024);
    let mut readable = false;
    for path in kernel_sources() {
        match open_kernel_source(&path) {
            Ok(source) => {
                readable = true;
                let tx = tx.clone();
                std::thread::Builder::new()
                    .name("kmsg".into())
                    .spawn(move || source.run(tx))
                    .ok();
            }
            Err(e) => debug!("kernel log {}: {e}", path.display()),
        }
    }
    drop(tx);
    if !readable {
        info!("the kernel log is not readable: OOM kills are counted, disk errors not watched");
        return oom_counter(agent).await;
    }
    let mut parser = KernelParser::default();
    // Disk lines gathered for a moment, by device and problem.
    let mut bursts: HashMap<(String, DiskProblem), (Instant, Vec<String>)> = HashMap::new();
    let mut last: HashMap<(String, DiskProblem), Instant> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            line = rx.recv() => {
                let Some(line) = line else { return };
                match parser.line(&line) {
                    Some(Kernel::Oom { program, memcg }) => {
                        kernel_oom(&agent, program, memcg).await;
                    }
                    Some(Kernel::Disk { device, problem }) => {
                        let key = (device, problem);
                        let burst = bursts.entry(key).or_insert_with(|| (Instant::now(), vec![]));
                        if burst.1.len() < 20 {
                            burst.1.push(kmsg_message(&line).trim().to_string());
                        }
                    }
                    None => {}
                }
            }
            _ = tick.tick() => {}
        }
        let due: Vec<_> = bursts
            .iter()
            .filter(|(_, (t, _))| t.elapsed() >= DISK_BURST)
            .map(|(k, _)| k.clone())
            .collect();
        for key in due {
            let (_, lines) = bursts.remove(&key).expect("present");
            if last.get(&key).is_some_and(|t| t.elapsed() < DISK_EVERY) {
                continue;
            }
            last.insert(key.clone(), Instant::now());
            let (device, problem) = key;
            let summary = disk_summary(&device, &problem);
            warn!("{summary}");
            agent.note(NodeEvent {
                at: facade_core::now(),
                kind: EventKind::DiskError,
                severity: Severity::Error,
                problem: true,
                app: None,
                on_node: true,
                summary,
                details: EventDetails {
                    logs: lines,
                    device: Some(device),
                    ..Default::default()
                },
            });
        }
    }
}

/// The kernel killed a program for memory. In one of this node's facade
/// containers, Docker reports it too, and the entry goes with the app (see
/// `on_die`, `oom_followup`); this names the program for it.
async fn kernel_oom(agent: &Arc<Agent>, program: String, memcg: Option<String>) {
    let mut container_name = None;
    if let Some(id) = memcg.as_deref().and_then(container_of)
        && let Ok(info) = agent
            .runtime
            .docker()
            .inspect_container(&id, None::<InspectContainerOptions>)
            .await
    {
        let labels = info
            .config
            .as_ref()
            .and_then(|c| c.labels.clone())
            .unwrap_or_default();
        let ours = labels.get("facade.node") == Some(&agent.token.node)
            && labels.contains_key("facade.app");
        if ours {
            agent
                .activity
                .watch
                .lock()
                .kernel_ooms
                .insert(id, (facade_core::now(), program));
            return;
        }
        container_name = info.name.map(|n| n.trim_start_matches('/').to_string());
    }
    let summary = match &container_name {
        Some(c) => format!("Kernel killed {program} in container {c} (out of memory)"),
        None => format!("Kernel killed {program} (out of memory)"),
    };
    warn!("{summary}");
    agent.note(NodeEvent {
        at: facade_core::now(),
        kind: EventKind::OomKill,
        severity: Severity::Error,
        problem: true,
        app: None,
        on_node: true,
        summary,
        details: EventDetails {
            reason: Some("out of memory".into()),
            program: Some(program),
            ..Default::default()
        },
    });
}

/// Without the kernel log: the kernel's count of OOM kills, less those Docker
/// reported for this node's apps.
async fn oom_counter(agent: Arc<Agent>) {
    let read = || -> Option<u64> {
        let vmstat = std::fs::read_to_string("/proc/vmstat").ok()?;
        vmstat
            .lines()
            .find_map(|l| l.strip_prefix("oom_kill "))
            .and_then(|v| v.trim().parse().ok())
    };
    let Some(mut before) = read() else { return };
    let mut docker_before = agent.activity.watch.lock().docker_ooms;
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let Some(now) = read() else { return };
        // Docker's events trail a little: give them the time to land.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let docker_now = agent.activity.watch.lock().docker_ooms;
        let kills = now
            .saturating_sub(before)
            .saturating_sub(docker_now.saturating_sub(docker_before));
        before = now;
        docker_before = docker_now;
        if kills > 0 {
            let s = if kills == 1 { "a process" } else { "processes" };
            let summary = if kills == 1 {
                format!("Kernel killed {s} (out of memory)")
            } else {
                format!("Kernel killed {kills} {s} (out of memory)")
            };
            agent.note(NodeEvent {
                at: facade_core::now(),
                kind: EventKind::OomKill,
                severity: Severity::Error,
                problem: true,
                app: None,
                on_node: true,
                summary,
                details: EventDetails {
                    reason: Some("out of memory".into()),
                    ..Default::default()
                },
            });
        }
    }
}

enum KernelSource {
    /// `/dev/kmsg`: one record per read, from its end on.
    Kmsg(std::fs::File),
    /// A file followed from its end, for tests.
    File(PathBuf, std::fs::File),
}

fn open_kernel_source(path: &Path) -> std::io::Result<KernelSource> {
    use std::os::unix::fs::FileTypeExt;
    let mut file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    file.seek(SeekFrom::End(0))?;
    if meta.file_type().is_char_device() {
        Ok(KernelSource::Kmsg(file))
    } else {
        Ok(KernelSource::File(path.to_path_buf(), file))
    }
}

impl KernelSource {
    fn run(self, tx: mpsc::Sender<String>) {
        match self {
            KernelSource::Kmsg(mut file) => {
                let mut buf = vec![0u8; 8192];
                loop {
                    match file.read(&mut buf) {
                        Ok(0) => return,
                        Ok(n) => {
                            let record = String::from_utf8_lossy(&buf[..n]).into_owned();
                            if tx.blocking_send(record).is_err() {
                                return;
                            }
                        }
                        // Records overwritten before they were read.
                        Err(e) if e.raw_os_error() == Some(libc::EPIPE) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            warn!("reading the kernel log: {e}");
                            return;
                        }
                    }
                }
            }
            KernelSource::File(path, file) => {
                follow_file(&path, file, |line| tx.blocking_send(line).is_ok())
            }
        }
    }
}

/// Follows a file like `tail -f` from where `file` stands, starting over if it
/// shrinks (rotated or truncated). Stops when `each` says so.
fn follow_file(path: &Path, mut file: std::fs::File, mut each: impl FnMut(String) -> bool) {
    let mut partial = String::new();
    let mut buf = vec![0u8; 8192];
    loop {
        match file.read(&mut buf) {
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(500));
                let pos = file.stream_position().unwrap_or(0);
                if let Ok(meta) = std::fs::metadata(path)
                    && meta.len() < pos
                    && let Ok(f) = std::fs::File::open(path)
                {
                    file = f;
                }
            }
            Ok(n) => {
                partial.push_str(&String::from_utf8_lossy(&buf[..n]));
                while let Some(i) = partial.find('\n') {
                    let line: String = partial.drain(..=i).collect();
                    if !each(line.trim_end().to_string()) {
                        return;
                    }
                }
            }
            Err(_) => std::thread::sleep(Duration::from_secs(1)),
        }
    }
}

// ---------------------------------------------------------------- SSH logins

/// A successful login, from sshd's `Accepted …` line.
#[derive(Debug, PartialEq)]
struct Login {
    user: String,
    ip: String,
    method: String,
}

fn parse_login(line: &str) -> Option<Login> {
    let i = line.find("Accepted ")?;
    let mut words = line[i + "Accepted ".len()..].split_whitespace();
    let method = words.next()?;
    (words.next()? == "for").then_some(())?;
    let user = words.next()?;
    // A user named `invalid` is still `for invalid from`: sshd only accepts
    // real ones.
    (words.next()? == "from").then_some(())?;
    let ip = words.next()?;
    let method = match method {
        "publickey" => "key",
        "password" => "password",
        m if m.starts_with("keyboard-interactive") => "password",
        m => m,
    };
    Some(Login {
        user: user.to_string(),
        ip: ip.to_string(),
        method: method.to_string(),
    })
}

fn note_login(agent: &Agent, login: Login, at: u64) {
    let summary = format!(
        "SSH login by {} from {} ({})",
        login.user, login.ip, login.method
    );
    info!("{summary}");
    agent.note(NodeEvent {
        at,
        kind: EventKind::SshLogin,
        severity: Severity::Info,
        problem: false,
        app: None,
        on_node: true,
        summary,
        details: EventDetails {
            ip: Some(login.ip),
            user: Some(login.user),
            method: Some(login.method),
            ..Default::default()
        },
    });
}

async fn watch_ssh(agent: Arc<Agent>) {
    let journald = Path::new("/run/systemd/system").exists()
        && std::env::var_os("FACADE_AUTH_LOG").is_none()
        && tokio::process::Command::new("journalctl")
            .arg("--version")
            .output()
            .await
            .is_ok_and(|o| o.status.success());
    if journald {
        loop {
            if let Err(e) = follow_journal(&agent).await {
                debug!("following sshd in the journal: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
    let path = match std::env::var_os("FACADE_AUTH_LOG") {
        Some(p) => PathBuf::from(p),
        None => ["/var/log/auth.log", "/var/log/secure"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
            .unwrap_or_else(|| PathBuf::from("/var/log/auth.log")),
    };
    // It may appear later (rsyslog installed after the agent).
    let file = loop {
        if let Ok(mut f) = std::fs::File::open(&path) {
            let _ = f.seek(SeekFrom::End(0));
            break f;
        }
        tokio::time::sleep(Duration::from_secs(300)).await;
    };
    let (tx, mut rx) = mpsc::channel::<String>(256);
    std::thread::Builder::new()
        .name("auth-log".into())
        .spawn(move || follow_file(&path, file, |line| tx.blocking_send(line).is_ok()))
        .ok();
    while let Some(line) = rx.recv().await {
        if (line.contains("sshd[") || line.contains("sshd-session["))
            && let Some(login) = parse_login(&line)
        {
            note_login(&agent, login, facade_core::now());
        }
    }
}

/// Follows sshd's lines in the journal from where it last stopped (its cursor
/// is kept in the agent's directory), or from now on.
async fn follow_journal(agent: &Arc<Agent>) -> anyhow::Result<()> {
    let cursor_file = agent.runtime.dir().join("ssh-journal-cursor");
    let cursor = std::fs::read_to_string(&cursor_file)
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty());
    let mut cmd = tokio::process::Command::new("journalctl");
    cmd.args(["--follow", "--output=json", "--no-pager"]);
    match &cursor {
        Some(c) => cmd.arg(format!("--after-cursor={c}")),
        None => cmd.arg("--lines=0"),
    };
    cmd.args(["SYSLOG_IDENTIFIER=sshd", "SYSLOG_IDENTIFIER=sshd-session"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("piped");
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let started = Instant::now();
    while let Some(line) = lines.next_line().await? {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(message) = entry["MESSAGE"].as_str() else {
            continue;
        };
        if let Some(login) = parse_login(message) {
            let at = entry["__REALTIME_TIMESTAMP"]
                .as_str()
                .and_then(|t| t.parse::<u64>().ok())
                .map_or_else(facade_core::now, |us| us / 1_000_000);
            note_login(agent, login, at);
            if let Some(c) = entry["__CURSOR"].as_str() {
                let _ = std::fs::write(&cursor_file, c);
            }
        }
    }
    let status = child.wait().await?;
    // Refused at once: most likely a cursor the journal no longer has.
    if !status.success() && started.elapsed() < Duration::from_secs(5) && cursor.is_some() {
        let _ = std::fs::remove_file(&cursor_file);
    }
    anyhow::bail!("journalctl exited ({status})")
}

// ---------------------------------------------------------------- upkeep

async fn upkeep_loop(agent: Arc<Agent>) {
    tokio::time::sleep(Duration::from_secs(20)).await;
    loop {
        let upkeep = read_upkeep().await;
        debug!(?upkeep, "upkeep");
        agent.activity.set_upkeep(upkeep);
        tokio::time::sleep(UPKEEP_EVERY).await;
    }
}

/// Pending updates and reboot, as the system already knows them: nothing is
/// downloaded (no `apt update`), the lists are those the system refreshes.
async fn read_upkeep() -> Upkeep {
    let run = PathBuf::from;
    let reboot_required =
        run("/var/run/reboot-required").exists() || run("/run/reboot-required").exists();
    let reboot_packages = ["/var/run/reboot-required.pkgs", "/run/reboot-required.pkgs"]
        .iter()
        .find_map(|p| std::fs::read_to_string(run(p)).ok())
        .map(|t| unique_lines(&t))
        .unwrap_or_default();
    let mut upkeep = Upkeep {
        at: facade_core::now(),
        reboot_required,
        reboot_packages,
        ..Default::default()
    };
    let apt_check = run("/usr/lib/update-notifier/apt-check");
    if apt_check.exists() {
        // Counts on stderr (`7;2`), then names with --package-names.
        if let Ok(out) = tokio::process::Command::new(&apt_check).output().await
            && let Some((updates, security)) =
                parse_apt_check(&String::from_utf8_lossy(&out.stderr))
        {
            upkeep.updates = Some(updates);
            upkeep.security = Some(security);
            if updates > 0
                && let Ok(out) = tokio::process::Command::new(&apt_check)
                    .arg("--package-names")
                    .output()
                    .await
            {
                upkeep.packages = unique_lines(&String::from_utf8_lossy(&out.stderr));
                upkeep.packages.truncate(100);
            }
            return upkeep;
        }
    }
    if let Ok(out) = tokio::process::Command::new("apt")
        .args(["list", "--upgradable"])
        .env("LANG", "C")
        .output()
        .await
        && out.status.success()
    {
        let (packages, security) = parse_apt_list(&String::from_utf8_lossy(&out.stdout));
        upkeep.updates = Some(packages.len() as u32);
        upkeep.security = Some(security);
        upkeep.packages = packages.into_iter().take(100).collect();
    }
    upkeep
}

fn unique_lines(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && seen.insert(l.to_string()))
        .map(str::to_string)
        .collect()
}

/// `apt-check`'s `updates;security`.
fn parse_apt_check(text: &str) -> Option<(u32, u32)> {
    let line = text.lines().map(str::trim).find(|l| l.contains(';'))?;
    let (a, b) = line.split_once(';')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// Package names of `apt list --upgradable`, and how many come from a
/// security suite.
fn parse_apt_list(text: &str) -> (Vec<String>, u32) {
    let mut names = Vec::new();
    let mut security = 0;
    for line in text.lines() {
        let Some((name, rest)) = line.split_once('/') else {
            continue;
        };
        if !line.contains("[upgradable from") {
            continue;
        }
        let suites = rest.split_whitespace().next().unwrap_or("");
        if suites.split(',').any(|s| s.ends_with("-security")) {
            security += 1;
        }
        names.push(name.to_string());
    }
    (names, security)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "4f1c3b2a9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a3b2c1d0e9f8a7b6c5d4e3f2a";

    #[test]
    fn kernel_oom_kills_name_the_program_and_its_container() {
        let mut p = KernelParser::default();
        // Ubuntu 24.04 (6.8), a container at its memory limit, systemd cgroups.
        let head = format!(
            "6,1052,105738466541,-;oom-kill:constraint=CONSTRAINT_MEMCG,nodemask=(null),cpuset=docker-{ID}.scope,mems_allowed=0,oom_memcg=/system.slice/docker-{ID}.scope,task_memcg=/system.slice/docker-{ID}.scope,task=valet,pid=230512,uid=0"
        );
        assert_eq!(p.line(&head), None);
        let kill = "3,1053,105738466600,-;Memory cgroup out of memory: Killed process 230512 (valet) total-vm:1100128kB, anon-rss:518656kB, file-rss:4352kB, shmem-rss:0kB, UID:0 pgtables:1104kB oom_score_adj:0";
        let Some(Kernel::Oom { program, memcg }) = p.line(kill) else {
            panic!("not an OOM kill");
        };
        assert_eq!(program, "valet");
        assert_eq!(container_of(&memcg.unwrap()).as_deref(), Some(ID));

        // The whole machine out of memory.
        p.line("6,2001,2,-;oom-kill:constraint=CONSTRAINT_NONE,nodemask=(null),cpuset=/,mems_allowed=0,global_oom,task_memcg=/user.slice/user-1000.slice/session-3.scope,task=stress,pid=4242,uid=1000");
        let Some(Kernel::Oom { program, memcg }) = p.line("3,2002,3,-;Out of memory: Killed process 4242 (stress) total-vm:2101336kB, anon-rss:1940000kB, file-rss:0kB, shmem-rss:0kB, UID:1000 pgtables:3880kB oom_score_adj:0") else {
            panic!("not an OOM kill");
        };
        assert_eq!(program, "stress");
        assert_eq!(container_of(&memcg.unwrap()), None);
        // cgroupfs driver, and Docker in Docker: the innermost container.
        assert_eq!(
            container_of(&format!("/docker/{}/docker/{ID}", "a".repeat(64))).as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn disk_errors_are_recognised() {
        let p = |l: &str| KernelParser::default().line(l);
        let disk = |d: &str, problem| {
            Some(Kernel::Disk {
                device: d.into(),
                problem,
            })
        };
        assert_eq!(
            p(
                "3,5012,9,-;I/O error, dev sdb, sector 2048 op 0x1:(WRITE) flags 0x800 phys_seg 1 prio class 2"
            ),
            disk("sdb", DiskProblem::Io("write errors".into()))
        );
        assert_eq!(
            p(
                "blk_update_request: I/O error, dev sda, sector 0 op 0x0:(READ) flags 0x0 phys_seg 1 prio class 0"
            ),
            disk("sda", DiskProblem::Io("read errors".into()))
        );
        assert_eq!(
            p("3,5013,9,-;Buffer I/O error on dev sdb1, logical block 0, lost async page write"),
            disk("sdb1", DiskProblem::Io("I/O error".into()))
        );
        assert_eq!(
            p(
                "2,5014,9,-;EXT4-fs error (device sdb1): ext4_journal_check_start:84: comm kworker/u8:2: Detected aborted journal"
            ),
            disk("sdb1", DiskProblem::Filesystem("ext4".into()))
        );
        assert_eq!(
            p("3,5015,9,-;Aborting journal on device sdb1-8."),
            disk("sdb1", DiskProblem::Filesystem("journal aborted".into()))
        );
        assert_eq!(
            p("2,5016,9,-;EXT4-fs (sdb1): Remounting filesystem read-only"),
            disk("sdb1", DiskProblem::ReadOnly)
        );
        assert_eq!(
            p("4,7,9,-;nvme nvme0: I/O tag 123 (a07b) opcode 0x2 (Read) QID 4 timeout, aborting"),
            disk("nvme0", DiskProblem::Io("I/O timeouts".into()))
        );
        assert_eq!(
            p(
                "XFS (dm-0): metadata I/O error in \"xfs_buf_ioend+0x1a0/0x3f0 [xfs]\" at daddr 0x0 len 8 error 5"
            ),
            disk("dm-0", DiskProblem::Filesystem("xfs".into()))
        );
        // A snap's loop device or a CD drive says nothing about the disks;
        // a sized volume's filesystem going read-only does.
        assert_eq!(
            p("I/O error, dev loop5, sector 0 op 0x0:(READ) flags 0x80700 phys_seg 1 prio class 0"),
            None
        );
        assert_eq!(
            p("EXT4-fs (loop3): Remounting filesystem read-only"),
            disk("loop3", DiskProblem::ReadOnly)
        );
        // Everyday lines.
        assert_eq!(
            p("6,9,9,-;EXT4-fs (sda1): mounted filesystem 1c8f with ordered data mode."),
            None
        );
        assert_eq!(
            p("6,9,9,-;docker0: port 1(veth3) entered forwarding state"),
            None
        );
        assert_eq!(
            disk_summary("sdb1", &DiskProblem::ReadOnly),
            "Filesystem on sdb1 remounted read-only"
        );
    }

    #[test]
    fn ssh_logins() {
        assert_eq!(
            parse_login(
                "Accepted publickey for ubuntu from 81.12.34.56 port 52044 ssh2: ED25519 SHA256:2o0MqXbq0p3y8l"
            ),
            Some(Login {
                user: "ubuntu".into(),
                ip: "81.12.34.56".into(),
                method: "key".into()
            })
        );
        // /var/log/auth.log on Ubuntu 24.04 (RFC 3339 times).
        assert_eq!(
            parse_login(
                "2026-11-04T09:12:44.123456+00:00 vps sshd[2115]: Accepted password for root from 2001:db8::7 port 61000 ssh2"
            ),
            Some(Login {
                user: "root".into(),
                ip: "2001:db8::7".into(),
                method: "password".into()
            })
        );
        assert_eq!(
            parse_login("Accepted keyboard-interactive/pam for alice from 10.0.0.2 port 4000 ssh2")
                .map(|l| l.method),
            Some("password".into())
        );
        // Failures are not recorded.
        assert_eq!(
            parse_login("Failed password for invalid user admin from 1.2.3.4 port 5555 ssh2"),
            None
        );
        assert_eq!(
            parse_login(
                "Connection closed by authenticating user root 1.2.3.4 port 5555 [preauth]"
            ),
            None
        );
    }

    #[test]
    fn upkeep_outputs() {
        assert_eq!(parse_apt_check("7;2"), Some((7, 2)));
        assert_eq!(parse_apt_check("0;0\n"), Some((0, 0)));
        assert_eq!(parse_apt_check("nonsense"), None);
        let list = "Listing...\n\
            libssl3t64/noble-updates,noble-security 3.0.13-0ubuntu3.4 amd64 [upgradable from: 3.0.13-0ubuntu3.1]\n\
            snapd/noble-updates 2.65.3+24.04 amd64 [upgradable from: 2.63+24.04]\n";
        assert_eq!(
            parse_apt_list(list),
            (vec!["libssl3t64".to_string(), "snapd".to_string()], 1)
        );
        assert_eq!(
            unique_lines("linux-image-6.8.0-48-generic\nlinux-base\nlinux-base\n"),
            vec!["linux-image-6.8.0-48-generic", "linux-base"]
        );
    }

    #[test]
    fn exits_explained() {
        let e = explain_exit(137, true, None, Some(512 * 1024 * 1024), None);
        assert_eq!(e.summary, "Killed: out of memory (512 MB limit)");
        assert_eq!(
            (e.kind, e.reason.as_str()),
            (EventKind::OomKill, "out of memory")
        );
        assert_eq!(
            explain_exit(137, true, None, None, Some("valet")).summary,
            "Killed: out of memory, the kernel stopped valet"
        );
        let e = explain_exit(1, false, None, None, None);
        assert_eq!(
            (e.summary.as_str(), e.severity),
            ("Crashed: exit code 1", Severity::Error)
        );
        assert_eq!(
            explain_exit(139, false, None, None, None).summary,
            "Crashed: killed by SIGSEGV (exit code 139)"
        );
        assert_eq!(
            explain_exit(143, false, Some("15"), None, None).summary,
            "Stopped outside facade (SIGTERM)"
        );
        assert_eq!(explain_exit(0, false, None, None, None).reason, "exited");
    }

    #[test]
    fn snapshots_stop_at_the_exit() {
        let exit = docker_time("2026-11-04T10:00:01.5Z").unwrap();
        assert_eq!(docker_time("1970-01-01T00:00:01Z"), Some(1_000_000_000));
        assert_eq!(
            docker_time("2026-10-20T00:00:00.000000001Z"),
            Some(1_792_454_400 * 1_000_000_000 + 1)
        );
        let mut text = String::new();
        for i in 0..60 {
            text.push_str(&format!("2026-11-04T10:00:00.{i:02}Z line {i}\n"));
        }
        text.push_str("2026-11-04T10:00:03.1Z restarted\n");
        let lines = snapshot(&text, exit);
        assert_eq!(lines.len(), SNAPSHOT_LINES);
        assert!(lines.last().unwrap().ends_with("line 59"));
        assert!(lines[0].ends_with("line 10"));
    }

    #[test]
    fn marks_cover_facade_stops_only() {
        let expected = Expected::default();
        {
            let _held = expected.hold("facade-blog");
            assert!(expected.take("abc", "facade-blog"));
            // Taken once.
            assert!(!expected.take("abc", "facade-blog"));
        }
        {
            let _held = expected.hold("abc");
        }
        // Still valid a moment after the stop returned.
        assert!(expected.take("abc", "facade-blog_old"));
        assert!(!expected.take("def", "facade-api"));
    }

    #[test]
    fn container_roles() {
        assert!(matches!(role("facade-blog", "blog"), Some(Role::Main)));
        assert!(matches!(
            role("facade-blog_next", "blog"),
            Some(Role::Transient)
        ));
        assert!(matches!(
            role("facade-blog-release", "blog"),
            Some(Role::Transient)
        ));
        assert!(role("facade-blogger", "blog").is_none());
        assert!(matches!(
            role("facade-shop.api", "shop.api"),
            Some(Role::Main)
        ));
    }
}
