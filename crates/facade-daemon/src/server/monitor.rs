//! Health checks over the state agents report, posted to Discord on change.
//!
//! A condition must hold for two consecutive checks before it fires, and a message
//! goes out when it starts and when it clears.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use facade_core::api::{
    AlertView, AppMetric, Mention, NotifySettings, NotifyUpdate, WRITTEN_WARNING,
};
use facade_core::proto::AppState;
use parking_lot::RwLock;
use serde_json::json;
use tracing::{info, warn};

use super::Server;
use super::secrets::SecretBox;
use super::store::{Store, USAGE_KEEP};

const CHECK_EVERY: Duration = Duration::from_secs(30);
const NODE_OFFLINE_AFTER: u64 = 120;
/// An app is at its CPU limit when it averaged it over this long. Each sample
/// covers a second, so one alone is mostly noise: a health check starting an
/// interpreter fills a half-CPU limit for that second.
const CPU_PINNED_FOR: u64 = 180;
/// Stored usage is pruned once an hour.
const PRUNE_EVERY: u32 = 120;

#[derive(Default)]
pub struct Alerts {
    /// Conditions seen once, waiting for confirmation.
    pending: HashMap<String, String>,
    firing: HashMap<String, AlertView>,
    resolved: VecDeque<AlertView>,
}

impl Alerts {
    pub fn views(&self) -> Vec<AlertView> {
        let mut all: Vec<AlertView> = self.firing.values().cloned().collect();
        all.sort_by_key(|a| std::cmp::Reverse(a.since));
        all.extend(self.resolved.iter().rev().cloned());
        all
    }
}

pub async fn run(server: Arc<Server>) -> anyhow::Result<()> {
    if !server.notifier.is_configured() {
        info!("no Discord webhook set: alerts are only logged (facade alerts config --webhook …)");
    }
    let mut tick = tokio::time::interval(CHECK_EVERY);
    tick.tick().await;
    let mut ticks = 0u32;
    loop {
        tick.tick().await;
        if ticks.is_multiple_of(PRUNE_EVERY) {
            let before = facade_core::now().saturating_sub(USAGE_KEEP);
            if let Err(e) = server.store.prune_usage(before) {
                warn!("pruning app usage: {e:#}");
            }
        }
        ticks = ticks.wrapping_add(1);
        let current = conditions(&server);
        let (fired, cleared) = {
            let mut alerts = server.alerts.lock();
            transition(&mut alerts, current)
        };
        for alert in &fired {
            warn!(key = %alert.key, "ALERT: {}", alert.message);
        }
        for alert in &cleared {
            info!(key = %alert.key, "resolved: {}", alert.message);
        }
        let notes: Vec<Note> = fired
            .iter()
            .map(|a| Note::problem(&a.message))
            .chain(cleared.iter().map(|a| Note::resolved(&a.message)))
            .collect();
        if let Err(e) = server.notifier.send(&notes).await {
            warn!("sending alerts to Discord: {e:#}");
        }
    }
}

fn transition(
    alerts: &mut Alerts,
    current: HashMap<String, String>,
) -> (Vec<AlertView>, Vec<AlertView>) {
    let now = facade_core::now();
    let mut fired = Vec::new();
    let mut cleared = Vec::new();

    let keys: Vec<String> = alerts.firing.keys().cloned().collect();
    for key in keys {
        if !current.contains_key(&key) {
            let mut alert = alerts.firing.remove(&key).expect("present");
            alert.resolved_at = Some(now);
            cleared.push(alert.clone());
            if alerts.resolved.len() == 100 {
                alerts.resolved.pop_front();
            }
            alerts.resolved.push_back(alert);
        }
    }
    let mut pending = HashMap::new();
    for (key, message) in current {
        if let Some(alert) = alerts.firing.get_mut(&key) {
            alert.message = message;
        } else if alerts.pending.contains_key(&key) {
            let alert = AlertView {
                key: key.clone(),
                message,
                since: now,
                resolved_at: None,
            };
            fired.push(alert.clone());
            alerts.firing.insert(key, alert);
        } else {
            pending.insert(key, message);
        }
    }
    alerts.pending = pending;
    (fired, cleared)
}

