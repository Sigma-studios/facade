//! The control plane.

mod acme;
mod api;
mod artifacts;
pub mod backups;
mod compose;
mod dns;
mod git;
mod hub;
mod inspect;
mod monitor;
pub mod secrets;
mod settings;
mod stack;
pub mod store;
mod ui;
mod updates;
pub mod users;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use anyhow::{Context, anyhow};
use facade_core::api::{AppMetric, NodeMetric};
use facade_core::proto::{
    AgentRequest, AppState, AppStats, BuildEntry, JoinToken, Listener, NodeStats, RegistryLogin,
    StorageReport, Workload,
};
use parking_lot::RwLock;
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tracing::{error, info, warn};

use crate::config::ServerConfig;
use crate::tls::{self, Identity};
use hub::Hub;
use secrets::SecretBox;
use store::Store;

/// One hour of samples at the agents' 15 s reporting interval.
const HISTORY: usize = 240;

pub struct Server {
    pub cfg: ServerConfig,
    pub store: Store,
    pub secrets: SecretBox,
    pub identity: Identity,
    pub hub: Hub,
    pub live: RwLock<Live>,
    pub routes: RwLock<Routes>,
    pub dns: dns::Dns,
    /// Last DNS (`dns:<domain>`) and certificate (`cert:<name>`) failures.
    pub domain_errors: RwLock<HashMap<String, String>>,
    /// `public_host`, resolved at start.
    pub public_host_ip: Option<String>,
    /// Re-checks DNS records and certificates.
    pub domains_wake: Notify,
    pub alerts: parking_lot::Mutex<monitor::Alerts>,
    pub notifier: monitor::Notifier,
    /// Stack deployments in progress.
    pub stack_runs: parking_lot::Mutex<HashMap<uuid::Uuid, stack::StackRun>>,
    /// Last failed backups, `app/volume` → why. Cleared by the next success.
    pub backup_errors: RwLock<HashMap<String, String>>,
    pub settings: settings::SettingsCell,
    /// Last binary sent to each node to update its agent, and when.
    pub upgrade_attempts: parking_lot::Mutex<HashMap<String, (String, u64)>>,
}

/// Runtime state reported by agents; not persisted.
#[derive(Default)]
pub struct Live {
    pub nodes: HashMap<String, NodeLive>,
    pub apps: HashMap<String, AppLive>,
}

#[derive(Default)]
pub struct NodeLive {
    pub stats: Option<NodeStats>,
    pub history: VecDeque<NodeMetric>,
    /// When the node was last seen disconnecting (or the server started).
    pub offline_since: Option<u64>,
    /// Its build queue, as last reported.
    pub builds: Vec<BuildEntry>,
    pub storage: Option<StorageReport>,
    /// Listening sockets, from the last report.
    pub listeners: Vec<Listener>,
    pub ingress_errors: Vec<String>,
}

pub struct AppLive {
    pub state: AppState,
    pub message: Option<String>,
    pub stats: Option<AppStats>,
    pub history: VecDeque<AppMetric>,
}

impl Default for AppLive {
    fn default() -> Self {
        Self {
            state: AppState::Unknown,
            message: None,
            stats: None,
            history: VecDeque::new(),
        }
    }
}

/// Derived from the apps table; rebuilt whenever apps change.
#[derive(Default)]
pub struct Routes {
    /// domain → app, redirect domains included
    pub domains: HashMap<String, String>,
    /// app → node
    pub nodes: HashMap<String, String>,
}

pub fn hash_secret(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn push_capped<T>(deque: &mut VecDeque<T>, item: T) {
    if deque.len() == HISTORY {
        deque.pop_front();
    }
    deque.push_back(item);
}

/// What workloads are built from: the database and the key to its secrets. Apart
/// from [`Server`] so tools (`facaded upgrade-check`) build workloads exactly as a
/// running server would, without one.
#[derive(Clone, Copy)]
pub struct Catalog<'a> {
    pub store: &'a Store,
    pub secrets: &'a SecretBox,
}

