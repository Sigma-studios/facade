//! Activity, on the server's side: entries for what the agents report, for
//! what accounts do through the API, and for what the server's own watchers
//! notice (alerts, certificates, DNS records, reboots, updates); the node
//! states derived from them (upkeep, disk errors); `GET /api/events`.
//!
//! Recording never fails the action it records: a database error is logged.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use facade_core::AppSpec;
use facade_core::api::{AlertView, DeployRequest, DeploymentView};
use facade_core::events::{
    Category, DiskTrouble, EventDetails, EventKind, EventView, EventsPage, NodeEvent, Scope,
    Severity, Upkeep,
};
use facade_core::proto::NodeReport;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;

use super::Server;
use super::api::{ApiResult, User, not_found};
use super::store::{EventQuery, EventRecord};
use super::users::{can_see_app, visible_apps};

/// How long entries are kept.
pub const KEEP: u64 = 90 * 86400;
/// A node shows disk errors (badge, alert) this long after the last one.
const DISK_TROUBLE_FOR: u64 = 86400;
/// Certificates closer than this to expiring are an alert.
const CERT_ALERT_WITHIN: u64 = 7 * 86400;

/// What the server keeps about each node from its agent's reports.
#[derive(Default)]
pub struct Activity {
    nodes: RwLock<HashMap<String, NodeTrack>>,
}

#[derive(Default, Clone)]
struct NodeTrack {
    upkeep: Option<Upkeep>,
    disk: Option<DiskTrouble>,
}

impl Activity {
    pub fn upkeep(&self, node: &str) -> Option<Upkeep> {
        self.nodes.read().get(node)?.upkeep.clone()
    }

    /// Disk errors of the last day.
    pub fn disk_trouble(&self, node: &str) -> Option<DiskTrouble> {
        let disk = self.nodes.read().get(node)?.disk.clone()?;
        (facade_core::now().saturating_sub(disk.at) < DISK_TROUBLE_FOR).then_some(disk)
    }
}

/// An entry being written: `Entry::app(…).by(user).record(server)`.
pub struct Entry(EventRecord);

impl Entry {
    fn new(scope: Scope, kind: EventKind, severity: Severity, summary: impl Into<String>) -> Self {
        Self(EventRecord {
            at: facade_core::now(),
            scope,
            kind: Some(kind),
            severity,
            summary: summary.into(),
            ..Default::default()
        })
    }

    pub fn app(app: &str, kind: EventKind, severity: Severity, summary: impl Into<String>) -> Self {
        let mut e = Self::new(Scope::App, kind, severity, summary);
        e.0.app = Some(app.to_string());
        e
    }

    pub fn node(
        node: &str,
        kind: EventKind,
        severity: Severity,
        summary: impl Into<String>,
    ) -> Self {
        let mut e = Self::new(Scope::Node, kind, severity, summary);
        e.0.node = Some(node.to_string());
        e
    }

    pub fn global(kind: EventKind, severity: Severity, summary: impl Into<String>) -> Self {
        Self::new(Scope::Global, kind, severity, summary)
    }

    pub fn by(mut self, actor: impl Into<String>) -> Self {
        self.0.actor = Some(actor.into());
        self
    }

    pub fn problem(mut self) -> Self {
        self.0.problem = true;
        self
    }

    pub fn at(mut self, at: u64) -> Self {
        self.0.at = at;
        self
    }

    pub fn details(mut self, details: EventDetails) -> Self {
        self.0.details = details;
        self
    }

    pub fn items(mut self, items: Vec<String>) -> Self {
        self.0.details.items = items;
        self
    }

    pub fn fold(mut self) -> Self {
        self.0.fold = true;
        self
    }

    /// Stores it; the app's node is filled in when known.
    pub fn record(mut self, server: &Server) -> Option<i64> {
        if self.0.node.is_none()
            && let Some(app) = &self.0.app
        {
            self.0.node = server.routes.read().nodes.get(app).cloned();
        }
        let e = self.0;
        match server.store.insert_event(&e) {
            Ok(id) => {
                if e.kind == Some(EventKind::DiskError)
                    && let Some(node) = &e.node
                {
                    server
                        .activity
                        .nodes
                        .write()
                        .entry(node.clone())
                        .or_default()
                        .disk = Some(DiskTrouble {
                        at: e.at,
                        summary: e.summary.clone(),
                        event: id,
                    });
                }
                Some(id)
            }
            Err(err) => {
                warn!("recording activity ({}): {err:#}", e.summary);
                None
            }
        }
    }
}

