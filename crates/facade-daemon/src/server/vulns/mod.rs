//! The vulnerability check. Agents say what they run (their inventory); the
//! server matches it against vulnerability data it downloads itself, so no
//! node's package list ever leaves it, and one download serves every node on
//! the same release.
//!
//! Findings with no fix released raise an alert each (`node:<name>:vuln:<CVE>`)
//! from `[vulns] alert_min` up (critical by default); high and critical ones
//! with a fix waiting, or fixed after a reboot, are named in the node's upkeep
//! alert instead. The others are listed and counted, never alerted on.

mod dpkg;
pub mod oval;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use facade_core::events::{EventKind, Severity};
use facade_core::platform::{Checked, PlatformInfo};
use facade_core::proto::AgentRequest;
use facade_core::vulns::{Finding, FindingStatus, Inventory, VulnSeverity, VulnSummary};
use parking_lot::{Mutex, RwLock};
use tokio::sync::Notify;
use tracing::{info, warn};

use super::Server;
use super::events::Entry;
use oval::Oval;

/// Agents speaking this protocol can be asked for their inventory.
const INVENTORY_PROTOCOL: u32 = 8;
/// Data is downloaded again after this long.
const REFRESH_EVERY: u64 = 24 * 3600;
/// And said to be stale after this long without a successful check.
const STALE_AFTER: u64 = 2 * 86_400;
/// Inventories are asked for again after this long (or when they change).
const INVENTORY_EVERY: u64 = 6 * 3600;
const TICK: Duration = Duration::from_secs(300);

#[derive(Default)]
pub struct Vulns {
    /// Ubuntu's data per release codename.
    data: RwLock<HashMap<String, Arc<Dataset>>>,
    /// Why a release's data could not be refreshed, last time.
    errors: RwLock<HashMap<String, String>>,
    /// Per node, from its findings: what views and alerts read.
    nodes: RwLock<HashMap<String, NodeState>>,
    /// Nodes to ask for their inventory at the next round.
    due: Mutex<HashSet<String>>,
    wake: Notify,
}

struct Dataset {
    oval: Oval,
    /// When it was last confirmed current (downloaded, or not modified).
    checked_at: AtomicU64,
}

impl Dataset {
    fn new(oval: Oval, checked_at: u64) -> Arc<Self> {
        Arc::new(Self {
            oval,
            checked_at: AtomicU64::new(checked_at),
        })
    }

    fn checked_at(&self) -> u64 {
        self.checked_at.load(Ordering::Relaxed)
    }
}

#[derive(Default, Clone)]
struct NodeState {
    summary: VulnSummary,
    /// Findings worth an alert of their own: `(cve, message)`.
    alerting: Vec<(String, String)>,
    /// High and critical CVEs an update fixes, and a reboot fixes.
    by_update: Vec<String>,
    by_reboot: Vec<String>,
    /// Why the node's inventory could not be had.
    inventory_error: Option<String>,
}

impl Vulns {
    /// A node changed its packages, or just connected: ask it soon.
    pub fn inventory_due(&self, node: &str) {
        self.due.lock().insert(node.to_string());
        self.wake.notify_one();
    }

    pub fn summary(&self, node: &str) -> Option<VulnSummary> {
        self.nodes.read().get(node).map(|n| n.summary.clone())
    }

    /// ` (fixes CVE-…)`, ` (a reboot fixes CVE-…)`: what the upkeep alert adds.
    pub fn upkeep_note(&self, node: &str, security: bool, reboot: bool) -> String {
        let nodes = self.nodes.read();
        let Some(n) = nodes.get(node) else {
            return String::new();
        };
        let list = |cves: &[String]| {
            let mut s = cves.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
            if cves.len() > 3 {
                s.push_str(&format!(" and {} more", cves.len() - 3));
            }
            s
        };
        let mut notes = vec![];
        if security && !n.by_update.is_empty() {
            notes.push(format!("fixes {}", list(&n.by_update)));
        }
        if reboot && !n.by_reboot.is_empty() {
            notes.push(format!("a reboot fixes {}", list(&n.by_reboot)));
        }
        if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join("; "))
        }
    }

    /// `Ubuntu 26.04 LTS (Canonical OVAL, 4 Nov)` for a node's platform.
    pub fn source_label(&self, platform: Option<&PlatformInfo>) -> Option<String> {
        let codename = ubuntu_codename(platform?)?;
        let data = self.data.read();
        let d = data.get(&codename)?;
        Some(format!("{}, Canonical's OVAL data", d.oval.release))
    }
}