impl Catalog<'_> {
    /// The full workload for an app: secrets decrypted, references between apps'
    /// variables resolved, and its owner's credentials.
    pub fn workload(&self, app: &str) -> anyhow::Result<Workload> {
        let row = self
            .store
            .get_app(app)?
            .ok_or_else(|| anyhow!("no app named {app}"))?;
        let file_env = self.stack_env(&row)?;
        let owner = row.owner;
        self.workload_of(
            row.spec,
            row.revision,
            row.commit,
            row.build_key,
            &file_env,
            owner,
        )
    }

    /// The workload of `spec` at a revision: its variables (those a stack's file
    /// gives it, then those set in facade) with references resolved, and the
    /// git credential and registry logins of `owner` (a member, or the admins).
    pub fn workload_of(
        &self,
        spec: facade_core::AppSpec,
        revision: u64,
        commit: Option<String>,
        build_key: Option<String>,
        file_env: &[(String, String, bool)],
        owner: Option<i64>,
    ) -> anyhow::Result<Workload> {
        let app = spec.name.clone();
        let mut vars: Vec<(String, String)> = file_env
            .iter()
            .map(|(k, v, _)| (k.clone(), v.clone()))
            .collect();
        for e in self.store.list_env(&app)? {
            let value = self.secrets.open_string(&e.sealed)?;
            vars.retain(|(k, _)| *k != e.key);
            vars.push((e.key, value));
        }
        let env = vars
            .into_iter()
            .map(|(key, raw)| {
                let value = self
                    .resolve_references(&raw, 0)
                    .with_context(|| format!("{app}: variable {key}"))?;
                Ok((key, value))
            })
            .collect::<anyhow::Result<_>>()?;
        let owner = owner.unwrap_or(store::ADMINS);
        let git_auth = match &spec.source {
            facade_core::Source::Git { repo, .. } => {
                git::credential_for(*self, owner, repo)?.map(|(_, auth)| auth)
            }
            _ => None,
        };
        let mut aliases: Vec<String> = self
            .store
            .list_apps()?
            .iter()
            .flat_map(|a| &a.spec.links)
            .filter(|l| l.app == app && l.host() != app)
            .map(|l| l.host().to_string())
            .collect();
        aliases.sort();
        aliases.dedup();
        Ok(Workload {
            spec,
            env,
            revision,
            commit,
            git_auth,
            build_key,
            aliases,
            registries: self.registry_logins(owner)?,
        })
    }

    /// The registry logins of `owner` (a user id, or the admins'), decrypted.
    pub fn registry_logins(&self, owner: i64) -> anyhow::Result<Vec<RegistryLogin>> {
        self.store
            .list_registry_logins(Some(owner))?
            .into_iter()
            .map(|r| {
                Ok(RegistryLogin {
                    host: r.host,
                    username: r.username,
                    secret: self.secrets.open_string(&r.sealed_secret)?,
                })
            })
            .collect()
    }

    /// The variables a stack's compose file gives one of its services:
    /// `(key, value, sensitive)`.
    pub fn stack_env(&self, row: &store::AppRow) -> anyhow::Result<Vec<(String, String, bool)>> {
        match &row.stack_env {
            Some(sealed) => Ok(serde_json::from_slice(&self.secrets.open(sealed)?)?),
            None => Ok(vec![]),
        }
    }

    /// The decrypted value of one variable, as stored (set in facade, or given by
    /// a stack's file).
    fn raw_env(&self, app: &str, key: &str) -> anyhow::Result<Option<String>> {
        if let Some(e) = self.store.list_env(app)?.into_iter().find(|e| e.key == key) {
            return Ok(Some(self.secrets.open_string(&e.sealed)?));
        }
        let Some(row) = self.store.get_app(app)? else {
            return Ok(None);
        };
        Ok(self
            .stack_env(&row)?
            .into_iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, v, _)| v))
    }

    /// `value` with `${{app.KEY}}` references replaced, recursively.
    fn resolve_references(&self, value: &str, depth: u8) -> anyhow::Result<String> {
        if depth > 4 {
            anyhow::bail!("variable references nest too deeply (do two refer to each other?)");
        }
        facade_core::env::substitute(value, |app, key| {
            let raw = self
                .raw_env(app, key)
                .map_err(|e| format!("{e:#}"))?
                .ok_or_else(|| {
                    format!(
                        "{} refers to a variable that is not set",
                        facade_core::env::reference(app, key)
                    )
                })?;
            self.resolve_references(&raw, depth + 1)
                .map_err(|e| format!("{e:#}"))
        })
        .map_err(|e| anyhow!(e))
    }
}