/// What an agent saw: its node is the one it speaks for; an app it names must
/// exist (otherwise the entry is the node's).
pub fn from_agent(server: &Server, node: &str, e: NodeEvent) {
    let app = e
        .app
        .filter(|a| server.store.get_app(a).ok().flatten().is_some());
    let scope = if e.on_node || app.is_none() {
        Scope::Node
    } else {
        Scope::App
    };
    let mut entry = Entry::new(scope, e.kind, e.severity, e.summary)
        .at(e.at)
        .details(e.details)
        .fold();
    entry.0.app = app;
    entry.0.node = Some(node.to_string());
    entry.0.problem = e.problem;
    entry.record(server);
}

// ---------------------------------------------------------------- restarts of facade, reboots

/// Kept per node, to tell a reboot, an update and a restart apart.
#[derive(Debug, Default, Serialize, Deserialize)]
struct AgentMark {
    started: Option<u64>,
    boot: Option<u64>,
    binary: Option<String>,
}

/// What an agent says about itself when it connects.
pub struct AgentHello<'a> {
    pub started: Option<u64>,
    pub boot: Option<u64>,
    pub binary: Option<&'a str>,
    pub commit: Option<&'a str>,
    pub version: &'a str,
}

/// An agent connected: the node rebooted, or its agent restarted or was
/// updated, since the last time? The server's own node's agent runs inside the
/// server, whose start says it all (`server_started`) but the reboot.
pub fn agent_connected(server: &Server, node: &str, hello: AgentHello<'_>) {
    if hello.started.is_none() {
        return;
    }
    let key = format!("activity.agent.{node}");
    let before: Option<AgentMark> = server
        .store
        .kv_get(&key)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok());
    let now = AgentMark {
        started: hello.started,
        boot: hello.boot,
        binary: hello.binary.map(str::to_string),
    };
    if let Some(before) = &before {
        let rebooted = matches!((before.boot, now.boot), (Some(a), Some(b)) if a.abs_diff(b) > 120);
        if rebooted {
            Entry::node(node, EventKind::Reboot, Severity::Info, "Rebooted")
                .at(now.boot.unwrap_or_else(facade_core::now))
                .problem()
                .record(server);
        }
        let restarted =
            matches!((before.started, now.started), (Some(a), Some(b)) if a.abs_diff(b) > 2);
        if restarted && node != server.cfg.local_node {
            let version = match hello.commit {
                Some(c) => format!("{} ({})", hello.version, crate::selfbin::short_commit(c)),
                None => hello.version.to_string(),
            };
            if before.binary.is_some() && before.binary != now.binary {
                Entry::node(
                    node,
                    EventKind::FacadeUpdate,
                    Severity::Info,
                    format!("facade agent updated to {version}"),
                )
                .at(now.started.unwrap_or_else(facade_core::now))
                .record(server);
            } else if !rebooted {
                Entry::node(
                    node,
                    EventKind::AgentRestart,
                    Severity::Info,
                    "facade agent restarted",
                )
                .at(now.started.unwrap_or_else(facade_core::now))
                .record(server);
            }
        }
    }
    if let Ok(json) = serde_json::to_string(&now)
        && let Err(e) = server.store.kv_put(&key, &json)
    {
        warn!(%node, "saving its agent's start: {e:#}");
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ServerMark {
    binary: Option<String>,
    version: String,
}

/// The server started: updated, or restarted. An entry of its own node.
pub fn server_started(server: &Server) {
    const KEY: &str = "activity.server";
    let before: Option<ServerMark> = server
        .store
        .kv_get(KEY)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok());
    let now = ServerMark {
        binary: crate::selfbin::current().map(|b| b.sha256.clone()),
        version: crate::selfbin::version_line().to_string(),
    };
    let node = &server.cfg.local_node;
    let entry = match &before {
        Some(b) if b.binary.is_some() && b.binary != now.binary => Entry::node(
            node,
            EventKind::FacadeUpdate,
            Severity::Info,
            if b.version != now.version {
                format!("facade updated to {} (from {})", now.version, b.version)
            } else {
                format!("facade updated ({})", now.version)
            },
        ),
        Some(_) => Entry::node(
            node,
            EventKind::ServerStart,
            Severity::Info,
            "facade server restarted",
        ),
        None => Entry::node(
            node,
            EventKind::ServerStart,
            Severity::Info,
            format!("facade server started ({})", now.version),
        ),
    };
    entry.record(server);
    if let Ok(json) = serde_json::to_string(&now) {
        let _ = server.store.kv_put(KEY, &json);
    }
}

