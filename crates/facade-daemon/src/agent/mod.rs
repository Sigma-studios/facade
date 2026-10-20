//! The agent: keeps one control connection to the server, converges Docker to the
//! desired workloads, and dials data connections on request.

mod build;
mod ingress;
mod ports;
mod runtime;
mod stats;
mod volumes;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use facade_core::proto::{
    AgentEvent, AgentMsg, AgentRequest, AppState, Hello, JoinToken, NodeReport, PROTOCOL_VERSION,
    ServerMsg, Workload,
};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::framing::{JsonLines, write_json};
use crate::tls;
pub use ingress::IngressOptions;
use runtime::Runtime;

const REPORT_EVERY: Duration = Duration::from_secs(15);
/// Re-check every app this often, so a removed or dead container comes back
/// without waiting for a reconnection.
const HEAL_EVERY: Duration = Duration::from_secs(120);
/// The server pings every 20 s; silence for this long means the link is dead.
const READ_TIMEOUT: Duration = Duration::from_secs(70);

/// `facaded agent`: join with a token once, then reuse the saved one.
pub async fn run_standalone(
    join: Option<String>,
    data_dir: PathBuf,
    ingress: IngressOptions,
) -> anyhow::Result<()> {
    tls::create_private_dir(&data_dir)?;
    let saved = data_dir.join("join-token");
    let token = match join {
        Some(raw) => {
            let token = JoinToken::decode(&raw).map_err(|e| anyhow!(e))?;
            tls::write_private(&saved, raw.trim().as_bytes())?;
            token
        }
        None => {
            let raw = std::fs::read_to_string(&saved).with_context(|| {
                format!(
                    "no join token: run once with --join <token> (looked in {})",
                    saved.display()
                )
            })?;
            JoinToken::decode(&raw).map_err(|e| anyhow!(e))?
        }
    };
    info!(node = %token.node, server = %token.server, "agent starting");
    Agent::new(token, data_dir, ingress).await?.run().await;
    Ok(())
}

pub struct Agent {
    token: JoinToken,
    runtime: Runtime,
    /// Last desired state received from the server.
    workloads: Mutex<HashMap<String, Workload>>,
    /// Serialises operations on the same app (a deploy and a sync must not interleave).
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Sender of the current control connection, if any.
    outbox: Mutex<Option<mpsc::Sender<AgentMsg>>>,
    /// Workloads whose last convergence failed, by hash. Periodic healing skips
    /// them so a broken build is not retried every two minutes.
    failed: Mutex<HashMap<String, String>>,
    certs: Arc<ingress::Certs>,
    routing: ingress::Routing,
    ingress: IngressOptions,
    /// Entry point addresses that cannot be bound yet, by address.
    ingress_errors: Mutex<std::collections::BTreeMap<std::net::SocketAddr, String>>,
}

impl Agent {
    pub async fn new(
        token: JoinToken,
        data_dir: PathBuf,
        ingress: IngressOptions,
    ) -> anyhow::Result<Arc<Self>> {
        let certs = Arc::new(ingress::Certs::load(&data_dir)?);
        let runtime = Runtime::new(data_dir, token.clone()).await?;
        Ok(Arc::new(Self {
            token,
            runtime,
            workloads: Default::default(),
            locks: Default::default(),
            outbox: Default::default(),
            failed: Default::default(),
            certs,
            routing: Default::default(),
            ingress,
            ingress_errors: Default::default(),
        }))
    }