impl Server {
    pub fn catalog(&self) -> Catalog<'_> {
        Catalog {
            store: &self.store,
            secrets: &self.secrets,
        }
    }

    /// The full workload for an app: secrets decrypted, references between apps'
    /// variables resolved, and its owner's credentials.
    pub fn workload(&self, app: &str) -> anyhow::Result<Workload> {
        self.catalog().workload(app)
    }

    /// See [`Catalog::workload_of`].
    pub fn workload_of(
        &self,
        spec: facade_core::AppSpec,
        revision: u64,
        commit: Option<String>,
        build_key: Option<String>,
        file_env: &[(String, String, bool)],
        owner: Option<i64>,
    ) -> anyhow::Result<Workload> {
        self.catalog()
            .workload_of(spec, revision, commit, build_key, file_env, owner)
    }

    pub fn stack_env(&self, row: &store::AppRow) -> anyhow::Result<Vec<(String, String, bool)>> {
        self.catalog().stack_env(row)
    }

    /// Apps whose variables refer to `app`'s, directly or through others.
    pub fn dependents(&self, app: &str) -> anyhow::Result<Vec<String>> {
        let mut refs: Vec<(String, Vec<String>)> = Vec::new();
        for a in self.store.list_apps()? {
            let mut targets = Vec::new();
            let mut values: Vec<String> =
                self.stack_env(&a)?.into_iter().map(|(_, v, _)| v).collect();
            for e in self.store.list_env(&a.spec.name)? {
                values.push(self.secrets.open_string(&e.sealed)?);
            }
            for raw in values {
                targets.extend(
                    facade_core::env::references(&raw)
                        .into_iter()
                        .map(|(t, _)| t),
                );
            }
            refs.push((a.spec.name, targets));
        }
        let mut found: Vec<String> = Vec::new();
        let mut frontier = vec![app.to_string()];
        while let Some(current) = frontier.pop() {
            for (name, targets) in &refs {
                if targets.contains(&current) && *name != app && !found.contains(name) {
                    found.push(name.clone());
                    frontier.push(name.clone());
                }
            }
        }
        Ok(found)
    }

    /// Apps run from their first deploy on; before that they are only configuration.
    pub fn node_workloads(&self, node: &str) -> anyhow::Result<Vec<Workload>> {
        self.store
            .list_apps()?
            .into_iter()
            .filter(|a| a.spec.node == node && a.revision > 0)
            .map(|a| self.workload(&a.spec.name))
            .collect()
    }

    /// Sends an app's current desired state to its node, if connected. A node that
    /// is offline gets everything on reconnection anyway.
    pub async fn push(&self, app: &str) -> anyhow::Result<()> {
        let workload = self.workload(app)?;
        let node = workload.spec.node.clone();
        if workload.revision == 0 || !self.hub.is_connected(&node) {
            return Ok(());
        }
        self.hub
            .request(
                &node,
                AgentRequest::Apply {
                    workload,
                    deployment: None,
                    rebuild: false,
                    phase: facade_core::proto::ApplyPhase::All,
                },
            )
            .await
    }

    pub fn refresh_routes(&self) -> anyhow::Result<()> {
        let mut routes = Routes::default();
        for app in self.store.list_apps()? {
            routes
                .nodes
                .insert(app.spec.name.clone(), app.spec.node.clone());
            for domain in app.spec.domains.iter().chain(&app.spec.redirects) {
                routes.domains.insert(domain.clone(), app.spec.name.clone());
            }
        }
        *self.routes.write() = routes;
        self.domains_wake.notify_one();
        Ok(())
    }

    pub fn record_report(&self, node: &str, report: facade_core::proto::NodeReport) {
        let at = facade_core::now();
        let mut live = self.live.write();
        let entry = live.nodes.entry(node.to_string()).or_default();
        entry.stats = Some(report.node.clone());
        if report.storage.is_some() {
            entry.storage = report.storage;
        }
        entry.listeners = report.listeners;
        entry.ingress_errors = report.ingress_errors;
        push_capped(
            &mut entry.history,
            NodeMetric {
                at,
                stats: report.node,
            },
        );
        for stats in report.apps {
            let app = live.apps.entry(stats.app.clone()).or_default();
            // A build in progress is more informative than "the old container runs".
            if app.state != AppState::Building {
                app.state = stats.state;
            }
            app.stats = Some(stats.clone());
            push_capped(&mut app.history, AppMetric { at, stats });
        }
    }

    pub fn set_app_state(&self, app: &str, state: AppState, message: Option<String>) {
        let mut live = self.live.write();
        let entry = live.apps.entry(app.to_string()).or_default();
        entry.state = state;
        if message.is_some() || state == AppState::Running {
            entry.message = message;
        }
    }

    /// Creates or re-keys a node, returning its join token.
    pub fn issue_join_token(&self, node: &str) -> anyhow::Result<String> {
        let host = self.cfg.public_host.clone().ok_or_else(|| {
            anyhow!("set `public_host` in the server config so other nodes can reach this server")
        })?;
        let secret = random_token();
        self.store.upsert_node_secret(node, &hash_secret(&secret))?;
        Ok(JoinToken {
            server: format!("{host}:{}", self.cfg.node_listen.port()),
            fingerprint: self.identity.fingerprint.clone(),
            node: node.to_string(),
            secret,
        }
        .encode())
    }
}