// ---------------------------------------------------------------- reports

/// A node's report: its upkeep, when it says.
pub fn on_report(server: &Server, node: &str, report: &NodeReport) {
    let Some(upkeep) = &report.upkeep else {
        return;
    };
    let before = {
        let mut nodes = server.activity.nodes.write();
        let track = nodes.entry(node.to_string()).or_default();
        track.upkeep.replace(upkeep.clone())
    };
    let was_required = before.as_ref().is_some_and(|u| u.reboot_required);
    if upkeep.reboot_required && !was_required {
        // A server restart forgets the upkeep it was told: the entries say
        // whether this reboot was asked for already (since the last one).
        let last = |kind| server.store.last_node_event(node, kind, 0).ok().flatten();
        let asked = last(EventKind::Upkeep).filter(|e| e.summary.starts_with("Reboot required"));
        let rebooted = last(EventKind::Reboot);
        let recent = asked.is_some_and(|a| rebooted.is_none_or(|r| a.at > r.at));
        if !recent {
            let why = if upkeep.reboot_packages.is_empty() {
                String::new()
            } else {
                format!(" ({})", upkeep.reboot_packages.join(", "))
            };
            Entry::node(
                node,
                EventKind::Upkeep,
                Severity::Warn,
                format!("Reboot required{why}"),
            )
            .items(upkeep.reboot_packages.clone())
            .record(server);
        }
    }
}

/// What a server restart must not forget: the nodes' disk errors of the last day.
pub fn load(server: &Server) -> anyhow::Result<()> {
    let since = facade_core::now().saturating_sub(DISK_TROUBLE_FOR);
    let mut nodes = server.activity.nodes.write();
    for node in server.store.list_nodes()? {
        if let Some(e) = server
            .store
            .last_node_event(&node.name, EventKind::DiskError, since)?
        {
            nodes.entry(node.name).or_default().disk = Some(DiskTrouble {
                at: e.at,
                summary: e.summary,
                event: e.id,
            });
        }
    }
    Ok(())
}

/// Prunes old entries once an hour.
pub async fn run(server: Arc<Server>) {
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tick.tick().await;
        let before = facade_core::now().saturating_sub(KEEP);
        if let Err(e) = server.store.prune_events(before) {
            warn!("pruning activity: {e:#}");
        }
    }
}

// ---------------------------------------------------------------- alerts

/// Problems of its own for the monitor: pending system upkeep, disk errors,
/// certificates about to expire.
pub fn conditions(server: &Server, out: &mut HashMap<String, String>) {
    let now = facade_core::now();
    {
        let nodes = server.activity.nodes.read();
        for (name, track) in nodes.iter() {
            if let Some(u) = &track.upkeep
                && u.needs_attention()
                && let Some(badge) = u.badge()
            {
                out.insert(
                    format!("node:{name}:upkeep"),
                    format!("node {name}: {badge}"),
                );
            }
            if let Some(d) = &track.disk
                && now.saturating_sub(d.at) < DISK_TROUBLE_FOR
            {
                out.insert(
                    format!("node:{name}:disk-errors"),
                    format!(
                        "node {name}: {} (check its disk)",
                        lowercase_first(&d.summary)
                    ),
                );
            }
        }
    }
    let wanted: Vec<String> = server
        .desired_domains()
        .iter()
        .map(|d| super::acme::covering_name(server, &d.domain))
        .collect();
    let errors = server.domain_errors.read();
    for cert in server.store.list_certs().unwrap_or_default() {
        if !wanted.contains(&cert.domain) || cert.not_after > now + CERT_ALERT_WITHIN {
            continue;
        }
        let when = if cert.not_after <= now {
            "has expired".to_string()
        } else {
            let days = (cert.not_after - now) / 86400;
            match days {
                0 => "expires today".to_string(),
                1 => "expires tomorrow".to_string(),
                d => format!("expires in {d} days"),
            }
        };
        let why = errors
            .get(&format!("cert:{}", cert.domain))
            .map(|e| format!(" (renewal failed: {e})"))
            .unwrap_or_default();
        out.insert(
            format!("cert:{}:expiring", cert.domain),
            format!("certificate for {} {when}{why}", cert.domain),
        );
    }
}

fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Where an alert's key says it belongs.
fn alert_entry(key: &str, kind: EventKind, severity: Severity, message: &str) -> Entry {
    let mut parts = key.splitn(3, ':');
    let entry = match (parts.next(), parts.next()) {
        (Some("app"), Some(app)) => Entry::app(app, kind, severity, message),
        (Some("node"), Some(node)) => Entry::node(node, kind, severity, message),
        (Some("backup"), Some(what)) => {
            let app = what.split('/').next().unwrap_or(what);
            Entry::app(app, kind, severity, message)
        }
        _ => Entry::global(kind, severity, message),
    };
    entry.details(EventDetails {
        alert: Some(key.to_string()),
        ..Default::default()
    })
}

/// Records alerts that fired and cleared; their entries' ids by alert key.
pub fn alerts_changed(
    server: &Server,
    fired: &[AlertView],
    cleared: &[AlertView],
) -> HashMap<String, i64> {
    let mut ids = HashMap::new();
    for a in fired {
        // Alerts live in memory: after a restart, those still firing fire
        // again. Their entry from before stands.
        if let Ok(Some((id, EventKind::AlertFired))) = server.store.last_alert_event(&a.key) {
            ids.insert(a.key.clone(), id);
            continue;
        }
        let message = capitalize(&a.message);
        // Pending updates are no outage.
        let severity = if a.key.ends_with(":upkeep") {
            Severity::Warn
        } else {
            Severity::Error
        };
        if let Some(id) = alert_entry(&a.key, EventKind::AlertFired, severity, &message)
            .problem()
            .record(server)
        {
            ids.insert(a.key.clone(), id);
        }
    }
    for a in cleared {
        let message = format!("Resolved: {}", a.message);
        if let Some(id) =
            alert_entry(&a.key, EventKind::AlertResolved, Severity::Ok, &message).record(server)
        {
            ids.insert(a.key.clone(), id);
        }
    }
    ids
}