    pub async fn run(self: Arc<Self>) {
        // Apps stay reachable while the server is away.
        ingress::run(self.clone(), self.ingress).await;
        let mut backoff = Duration::from_secs(1);
        loop {
            let started = std::time::Instant::now();
            match self.session().await {
                Ok(()) => info!("disconnected from server"),
                Err(e) => warn!("server connection: {e:#}"),
            }
            *self.outbox.lock() = None;
            if started.elapsed() > Duration::from_secs(60) {
                backoff = Duration::from_secs(1);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    async fn session(self: &Arc<Self>) -> anyhow::Result<()> {
        let stream = tls::connect_pinned(&self.token.server, &self.token.fingerprint).await?;
        let (read, mut write) = tokio::io::split(stream);
        write_json(
            &mut write,
            &Hello::Control {
                node: self.token.node.clone(),
                secret: self.token.secret.clone(),
                protocol: PROTOCOL_VERSION,
                agent_version: env!("CARGO_PKG_VERSION").into(),
                hostname: sysinfo::System::host_name().unwrap_or_default(),
            },
        )
        .await?;

        let (tx, mut rx) = mpsc::channel::<AgentMsg>(1024);
        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if write_json(&mut write, &msg).await.is_err() {
                    break;
                }
            }
        });
        let reporter = tokio::spawn(self.clone().report_loop(tx.clone()));
        let healer = tokio::spawn(self.clone().heal_loop());
        let _guard = AbortOnDrop(vec![writer, reporter, healer]);

        let mut lines = JsonLines::new(read);
        let mut connected = false;
        loop {
            let msg = tokio::time::timeout(READ_TIMEOUT, lines.next::<ServerMsg>())
                .await
                .context("server went silent")??;
            let Some(msg) = msg else { return Ok(()) };
            if !connected {
                connected = true;
                info!("connected to server {}", self.token.server);
                *self.outbox.lock() = Some(tx.clone());
            }
            match msg {
                ServerMsg::Ping => {
                    let _ = tx.send(AgentMsg::Pong).await;
                }
                ServerMsg::Rejected { reason } => bail!("server refused this agent: {reason}"),
                ServerMsg::Request { id, req } => {
                    let agent = self.clone();
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let (data, error) = match agent.handle(req).await {
                            Ok(data) => (data, None),
                            Err(e) => (None, Some(format!("{e:#}"))),
                        };
                        let _ = tx.send(AgentMsg::Response { id, error, data }).await;
                    });
                }
            }
        }
    }

    fn emit(&self, event: AgentEvent) {
        if let Some(tx) = self.outbox.lock().as_ref() {
            let _ = tx.try_send(AgentMsg::Event { event });
        }
    }

    fn lock_for(&self, app: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .entry(app.to_string())
            .or_default()
            .clone()
    }

    /// Requests answer quickly; long work (builds) runs in the background and
    /// reports through events. Some requests answer with data.
    async fn handle(
        self: &Arc<Self>,
        req: AgentRequest,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        match req {
            AgentRequest::Sync { workloads } => {
                let map: HashMap<_, _> = workloads
                    .into_iter()
                    .map(|w| (w.spec.name.clone(), w))
                    .collect();
                *self.workloads.lock() = map;
                tokio::spawn(self.clone().reconcile_all());
            }
            AgentRequest::Apply {
                workload,
                deployment,
            } => {
                self.workloads
                    .lock()
                    .insert(workload.spec.name.clone(), workload.clone());
                tokio::spawn(self.clone().converge(workload, deployment));
            }
            AgentRequest::Remove {
                app,
                delete_volumes,
            } => {
                self.workloads.lock().remove(&app);
                let lock = self.lock_for(&app);
                let _held = lock.lock().await;
                self.runtime.remove(&app, delete_volumes).await?;
            }
            AgentRequest::Restart { app } => self.runtime.restart(&app).await?,
            AgentRequest::Ingress { certs, proxies } => {
                self.certs.replace(&certs);
                *self.routing.proxies.write() = proxies;
            }
            AgentRequest::Challenge {
                token,
                key_authorization,
            } => {
                let mut challenges = self.routing.challenges.write();
                match key_authorization {
                    Some(key) => challenges.insert(token, key),
                    None => challenges.remove(&token),
                };
            }
            AgentRequest::Logs {
                tunnel,
                app,
                tail,
                follow,
            } => {
                let stream = self.dial_data(tunnel).await?;
                let runtime = self.runtime.clone();
                tokio::spawn(async move {
                    if let Err(e) = runtime.stream_logs(&app, tail, follow, stream).await {
                        tracing::debug!("log stream for {app} ended: {e:#}");
                    }
                });
            }
            AgentRequest::Volumes => {
                return Ok(Some(serde_json::to_value(
                    self.runtime.list_volumes().await?,
                )?));
            }
            AgentRequest::VolumeExport {
                tunnel,
                app,
                volume,
            } => {
                let stream = self.dial_data(tunnel).await?;
                let agent = self.clone();
                tokio::spawn(async move {
                    // Exports and imports wait for deploys of the app, and the
                    // reverse, so a container is never recreated mid-copy.
                    let lock = agent.lock_for(&app);
                    let _held = lock.lock().await;
                    if let Err(e) = agent.runtime.export_volume(&app, &volume, stream).await {
                        warn!("exporting volume {volume} of {app}: {e:#}");
                    }
                });
            }
            AgentRequest::VolumeImport {
                tunnel,
                app,
                volume,
            } => {
                let stream = self.dial_data(tunnel).await?;
                let agent = self.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut stream = stream;
                    let lock = agent.lock_for(&app);
                    let _held = lock.lock().await;
                    let result = async {
                        let size: u64 = crate::framing::read_line_unbuffered(&mut stream)
                            .await?
                            .trim()
                            .parse()?;
                        let upload = (&mut stream).take(size);
                        agent
                            .runtime
                            .import_volume(&app, &volume, upload, size)
                            .await
                    }
                    .await;
                    let answer = match result {
                        Ok(()) => {
                            info!("restored volume {volume} of {app}");
                            "ok\n".to_string()
                        }
                        Err(e) => {
                            warn!("importing volume {volume} of {app}: {e:#}");
                            format!("error {}\n", format!("{e:#}").replace('\n', " "))
                        }
                    };
                    let _ = stream.write_all(answer.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        }
        Ok(None)
    }

    async fn converge(self: Arc<Self>, workload: Workload, deployment: Option<Uuid>) {
        let app = workload.spec.name.clone();
        let lock = self.lock_for(&app);
        let _held = lock.lock().await;
        let log = DeployLog {
            agent: self.clone(),
            deployment,
            app: app.clone(),
        };
        let result = self.runtime.converge(&workload, &log).await;
        let hash = workload_hash(&workload);
        match &result {
            Ok(_) => self.failed.lock().remove(&app),
            Err(_) => self.failed.lock().insert(app.clone(), hash),
        };
        let (state, message) = match &result {
            Ok(state) => (*state, None),
            Err(e) => {
                log.line(format!("error: {e:#}"));
                (AppState::Failed, Some(format!("{e:#}")))
            }
        };
        self.emit(AgentEvent::AppState {
            app,
            state,
            message,
        });
        if let Some(deployment) = deployment {
            self.emit(AgentEvent::DeployDone {
                deployment,
                error: result.err().map(|e| format!("{e:#}")),
            });
        }
    }

    async fn reconcile_all(self: Arc<Self>) {
        let workloads: Vec<Workload> = self.workloads.lock().values().cloned().collect();
        let wanted: Vec<String> = workloads.iter().map(|w| w.spec.name.clone()).collect();
        match self.runtime.managed_apps().await {
            Ok(present) => {
                for app in present.into_iter().filter(|a| !wanted.contains(a)) {
                    info!("removing {app}: no longer assigned to this node");
                    let lock = self.lock_for(&app);
                    let _held = lock.lock().await;
                    if let Err(e) = self.runtime.remove(&app, false).await {
                        warn!("removing {app}: {e:#}");
                    }
                }
            }
            Err(e) => warn!("listing managed apps: {e:#}"),
        }
        // One at a time: parallel builds would starve a small VPS.
        for workload in workloads {
            self.clone().converge(workload, None).await;
        }
    }

    async fn heal_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(HEAL_EVERY);
        tick.tick().await;
        loop {
            tick.tick().await;
            let workloads: Vec<Workload> = self.workloads.lock().values().cloned().collect();
            for workload in workloads {
                let known_bad = self
                    .failed
                    .lock()
                    .get(&workload.spec.name)
                    .is_some_and(|h| *h == workload_hash(&workload));
                if !known_bad {
                    self.clone().converge(workload, None).await;
                }
            }
        }
    }

    async fn report_loop(self: Arc<Self>, tx: mpsc::Sender<AgentMsg>) {
        let mut sampler = stats::NodeSampler::new();
        let mut tick = tokio::time::interval(REPORT_EVERY);
        loop {
            tick.tick().await;
            let workloads: Vec<Workload> = self.workloads.lock().values().cloned().collect();
            let listeners = match self.runtime.port_owners().await {
                Ok((containers, published)) => {
                    tokio::task::spawn_blocking(move || ports::scan(&containers, &published))
                        .await
                        .unwrap_or_default()
                }
                Err(e) => {
                    warn!("listing containers for port owners: {e:#}");
                    vec![]
                }
            };
            let report = NodeReport {
                node: sampler.sample(),
                apps: self.runtime.app_stats(&workloads).await,
                listeners,
                ingress_errors: self.ingress_errors.lock().values().cloned().collect(),
            };
            if tx
                .send(AgentMsg::Event {
                    event: AgentEvent::Report(report),
                })
                .await
                .is_err()
            {
                return;
            }
        }
    }

    async fn dial_data(&self, tunnel: Uuid) -> anyhow::Result<tls::ClientStream> {
        let mut stream = tls::connect_pinned(&self.token.server, &self.token.fingerprint).await?;
        write_json(
            &mut stream,
            &Hello::Data {
                node: self.token.node.clone(),
                secret: self.token.secret.clone(),
                tunnel,
            },
        )
        .await?;
        Ok(stream)
    }
}