pub async fn run(cfg: ServerConfig) -> anyhow::Result<()> {
    tls::create_private_dir(&cfg.data_dir)
        .with_context(|| format!("creating {}", cfg.data_dir.display()))?;
    let store = Store::open(&cfg.data_dir.join("facade.db"))?;
    let secrets = SecretBox::load_or_create(&cfg.data_dir.join("master.key"))?;
    let settings = settings::SettingsCell::load(&store, &cfg)?;
    let identity =
        tls::load_or_create_identity(&cfg.data_dir, cfg.public_host.iter().cloned().collect())?;

    let origin = cfg
        .public_host
        .clone()
        .or_else(sysinfo::System::host_name)
        .unwrap_or_else(|| cfg.local_node.clone());
    let notifier = monitor::Notifier::new(origin)?;
    let public_host_ip = match &cfg.public_host {
        Some(host) => tokio::net::lookup_host((host.as_str(), 0))
            .await
            .ok()
            .and_then(|mut a| a.find(|a| a.is_ipv4()).or_else(|| a.next()))
            .map(|a| a.ip().to_string()),
        None => None,
    };
    let server = Arc::new(Server {
        store,
        secrets,
        identity,
        hub: Hub::default(),
        live: Default::default(),
        routes: Default::default(),
        dns: Default::default(),
        domain_errors: Default::default(),
        public_host_ip,
        domains_wake: Notify::new(),
        alerts: Default::default(),
        notifier,
        stack_runs: Default::default(),
        backup_errors: Default::default(),
        settings,
        upgrade_attempts: Default::default(),
        cfg,
    });
    if let Err(e) = dns::load(&server).await {
        warn!("DNS provider: {e:#}");
    }
    server.notifier.load(&server.store, &server.secrets)?;
    server.refresh_routes()?;
    api::ensure_admin(&server)?;

    // Every node starts offline until its agent connects.
    {
        let now = facade_core::now();
        let mut live = server.live.write();
        for node in server.store.list_nodes()? {
            live.nodes.entry(node.name).or_default().offline_since = Some(now);
        }
    }

    // The server's own machine is a node like any other, with an in-process agent.
    let local_secret = random_token();
    server
        .store
        .upsert_node_secret(&server.cfg.local_node, &hash_secret(&local_secret))?;
    let local_token = JoinToken {
        server: server.cfg.loopback_node_addr().to_string(),
        fingerprint: server.identity.fingerprint.clone(),
        node: server.cfg.local_node.clone(),
        secret: local_secret,
    };

    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(hub::listen(server.clone()));
    tasks.spawn(api::serve_direct(server.clone()));
    // Always there, so an API domain set later works without a restart.
    {
        let server = server.clone();
        tokio::spawn(async move {
            if let Err(e) = api::serve_loopback(server).await {
                warn!("loopback API (used by the API domain): {e:#}");
            }
        });
    }
    tasks.spawn(acme::run(server.clone()));
    tasks.spawn(monitor::run(server.clone()));
    tasks.spawn(artifacts::collect_garbage(server.clone()));
    tasks.spawn(backups::run(server.clone()));

    let agent_dir = server.cfg.data_dir.join("agent");
    let ingress = match server.cfg.ingress {
        crate::config::IngressSetting::Embedded => crate::agent::IngressMode::Embedded {
            http: Some(server.cfg.http_listen),
            https: Some(server.cfg.https_listen),
        },
        crate::config::IngressSetting::External => crate::agent::IngressMode::External {
            control: server.cfg.ingress_control.clone(),
        },
    };
    tokio::spawn(async move {
        loop {
            match crate::agent::Agent::new(local_token.clone(), agent_dir.clone(), ingress.clone())
                .await
            {
                Ok(agent) => return agent.run().await,
                Err(e) => {
                    error!("local agent cannot start, retrying in 30s: {e:#}");
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                }
            }
        }
    });

    info!(
        "facade server ready. API: https://<host>:{} (certificate fingerprint {})",
        server.cfg.api_listen.port(),
        server.identity.fingerprint
    );
    if server.cfg.public_host.is_none() {
        warn!("public_host is not set: remote nodes cannot be added until it is");
    }

    tokio::select! {
        Some(result) = tasks.join_next() => {
            result?.context("a server task stopped")?;
            Err(anyhow!("a server task stopped unexpectedly"))
        }
        _ = shutdown_signal() => {
            info!("shutting down");
            Ok(())
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