/// A link to an entry on the web UI, when the server has a domain.
pub fn link(server: &Server, id: Option<&i64>) -> Option<String> {
    let id = id?;
    let domain = server.settings.api_domain()?;
    Some(format!("https://{domain}/activity?event={id}"))
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------- deploys

fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// The commit (and its title), else the image, else the revision.
fn what_deployed(server: &Server, d: &DeploymentView) -> String {
    match (&d.commit, &d.commit_title) {
        (Some(c), Some(t)) => format!("{} · {t}", short(c)),
        (Some(c), None) => short(c).to_string(),
        _ => match server
            .store
            .get_app(&d.app)
            .ok()
            .flatten()
            .map(|a| a.spec.source)
        {
            Some(facade_core::Source::Image { image }) => image,
            _ => format!("revision {}", d.revision),
        },
    }
}

/// Someone asked for a deploy (`previous`: the commit running before). Going
/// back to a commit deployed before is a rollback.
pub fn deploy_requested(
    server: &Server,
    user: &User,
    req: &DeployRequest,
    previous: Option<&str>,
    d: &DeploymentView,
) {
    let rollback = req.commit.is_some()
        && d.commit.is_some()
        && d.commit.as_deref() != previous
        && server
            .store
            .list_deployments(&d.app, 50)
            .unwrap_or_default()
            .iter()
            .any(|old| {
                old.id != d.id
                    && old.status == facade_core::api::DeploymentStatus::Succeeded
                    && old.commit == d.commit
            });
    let (kind, summary) = if rollback {
        (
            EventKind::Rollback,
            format!(
                "Rolled back to {} by {}",
                short(d.commit.as_deref().unwrap_or_default()),
                user.username
            ),
        )
    } else if req.rebuild {
        (
            EventKind::DeployStarted,
            format!("Rebuild by {}: {}", user.username, what_deployed(server, d)),
        )
    } else {
        (
            EventKind::DeployStarted,
            format!("Deploy by {}: {}", user.username, what_deployed(server, d)),
        )
    };
    Entry::app(&d.app, kind, Severity::Info, summary)
        .by(&user.username)
        .details(EventDetails {
            deployment: Some(d.id),
            commit: d.commit.clone(),
            ..Default::default()
        })
        .record(server);
}

/// Finishes a deployment as the agent reports it, recording how it went the
/// first time only (a cancel by hand has its own entry, with who did it).
pub fn finish_deployment(
    server: &Server,
    id: Uuid,
    error: Option<&str>,
    cancelled: bool,
) -> anyhow::Result<()> {
    let before = server.store.get_deployment(id)?;
    server.store.finish_deployment(id, error, cancelled)?;
    let Some(before) = before.filter(|d| !d.status.is_finished()) else {
        return Ok(());
    };
    if cancelled {
        return Ok(());
    }
    let d = server.store.get_deployment(id)?.unwrap_or(before);
    let details = EventDetails {
        deployment: Some(id),
        commit: d.commit.clone(),
        ..Default::default()
    };
    let entry = match error {
        None => {
            let took = match (d.started_at, d.finished_at) {
                (Some(s), Some(f)) => format!(" in {}s", f.saturating_sub(s)),
                _ => String::new(),
            };
            Entry::app(
                &d.app,
                EventKind::Deployed,
                Severity::Ok,
                format!("Deployed {}{took}", what_deployed(server, &d)),
            )
        }
        Some(e) => Entry::app(
            &d.app,
            EventKind::DeployFailed,
            Severity::Error,
            format!("Deploy failed: {}", one_line(e, 200)),
        )
        .problem(),
    };
    entry.details(details).record(server);
    Ok(())
}

/// A noticed change and the deploy it started (`target`: an app or a stack).
pub fn auto_deploy(server: &Server, target: &str, what: &str, via: &str, deployment: Uuid) {
    Entry::app(
        target,
        EventKind::AutoDeploy,
        Severity::Info,
        format!("{} noticed ({via}) → deploying", capitalize(what)),
    )
    .by("auto-deploy")
    .details(EventDetails {
        deployment: Some(deployment),
        ..Default::default()
    })
    .record(server);
}

fn one_line(s: &str, max: usize) -> String {
    let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        line
    } else {
        format!("{}…", line.chars().take(max).collect::<String>())
    }
}

// ---------------------------------------------------------------- who did what

/// `summary` done to `app` by `user`.
pub fn audit(server: &Server, user: &User, app: &str, kind: EventKind, summary: String) {
    Entry::app(app, kind, Severity::Info, summary)
        .by(&user.username)
        .record(server);
}

/// What changed between two versions of an app's settings, in words. Domains
/// get entries of their own; starting and stopping too.
pub fn spec_changed(server: &Server, user: &User, old: &AppSpec, new: &AppSpec) {
    let who = &user.username;
    let app = &new.name;
    let added: Vec<String> = new
        .domains
        .iter()
        .chain(&new.redirects)
        .filter(|d| !old.domains.contains(d) && !old.redirects.contains(d))
        .cloned()
        .collect();
    let removed: Vec<String> = old
        .domains
        .iter()
        .chain(&old.redirects)
        .filter(|d| !new.domains.contains(d) && !new.redirects.contains(d))
        .cloned()
        .collect();
    for (list, verb) in [(&added, "added"), (&removed, "removed")] {
        if !list.is_empty() {
            let s = if list.len() == 1 { "" } else { "s" };
            Entry::app(
                app,
                EventKind::Domain,
                Severity::Info,
                format!("Domain{s} {verb} by {who}: {}", list.join(", ")),
            )
            .by(who)
            .items(list.clone())
            .record(server);
        }
    }
    let changed = setting_names(old, new);
    if !changed.is_empty() {
        Entry::app(
            app,
            EventKind::Settings,
            Severity::Info,
            format!("Settings changed by {who}: {}", changed.join(", ")),
        )
        .by(who)
        .items(changed)
        .record(server);
    }
}