/// Whether the running samples of the last `CPU_PINNED_FOR` seconds, covering
/// most of it, average at least `threshold` percent.
fn cpu_pinned(history: &VecDeque<AppMetric>, threshold: f64, now: u64) -> bool {
    let since = now.saturating_sub(CPU_PINNED_FOR);
    let recent: Vec<&AppMetric> = history
        .iter()
        .filter(|m| m.at >= since && m.stats.state == AppState::Running)
        .collect();
    let (Some(first), Some(last)) = (recent.first(), recent.last()) else {
        return false;
    };
    if last.at - first.at < CPU_PINNED_FOR * 3 / 4 {
        return false;
    }
    let mean = recent
        .iter()
        .map(|m| f64::from(m.stats.cpu_percent))
        .sum::<f64>()
        / recent.len() as f64;
    mean >= threshold
}

/// Everything wrong right now, keyed so the same problem keeps the same key.
fn conditions(server: &Server) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let now = facade_core::now();
    let live = server.live.read();

    for (name, node) in &live.nodes {
        if let Some(since) = node.offline_since {
            if now.saturating_sub(since) > NODE_OFFLINE_AFTER {
                out.insert(
                    format!("node:{name}:offline"),
                    format!("node {name} is offline"),
                );
            }
            continue;
        }
        if server.hub.is_connected(name) {
            for error in &node.ingress_errors {
                // One alert per node: the key must not depend on the message.
                out.insert(
                    format!("node:{name}:ingress"),
                    format!("node {name}: {error} (its apps are unreachable there)"),
                );
            }
        }
        if let Some(stats) = &node.stats {
            if stats.disk_total > 0 && stats.disk_used * 100 / stats.disk_total >= 90 {
                out.insert(
                    format!("node:{name}:disk"),
                    format!(
                        "node {name} disk is {}% full",
                        stats.disk_used * 100 / stats.disk_total
                    ),
                );
            }
            if stats.mem_total > 0 && stats.mem_used * 100 / stats.mem_total >= 95 {
                out.insert(
                    format!("node:{name}:memory"),
                    format!(
                        "node {name} memory is {}% used",
                        stats.mem_used * 100 / stats.mem_total
                    ),
                );
            }
        }
    }

    for (what, error) in server.backup_errors.read().iter() {
        out.insert(
            format!("backup:{what}"),
            format!("backup of {what} failed: {error}"),
        );
    }

    let apps = match server.store.list_apps() {
        Ok(apps) => apps,
        Err(e) => {
            warn!("monitor: {e:#}");
            return out;
        }
    };
    for app in apps {
        let spec = app.spec;
        if !spec.enabled || !server.hub.is_connected(&spec.node) {
            continue;
        }
        let Some(state) = live.apps.get(&spec.name) else {
            continue;
        };
        match state.state {
            AppState::Crashed => {
                out.insert(
                    format!("app:{}:down", spec.name),
                    format!("app {} is down (crashed or restarting)", spec.name),
                );
            }
            AppState::Failed => {
                out.insert(
                    format!("app:{}:down", spec.name),
                    format!(
                        "app {} failed: {}",
                        spec.name,
                        state.message.as_deref().unwrap_or("unknown error")
                    ),
                );
            }
            _ => {}
        }
        if let Some(stats) = &state.stats
            && spec.limits.memory_mb.is_some()
            && stats.mem_limit > 0
            && stats.mem_used * 100 / stats.mem_limit >= 90
        {
            out.insert(
                format!("app:{}:memory", spec.name),
                format!(
                    "app {} uses {}% of its memory limit",
                    spec.name,
                    stats.mem_used * 100 / stats.mem_limit
                ),
            );
        }
        if let Some(cpus) = spec.limits.cpus
            && cpu_pinned(&state.history, cpus * 100.0 * 0.95, now)
        {
            out.insert(
                format!("app:{}:cpu", spec.name),
                format!(
                    "app {} has been at its CPU limit for {} minutes",
                    spec.name,
                    CPU_PINNED_FOR / 60
                ),
            );
        }
        if let Some(disk) = live
            .nodes
            .get(&spec.node)
            .and_then(|n| n.storage.as_ref())
            .and_then(|s| s.apps.iter().find(|a| a.app == spec.name))
            && disk.written >= WRITTEN_WARNING
        {
            out.insert(
                format!("app:{}:written", spec.name),
                format!(
                    "app {} wrote {} MB inside its container, which a redeploy throws away: \
                     keep that data in a volume",
                    spec.name,
                    disk.written / (1024 * 1024)
                ),
            );
        }
    }
    out
}

