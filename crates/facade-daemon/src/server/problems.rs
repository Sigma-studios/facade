//! Problems: everything wrong right now, for the Overview and `facade problems`.
//!
//! The firing alerts, plus standing conditions no alert covers (or not yet): a
//! DNS record missing or pointing elsewhere, a certificate missing, failing to
//! renew or expiring within 14 days, a failed deploy while the previous version
//! keeps running, a deployment stuck, a failing health check, auto-deploy
//! unable to check its source, a volume nearly full, alerts not reaching
//! Discord. A problem lasts as long as its condition: nothing is kept but when
//! each was first seen (for those that do not say themselves), so a fixed one
//! is gone at the next look. History is the alerts' and the Activity's.
//!
//! One cause makes one row: a site unreachable because its DNS record is
//! wrong is the DNS row, made critical; one unreachable because its node is
//! offline is the node's row; an app that is down does not also answer errors
//! or fail its health check.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use facade_core::api::{
    AlertView, DeploymentStatus, DeploymentView, DomainStatus, Problem, ProblemSeverity,
    StackDeploymentView, main_domain,
};
use facade_core::events::Upkeep;
use facade_core::proto::AppState;
use parking_lot::Mutex;
use tracing::warn;

use super::Server;
use super::api::User;
use super::users::can_see_app;
use super::weekly::{bytes, minutes};

const DAY: u64 = 86_400;
/// Certificates expiring this soon are a warning (the alert starts at 7 days,
/// and makes it critical).
const CERT_WARN_WITHIN: u64 = 14 * DAY;
const CERT_CRITICAL_WITHIN: u64 = 7 * DAY;
/// A domain still without a certificate this long after it was first seen,
/// with nothing failing: certificates usually take a minute.
const CERT_PENDING_FOR: u64 = 30 * 60;
/// A record facade sets itself takes a few minutes to show in DNS.
const DNS_SPREADS_IN: u64 = 15 * 60;
/// Builds are stopped after an hour, so a deployment running longer is stuck;
/// one waiting in the queue this long is behind something stuck.
const STUCK_RUNNING: u64 = 75 * 60;
const STUCK_QUEUED: u64 = 2 * 3600;
/// Volumes with a size this full (percent).
const VOLUME_FULL: u64 = 90;
/// Every open page polls the list: one computation serves them for this long.
const CACHE_FOR: Duration = Duration::from_secs(5);
/// Computed this often even with nobody looking, so first-seen times hold.
const REFRESH_EVERY: Duration = Duration::from_secs(60);
const KV_SEEN: &str = "problems.seen";
/// A problem gone this long is forgotten; back sooner (a flap, or an alert
/// confirming itself again after a restart), it keeps its first time.
const FORGET_AFTER: u64 = 5 * 60;
/// Hints quote errors: their first line, this long at most.
const HINT_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Seen {
    first: u64,
    last: u64,
}

#[derive(Default)]
pub struct Problems {
    /// When each current problem was first seen. Kept in the database, so an
    /// update (a restart) does not make them all new.
    seen: Mutex<HashMap<String, Seen>>,
    cache: Mutex<Option<(Instant, Vec<Problem>)>>,
}

/// `GET /api/problems`: members see their apps' problems; nodes, stacks and
/// the server's are for admins.
pub async fn list(State(server): State<Arc<Server>>, user: User) -> Json<Vec<Problem>> {
    let all = current(&server).await;
    Json(
        all.into_iter()
            .filter(|p| visible(&server, &user, p))
            .collect(),
    )
}

fn visible(server: &Server, user: &User, problem: &Problem) -> bool {
    if user.is_admin() {
        return true;
    }
    match (&problem.app, &problem.stack) {
        (Some(app), None) => can_see_app(server, user, app),
        _ => false,
    }
}

pub async fn run(server: Arc<Server>) {
    match server.store.kv_get(KV_SEEN) {
        Ok(Some(json)) => {
            let mut seen: HashMap<String, Seen> = serde_json::from_str(&json).unwrap_or_default();
            // Last seen is only saved when the list changes: count them as
            // seen now, so they get the time to show again.
            let now = facade_core::now();
            for s in seen.values_mut() {
                s.last = now;
            }
            *server.problems.seen.lock() = seen;
        }
        Ok(None) => {}
        Err(e) => warn!("reading when problems were first seen: {e:#}"),
    }
    let mut tick = tokio::time::interval(REFRESH_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        current(&server).await;
    }
}

/// Every problem, whoever may see it.
pub async fn current(server: &Server) -> Vec<Problem> {
    if let Some((at, list)) = &*server.problems.cache.lock()
        && at.elapsed() < CACHE_FOR
    {
        return list.clone();
    }
    let facts = gather(server).await;
    let now = facade_core::now();
    let (list, changed) = {
        let mut seen = server.problems.seen.lock();
        let before: HashSet<String> = seen.keys().cloned().collect();
        let list = assemble(&facts, now, &mut seen);
        let after: HashSet<String> = seen.keys().cloned().collect();
        (list, (before != after).then(|| seen.clone()))
    };
    if let Some(seen) = changed
        && let Err(e) = serde_json::to_string(&seen)
            .map_err(anyhow::Error::from)
            .and_then(|json| server.store.kv_put(KV_SEEN, &json))
    {
        warn!("saving when problems were first seen: {e:#}");
    }
    *server.problems.cache.lock() = Some((Instant::now(), list.clone()));
    list
}

// ---------------------------------------------------------------- facts

/// What the problems are made of, read from the server in one go, so that
/// [`assemble`] is a plain function of it.
#[derive(Default)]
struct Facts {
    /// Firing ones only.
    alerts: Vec<AlertView>,
    nodes: Vec<NodeFact>,
    /// Domains of apps meant to run (deployed, not stopped), and the API
    /// domain; with their outside check.
    domains: Vec<DomainStatus>,
    certs: Vec<CertFact>,
    /// The server gets certificates (`[acme] enabled`).
    acme: bool,
    /// Apps meant to run.
    apps: Vec<AppFact>,
    stacks: Vec<StackFact>,
    auto_deploy: Vec<AutoDeployFact>,
    volumes: Vec<VolumeFact>,
    /// Why the last message to Discord failed, if none got through since.
    notify_error: Option<String>,
    /// Why the DNS provider cannot be used.
    dns_error: Option<String>,
}

struct NodeFact {
    name: String,
    /// The server's own node (its agent runs inside `facaded`).
    local: bool,
    /// Disconnected since.
    offline_since: Option<u64>,
    upkeep: Option<Upkeep>,
}