/// Downloads an uploaded executable from the server and checks its hash.
pub async fn fetch_artifact(
    token: &JoinToken,
    sha256: &str,
    dest: &std::path::Path,
) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tls::connect_pinned(&token.server, &token.fingerprint).await?;
    write_json(
        &mut stream,
        &Hello::Fetch {
            node: token.node.clone(),
            secret: token.secret.clone(),
            sha256: sha256.to_string(),
        },
    )
    .await?;
    let status = crate::framing::read_line_unbuffered(&mut stream).await?;
    let size: u64 = match status.split_once(' ') {
        Some(("ok", size)) => size.parse()?,
        Some(("error", message)) => bail!("{message}"),
        _ => bail!("unexpected answer from server: {status}"),
    };
    let mut file = tokio::fs::File::create(dest).await?;
    let mut hasher = Sha256::new();
    let mut remaining = size;
    let mut buf = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            bail!("download interrupted");
        }
        let n = n.min(remaining as usize);
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).await?;
        remaining -= n as u64;
    }
    file.flush().await?;
    if hex::encode(hasher.finalize()) != sha256 {
        bail!("downloaded file does not match its checksum");
    }
    Ok(())
}

/// Build/deploy output, forwarded to the server when it belongs to a deployment.
pub struct DeployLog {
    agent: Arc<Agent>,
    deployment: Option<Uuid>,
    app: String,
}

impl DeployLog {
    pub fn line(&self, line: impl Into<String>) {
        let line = line.into();
        tracing::debug!(app = %self.app, "{line}");
        if let Some(deployment) = self.deployment {
            self.agent.emit(AgentEvent::DeployLog { deployment, line });
        }
    }

    pub fn state(&self, state: AppState) {
        self.agent.emit(AgentEvent::AppState {
            app: self.app.clone(),
            state,
            message: None,
        });
    }

    pub fn is_deployment(&self) -> bool {
        self.deployment.is_some()
    }
}

fn workload_hash(w: &Workload) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(serde_json::to_vec(w).unwrap_or_default()))
}

struct AbortOnDrop(Vec<tokio::task::JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}
