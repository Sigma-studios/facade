//! Reboots facade asks for (now, at a time, or in a node's maintenance window)
//! and the updates it installs in those windows.
//!
//! Discord hears of every reboot before it happens (a heads-up a quarter of an
//! hour before a planned one, a word as it starts), and of how it went: back,
//! with its apps running and the reboot applied, or not. A node not back after
//! ten minutes is a problem; the server's own node cannot say so (facade goes
//! down with it), so its heads-up says to expect a message once it is back.
//!
//! What is planned and in progress is kept in the database: the server's own
//! node reboots the server too, which then picks up where it was.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use facade_core::api::{
    MaintenanceOutcome, MaintenanceWindow, RebootPlan, RebootRequest, RebootRun, ScheduledReboot,
    SetMaintenance, utc_time,
};
use facade_core::events::{EventDetails, EventKind, Severity, Updates};
use facade_core::proto::{AgentRequest, AppState, InstalledUpdates, REBOOT_PROTOCOL};
use facade_core::vulns::FindingStatus;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::Server;
use super::api::{ApiResult, User, bad_request, conflict, not_found, unavailable};
use super::events::{self, Entry};
use super::monitor::Note;

/// A planned reboot is announced this long before.
const WARN_BEFORE: u64 = 15 * 60;
/// A reboot or window missed by less than this (the server was down, builds
/// were running) still happens; later, it is skipped.
const GRACE: u64 = 30 * 60;
/// A node not back after this is a problem (an alert, until it is back).
const LATE: u64 = 10 * 60;
/// Time for apps to start again and the node to read its upkeep, before the
/// report.
const SETTLE: u64 = 3 * 60;
const TICK: Duration = Duration::from_secs(15);
/// How long after a node is back its reboot report is waited for.
const REPORT_WAIT: u64 = 7 * 60;

#[derive(Default)]
pub struct Maintenance {
    /// Nodes asked to reboot and not back yet, and when they were asked:
    /// going offline meanwhile is no alert of its own (a late reboot is).
    rebooting: Mutex<HashMap<String, u64>>,
    /// Nodes whose updates are being installed or reboot being asked.
    busy: Mutex<HashSet<String>>,
    /// Serialises changes to what is stored.
    lock: Mutex<()>,
}

impl Maintenance {
    /// The node was asked to reboot and is not back yet.
    pub fn rebooting(&self, node: &str) -> bool {
        self.rebooting.lock().contains_key(node)
    }
}

/// Nodes not back 10 minutes after facade rebooted them: alerts, until they
/// are (a reboot asked again, or by hand, counts).
pub fn conditions(server: &Server, out: &mut HashMap<String, String>) {
    let now = facade_core::now();
    let late: Vec<(String, u64)> = server
        .maintenance
        .rebooting
        .lock()
        .iter()
        .filter(|(_, asked)| now.saturating_sub(**asked) >= LATE)
        .map(|(n, a)| (n.clone(), *a))
        .collect();
    for (node, asked) in late {
        let message = if server.hub.is_connected(&node) {
            format!(
                "node {node} has not rebooted since facade asked it to ({}): it is still on its last boot. Reboot it again from its page, or by hand (sudo reboot)",
                utc_time(asked)
            )
        } else {
            format!(
                "node {node} is not back from the reboot facade asked ({}): check it from your provider's console",
                utc_time(asked)
            )
        };
        out.insert(format!("node:{node}:reboot"), message);
    }
}

/// Marks a node busy while it lives.
struct Busy<'a>(&'a Server, String);