struct CertFact {
    /// `*.example.com` or the domain itself.
    name: String,
    /// The app of all the domains it covers, if they are one app's.
    app: Option<String>,
    not_after: Option<u64>,
    error: Option<String>,
}

struct AppFact {
    name: String,
    stack: Option<String>,
    /// As its node last said; `None` while the node is away.
    state: Option<AppState>,
    /// Its newest deployment.
    latest: Option<DeploymentView>,
    /// A deployment before the newest one succeeded: there is an older
    /// version to keep running when the newest fails. (One whose first deploy
    /// failed and runs since a settings change has none.)
    ran_before: bool,
    /// Its health check fails: since when, and what the last one printed.
    unhealthy: Option<(u64, String)>,
}

struct StackFact {
    name: String,
    latest: Option<StackDeploymentView>,
}

struct AutoDeployFact {
    /// `app` or `stack`.
    kind: &'static str,
    name: String,
    error: String,
}

struct VolumeFact {
    node: String,
    /// Its app's or stack's name.
    owner: String,
    is_stack: bool,
    volume: String,
    size: u64,
    limit: u64,
}

async fn gather(server: &Server) -> Facts {
    let rows = server.store.list_apps().unwrap_or_else(|e| {
        warn!("problems: {e:#}");
        vec![]
    });
    // A stopped app's domains, and one never deployed, serve nothing on purpose.
    let running: HashSet<String> = rows
        .iter()
        .filter(|a| a.revision > 0 && a.spec.enabled)
        .map(|a| a.spec.name.clone())
        .collect();
    let mut domains = super::dns::statuses(server).await;
    domains.retain(|d| d.app.as_ref().is_none_or(|a| running.contains(a)));
    server.reach.annotate(&mut domains);

    let stored = server.store.list_certs().unwrap_or_default();
    let mut certs: Vec<(CertFact, HashSet<Option<String>>)> = Vec::new();
    {
        let errors = server.domain_errors.read();
        for d in &domains {
            let name = super::acme::covering_name(server, &d.domain);
            if let Some((_, apps)) = certs.iter_mut().find(|(c, _)| c.name == name) {
                apps.insert(d.app.clone());
                continue;
            }
            let fact = CertFact {
                not_after: stored
                    .iter()
                    .find(|c| c.domain == name)
                    .map(|c| c.not_after),
                error: errors.get(&format!("cert:{name}")).cloned(),
                app: None,
                name,
            };
            certs.push((fact, HashSet::from([d.app.clone()])));
        }
    }
    let certs = certs
        .into_iter()
        .map(|(mut c, apps)| {
            if apps.len() == 1 {
                c.app = apps.into_iter().next().flatten();
            }
            c
        })
        .collect();

    let nodes = server
        .store
        .list_nodes()
        .unwrap_or_default()
        .into_iter()
        .map(|n| {
            let offline_since = if server.hub.is_connected(&n.name) {
                None
            } else {
                Some(
                    server
                        .live
                        .read()
                        .nodes
                        .get(&n.name)
                        .and_then(|l| l.offline_since)
                        .unwrap_or(0),
                )
            };
            NodeFact {
                local: n.name == server.cfg.local_node,
                upkeep: server.activity.upkeep(&n.name),
                offline_since,
                name: n.name,
            }
        })
        .collect();

    // What the nodes say, read at once and let go.
    let live: HashMap<String, (AppState, Option<bool>)> = {
        let live = server.live.read();
        rows.iter()
            .filter(|a| running.contains(&a.spec.name) && server.hub.is_connected(&a.spec.node))
            .filter_map(|a| {
                let l = live.apps.get(&a.spec.name)?;
                let health = l
                    .stats
                    .as_ref()
                    .and_then(|s| s.resources.health.last())
                    .map(|c| c.ok);
                Some((a.spec.name.clone(), (l.state, health)))
            })
            .collect()
    };
    let mut apps = Vec::new();
    for row in rows.iter().filter(|a| running.contains(&a.spec.name)) {
        let name = &row.spec.name;
        let (state, health) = match live.get(name) {
            Some((state, health)) => (Some(*state), *health),
            None => (None, None),
        };
        let latest = server
            .store
            .list_deployments(name, 5)
            .ok()
            .and_then(|d| d.into_iter().max_by_key(|d| d.created_at));
        let ran_before = latest.as_ref().is_some_and(|latest| {
            server
                .store
                .last_success(name)
                .ok()
                .flatten()
                .is_some_and(|(revision, _)| revision < latest.revision)
        });
        let unhealthy = (health == Some(false))
            .then(|| server.store.health_checks(name).ok())
            .flatten()
            .and_then(|checks| failing_since(&checks));
        apps.push(AppFact {
            name: name.clone(),
            stack: row.spec.stack.as_ref().map(|s| s.stack.clone()),
            state,
            latest,
            ran_before,
            unhealthy,
        });
    }

    let stack_rows = server.store.list_stacks().unwrap_or_default();
    let stacks = stack_rows
        .iter()
        .map(|s| StackFact {
            name: s.spec.name.clone(),
            latest: server
                .store
                .list_stack_deployments(&s.spec.name, 1)
                .ok()
                .and_then(|d| d.into_iter().next()),
        })
        .collect();

    let auto_deploy = server
        .store
        .list_auto_deploy()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let error = row.error?;
            let (kind, name) = match row.target.split_once(':')? {
                // Auto-deploy leaves stopped apps alone.
                ("app", name) if running.contains(name) => ("app", name),
                ("stack", name) => ("stack", name),
                _ => return None,
            };
            Some(AutoDeployFact {
                kind,
                name: name.to_string(),
                error,
            })
        })
        .collect();

    let volumes = {
        let live = server.live.read();
        let mut out = Vec::new();
        for (node, l) in &live.nodes {
            let Some(storage) = &l.storage else { continue };
            for v in &storage.volumes {
                let (Some(size), Some(limit)) = (v.size, v.limit) else {
                    continue;
                };
                if limit == 0 || size * 100 / limit < VOLUME_FULL {
                    continue;
                }
                // Volumes of apps and stacks gone are orphans, listed as such.
                let is_app = rows.iter().any(|a| a.spec.name == v.app);
                let is_stack = !is_app && stack_rows.iter().any(|s| s.spec.name == v.app);
                if is_app || is_stack {
                    out.push(VolumeFact {
                        node: node.clone(),
                        owner: v.app.clone(),
                        is_stack,
                        volume: v.volume.clone(),
                        size,
                        limit,
                    });
                }
            }
        }
        out
    };

    let notify = server.notifier.settings();
    let notify_error = notify
        .last_error
        .filter(|e| notify.discord_webhook.is_some() && e.at >= notify.last_sent.unwrap_or(0))
        .map(|e| e.error);
    let dns = server.dns.settings();

    Facts {
        alerts: server
            .alerts
            .lock()
            .views()
            .into_iter()
            .filter(|a| a.resolved_at.is_none())
            .collect(),
        nodes,
        domains,
        certs,
        acme: server.cfg.acme.enabled,
        apps,
        stacks,
        auto_deploy,
        volumes,
        notify_error,
        dns_error: dns.error.filter(|_| dns.provider.is_some()),
    }
}