/// Alerts of its own for the monitor.
pub fn conditions(server: &Server, out: &mut HashMap<String, String>) {
    if !server.cfg.vulns.enabled {
        return;
    }
    for (node, state) in server.vulns.nodes.read().iter() {
        for (cve, message) in &state.alerting {
            out.insert(
                format!("node:{node}:vuln:{cve}"),
                format!("node {node}: {message}"),
            );
        }
    }
}

fn ubuntu_codename(p: &PlatformInfo) -> Option<String> {
    (p.os.id == "ubuntu")
        .then(|| p.os.codename.clone())
        .flatten()
        .filter(|c| c.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn alert_min(server: &Server) -> VulnSeverity {
    match VulnSeverity::parse(&server.cfg.vulns.alert_min) {
        VulnSeverity::Unknown => VulnSeverity::Critical,
        s => s,
    }
}

// ---------------------------------------------------------------- the task

pub async fn run(server: Arc<Server>) {
    if !server.cfg.vulns.enabled {
        return;
    }
    // What a restart must not forget: findings, and the data already here.
    for node in server
        .store
        .list_nodes()
        .map(|n| n.into_iter().map(|n| n.name).collect::<Vec<_>>())
        .unwrap_or_default()
    {
        refresh_state(&server, &node);
    }
    load_cached(&server).await;
    // Give agents a moment to connect and say their platform.
    tokio::time::sleep(Duration::from_secs(30)).await;
    loop {
        if let Err(e) = round(&server).await {
            warn!("vulnerability check: {e:#}");
        }
        tokio::select! {
            _ = tokio::time::sleep(TICK) => {}
            _ = server.vulns.wake.notified() => {
                // Let a burst of changes (several nodes connecting) gather.
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn round(server: &Arc<Server>) -> anyhow::Result<()> {
    let now = facade_core::now();
    // Which releases are in use, and which connected nodes can say what they run.
    let mut codenames = HashSet::new();
    let mut nodes = vec![];
    {
        let live = server.live.read();
        for (name, n) in &live.nodes {
            let Some(p) = &n.platform else { continue };
            if let Some(c) = ubuntu_codename(p) {
                codenames.insert(c);
            }
            if server.hub.is_connected(name) && n.protocol >= INVENTORY_PROTOCOL {
                nodes.push(name.clone());
            }
        }
    }
    // Data first, so a new inventory is matched against fresh data.
    let mut refreshed = HashSet::new();
    for codename in codenames {
        let fresh = server
            .vulns
            .data
            .read()
            .get(&codename)
            .is_some_and(|d| now.saturating_sub(d.checked_at()) < REFRESH_EVERY);
        let recently_failed = server.vulns.errors.read().contains_key(&codename)
            && server
                .store
                .kv_get(&format!("vulns:tried:{codename}"))?
                .and_then(|t| t.parse::<u64>().ok())
                .is_some_and(|t| now.saturating_sub(t) < 3600);
        if fresh || recently_failed {
            continue;
        }
        server
            .store
            .kv_put(&format!("vulns:tried:{codename}"), &now.to_string())?;
        match refresh_ubuntu(server, &codename).await {
            Ok(changed) => {
                server.vulns.errors.write().remove(&codename);
                if changed {
                    refreshed.insert(codename);
                }
            }
            Err(e) => {
                warn!(%codename, "vulnerability data: {e:#}");
                server
                    .vulns
                    .errors
                    .write()
                    .insert(codename, format!("{e:#}"));
            }
        }
    }

    let due: HashSet<String> = std::mem::take(&mut *server.vulns.due.lock());
    for node in nodes {
        let last = server.store.inventory(&node)?.map(|(_, at)| at);
        let stale = last.is_none_or(|at| now.saturating_sub(at) >= INVENTORY_EVERY);
        let mut rematch = refreshed
            .iter()
            .any(|c| node_codename(server, &node).as_ref() == Some(c));
        if stale || due.contains(&node) {
            match ask_inventory(server, &node).await {
                Ok(changed) => rematch |= changed,
                Err(e) => {
                    warn!(%node, "inventory: {e:#}");
                    if let Some(n) = server.vulns.nodes.write().get_mut(&node) {
                        n.inventory_error = Some(format!("{e:#}"));
                    }
                }
            }
        }
        if rematch || server.store.inventory_matched(&node)?.is_none() {
            if let Err(e) = rematch_node(server, &node) {
                warn!(%node, "matching vulnerabilities: {e:#}");
            }
        }
        refresh_state(server, &node);
    }
    Ok(())
}

fn node_codename(server: &Server, node: &str) -> Option<String> {
    let live = server.live.read();
    ubuntu_codename(live.nodes.get(node)?.platform.as_ref()?)
}

/// Asks a node for its inventory; whether it changed.
async fn ask_inventory(server: &Server, node: &str) -> anyhow::Result<bool> {
    let known = server.store.inventory_hash(node)?;
    let answer: Checked<Option<Inventory>> = server
        .hub
        .query(node, AgentRequest::Inventory { known })
        .await?;
    let now = facade_core::now();
    let (new, error) = match answer {
        Checked::Known(Some(inv)) => {
            server.store.put_inventory(node, &inv, now)?;
            (true, None)
        }
        Checked::Known(None) => {
            server.store.touch_inventory(node, now)?;
            (false, None)
        }
        Checked::Unsupported(why) | Checked::Failed(why) => (false, Some(why)),
    };
    server
        .vulns
        .nodes
        .write()
        .entry(node.to_string())
        .or_default()
        .inventory_error = error;
    Ok(new)
}

/// Matches a node's inventory against its release's data and records what
/// changed.
fn rematch_node(server: &Server, node: &str) -> anyhow::Result<()> {
    let Some(codename) = node_codename(server, node) else {
        return Ok(());
    };
    let Some(data) = server.vulns.data.read().get(&codename).cloned() else {
        return Ok(());
    };
    let Some((inventory, _)) = server.store.inventory(node)? else {
        return Ok(());
    };
    let now = facade_core::now();
    let findings: Vec<Finding> = data
        .oval
        .matches(&inventory)
        .into_iter()
        .map(|m| Finding {
            node: node.to_string(),
            cve: m.cve,
            package: m.package,
            installed: m.installed,
            fixed: m.fixed,
            severity: m.severity,
            status: m.status,
            title: m.title,
            url: m.url,
            first_seen: now,
            ack: None,
        })
        .collect();
    let changes = server.store.replace_findings(node, findings, now)?;
    record_changes(server, node, &changes);
    Ok(())
}

/// Activity entries: a summary at a node's first check, then each high or
/// critical finding appearing, changing status or going away.
fn record_changes(server: &Server, node: &str, changes: &super::store::FindingChanges) {
    let serious = |f: &Finding| f.severity.at_least(VulnSeverity::High);
    if changes.first {
        let all = &changes.appeared;
        let count = |s: VulnSeverity| all.iter().filter(|f| f.severity == s).count();
        let (critical, high) = (count(VulnSeverity::Critical), count(VulnSeverity::High));
        let summary = format!(
            "Vulnerability check: {critical} critical, {high} high, {} other",
            all.len() - critical - high
        );
        let severity = if critical + high > 0 {
            Severity::Warn
        } else {
            Severity::Ok
        };
        Entry::node(node, EventKind::Vulnerability, severity, summary)
            .items(all.iter().filter(|f| serious(f)).map(line).collect())
            .record(server);
        return;
    }
    for f in changes.appeared.iter().filter(|f| serious(f)) {
        let severity = if f.status == FindingStatus::NoFix {
            Severity::Error
        } else {
            Severity::Warn
        };
        Entry::node(node, EventKind::Vulnerability, severity, line(f)).record(server);
    }
    for (before, now) in changes.changed.iter().filter(|(_, f)| serious(f)) {
        Entry::node(
            node,
            EventKind::Vulnerability,
            Severity::Info,
            format!(
                "{} in {}: {} (was: {})",
                now.cve,
                now.package,
                status_text(now),
                before.status.label()
            ),
        )
        .record(server);
    }
    for f in changes.gone.iter().filter(|f| serious(f)) {
        Entry::node(
            node,
            EventKind::Vulnerability,
            Severity::Ok,
            format!(
                "{} no longer affects {} ({})",
                f.cve,
                f.package,
                f.severity.as_str()
            ),
        )
        .record(server);
    }
}

/// `CVE-2026-98432 (critical) in linux 7.0.0-42-generic: no fix released`.
fn line(f: &Finding) -> String {
    format!(
        "{} ({}) in {} {}: {}",
        f.cve,
        f.severity.as_str(),
        f.package,
        f.installed,
        status_text(f)
    )
}

fn status_text(f: &Finding) -> String {
    match (&f.status, &f.fixed) {
        (FindingStatus::FixAvailable, Some(v)) => format!("fix available ({v})"),
        (FindingStatus::FixedAfterReboot, Some(v)) => format!("fixed after reboot ({v} installed)"),
        (s, _) => s.label().to_string(),
    }
}

/// Recomputes what views and alerts read for a node (after an acknowledgement).
pub fn refresh(server: &Server, node: &str) {
    refresh_state(server, node);
}

/// Recomputes what views and alerts read for a node.
fn refresh_state(server: &Server, node: &str) {
    let findings = server.store.findings(Some(node)).unwrap_or_default();
    let min = alert_min(server);
    let mut state = NodeState {
        inventory_error: server
            .vulns
            .nodes
            .read()
            .get(node)
            .and_then(|n| n.inventory_error.clone()),
        ..Default::default()
    };
    for f in &findings {
        match f.severity {
            VulnSeverity::Critical => state.summary.critical += 1,
            VulnSeverity::High => state.summary.high += 1,
            _ => state.summary.other += 1,
        }
        let serious = f.severity.at_least(VulnSeverity::High);
        match f.status {
            FindingStatus::NoFix if serious && f.ack.is_none() => {
                state.summary.unfixed += 1;
            }
            FindingStatus::FixAvailable if serious => state.by_update.push(f.cve.clone()),
            FindingStatus::FixedAfterReboot if serious => state.by_reboot.push(f.cve.clone()),
            _ => {}
        }
        if f.status == FindingStatus::NoFix && f.severity.at_least(min) && f.ack.is_none() {
            state.alerting.push((
                f.cve.clone(),
                format!(
                    "{} ({}) in {} has no fix released: {}",
                    f.cve,
                    f.severity.as_str(),
                    f.package,
                    f.title
                ),
            ));
        }
    }
    for list in [&mut state.by_update, &mut state.by_reboot] {
        list.sort();
        list.dedup();
    }
    state.alerting.sort();
    state.alerting.dedup_by(|a, b| a.0 == b.0);

    let platform = server
        .live
        .read()
        .nodes
        .get(node)
        .and_then(|n| n.platform.clone());
    let now = facade_core::now();
    let codename = platform.as_ref().and_then(ubuntu_codename);
    let data = codename
        .as_ref()
        .and_then(|c| server.vulns.data.read().get(c).cloned());
    state.summary.data_as_of = data.as_ref().and_then(|d| d.oval.generated);
    let has_inventory = server.store.inventory(node).ok().flatten().is_some();
    state.summary.unknown = match (&platform, &codename, &data) {
        (None, _, _) => Some("its agent does not say what it runs yet (it updates itself)".into()),
        (Some(p), None, _) => Some(format!(
            "facade has no vulnerability data for {}",
            p.os.name()
        )),
        (_, Some(c), None) => Some(match server.vulns.errors.read().get(c) {
            Some(e) => format!("the vulnerability data could not be downloaded: {e}"),
            None => "the vulnerability data is not downloaded yet".into(),
        }),
        _ if state.inventory_error.is_some() => Some(format!(
            "couldn't list its packages: {}",
            state.inventory_error.as_deref().unwrap_or_default()
        )),
        _ if !has_inventory => Some("waiting for its package list".into()),
        (_, _, Some(d)) if now.saturating_sub(d.checked_at()) > STALE_AFTER => Some(format!(
            "the vulnerability data is {} days old{}",
            now.saturating_sub(d.checked_at()) / 86_400,
            codename
                .as_ref()
                .and_then(|c| server.vulns.errors.read().get(c).cloned())
                .map(|e| format!(" (refreshing it fails: {e})"))
                .unwrap_or_default()
        )),
        _ => None,
    };
    server.vulns.nodes.write().insert(node.to_string(), state);
}

// ---------------------------------------------------------------- data

fn data_dir(server: &Server) -> std::path::PathBuf {
    server.cfg.data_dir.join("vulns")
}

fn oval_file(server: &Server, codename: &str) -> std::path::PathBuf {
    data_dir(server).join(format!("com.ubuntu.{codename}.cve.oval.xml.bz2"))
}

/// Loads the data downloaded before a restart.
async fn load_cached(server: &Arc<Server>) {
    let Ok(entries) = std::fs::read_dir(data_dir(server)) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(codename) = name
            .strip_prefix("com.ubuntu.")
            .and_then(|n| n.strip_suffix(".cve.oval.xml.bz2"))
        else {
            continue;
        };
        let checked_at = server
            .store
            .kv_get(&format!("vulns:checked:{codename}"))
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        match parse_file(e.path()).await {
            Ok(oval) => {
                server
                    .vulns
                    .data
                    .write()
                    .insert(codename.to_string(), Dataset::new(oval, checked_at));
            }
            Err(e) => warn!(%codename, "reading the vulnerability data kept: {e:#}"),
        }
    }
}

async fn parse_file(path: std::path::PathBuf) -> anyhow::Result<Oval> {
    tokio::task::spawn_blocking(move || {
        let file =
            std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let reader = std::io::BufReader::new(bzip2::read::MultiBzDecoder::new(
            std::io::BufReader::new(file),
        ));
        Oval::parse(reader)
    })
    .await?
}

/// Downloads a release's OVAL file if it changed; whether it did.
async fn refresh_ubuntu(server: &Arc<Server>, codename: &str) -> anyhow::Result<bool> {
    let base = server.cfg.vulns.ubuntu_oval_url.trim_end_matches('/');
    let url = format!("{base}/com.ubuntu.{codename}.cve.oval.xml.bz2");
    let path = oval_file(server, codename);
    let etag_key = format!("vulns:etag:{codename}");
    let etag = server.store.kv_get(&etag_key)?.filter(|_| path.exists());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .user_agent(concat!("facade/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut req = client.get(&url);
    if let Some(etag) = &etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let now = facade_core::now();
    let loaded = server.vulns.data.read().get(codename).cloned();
    if resp.status() == reqwest::StatusCode::NOT_MODIFIED
        && let Some(d) = loaded
    {
        // Same data, confirmed now.
        server
            .store
            .kv_put(&format!("vulns:checked:{codename}"), &now.to_string())?;
        d.checked_at.store(now, Ordering::Relaxed);
        return Ok(false);
    }
    if resp.status() != reqwest::StatusCode::NOT_MODIFIED && !resp.status().is_success() {
        bail!("GET {url}: {}", resp.status());
    }
    let new_etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if resp.status().is_success() {
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("downloading {url}"))?;
        std::fs::create_dir_all(data_dir(server))?;
        let partial = path.with_extension("partial");
        std::fs::write(&partial, &bytes)?;
        // Only data that reads replaces what is there.
        let oval = parse_file(partial.clone())
            .await
            .context("reading the new data")?;
        std::fs::rename(&partial, &path)?;
        info!(
            %codename,
            definitions = oval.definitions(),
            size = bytes.len(),
            "vulnerability data downloaded"
        );
        server
            .vulns
            .data
            .write()
            .insert(codename.to_string(), Dataset::new(oval, now));
    } else {
        // Not modified, but not loaded yet either (its file was kept).
        let oval = parse_file(path.clone()).await?;
        server
            .vulns
            .data
            .write()
            .insert(codename.to_string(), Dataset::new(oval, now));
    }
    if let Some(etag) = new_etag {
        server.store.kv_put(&etag_key, &etag)?;
    }
    server
        .store
        .kv_put(&format!("vulns:checked:{codename}"), &now.to_string())?;
    Ok(true)
}