/// The settings that differ, by name (domains, redirects and the on/off
/// switch left out: they have entries of their own).
fn setting_names(old: &AppSpec, new: &AppSpec) -> Vec<String> {
    let (Ok(serde_json::Value::Object(a)), Ok(serde_json::Value::Object(b))) =
        (serde_json::to_value(old), serde_json::to_value(new))
    else {
        return vec![];
    };
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    let mut out = Vec::new();
    for key in keys {
        if matches!(key.as_str(), "domains" | "redirects" | "enabled" | "name") {
            continue;
        }
        let (x, y) = (a.get(key), b.get(key));
        if x == y {
            continue;
        }
        if key == "limits" {
            let empty = serde_json::Map::new();
            let (x, y) = (
                x.and_then(|v| v.as_object()).unwrap_or(&empty),
                y.and_then(|v| v.as_object()).unwrap_or(&empty),
            );
            let mut sub: Vec<&String> = x.keys().chain(y.keys()).collect();
            sub.sort();
            sub.dedup();
            for k in sub {
                if x.get(k) != y.get(k) {
                    out.push(setting_label(&format!("limits.{k}")));
                }
            }
            continue;
        }
        out.push(setting_label(key));
    }
    out
}

fn setting_label(key: &str) -> String {
    match key {
        "node" => "node",
        "source" => "source",
        "http_port" => "port",
        "ports" => "published ports",
        "tls_ports" => "TLS ports",
        "host_network" => "host network",
        "limits.memory_mb" => "memory limit",
        "limits.cpus" => "CPU limit",
        "limits.pids" => "process limit",
        "delegate_cgroups" => "cgroup delegation",
        "command" => "command",
        "release_command" => "release command",
        "volumes" => "volumes",
        "links" => "links",
        "healthcheck" => "health check",
        "stop_timeout_secs" => "stop timeout",
        "stop_signal" => "stop signal",
        "backup" => "backup commands",
        "deploy" => "deploy strategy",
        other => return other.replace(['_', '.'], " "),
    }
    .to_string()
}

/// Variables set and removed: their names only, never their values.
pub fn env_changed(server: &Server, user: &User, app: &str, set: &[String], unset: &[String]) {
    let mut parts = Vec::new();
    if !set.is_empty() {
        parts.push(format!("set {}", set.join(", ")));
    }
    if !unset.is_empty() {
        parts.push(format!("removed {}", unset.join(", ")));
    }
    if parts.is_empty() {
        return;
    }
    Entry::app(
        app,
        EventKind::Env,
        Severity::Info,
        format!(
            "Variables changed by {}: {}",
            user.username,
            parts.join("; ")
        ),
    )
    .by(&user.username)
    .items(set.iter().chain(unset).cloned().collect())
    .record(server);
}

// ---------------------------------------------------------------- certificates and DNS

/// The app a domain routes to, for entries about it.
fn domain_entry(
    server: &Server,
    domain: &str,
    kind: EventKind,
    severity: Severity,
    summary: String,
) -> Entry {
    let app = server.routes.read().domains.get(domain).cloned();
    match app {
        Some(app) => Entry::app(&app, kind, severity, summary),
        None => Entry::global(kind, severity, summary),
    }
}

/// A certificate was issued; `renewed` when one existed before.
pub fn cert_issued(server: &Server, name: &str, renewed: bool) {
    let until = server
        .store
        .list_certs()
        .unwrap_or_default()
        .into_iter()
        .find(|c| c.domain == name)
        .map(|c| format!(", valid until {}", date(c.not_after)))
        .unwrap_or_default();
    let verb = if renewed { "renewed" } else { "issued" };
    domain_entry(
        server,
        name,
        EventKind::CertIssued,
        Severity::Ok,
        format!("Certificate for {name} {verb}{until}"),
    )
    .record(server);
}

/// A certificate request failed (`before`: the previous failure, so a retry
/// failing the same way adds no entry).
pub fn cert_failed(server: &Server, name: &str, error: &str, before: Option<&str>) {
    if before == Some(error) {
        return;
    }
    domain_entry(
        server,
        name,
        EventKind::CertFailed,
        Severity::Error,
        format!("Certificate for {name} failed: {}", one_line(error, 200)),
    )
    .problem()
    .record(server);
}

/// A DNS record facade manages was set (`old`: the address before) or removed.
pub fn dns_record(server: &Server, domain: &str, ip: Option<&str>, old: Option<&str>) {
    let summary = match (ip, old) {
        (Some(ip), Some(old)) => format!("DNS record for {domain} changed: {old} → {ip}"),
        (Some(ip), None) => format!("DNS record for {domain} created: {ip}"),
        (None, _) => format!("DNS record for {domain} removed"),
    };
    domain_entry(
        server,
        domain,
        EventKind::DnsRecord,
        Severity::Info,
        summary,
    )
    .record(server);
}