/// When the health checks started failing (the first of the failures in a row
/// they end with), and what the last one printed.
fn failing_since(checks: &[facade_core::resources::HealthCheck]) -> Option<(u64, String)> {
    let streak = facade_core::resources::failing_streak(checks);
    let last = checks.last().filter(|_| streak > 0)?;
    let first = &checks[checks.len() - streak];
    Some((first.at_ms / 1000, last.output.clone()))
}

// ---------------------------------------------------------------- assembly

/// A problem being put together.
struct Found {
    problem: Problem,
    /// Since when it is known to be wrong, when it says; otherwise since it
    /// was first seen.
    known: Option<u64>,
    /// Listed only once it has lasted this long.
    after: u64,
}

impl Found {
    fn new(key: impl Into<String>, severity: ProblemSeverity, message: impl Into<String>) -> Self {
        Self {
            problem: Problem {
                key: key.into(),
                severity,
                message: message.into(),
                app: None,
                stack: None,
                node: None,
                domain: None,
                since: None,
                fix: "/".into(),
                fix_label: "Overview".into(),
                hint: None,
            },
            known: None,
            after: 0,
        }
    }

    fn app(mut self, app: impl Into<String>) -> Self {
        self.problem.app = Some(app.into());
        self
    }

    fn node(mut self, node: impl Into<String>) -> Self {
        self.problem.node = Some(node.into());
        self
    }

    fn domain(mut self, domain: impl Into<String>) -> Self {
        self.problem.domain = Some(domain.into());
        self
    }

    fn stack(mut self, stack: impl Into<String>) -> Self {
        self.problem.stack = Some(stack.into());
        self
    }

    fn fix(mut self, href: impl Into<String>, label: &str) -> Self {
        self.problem.fix = href.into();
        self.problem.fix_label = label.into();
        self
    }

    fn hint(mut self, hint: impl Into<String>) -> Self {
        let hint = hint.into();
        self.problem.hint = (!hint.trim().is_empty()).then_some(hint);
        self
    }

    fn since(mut self, at: Option<u64>) -> Self {
        self.known = at;
        self
    }

    fn after(mut self, secs: u64) -> Self {
        self.after = secs;
        self
    }

    /// Takes in another symptom of the same cause: the worse severity, the
    /// earlier start.
    fn absorb(&mut self, severity: ProblemSeverity, since: Option<u64>) {
        self.problem.severity = self.problem.severity.min(severity);
        self.known = match (self.known, since) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        // It is plainly wrong now, whatever the grace period.
        self.after = 0;
    }
}