impl<'a> Busy<'a> {
    fn take(server: &'a Server, node: &str) -> Option<Self> {
        server
            .maintenance
            .busy
            .lock()
            .insert(node.to_string())
            .then(|| Busy(server, node.to_string()))
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.maintenance.busy.lock().remove(&self.1);
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct NodeState {
    #[serde(default)]
    scheduled: Option<ScheduledReboot>,
    /// Reboots and window times a heads-up went out for (the last few).
    #[serde(default)]
    warned: Vec<u64>,
    /// The last window time run (or that had nothing to do).
    #[serde(default)]
    window_done: Option<u64>,
    /// A window time skipped by hand.
    #[serde(default)]
    skip: Option<u64>,
    #[serde(default)]
    run: Option<Run>,
    #[serde(default)]
    last: Option<MaintenanceOutcome>,
}

impl NodeState {
    fn warn(&mut self, at: u64) {
        self.warned.push(at);
        let extra = self.warned.len().saturating_sub(4);
        self.warned.drain(..extra);
    }
}

/// A reboot asked, followed until the node is back and settled.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Run {
    by: String,
    asked: u64,
    why: String,
    /// When the machine had booted, to tell it rebooted.
    boot_before: Option<u64>,
    /// Apps running before, which should run again.
    apps: Vec<String>,
    #[serde(default)]
    back: Option<u64>,
    #[serde(default)]
    late_warned: bool,
}

fn state_key(node: &str) -> String {
    format!("maintenance.state.{node}")
}

fn window_key(node: &str) -> String {
    format!("maintenance.window.{node}")
}

fn load(server: &Server, node: &str) -> NodeState {
    server
        .store
        .kv_get(&state_key(node))
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

/// Changes a node's stored state.
fn update<R>(server: &Server, node: &str, f: impl FnOnce(&mut NodeState) -> R) -> R {
    let _held = server.maintenance.lock.lock();
    let mut state = load(server, node);
    let out = f(&mut state);
    match serde_json::to_string(&state) {
        Ok(json) => {
            if let Err(e) = server.store.kv_put(&state_key(node), &json) {
                warn!(%node, "saving its maintenance state: {e:#}");
            }
        }
        Err(e) => warn!(%node, "saving its maintenance state: {e:#}"),
    }
    let mut rebooting = server.maintenance.rebooting.lock();
    match state.run.as_ref().filter(|r| r.back.is_none()) {
        Some(run) => rebooting.insert(node.to_string(), run.asked),
        None => rebooting.remove(node),
    };
    out
}

pub fn window(server: &Server, node: &str) -> Option<MaintenanceWindow> {
    server
        .store
        .kv_get(&window_key(node))
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
}

/// The next time the node's window runs, skipped ones left out.
fn next_window(w: &MaintenanceWindow, state: &NodeState, now: u64) -> u64 {
    let next = w.next_after(now);
    if state.skip == Some(next) {
        w.next_after(next + 1)
    } else {
        next
    }
}

/// What the problems say about a node's reboot when one is planned.
pub fn planned_note(server: &Server, node: &str) -> Option<String> {
    let state = load(server, node);
    if state.run.is_some() {
        return Some("facade is rebooting it".into());
    }
    if let Some(s) = &state.scheduled {
        return Some(format!(
            "a reboot is scheduled for {} (by {})",
            utc_time(s.at),
            s.by
        ));
    }
    let w = window(server, node).filter(|w| w.reboot)?;
    Some(format!(
        "its maintenance window reboots it on {}",
        utc_time(next_window(&w, &state, facade_core::now()))
    ))
}

// ---------------------------------------------------------------- facts

fn running_apps(server: &Server, node: &str) -> Vec<String> {
    let routes = server.routes.read();
    let live = server.live.read();
    let mut apps: Vec<String> = routes
        .nodes
        .iter()
        .filter(|(app, n)| {
            n.as_str() == node
                && live
                    .apps
                    .get(app.as_str())
                    .is_some_and(|l| l.state == AppState::Running)
        })
        .map(|(app, _)| app.clone())
        .collect();
    apps.sort();
    apps
}

fn builds_running(server: &Server, node: &str) -> Vec<String> {
    server
        .live
        .read()
        .nodes
        .get(node)
        .map(|n| n.builds.iter().map(|b| b.app.clone()).collect())
        .unwrap_or_default()
}

/// Why facade cannot ask the node anything now.
fn unavailable_now(server: &Server, node: &str) -> Option<String> {
    if !server.hub.is_connected(node) {
        return Some(format!("node {node} is offline"));
    }
    let protocol = server.live.read().nodes.get(node).map_or(0, |n| n.protocol);
    (protocol < REBOOT_PROTOCOL).then(|| {
        format!(
            "node {node} runs an older agent, which cannot reboot it: it updates itself to the server's build when it reconnects"
        )
    })
}

fn updates_unavailable(server: &Server, node: &str) -> Option<String> {
    let live = server.live.read();
    match live.nodes.get(node).and_then(|n| n.platform.as_ref()) {
        Some(p) => p.installs_updates().err(),
        None => Some(format!("node {node} has not said what it runs yet")),
    }
}

fn pending_updates(server: &Server, node: &str) -> Option<u32> {
    match server.activity.upkeep(node)?.updates.known()? {
        Updates::Packages { count, .. } => Some(*count),
        Updates::Channel(_) => None,
    }
}

fn reboot_reasons(server: &Server, node: &str) -> (bool, Vec<String>) {
    server.activity.upkeep(node).map_or((false, vec![]), |u| {
        (u.reboot_required(), u.reboot_reasons().to_vec())
    })
}

fn local(server: &Server, node: &str) -> bool {
    server.cfg.local_node == node
}

/// `9 apps stop meanwhile and start again by themselves (a, b, c…)`.
fn apps_line(apps: &[String]) -> String {
    match apps.len() {
        0 => "No app runs there.".into(),
        n => {
            let mut names = apps.iter().take(8).cloned().collect::<Vec<_>>().join(", ");
            if n > 8 {
                names.push_str(&format!(" and {} more", n - 8));
            }
            format!(
                "{n} app{} stop{} for a minute or two and start again by {}: {names}.",
                if n == 1 { "" } else { "s" },
                if n == 1 { "s" } else { "" },
                if n == 1 { "itself" } else { "themselves" }
            )
        }
    }
}

fn lasted(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min {:02} s", secs / 60, secs % 60),
        _ => format!("{} h {:02} min", secs / 3600, secs % 3600 / 60),
    }
}

async fn tell(server: &Server, note: Note) {
    if let Err(e) = server.notifier.send(&server.store, &[note]).await {
        warn!("posting to Discord: {e:#}");
    }
}

// ---------------------------------------------------------------- the loop

pub async fn run(server: Arc<Server>) {
    // What was in progress before a restart (the server's own node rebooting).
    if let Ok(nodes) = server.store.list_nodes() {
        for node in nodes {
            update(&server, &node.name, |_| ());
        }
    }
    loop {
        if let Ok(nodes) = server.store.list_nodes() {
            for node in nodes {
                tick(&server, &node.name).await;
            }
        }
        tokio::time::sleep(TICK).await;
    }
}

async fn tick(server: &Arc<Server>, node: &str) {
    let now = facade_core::now();
    let state = load(server, node);
    if let Some(run) = state.run.clone() {
        follow(server, node, run, now).await;
        return;
    }
    if server.maintenance.busy.lock().contains(node) {
        return;
    }
    let builds = builds_running(server, node);

    if let Some(s) = state.scheduled.clone() {
        if s.at <= now {
            // A build in progress is let finish, for a while.
            if !builds.is_empty() && now < s.at + GRACE {
                return;
            }
            update(server, node, |st| st.scheduled = None);
            if now > s.at + GRACE {
                let missed = format!(
                    "The reboot of node {node} scheduled for {} was skipped: facade was not running then, or the node was busy.",
                    utc_time(s.at)
                );
                Entry::node(node, EventKind::Maintenance, Severity::Warn, &missed).record(server);
                tell(server, Note::notice("Reboot skipped", &missed, false)).await;
                return;
            }
            let (server, node) = (server.clone(), node.to_string());
            tokio::spawn(async move {
                let why = format!("scheduled by {}", s.by);
                let _ = start_reboot(&server, &node, &s.by, &why).await;
            });
            return;
        }
        if s.at - now <= WARN_BEFORE && !state.warned.contains(&s.at) {
            update(server, node, |st| st.warn(s.at));
            heads_up(server, node, s.at, None, &format!("Scheduled by {}.", s.by)).await;
        }
    }

    let Some(w) = window(server, node) else {
        return;
    };
    let at = w.next_after(now.saturating_sub(GRACE));
    if state.window_done == Some(at) || state.skip == Some(at) {
        return;
    }
    if at <= now {
        if !builds.is_empty() && now < at + GRACE {
            return;
        }
        update(server, node, |st| st.window_done = Some(at));
        let (server, node) = (server.clone(), node.to_string());
        tokio::spawn(async move { run_window(&server, &node, &w).await });
    } else if at - now <= WARN_BEFORE && !state.warned.contains(&at) {
        update(server, node, |st| st.warn(at));
        if work_expected(server, node, &w) {
            let origin = format!("Its maintenance window ({}) {}.", w.when(), w.what());
            heads_up(server, node, at, Some(&w), &origin).await;
        }
    }
}

/// The window will do something, as far as is known now.
fn work_expected(server: &Server, node: &str, w: &MaintenanceWindow) -> bool {
    let updates = w.updates
        && updates_unavailable(server, node).is_none()
        && pending_updates(server, node).is_some_and(|n| n > 0);
    let reboot = w.reboot && reboot_reasons(server, node).0;
    updates || reboot
}

/// Announces a reboot (or a window that may reboot) at `at`.
async fn heads_up(
    server: &Server,
    node: &str,
    at: u64,
    window: Option<&MaintenanceWindow>,
    origin: &str,
) {
    let mut lines = vec![origin.to_string()];
    if window.is_some_and(|w| w.updates)
        && let Some(n) = pending_updates(server, node).filter(|n| *n > 0)
    {
        lines.push(format!(
            "{n} update{} pending.",
            if n == 1 { "" } else { "s" }
        ));
    }
    let (required, reasons) = reboot_reasons(server, node);
    if required {
        lines.push(if reasons.is_empty() {
            "A reboot is required now.".to_string()
        } else {
            format!("A reboot is required now ({}).", reasons.join(", "))
        });
    }
    if window.is_none_or(|w| w.reboot) {
        lines.push(apps_line(&running_apps(server, node)));
        lines.push(if local(server, node) {
            "facade runs on this node and goes down with it: a message here says when it is back. None within 15 minutes of the reboot means it did not come back: check the VPS from your provider's console.".into()
        } else {
            "facade says here when it is back, and warns if it is not within 10 minutes.".into()
        });
    }
    lines.push(match window {
        None => format!("Cancel: facade node reboot {node} --cancel, or on the node's page."),
        Some(_) => {
            format!("Skip this time: facade node maintenance {node} --skip, or on the node's page.")
        }
    });
    let title = match window {
        None => format!("🕓 Node {node} reboots at {}", utc_time(at)),
        Some(_) => format!("🕓 Maintenance of node {node} at {}", utc_time(at)),
    };
    info!(%node, "announcing {title}");
    tell(server, Note::notice(&title, &lines.join("\n"), false)).await;
}

/// Asks the node to reboot, and follows it from there.
async fn start_reboot(server: &Server, node: &str, by: &str, why: &str) -> Result<(), String> {
    let Some(_busy) = Busy::take(server, node) else {
        return Err(format!("node {node} is busy with its maintenance"));
    };
    if let Some(e) = unavailable_now(server, node) {
        fail(
            server,
            node,
            &format!("Reboot of node {node} ({why}) not done: {e}"),
        )
        .await;
        return Err(e);
    }
    let apps = running_apps(server, node);
    let run = Run {
        by: by.to_string(),
        asked: facade_core::now(),
        why: why.to_string(),
        boot_before: events::agent_boot(server, node),
        apps: apps.clone(),
        back: None,
        late_warned: false,
    };
    // Kept before asking: the server's own node takes the server down with it.
    update(server, node, |st| st.run = Some(run));
    let id = Entry::node(
        node,
        EventKind::Maintenance,
        Severity::Info,
        format!("Rebooting node {node} ({why})"),
    )
    .by(by)
    .items(apps.clone())
    .record(server);
    let mut lines = vec![format!("{}.", capitalize(why)), apps_line(&apps)];
    if local(server, node) {
        lines.push("facade goes down with it and says here when it is back. No message within 15 minutes means it did not come back: check the VPS from your provider's console.".into());
    }
    tell(
        server,
        Note::notice(
            &format!("🔄 Rebooting node {node}"),
            &lines.join("\n"),
            false,
        )
        .linked(events::link(server, id.as_ref())),
    )
    .await;
    info!(%node, "asking the node to reboot ({why})");
    if let Err(e) = server.hub.request(node, AgentRequest::Reboot).await {
        let e = format!("{e:#}");
        update(server, node, |st| st.run = None);
        fail(server, node, &format!("Reboot of node {node} failed: {e}")).await;
        return Err(e);
    }
    Ok(())
}

/// Something that did not happen: an entry, a pinging message, the outcome.
async fn fail(server: &Server, node: &str, message: &str) {
    warn!(%node, "{message}");
    let id = Entry::node(node, EventKind::Maintenance, Severity::Error, message)
        .problem()
        .record(server);
    update(server, node, |st| {
        st.last = Some(MaintenanceOutcome {
            at: facade_core::now(),
            ok: false,
            summary: message.to_string(),
        })
    });
    tell(
        server,
        Note::problem(message).linked(events::link(server, id.as_ref())),
    )
    .await;
}

/// One time of a node's window: updates, then the reboot they (or earlier
/// ones) need.
async fn run_window(server: &Server, node: &str, w: &MaintenanceWindow) {
    let (mut required, mut reasons) = reboot_reasons(server, node);
    let mut installed = None;
    {
        let Some(_busy) = Busy::take(server, node) else {
            return;
        };
        let pending = pending_updates(server, node);
        if w.updates && updates_unavailable(server, node).is_none() && pending != Some(0) {
            if let Some(e) = unavailable_now(server, node) {
                fail(
                    server,
                    node,
                    &format!("Maintenance of node {node} not done: {e}"),
                )
                .await;
                return;
            }
            info!(%node, "maintenance window: installing updates");
            match server
                .hub
                .query::<InstalledUpdates>(node, AgentRequest::InstallUpdates)
                .await
            {
                Ok(done) => {
                    let what = match pending {
                        Some(n) => format!("{n} update{}", if n == 1 { "" } else { "s" }),
                        None => "the pending updates".into(),
                    };
                    Entry::node(
                        node,
                        EventKind::Maintenance,
                        Severity::Ok,
                        format!("Installed {what} on node {node} (maintenance window)"),
                    )
                    .details(EventDetails {
                        logs: done.log,
                        ..Default::default()
                    })
                    .record(server);
                    required = done.upkeep.reboot_required();
                    reasons = done.upkeep.reboot_reasons().to_vec();
                    installed = Some(what);
                }
                Err(e) => {
                    fail(
                        server,
                        node,
                        &format!(
                            "Maintenance of node {node}: installing updates failed ({e:#}); it was not rebooted"
                        ),
                    )
                    .await;
                    return;
                }
            }
        }
    }
    if w.reboot && required {
        let mut why = "maintenance window".to_string();
        if let Some(what) = &installed {
            why.push_str(&format!(", after installing {what}"));
        }
        if !reasons.is_empty() {
            why.push_str(&format!("; asked for by {}", reasons.join(", ")));
        }
        let _ = start_reboot(server, node, "maintenance window", &why).await;
        return;
    }
    let Some(what) = installed else {
        return;
    };
    let summary = if required {
        format!(
            "Installed {what}. A reboot is required ({}), and this window does not reboot: reboot it from its page.",
            reasons.join(", ")
        )
    } else {
        format!("Installed {what}; no reboot needed.")
    };
    update(server, node, |st| {
        st.last = Some(MaintenanceOutcome {
            at: facade_core::now(),
            ok: !required,
            summary: summary.clone(),
        })
    });
    tell(
        server,
        Note::notice(&format!("🛠 Maintenance of node {node}"), &summary, false),
    )
    .await;
}

/// Follows a reboot: back, then settled, then the report.
async fn follow(server: &Server, node: &str, run: Run, now: u64) {
    let connected = server.hub.is_connected(node);
    let Some(back) = run.back else {
        let boot = events::agent_boot(server, node);
        let rebooted = connected
            && match (run.boot_before, boot) {
                (Some(before), Some(after)) => {
                    before.abs_diff(after) > facade_core::reboot_report::SAME_BOOT_SECS
                }
                (None, Some(after)) => after + 60 >= run.asked,
                _ => false,
            };
        if rebooted {
            info!(%node, "back from its reboot");
            update(server, node, |st| {
                if let Some(r) = st.run.as_mut() {
                    r.back = Some(now);
                }
            });
            return;
        }
        return;
    };
    if now.saturating_sub(back) < SETTLE {
        return;
    }
    // The agent's account of the reboot (each app's stop and return) comes
    // once its apps are back, or 5 minutes after it started: worth waiting for.
    let report = events::reboot_report_since(server, node, run.asked.saturating_sub(60));
    if report.is_none() && now.saturating_sub(back) < REPORT_WAIT {
        return;
    }

    let down: Vec<String> = {
        let live = server.live.read();
        run.apps
            .iter()
            .filter(|a| {
                live.apps
                    .get(a.as_str())
                    .is_none_or(|l| l.state != AppState::Running)
            })
            .cloned()
            .collect()
    };
    let (still, reasons) = reboot_reasons(server, node);
    let kernel = server
        .live
        .read()
        .nodes
        .get(node)
        .and_then(|n| n.platform.as_ref()?.kernel.clone());
    let mut lines = vec![format!(
        "Back {} after facade asked ({}).",
        lasted(back.saturating_sub(run.asked)),
        run.why
    )];
    lines.push(match (run.apps.len(), down.len()) {
        (0, _) => "No app ran there before.".into(),
        (n, 0) => format!(
            "{} running again.",
            if n == 1 {
                "Its app is".to_string()
            } else {
                format!("All {n} apps that ran before are")
            }
        ),
        (_, _) => format!("Not running again: {}.", down.join(", ")),
    });
    if still {
        lines.push(if reasons.is_empty() {
            "It still asks for a reboot.".into()
        } else {
            format!("It still asks for a reboot ({}).", reasons.join(", "))
        });
    } else if let Some(k) = &kernel {
        lines.push(format!("Running kernel {k}; no reboot pending."));
    }
    let mut killed = vec![];
    if let Some(r) = &report {
        use facade_core::reboot_report::duration;
        if let Some((a, d)) = r.longest_down() {
            lines.push(format!(
                "Apps were down {} at most ({}).",
                duration(d),
                a.name
            ));
        }
        if let Some(off) = r.off_for() {
            lines.push(format!("The machine was off {}.", duration(off)));
        }
        killed = r.killed().iter().map(|a| a.name.clone()).collect();
        if !killed.is_empty() {
            lines.push(format!(
                "Killed at shutdown, past their stop timeout: {}.",
                killed.join(", ")
            ));
        }
    }
    let ok = down.is_empty() && !still && killed.is_empty();
    let summary = lines.join(" ");
    let id = Entry::node(
        node,
        EventKind::Maintenance,
        if ok { Severity::Ok } else { Severity::Warn },
        format!("Node {node} back from its reboot: {}", lines[1..].join(" ")),
    )
    .problem_if(!ok)
    .record(server);
    update(server, node, |st| {
        st.run = None;
        st.last = Some(MaintenanceOutcome {
            at: now,
            ok,
            summary: summary.clone(),
        });
    });
    let title = if ok {
        format!("✅ Node {node} is back")
    } else {
        format!("⚠️ Node {node} is back, with problems")
    };
    tell(
        server,
        Note::notice(&title, &lines.join("\n"), !ok).linked(events::link(server, id.as_ref())),
    )
    .await;
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------- API

type S = State<Arc<Server>>;

fn plan(server: &Server, node: &str) -> anyhow::Result<RebootPlan> {
    let state = load(server, node);
    let window = window(server, node);
    let now = facade_core::now();
    let (reboot_required, reasons) = reboot_reasons(server, node);
    let fixes = server
        .store
        .findings(Some(node))?
        .into_iter()
        .filter(|f| {
            f.status == FindingStatus::FixedAfterReboot
                && f.severity <= facade_core::vulns::VulnSeverity::High
        })
        .map(|f| f.cve)
        .collect();
    Ok(RebootPlan {
        node: node.to_string(),
        local: local(server, node),
        connected: server.hub.is_connected(node),
        unavailable: unavailable_now(server, node),
        updates_unavailable: updates_unavailable(server, node),
        apps: running_apps(server, node),
        reasons,
        reboot_required,
        updates: pending_updates(server, node),
        fixes,
        builds: builds_running(server, node),
        scheduled: state.scheduled.clone(),
        next_window: window.as_ref().map(|w| next_window(w, &state, now)),
        maintenance: window,
        running: state.run.as_ref().map(|r| RebootRun {
            by: r.by.clone(),
            asked: r.asked,
            back: r.back,
        }),
        last: state.last,
    })
}

fn node_for(server: &Server, user: &User, name: &str) -> ApiResult<()> {
    user.require_admin()?;
    if !server.store.node_exists(name)? {
        return Err(not_found(format!("no node named {name}")));
    }
    Ok(())
}

/// `GET /api/nodes/{name}/reboot`.
pub async fn get_plan(
    State(server): S,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<Json<RebootPlan>> {
    node_for(&server, &user, &name)?;
    Ok(Json(plan(&server, &name)?))
}

/// `POST /api/nodes/{name}/reboot`: now, or at a time.
pub async fn reboot(
    State(server): S,
    user: User,
    Path(name): Path<String>,
    Json(req): Json<RebootRequest>,
) -> ApiResult<Json<RebootPlan>> {
    node_for(&server, &user, &name)?;
    let now = facade_core::now();
    // A reboot that did not happen (late) may be asked again.
    if let Some(run) = load(&server, &name).run
        && (run.back.is_some() || now.saturating_sub(run.asked) < LATE)
    {
        return Err(conflict(format!("node {name} is already rebooting")));
    }
    match req.at {
        Some(at) if at > now + 60 => {
            if at > now + 30 * 86_400 {
                return Err(bad_request("a reboot can be scheduled up to 30 days ahead"));
            }
            update(&server, &name, |st| {
                st.scheduled = Some(ScheduledReboot {
                    at,
                    by: user.username.clone(),
                });
                st.warned.retain(|w| *w != at);
            });
            Entry::node(
                &name,
                EventKind::Maintenance,
                Severity::Info,
                format!("Reboot of node {name} scheduled for {}", utc_time(at)),
            )
            .by(&user.username)
            .record(&server);
        }
        Some(at) if at + 60 < now => {
            return Err(bad_request(format!("{} has passed", utc_time(at))));
        }
        _ => {
            if let Some(e) = unavailable_now(&server, &name) {
                return Err(unavailable(e));
            }
            let why = format!("asked by {}", user.username);
            start_reboot(&server, &name, &user.username, &why)
                .await
                .map_err(unavailable)?;
        }
    }
    Ok(Json(plan(&server, &name)?))
}

#[derive(Deserialize)]
pub struct CancelQuery {
    /// Skip the window's next time rather than cancel a scheduled reboot.
    #[serde(default)]
    window: bool,
}

/// `DELETE /api/nodes/{name}/reboot`: cancels the scheduled reboot, or skips
/// the window's next time (`?window=true`).
pub async fn cancel(
    State(server): S,
    user: User,
    Path(name): Path<String>,
    Query(q): Query<CancelQuery>,
) -> ApiResult<Json<RebootPlan>> {
    node_for(&server, &user, &name)?;
    let now = facade_core::now();
    let (what, announced) = if q.window {
        let Some(w) = window(&server, &name) else {
            return Err(bad_request(format!(
                "node {name} has no maintenance window"
            )));
        };
        let state = load(&server, &name);
        let at = next_window(&w, &state, now);
        update(&server, &name, |st| st.skip = Some(at));
        (
            format!("Maintenance of node {name} on {} skipped", utc_time(at)),
            state.warned.contains(&at),
        )
    } else {
        let Some(s) = update(&server, &name, |st| st.scheduled.take()) else {
            return Err(bad_request(format!(
                "no reboot of node {name} is scheduled"
            )));
        };
        (
            format!("Reboot of node {name} on {} cancelled", utc_time(s.at)),
            load(&server, &name).warned.contains(&s.at),
        )
    };
    Entry::node(&name, EventKind::Maintenance, Severity::Info, &what)
        .by(&user.username)
        .record(&server);
    if announced {
        tell(
            &server,
            Note::notice(&what, &format!("By {}.", user.username), false),
        )
        .await;
    }
    Ok(Json(plan(&server, &name)?))
}

/// `PUT /api/nodes/{name}/maintenance`.
pub async fn set_window(
    State(server): S,
    user: User,
    Path(name): Path<String>,
    Json(req): Json<SetMaintenance>,
) -> ApiResult<Json<RebootPlan>> {
    node_for(&server, &user, &name)?;
    let summary = match &req.window {
        Some(w) => {
            w.validate().map_err(bad_request)?;
            if w.updates
                && let Some(why) = updates_unavailable(&server, &name)
            {
                return Err(bad_request(why));
            }
            server.store.kv_put(
                &window_key(&name),
                &serde_json::to_string(w).map_err(anyhow::Error::from)?,
            )?;
            // A time that just passed is not run now: the next one is the first.
            let now = facade_core::now();
            let recent = w.next_after(now.saturating_sub(GRACE));
            update(&server, &name, |st| {
                st.skip = None;
                if recent <= now {
                    st.window_done = Some(recent);
                }
            });
            format!(
                "Maintenance window of node {name}: {}, {}",
                w.when(),
                w.what()
            )
        }
        None => {
            server.store.kv_delete(&window_key(&name))?;
            format!("Maintenance window of node {name} turned off")
        }
    };
    Entry::node(&name, EventKind::Maintenance, Severity::Info, summary)
        .by(&user.username)
        .record(&server);
    Ok(Json(plan(&server, &name)?))
}