/// `YYYY-MM-DD` (UTC).
fn date(at: u64) -> String {
    let days = (at / 86_400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

// ---------------------------------------------------------------- API

#[derive(Deserialize)]
pub struct ListQuery {
    app: Option<String>,
    node: Option<String>,
    /// A category (`restarts`) or a kind (`oom_kill`).
    kind: Option<String>,
    #[serde(default)]
    problems: bool,
    before: Option<i64>,
    limit: Option<u32>,
}

type S = State<Arc<Server>>;

/// `GET /api/events`: members see the entries of the apps they can see;
/// node and global entries are for admins.
pub async fn list(
    State(server): S,
    user: User,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<EventsPage>> {
    let mut query = EventQuery {
        app: q.app.filter(|a| !a.is_empty()),
        node: q.node.filter(|n| !n.is_empty()),
        problems: q.problems,
        before: q.before,
        limit: q.limit.unwrap_or(50),
        ..Default::default()
    };
    if let Some(kind) = q.kind.as_deref().filter(|k| !k.is_empty()) {
        match Category::parse(kind) {
            Some(c) => query.category = Some(c),
            None => match EventKind::parse(kind) {
                EventKind::Other => {
                    return Err(super::api::bad_request(format!(
                        "no kind of entry named {kind}"
                    )));
                }
                k => query.kind = Some(k),
            },
        }
    }
    if !user.is_admin() {
        if let Some(app) = &query.app
            && !can_see_app(&server, &user, app)
        {
            return Err(not_found(format!("no app named {app}")));
        }
        query.apps = Some(
            visible_apps(&server, &user)?
                .into_iter()
                .map(|a| a.spec.name)
                .collect(),
        );
    }
    Ok(Json(server.store.query_events(&query)?))
}

/// `GET /api/events/{id}`, log lines included.
pub async fn get(State(server): S, user: User, Path(id): Path<i64>) -> ApiResult<Json<EventView>> {
    let event = server
        .store
        .get_event(id)?
        .filter(|e| {
            user.is_admin()
                || e.app
                    .as_deref()
                    .is_some_and(|app| can_see_app(&server, &user, app))
        })
        .ok_or_else(|| not_found("no such entry"))?;
    Ok(Json(event))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: serde_json::Value) -> AppSpec {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn settings_changes_are_named() {
        let old = spec(serde_json::json!({
            "name": "blog", "node": "main", "source": {"type": "image", "image": "nginx"},
            "domains": ["a.example.com"], "limits": {"memory_mb": 256}
        }));
        let mut new = old.clone();
        new.limits.memory_mb = Some(512);
        new.limits.cpus = Some(1.0);
        new.command = vec!["./run".into()];
        new.domains.push("b.example.com".into());
        new.enabled = false;
        assert_eq!(
            setting_names(&old, &new),
            vec!["command", "CPU limit", "memory limit"]
        );
        assert!(setting_names(&old, &old).is_empty());
    }

    #[test]
    fn alert_keys_find_their_app_or_node() {
        let e = alert_entry("app:blog:down", EventKind::AlertFired, Severity::Error, "x");
        assert_eq!((e.0.scope, e.0.app.as_deref()), (Scope::App, Some("blog")));
        let e = alert_entry(
            "backup:blog/data",
            EventKind::AlertFired,
            Severity::Error,
            "x",
        );
        assert_eq!(e.0.app.as_deref(), Some("blog"));
        let e = alert_entry(
            "node:eu-2:disk",
            EventKind::AlertFired,
            Severity::Error,
            "x",
        );
        assert_eq!(
            (e.0.scope, e.0.node.as_deref()),
            (Scope::Node, Some("eu-2"))
        );
        let e = alert_entry(
            "cert:*.example.com:expiring",
            EventKind::AlertFired,
            Severity::Error,
            "x",
        );
        assert_eq!(e.0.scope, Scope::Global);
        assert_eq!(
            e.0.details.alert.as_deref(),
            Some("cert:*.example.com:expiring")
        );
    }

    #[test]
    fn dates() {
        assert_eq!(date(1_792_454_400), "2026-10-20");
    }
}