/// The first line of an error, short enough for a hint.
fn short(error: &str) -> String {
    let line = error
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    match line.char_indices().nth(HINT_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

fn domains_href(domain: &str) -> String {
    format!(
        "/domains?domain={}",
        main_domain(domain.trim_start_matches("*."))
    )
}

/// Whether the certificate `cert` (`*.example.com` or a name) covers `domain`.
fn covers(cert: &str, domain: &str) -> bool {
    match cert.strip_prefix("*.") {
        Some(zone) => {
            domain == zone
                || domain
                    .strip_suffix(zone)
                    .and_then(|label| label.strip_suffix('.'))
                    .is_some_and(|label| !label.is_empty() && !label.contains('.'))
        }
        None => cert == domain,
    }
}

fn expires(not_after: u64, now: u64) -> String {
    if not_after <= now {
        return "has expired".into();
    }
    match (not_after - now) / DAY {
        0 => "expires today".into(),
        1 => "expires tomorrow".into(),
        d => format!("expires in {d} days"),
    }
}

/// `for 3 h`, `for 2 d`.
fn lasted(secs: u64) -> String {
    format!("for {}", minutes(secs / 60))
}

/// Everything wrong in `facts`, one row per cause, critical first then oldest.
/// `seen` holds when each was first seen; it is updated to the current ones.
fn assemble(facts: &Facts, now: u64, seen: &mut HashMap<String, Seen>) -> Vec<Problem> {
    use ProblemSeverity::{Critical, Warning};
    let mut out: Vec<Found> = Vec::new();
    let node_fact = |name: &str| facts.nodes.iter().find(|n| n.name == name);

    // Alerts first: they say what the other rows defer to.
    let mut offline: HashSet<&str> = HashSet::new();
    let mut unbound: HashSet<&str> = HashSet::new();
    let mut down: HashSet<&str> = HashSet::new();
    let mut unreachable: Vec<&AlertView> = Vec::new();
    let mut app_alerts: Vec<(&AlertView, &str, &str)> = Vec::new();
    for alert in &facts.alerts {
        let (scope, rest) = alert.key.split_once(':').unwrap_or((&alert.key, ""));
        let (name, kind) = rest.rsplit_once(':').unwrap_or((rest, ""));
        match scope {
            "node" if rest.contains(":vuln:") => {
                let (name, cve) = rest.split_once(":vuln:").unwrap_or((rest, ""));
                let severity = if alert.message.contains("(critical)") {
                    Critical
                } else {
                    Warning
                };
                out.push(
                    Found::new(&alert.key, severity, &alert.message)
                        .node(name)
                        .since(Some(alert.since))
                        .fix(format!("/nodes/{name}?tab=vulnerabilities"), "Vulnerabilities")
                        .hint(format!(
                            "no fix yet: mitigate it, then facade vulns ack {cve} --node {name} --note \"why\""
                        )),
                );
            }
            "node" => {
                match kind {
                    "offline" => offline.insert(name),
                    "ingress" => unbound.insert(name),
                    _ => false,
                };
                out.push(node_alert(alert, name, kind, node_fact(name)));
            }
            "app" => {
                if kind == "down" {
                    down.insert(name);
                }
                app_alerts.push((alert, name, kind));
            }
            "domain" => unreachable.push(alert),
            // Made from the certificates themselves below, from 14 days on.
            "cert" => {}
            "backup" => {
                let f = Found::new(&alert.key, Warning, &alert.message).since(Some(alert.since));
                out.push(match rest.split_once('/') {
                    Some((app, _)) => f
                        .app(app)
                        .fix(format!("/apps/{app}?tab=volumes"), "Volumes")
                        .hint(format!("retry: facade backup run {app}")),
                    // The server's own backup, or the backup store.
                    None => f
                        .fix("/settings/general", "Settings")
                        .hint("retry: facade backup run"),
                });
            }
            _ => out.push(
                Found::new(&alert.key, Warning, &alert.message)
                    .since(Some(alert.since))
                    .fix("/settings/alerts", "Alerts"),
            ),
        }
    }
    for (alert, app, kind) in app_alerts {
        let f = Found::new(&alert.key, Critical, &alert.message)
            .since(Some(alert.since))
            .app(app);
        let base = format!("/apps/{app}");
        out.push(match kind {
            "down" if alert.message.contains(" failed: ") => {
                f.fix(format!("{base}?tab=deployments"), "Deployments")
            }
            "down" => f
                .fix(format!("{base}?tab=activity"), "Activity")
                .hint("its Activity says why it stopped, with its last lines"),
            // A down app's visitors get errors from the entry point: same cause.
            "errors" if down.contains(app) => continue,
            "errors" => f.fix(base, "Failed requests"),
            "memory" => {
                let mut f = f.fix(format!("{base}?tab=settings"), "Settings");
                f.problem.severity = Warning;
                f.hint("raise its memory limit, or find what grows")
            }
            "cpu" => {
                let mut f = f.fix(format!("{base}?tab=settings"), "Settings");
                f.problem.severity = Warning;
                f.hint("raise its CPU limit")
            }
            "written" => {
                let mut f = f.fix(format!("{base}?tab=settings"), "Settings");
                f.problem.severity = Warning;
                f.hint("give it a volume where it writes")
            }
            _ => {
                let mut f = f.fix(base, "App");
                f.problem.severity = Warning;
                f
            }
        });
    }

    // DNS records, as the server's resolver sees them.
    let mut dns_rows: HashMap<&str, usize> = HashMap::new();
    for d in facts.domains.iter().filter(|d| !d.dns_ok) {
        let reach = d.reach.as_ref();
        // It answers visitors: a proxy in front (a CDN) resolves elsewhere on
        // purpose, and nothing needs fixing.
        if !d.resolved.is_empty() && reach.is_some_and(|r| r.ok) {
            continue;
        }
        let domain = &d.domain;
        let mut f = Found::new(format!("dns:{domain}"), Warning, "")
            .domain(domain)
            .node(&d.node)
            .since(reach.and_then(|r| r.down_since))
            .fix(domains_href(domain), "Domains");
        if let Some(app) = &d.app {
            f = f.app(app);
        }
        f = match &d.expected_ip {
            None => {
                let node = &d.node;
                let mut f = f
                    .fix(format!("/nodes/{node}"), "Node")
                    .hint(format!("set it: facade node ip {node} <address>"));
                f.problem.message =
                    format!("node {node} has no public IP yet: {domain} cannot point at it");
                f
            }
            Some(_) if d.managed => {
                let mut f = match &d.dns_error {
                    Some(e) => f.hint(format!("DNS provider: {}", short(e))),
                    // Records take a few minutes to show.
                    None => f
                        .after(DNS_SPREADS_IN)
                        .hint("facade set its record: check it at the DNS provider"),
                };
                f.problem.message = format!("{domain} does not point at its node");
                f
            }
            Some(ip) if d.resolved.is_empty() => {
                let mut f = f.hint(format!("create an A record for {domain} pointing to {ip}"));
                f.problem.message = format!("{domain} has no DNS record");
                f
            }
            Some(ip) => {
                let mut f = f.hint(format!("change its A record to {ip}"));
                f.problem.message = format!(
                    "{domain} points to {}, not to its node",
                    d.resolved.join(", ")
                );
                f
            }
        };
        dns_rows.insert(domain, out.len());
        out.push(f);
    }

    // Certificates.
    let mut cert_rows: Vec<(&str, usize)> = Vec::new();
    for c in &facts.certs {
        let name = &c.name;
        // A certificate for one name needs the name to point at its node: with
        // its DNS wrong, that is the cause.
        let dns_row = (!name.starts_with("*."))
            .then(|| dns_rows.get(name.as_str()))
            .flatten();
        let mut f = match c.not_after {
            Some(t) if t <= now + CERT_WARN_WITHIN => {
                let severity = if t <= now + CERT_CRITICAL_WITHIN {
                    Critical
                } else {
                    Warning
                };
                let message = format!("certificate for {name} {}", expires(t, now));
                if let Some(&i) = dns_row {
                    let row = &mut out[i];
                    row.absorb(severity, None);
                    row.problem.message.push_str(&format!("; its {message}"));
                    continue;
                }
                let f = Found::new(format!("cert:{name}:expiring"), severity, message)
                    .since(Some(t.saturating_sub(CERT_WARN_WITHIN)));
                match &c.error {
                    Some(e) => f.hint(format!("renewal failing: {}", short(e))),
                    None => f,
                }
            }
            _ if !facts.acme || dns_row.is_some() => continue,
            Some(_) => {
                let Some(e) = &c.error else { continue };
                Found::new(
                    format!("cert:{name}:renewal"),
                    Warning,
                    format!("renewal of the certificate for {name} is failing"),
                )
                .hint(short(e))
            }
            None => {
                let f = Found::new(
                    format!("cert:{name}:missing"),
                    Warning,
                    format!("no certificate for {name} yet"),
                );
                match &c.error {
                    Some(e) => f.hint(format!("failing: {}", short(e))),
                    None => f.after(CERT_PENDING_FOR),
                }
            }
        };
        f = f.domain(name).fix(domains_href(name), "Domains");
        if let Some(app) = &c.app {
            f = f.app(app);
        }
        cert_rows.push((name, out.len()));
        out.push(f);
    }

    // Sites unreachable from outside, unless something above says why.
    for alert in unreachable {
        let domain = alert
            .key
            .strip_prefix("domain:")
            .and_then(|k| k.strip_suffix(":unreachable"))
            .unwrap_or(&alert.key);
        let status = facts.domains.iter().find(|d| d.domain == domain);
        let node = status.map(|s| s.node.as_str());
        if node.is_some_and(|n| offline.contains(n) || unbound.contains(n)) {
            continue;
        }
        let since = status
            .and_then(|s| s.reach.as_ref())
            .and_then(|r| r.down_since)
            .or(Some(alert.since));
        if let Some(&i) = dns_rows.get(domain) {
            out[i].absorb(Critical, since);
            continue;
        }
        if let Some(&(_, i)) = cert_rows.iter().find(|(c, _)| covers(c, domain)) {
            out[i].absorb(Critical, since);
            continue;
        }
        let mut f = Found::new(&alert.key, Critical, &alert.message)
            .domain(domain)
            .since(since)
            .fix(domains_href(domain), "Domains");
        if let Some(s) = status {
            f = f.node(&s.node);
            if let Some(app) = &s.app {
                f = f.app(app);
            }
            let error = s
                .reach
                .as_ref()
                .and_then(|r| r.error.as_deref())
                .unwrap_or("");
            if error == "timed out" || error == "connection refused" {
                f = f.hint(format!("ports 80 and 443 of node {} must be open", s.node));
            }
        }
        out.push(f);
    }

    for a in &facts.apps {
        let name = &a.name;
        let is_down = down.contains(name.as_str());
        let running = a.state == Some(AppState::Running) && !is_down;
        let deployments = format!("/apps/{name}?tab=deployments");
        if let Some(d) = &a.latest {
            match d.status {
                // A stack's services deploy together: the stack's row says it.
                DeploymentStatus::Failed if running && a.ran_before && a.stack.is_none() => {
                    let f = Found::new(
                        format!("deploy:{name}"),
                        Warning,
                        format!(
                            "latest deploy of {name} failed; the previous version keeps running"
                        ),
                    )
                    .app(name)
                    .since(d.finished_at.or(Some(d.created_at)))
                    .fix(format!("{deployments}&deployment={}", d.id), "Deployment");
                    out.push(match &d.error {
                        Some(e) => f.hint(short(e)),
                        None => f,
                    });
                }
                DeploymentStatus::Queued if now.saturating_sub(d.created_at) > STUCK_QUEUED => {
                    out.push(stuck(
                        a,
                        d,
                        "has waited in the build queue",
                        d.created_at,
                        now,
                    ));
                }
                DeploymentStatus::Running
                    if d.started_at
                        .is_some_and(|s| now.saturating_sub(s) > STUCK_RUNNING) =>
                {
                    let started = d.started_at.unwrap_or(d.created_at);
                    out.push(stuck(a, d, "has been running", started, now));
                }
                _ => {}
            }
        }
        if let Some((since, output)) = &a.unhealthy
            && running
        {
            out.push(
                Found::new(
                    format!("health:{name}"),
                    Critical,
                    format!("health check of {name} is failing"),
                )
                .app(name)
                .since(Some(*since))
                .fix(format!("/apps/{name}?tab=info"), "Health checks")
                .hint(short(output)),
            );
        }
    }

    for s in &facts.stacks {
        if let Some(d) = s
            .latest
            .as_ref()
            .filter(|d| d.status == DeploymentStatus::Failed)
        {
            let name = &s.name;
            let f = Found::new(
                format!("stack-deploy:{name}"),
                Warning,
                format!("latest deploy of stack {name} failed"),
            )
            .stack(name)
            .since(d.finished_at.or(Some(d.created_at)))
            .fix(format!("/stacks/{name}?tab=deployments"), "Deployments");
            out.push(match &d.error {
                Some(e) => f.hint(short(e)),
                None => f,
            });
        }
    }

    for a in &facts.auto_deploy {
        let name = &a.name;
        let f = Found::new(
            format!("auto-deploy:{}:{name}", a.kind),
            Warning,
            format!("auto-deploy of {name} cannot check its source"),
        )
        .hint(short(&a.error));
        out.push(match a.kind {
            "stack" => f
                .stack(name)
                .fix(format!("/stacks/{name}?tab=settings"), "Settings"),
            _ => f
                .app(name)
                .fix(format!("/apps/{name}?tab=settings"), "Settings"),
        });
    }

    for v in &facts.volumes {
        let share = v.size * 100 / v.limit;
        let owner = &v.owner;
        let f = Found::new(
            format!("volume:{}:{owner}/{}", v.node, v.volume),
            if share >= 100 { Critical } else { Warning },
            format!(
                "volume {} of {owner} is {share} % full ({} of {})",
                v.volume,
                bytes(v.size),
                bytes(v.limit)
            ),
        )
        .node(&v.node)
        .hint("give it a bigger size (sizes grow, never shrink)");
        out.push(if v.is_stack {
            f.stack(owner)
                .fix(format!("/stacks/{owner}?tab=settings"), "Settings")
        } else {
            f.app(owner)
                .fix(format!("/apps/{owner}?tab=volumes"), "Volumes")
        });
    }

    if let Some(e) = &facts.notify_error {
        out.push(
            Found::new("notify", Warning, "alerts are not reaching Discord")
                .fix("/settings/alerts", "Alert settings")
                .hint(short(e)),
        );
    }
    if let Some(e) = &facts.dns_error {
        out.push(
            Found::new("dns-provider", Warning, "the DNS provider cannot be used")
                .fix("/domains", "Domains")
                .hint(short(e)),
        );
    }

    for f in &out {
        // What it says of itself counts as seen, for when it says later (an
        // alert confirmed again after a restart).
        let s = seen.entry(f.problem.key.clone()).or_insert(Seen {
            first: f.known.unwrap_or(now).min(now),
            last: now,
        });
        s.last = now;
    }
    seen.retain(|_, s| now.saturating_sub(s.last) <= FORGET_AFTER);
    let mut list: Vec<Problem> = out
        .into_iter()
        .filter_map(|mut f| {
            let first = seen.get(&f.problem.key).map_or(now, |s| s.first);
            let since = f.known.map_or(first, |known| known.min(first));
            if now.saturating_sub(since) < f.after {
                return None;
            }
            f.problem.since = Some(since);
            Some(f.problem)
        })
        .collect();
    list.sort_by(|a, b| (a.severity, a.since, &a.key).cmp(&(b.severity, b.since, &b.key)));
    list
}

fn node_alert(alert: &AlertView, name: &str, kind: &str, node: Option<&NodeFact>) -> Found {
    use ProblemSeverity::{Critical, Warning};
    let f = Found::new(&alert.key, Critical, &alert.message)
        .node(name)
        .since(Some(alert.since))
        .fix(format!("/nodes/{name}"), "Node");
    match kind {
        "offline" => {
            let f = f.since(
                node.and_then(|n| n.offline_since)
                    .filter(|t| *t > 0)
                    .or(Some(alert.since)),
            );
            if node.is_some_and(|n| n.local) {
                f.hint("its agent runs in the server: journalctl -u facaded")
            } else {
                f.hint("check its agent: sudo systemctl status facaded-agent")
            }
        }
        "ingress" => f
            .fix(format!("/nodes/{name}?tab=ports"), "Ports")
            .hint("stop what holds the port: facade takes it within seconds"),
        "disk" => f.hint(format!(
            "free space: Prune on its page, or facade node prune {name} --aggressive"
        )),
        "disk-errors" => f
            .fix(format!("/nodes/{name}?tab=activity"), "Activity")
            .hint("check the disk: sudo dmesg, smartctl -a"),
        _ => {
            let mut f = f;
            f.problem.severity = Warning;
            if kind == "upkeep"
                && let Some(u) = node.and_then(|n| n.upkeep.as_ref())
            {
                let security = u.security() > 0;
                f = match (security, u.reboot_required()) {
                    (true, true) => f.hint("run sudo apt upgrade on the node, then reboot it"),
                    (true, false) => f.hint("run sudo apt upgrade on the node"),
                    (false, true) => f.hint("reboot the node when you can: sudo reboot"),
                    (false, false) => f,
                };
            }
            f
        }
    }
}

fn stuck(app: &AppFact, d: &DeploymentView, what: &str, since: u64, now: u64) -> Found {
    let name = &app.name;
    let f = Found::new(
        format!("stuck:{name}"),
        ProblemSeverity::Warning,
        format!(
            "deploy of {name} {what} {}",
            lasted(now.saturating_sub(since))
        ),
    )
    .app(name)
    .since(Some(since));
    match &app.stack {
        Some(stack) => f
            .stack(stack)
            .fix(format!("/stacks/{stack}?tab=deployments"), "Deployments"),
        None => f
            .fix(
                format!("/apps/{name}?tab=deployments&deployment={}", d.id),
                "Deployment",
            )
            .hint(format!("cancel it: facade cancel {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use facade_core::insights::Reach;

    const NOW: u64 = 1_800_000_000;

    fn alert(key: &str, message: &str, since: u64) -> AlertView {
        AlertView {
            key: key.into(),
            message: message.into(),
            since,
            resolved_at: None,
        }
    }

    fn domain(name: &str, app: &str, resolved: &[&str]) -> DomainStatus {
        DomainStatus {
            domain: name.into(),
            app: Some(app.into()),
            node: "main".into(),
            expected_ip: Some("51.83.42.59".into()),
            resolved: resolved.iter().map(|s| s.to_string()).collect(),
            managed: false,
            dns_ok: resolved.contains(&"51.83.42.59"),
            dns_error: None,
            certificate: None,
            cert_expires: None,
            cert_error: None,
            reach: None,
        }
    }

    fn node(name: &str) -> NodeFact {
        NodeFact {
            name: name.into(),
            local: name == "main",
            offline_since: None,
            upkeep: None,
        }
    }

    fn app(name: &str, state: AppState) -> AppFact {
        AppFact {
            name: name.into(),
            stack: None,
            state: Some(state),
            latest: None,
            ran_before: true,
            unhealthy: None,
        }
    }

    fn deployment(app: &str, status: DeploymentStatus, at: u64) -> DeploymentView {
        DeploymentView {
            id: uuid::Uuid::nil(),
            app: app.into(),
            revision: 3,
            status,
            created_at: at,
            started_at: Some(at),
            finished_at: Some(at + 60),
            error: Some("build failed: exit code 101\nmore".into()),
            commit: None,
            commit_title: None,
            log: String::new(),
        }
    }

    fn keys(list: &[Problem]) -> Vec<&str> {
        list.iter().map(|p| p.key.as_str()).collect()
    }

    fn run(facts: &Facts) -> Vec<Problem> {
        assemble(facts, NOW, &mut HashMap::new())
    }

    #[test]
    fn nothing_wrong_is_nothing_listed() {
        let facts = Facts {
            nodes: vec![node("main")],
            domains: vec![domain("blog.example.com", "blog", &["51.83.42.59"])],
            apps: vec![app("blog", AppState::Running)],
            ..Default::default()
        };
        assert!(run(&facts).is_empty());
    }

    #[test]
    fn alerts_become_problems_with_where_to_fix_them() {
        let facts = Facts {
            alerts: vec![
                alert(
                    "app:blog:down",
                    "app blog is down (crashed or restarting)",
                    NOW - 300,
                ),
                alert("node:eu-2:offline", "node eu-2 is offline", NOW - 100),
                alert(
                    "node:main:upkeep",
                    "node main: 3 updates (1 security)",
                    NOW - 9000,
                ),
                alert(
                    "backup:blog/data",
                    "backup of blog/data failed: disk full",
                    NOW - 50,
                ),
            ],
            nodes: vec![
                node("main"),
                NodeFact {
                    offline_since: Some(NOW - 400),
                    ..node("eu-2")
                },
            ],
            ..Default::default()
        };
        let mut facts = facts;
        facts.nodes[0].upkeep = Some(Upkeep {
            at: NOW,
            updates: facade_core::events::Checked::Known(facade_core::events::Updates::Packages {
                count: 3,
                security: 1,
                names: vec![],
            }),
            reboot: facade_core::events::Checked::Known(Default::default()),
        });
        let list = run(&facts);
        // Critical first, oldest first among them.
        assert_eq!(
            keys(&list),
            [
                "node:eu-2:offline",
                "app:blog:down",
                "node:main:upkeep",
                "backup:blog/data"
            ]
        );
        // The node's own offline time, not the alert's confirmation.
        assert_eq!(list[0].since, Some(NOW - 400));
        assert_eq!(list[0].fix, "/nodes/eu-2");
        assert_eq!(list[1].fix, "/apps/blog?tab=activity");
        assert_eq!(list[1].app.as_deref(), Some("blog"));
        assert_eq!(list[2].severity, ProblemSeverity::Warning);
        assert_eq!(
            list[2].hint.as_deref(),
            Some("run sudo apt upgrade on the node")
        );
        assert_eq!(list[3].fix, "/apps/blog?tab=volumes");
    }

    #[test]
    fn a_missing_record_says_what_to_create() {
        let facts = Facts {
            domains: vec![domain("shop.example.co.uk", "shop", &[])],
            apps: vec![app("shop", AppState::Running)],
            ..Default::default()
        };
        let list = run(&facts);
        assert_eq!(keys(&list), ["dns:shop.example.co.uk"]);
        let p = &list[0];
        assert_eq!(p.message, "shop.example.co.uk has no DNS record");
        assert_eq!(
            p.hint.as_deref(),
            Some("create an A record for shop.example.co.uk pointing to 51.83.42.59")
        );
        assert_eq!(p.fix, "/domains?domain=example.co.uk");
        assert_eq!(p.subject(), ("domain", "shop.example.co.uk"));
        assert_eq!(p.since, Some(NOW));
    }

    #[test]
    fn a_record_facade_manages_gets_time_to_spread() {
        let mut d = domain("blog.example.com", "blog", &[]);
        d.managed = true;
        let facts = Facts {
            domains: vec![d],
            ..Default::default()
        };
        let mut seen = HashMap::new();
        assert!(assemble(&facts, NOW, &mut seen).is_empty());
        // Still seen, so its time counts from the first look.
        assert_eq!(seen.get("dns:blog.example.com").map(|s| s.first), Some(NOW));
        let later = assemble(&facts, NOW + DNS_SPREADS_IN, &mut seen);
        assert_eq!(keys(&later), ["dns:blog.example.com"]);
        assert_eq!(later[0].since, Some(NOW));
        // Fixed: gone at once, forgotten a while later.
        let mut fixed = facts;
        fixed.domains[0].dns_ok = true;
        let at = NOW + DNS_SPREADS_IN + 60;
        assert!(assemble(&fixed, at, &mut seen).is_empty());
        assemble(&fixed, at + FORGET_AFTER, &mut seen);
        assert!(seen.is_empty());
    }

    #[test]
    fn a_site_behind_a_proxy_is_not_a_dns_problem() {
        let mut d = domain("blog.example.com", "blog", &["104.16.1.1"]);
        d.reach = Some(Reach {
            ok: true,
            checked_at: NOW,
            ..Default::default()
        });
        let facts = Facts {
            domains: vec![d],
            ..Default::default()
        };
        assert!(run(&facts).is_empty());
    }

    #[test]
    fn an_unreachable_site_with_wrong_dns_is_one_critical_row() {
        let mut d = domain("blog.example.com", "blog", &["203.0.113.9"]);
        d.reach = Some(Reach {
            ok: false,
            checked_at: NOW,
            error: Some("timed out".into()),
            down_since: Some(NOW - 600),
            ..Default::default()
        });
        let facts = Facts {
            alerts: vec![alert(
                "domain:blog.example.com:unreachable",
                "site blog.example.com (app blog) is unreachable from outside",
                NOW - 400,
            )],
            domains: vec![d],
            certs: vec![CertFact {
                name: "blog.example.com".into(),
                app: Some("blog".into()),
                not_after: Some(NOW + 5 * DAY),
                error: Some("no answer to the challenge".into()),
            }],
            acme: true,
            ..Default::default()
        };
        let list = run(&facts);
        assert_eq!(keys(&list), ["dns:blog.example.com"]);
        let p = &list[0];
        assert_eq!(p.severity, ProblemSeverity::Critical);
        assert_eq!(p.since, Some(NOW - 600));
        assert_eq!(
            p.message,
            "blog.example.com points to 203.0.113.9, not to its node; its certificate for blog.example.com expires in 5 days"
        );
        assert_eq!(
            p.hint.as_deref(),
            Some("change its A record to 51.83.42.59")
        );
    }

    #[test]
    fn an_unreachable_site_on_an_offline_node_is_the_nodes_problem() {
        let facts = Facts {
            alerts: vec![
                alert("node:main:offline", "node main is offline", NOW - 300),
                alert(
                    "domain:blog.example.com:unreachable",
                    "site blog.example.com is unreachable from outside",
                    NOW - 200,
                ),
            ],
            domains: vec![domain("blog.example.com", "blog", &["51.83.42.59"])],
            nodes: vec![node("main")],
            ..Default::default()
        };
        assert_eq!(keys(&run(&facts)), ["node:main:offline"]);
    }

    #[test]
    fn an_expired_certificate_takes_in_the_unreachable_site() {
        let facts = Facts {
            alerts: vec![
                alert(
                    "cert:*.example.com:expiring",
                    "certificate for *.example.com has expired",
                    NOW - 100,
                ),
                alert(
                    "domain:api.example.com:unreachable",
                    "site api.example.com is unreachable from outside: TLS: certificate expired",
                    NOW - 50,
                ),
            ],
            domains: vec![domain("api.example.com", "api", &["51.83.42.59"])],
            certs: vec![CertFact {
                name: "*.example.com".into(),
                app: None,
                not_after: Some(NOW - 10),
                error: None,
            }],
            acme: true,
            ..Default::default()
        };
        let list = run(&facts);
        assert_eq!(keys(&list), ["cert:*.example.com:expiring"]);
        assert_eq!(list[0].severity, ProblemSeverity::Critical);
        assert_eq!(list[0].message, "certificate for *.example.com has expired");
        assert_eq!(list[0].fix, "/domains?domain=example.com");
        // A wildcard of several apps is the admins'.
        assert_eq!(list[0].app, None);
    }

    #[test]
    fn certificates_warn_two_weeks_ahead() {
        let cert = |not_after, error: Option<&str>| CertFact {
            name: "blog.example.com".into(),
            app: Some("blog".into()),
            not_after,
            error: error.map(String::from),
        };
        let facts = |c| Facts {
            certs: vec![c],
            acme: true,
            ..Default::default()
        };
        assert!(run(&facts(cert(Some(NOW + 20 * DAY), None))).is_empty());
        let soon = run(&facts(cert(Some(NOW + 10 * DAY), None)));
        assert_eq!(soon[0].severity, ProblemSeverity::Warning);
        assert_eq!(soon[0].since, Some(NOW + 10 * DAY - CERT_WARN_WITHIN));
        let failing = run(&facts(cert(Some(NOW + 20 * DAY), Some("rate limited"))));
        assert_eq!(keys(&failing), ["cert:blog.example.com:renewal"]);
        assert_eq!(failing[0].hint.as_deref(), Some("rate limited"));
        // Missing: only once it is late, unless something fails.
        assert!(run(&facts(cert(None, None))).is_empty());
        assert_eq!(
            keys(&run(&facts(cert(None, Some("refused"))))),
            ["cert:blog.example.com:missing"]
        );
        // Without ACME, nobody is getting one.
        let mut off = facts(cert(None, Some("refused")));
        off.acme = false;
        assert!(run(&off).is_empty());
    }

    #[test]
    fn a_failed_deploy_counts_while_the_old_version_runs() {
        let mut blog = app("blog", AppState::Running);
        blog.latest = Some(deployment("blog", DeploymentStatus::Failed, NOW - 1000));
        let mut fine = app("api", AppState::Running);
        fine.latest = Some(deployment("api", DeploymentStatus::Succeeded, NOW - 1000));
        let mut cancelled = app("web", AppState::Running);
        cancelled.latest = Some(deployment("web", DeploymentStatus::Cancelled, NOW - 1000));
        let facts = Facts {
            apps: vec![blog, fine, cancelled],
            ..Default::default()
        };
        let list = run(&facts);
        assert_eq!(keys(&list), ["deploy:blog"]);
        assert_eq!(list[0].since, Some(NOW - 940));
        assert_eq!(list[0].hint.as_deref(), Some("build failed: exit code 101"));
        assert_eq!(
            list[0].fix,
            format!(
                "/apps/blog?tab=deployments&deployment={}",
                uuid::Uuid::nil()
            )
        );
        // Its first deploy failed, and it runs since a settings change fixed
        // it: there was no older version, and nothing is wrong.
        let mut first = app("blog", AppState::Running);
        first.latest = Some(deployment("blog", DeploymentStatus::Failed, NOW - 1000));
        first.ran_before = false;
        assert!(
            run(&Facts {
                apps: vec![first],
                ..Default::default()
            })
            .is_empty()
        );
        // Down instead: the alert says so, once.
        let mut down = app("blog", AppState::Crashed);
        down.latest = Some(deployment("blog", DeploymentStatus::Failed, NOW - 1000));
        let facts = Facts {
            alerts: vec![alert("app:blog:down", "app blog is down", NOW - 100)],
            apps: vec![down],
            ..Default::default()
        };
        assert_eq!(keys(&run(&facts)), ["app:blog:down"]);
    }

    #[test]
    fn a_down_app_is_not_also_failing_requests_or_checks() {
        let mut blog = app("blog", AppState::Crashed);
        blog.unhealthy = Some((NOW - 100, "curl: (7) refused".into()));
        let facts = Facts {
            alerts: vec![
                alert("app:blog:down", "app blog is down", NOW - 100),
                alert("app:blog:errors", "app blog answers errors", NOW - 50),
            ],
            apps: vec![blog],
            ..Default::default()
        };
        assert_eq!(keys(&run(&facts)), ["app:blog:down"]);
        // Up but unhealthy: that is the problem.
        let mut blog = app("blog", AppState::Running);
        blog.unhealthy = Some((NOW - 100, "curl: (7) refused".into()));
        let list = run(&Facts {
            apps: vec![blog],
            ..Default::default()
        });
        assert_eq!(keys(&list), ["health:blog"]);
        assert_eq!(list[0].hint.as_deref(), Some("curl: (7) refused"));
    }

    #[test]
    fn stuck_deployments_and_the_rest() {
        let mut blog = app("blog", AppState::Running);
        let mut d = deployment("blog", DeploymentStatus::Running, NOW - 2 * 3600);
        d.finished_at = None;
        blog.latest = Some(d);
        let facts = Facts {
            apps: vec![blog],
            auto_deploy: vec![AutoDeployFact {
                kind: "app",
                name: "api".into(),
                error: "git ls-remote: authentication failed".into(),
            }],
            volumes: vec![VolumeFact {
                node: "main".into(),
                owner: "db".into(),
                is_stack: false,
                volume: "data".into(),
                size: 19_000_000_000,
                limit: 20_000_000_000,
            }],
            notify_error: Some("Discord answered 404".into()),
            ..Default::default()
        };
        let list = run(&facts);
        assert_eq!(
            keys(&list),
            [
                "stuck:blog",
                "auto-deploy:app:api",
                "notify",
                "volume:main:db/data"
            ]
        );
        assert_eq!(list[0].message, "deploy of blog has been running for 2 h");
        assert_eq!(
            list[3].message,
            "volume data of db is 95 % full (19 GB of 20 GB)"
        );
        assert_eq!(list[3].fix, "/apps/db?tab=volumes");
    }

    #[test]
    fn first_seen_times_hold_until_fixed_for_a_while() {
        let facts = Facts {
            notify_error: Some("Discord answered 404".into()),
            ..Default::default()
        };
        let mut seen = HashMap::new();
        assemble(&facts, NOW, &mut seen);
        let later = assemble(&facts, NOW + 600, &mut seen);
        assert_eq!(later[0].since, Some(NOW));
        // Gone a minute (a restart, before its alert confirms itself again):
        // the same problem, since the same time.
        assert!(assemble(&Facts::default(), NOW + 700, &mut seen).is_empty());
        let back = assemble(&facts, NOW + 760, &mut seen);
        assert_eq!(back[0].since, Some(NOW));
        // Gone longer: a new one.
        assemble(&Facts::default(), NOW + 800, &mut seen);
        assemble(&Facts::default(), NOW + 800 + FORGET_AFTER + 1, &mut seen);
        let again = assemble(&facts, NOW + 2000, &mut seen);
        assert_eq!(again[0].since, Some(NOW + 2000));
    }

    #[test]
    fn an_alert_confirmed_again_keeps_the_first_time() {
        let mut seen = HashMap::new();
        let down = |since| Facts {
            alerts: vec![alert("app:blog:down", "app blog is down", since)],
            ..Default::default()
        };
        assemble(&down(NOW), NOW + 30, &mut seen);
        // The server restarted: the alert fires again, later.
        assemble(&Facts::default(), NOW + 100, &mut seen);
        let list = assemble(&down(NOW + 160), NOW + 160, &mut seen);
        assert_eq!(list[0].since, Some(NOW));
    }

    #[test]
    fn wildcards_cover_one_label() {
        assert!(covers("*.example.com", "example.com"));
        assert!(covers("*.example.com", "api.example.com"));
        assert!(!covers("*.example.com", "a.b.example.com"));
        assert!(!covers("*.example.com", "notexample.com"));
        assert!(covers("api.example.com", "api.example.com"));
    }

    #[test]
    fn hints_quote_the_first_line_shortly() {
        assert_eq!(short("\n  first\nsecond"), "first");
        let long = "x".repeat(300);
        assert_eq!(short(&long).chars().count(), HINT_CHARS + 1);
    }
}