pub struct Note {
    title: String,
    description: String,
    color: u32,
    /// Tag the configured people.
    ping: bool,
}

impl Note {
    pub fn problem(message: &str) -> Self {
        Self {
            title: "🔴 Problem".into(),
            description: message.into(),
            color: 0xd93f3f,
            ping: true,
        }
    }

    pub fn resolved(message: &str) -> Self {
        Self {
            title: "🟢 Resolved".into(),
            description: message.into(),
            color: 0x3fa45b,
            ping: false,
        }
    }

    pub fn test() -> Self {
        Self {
            title: "🔔 Test".into(),
            description: "Alerts from this facade server will arrive here.".into(),
            color: 0x5865f2,
            ping: true,
        }
    }
}

#[derive(Clone, Default)]
struct Target {
    webhook: Option<String>,
    mentions: Vec<Mention>,
}

const KV_WEBHOOK: &str = "notify.discord_webhook";
const KV_MENTIONS: &str = "notify.mentions";

/// Posts notes to a Discord webhook, tagging the configured people on problems.
/// Settings live in the database (the webhook encrypted) and change at runtime.
pub struct Notifier {
    client: reqwest::Client,
    target: RwLock<Target>,
    /// Shown under each message, to tell several servers apart.
    origin: String,
}

impl Notifier {
    pub fn new(origin: String) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            target: Default::default(),
            origin,
        })
    }

    pub fn load(&self, store: &Store, secrets: &SecretBox) -> anyhow::Result<()> {
        let webhook = store
            .kv_get(KV_WEBHOOK)?
            .map(|sealed| secrets.open_string(&hex::decode(sealed)?))
            .transpose()?;
        let mentions = match store.kv_get(KV_MENTIONS)? {
            Some(json) => serde_json::from_str::<Vec<String>>(&json)?
                .iter()
                .filter_map(|m| m.parse().ok())
                .collect(),
            None => vec![],
        };
        *self.target.write() = Target { webhook, mentions };
        Ok(())
    }

    pub fn update(
        &self,
        store: &Store,
        secrets: &SecretBox,
        update: NotifyUpdate,
    ) -> Result<(), String> {
        let mut target = self.target.read().clone();
        if let Some(webhook) = update.discord_webhook {
            let webhook = webhook.trim().to_string();
            if webhook.is_empty() {
                target.webhook = None;
            } else if webhook.starts_with("https://discord.com/api/webhooks/")
                || webhook.starts_with("https://discordapp.com/api/webhooks/")
            {
                target.webhook = Some(webhook);
            } else {
                return Err("the webhook must be a https://discord.com/api/webhooks/… URL".into());
            }
        }
        if let Some(mentions) = update.mentions {
            target.mentions = mentions
                .iter()
                .map(|m| m.parse())
                .collect::<Result<_, _>>()?;
        }
        let save = || -> anyhow::Result<()> {
            match &target.webhook {
                Some(w) => store.kv_put(KV_WEBHOOK, &hex::encode(secrets.seal(w.as_bytes())))?,
                None => store.kv_delete(KV_WEBHOOK)?,
            }
            let mentions: Vec<String> = target.mentions.iter().map(|m| m.to_string()).collect();
            store.kv_put(KV_MENTIONS, &serde_json::to_string(&mentions)?)
        };
        save().map_err(|e| format!("{e:#}"))?;
        *self.target.write() = target;
        Ok(())
    }

    /// The settings, with the webhook's secret token hidden.
    pub fn settings(&self) -> NotifySettings {
        let target = self.target.read();
        NotifySettings {
            discord_webhook: target.webhook.as_ref().map(|w| {
                let cut = w.rfind('/').map_or(w.len(), |i| i + 1);
                format!("{}••••••", &w[..cut])
            }),
            mentions: target.mentions.iter().map(|m| m.to_string()).collect(),
        }
    }

    pub fn is_configured(&self) -> bool {
        self.target.read().webhook.is_some()
    }

    pub async fn send(&self, notes: &[Note]) -> anyhow::Result<()> {
        let target = self.target.read().clone();
        let Some(webhook) = &target.webhook else {
            return Ok(());
        };
        let problems = notes.iter().any(|n| n.ping);
        // Only problems ping; `allowed_mentions` makes Discord honour exactly these tags.
        let (content, allowed) = if problems && !target.mentions.is_empty() {
            let mut parse = vec![];
            let mut users = vec![];
            let mut roles = vec![];
            let mut tags = vec![];
            for mention in &target.mentions {
                match mention {
                    Mention::Here => tags.push("@here".to_string()),
                    Mention::Everyone => tags.push("@everyone".to_string()),
                    Mention::User(id) => {
                        tags.push(format!("<@{id}>"));
                        users.push(id.to_string());
                    }
                    Mention::Role(id) => {
                        tags.push(format!("<@&{id}>"));
                        roles.push(id.to_string());
                    }
                }
                if matches!(mention, Mention::Here | Mention::Everyone) && parse.is_empty() {
                    parse.push("everyone");
                }
            }
            (
                tags.join(" "),
                json!({ "parse": parse, "users": users, "roles": roles }),
            )
        } else {
            (String::new(), json!({ "parse": [] }))
        };

        // Discord takes at most 10 embeds per message.
        for (i, chunk) in notes.chunks(10).enumerate() {
            let embeds: Vec<_> = chunk
                .iter()
                .map(|n| {
                    json!({
                        "title": n.title,
                        "description": n.description,
                        "color": n.color,
                        "footer": { "text": format!("facade · {}", self.origin) },
                    })
                })
                .collect();
            let body = json!({
                "username": "facade",
                "content": if i == 0 { content.as_str() } else { "" },
                "allowed_mentions": allowed,
                "embeds": embeds,
            });
            let mut response = self.client.post(webhook).json(&body).send().await?;
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let wait = response
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v["retry_after"].as_f64())
                    .unwrap_or(2.0);
                tokio::time::sleep(Duration::from_secs_f64(wait.min(30.0))).await;
                response = self.client.post(webhook).json(&body).send().await?;
            }
            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                bail!("Discord answered {status}: {text}");
            }
        }
        Ok(())
    }

    /// Sends a test message (pinging like a problem would), failing loudly when
    /// nothing is configured.
    pub async fn test(&self) -> anyhow::Result<()> {
        if !self.is_configured() {
            bail!("no Discord webhook set (facade alerts config --webhook <url>)");
        }
        self.send(&[Note::test()])
            .await
            .context("sending test message")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_after_two_checks_and_resolves() {
        let mut alerts = Alerts::default();
        let bad = HashMap::from([("k".to_string(), "broken".to_string())]);
        let (fired, _) = transition(&mut alerts, bad.clone());
        assert!(fired.is_empty());
        let (fired, _) = transition(&mut alerts, bad.clone());
        assert_eq!(fired.len(), 1);
        let (fired, _) = transition(&mut alerts, bad);
        assert!(fired.is_empty());
        let (_, cleared) = transition(&mut alerts, HashMap::new());
        assert_eq!(cleared.len(), 1);
        assert_eq!(alerts.views().len(), 1);
    }

    fn samples(cpu: &[f32], every: u64, now: u64) -> VecDeque<AppMetric> {
        let n = cpu.len() as u64;
        cpu.iter()
            .enumerate()
            .map(|(i, &cpu_percent)| AppMetric {
                at: now - (n - 1 - i as u64) * every,
                stats: facade_core::proto::AppStats {
                    app: "a".into(),
                    state: AppState::Running,
                    cpu_percent,
                    mem_used: 0,
                    mem_limit: 0,
                    net_rx: 0,
                    net_tx: 0,
                    restarts: 0,
                },
            })
            .collect()
    }

    #[test]
    fn cpu_alerts_need_a_sustained_average() {
        let now = 10_000;
        // A health check's spikes: a few samples at the limit, the rest idle.
        let spiky = samples(
            &[
                50.0, 49.0, 0.1, 0.1, 50.0, 0.1, 48.0, 0.1, 0.1, 50.0, 0.1, 0.1, 50.0,
            ],
            15,
            now,
        );
        assert!(!cpu_pinned(&spiky, 47.5, now));
        let pinned = samples(&[50.0; 13], 15, now);
        assert!(cpu_pinned(&pinned, 47.5, now));
        // Too little history (just started) is not enough to say.
        let short = samples(&[50.0; 4], 15, now);
        assert!(!cpu_pinned(&short, 47.5, now));
    }
}
