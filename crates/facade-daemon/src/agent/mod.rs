//! The agent: keeps one control connection to the server, converges Docker to the
//! desired workloads, and dials data connections on request.

mod build;
mod runtime;
mod stats;

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
use runtime::Runtime;

const REPORT_EVERY: Duration = Duration::from_secs(15);
/// Re-check every app this often, so a removed or dead container comes back
/// without waiting for a reconnection.
const HEAL_EVERY: Duration = Duration::from_secs(120);
/// The server pings every 20 s; silence for this long means the link is dead.
const READ_TIMEOUT: Duration = Duration::from_secs(70);

/// `facaded agent`: join with a token once, then reuse the saved one.
pub async fn run_standalone(join: Option<String>, data_dir: PathBuf) -> anyhow::Result<()> {
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
    Agent::new(token, data_dir).await?.run().await;
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
}

impl Agent {
    pub async fn new(token: JoinToken, data_dir: PathBuf) -> anyhow::Result<Arc<Self>> {
        let runtime = Runtime::new(data_dir, token.node.clone()).await?;
        Ok(Arc::new(Self {
            token,
            runtime,
            workloads: Default::default(),
            locks: Default::default(),
            outbox: Default::default(),
            failed: Default::default(),
        }))
    }

    pub async fn run(self: Arc<Self>) {
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
                        let error = agent.handle(req).await.err().map(|e| format!("{e:#}"));
                        let _ = tx.send(AgentMsg::Response { id, error }).await;
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
    /// reports through events.
    async fn handle(self: &Arc<Self>, req: AgentRequest) -> anyhow::Result<()> {
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
            AgentRequest::Remove { app } => {
                self.workloads.lock().remove(&app);
                let lock = self.lock_for(&app);
                let _held = lock.lock().await;
                self.runtime.remove(&app).await?;
            }
            AgentRequest::Restart { app } => self.runtime.restart(&app).await?,
            AgentRequest::OpenTunnel { tunnel, app } => {
                let workload = self
                    .workloads
                    .lock()
                    .get(&app)
                    .cloned()
                    .ok_or_else(|| anyhow!("app {app} is not on this node"))?;
                self.open_tunnel(tunnel, workload).await?;
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
        }
        Ok(())
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
                    if let Err(e) = self.runtime.remove(&app).await {
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
            let report = NodeReport {
                node: sampler.sample(),
                apps: self.runtime.app_stats(&workloads).await,
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

    async fn open_tunnel(&self, tunnel: Uuid, workload: Workload) -> anyhow::Result<()> {
        if !workload.spec.enabled {
            bail!("app {} is stopped", workload.spec.name);
        }
        if workload.spec.is_static() {
            let root = self.runtime.static_root(&workload)?;
            let stream = self.dial_data(tunnel).await?;
            tokio::spawn(serve_static(stream, root));
            return Ok(());
        }
        let target = self.runtime.http_target(&workload).await?;
        let mut upstream = tokio::net::TcpStream::connect(target)
            .await
            .with_context(|| format!("connecting to {} on {target}", workload.spec.name))?;
        upstream.set_nodelay(true)?;
        let mut stream = self.dial_data(tunnel).await?;
        tokio::spawn(async move {
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
        });
        Ok(())
    }
}

async fn serve_static(stream: tls::ClientStream, root: PathBuf) {
    use tower_http::services::{ServeDir, ServeFile};
    use tower_http::set_status::SetStatus;
    let not_found = SetStatus::new(
        ServeFile::new(root.join("404.html")),
        hyper::StatusCode::NOT_FOUND,
    );
    let service = ServeDir::new(&root)
        .append_index_html_on_directories(true)
        .precompressed_gzip()
        .precompressed_br()
        .not_found_service(not_found);
    let service = hyper_util::service::TowerToHyperService::new(service);
    let io = hyper_util::rt::TokioIo::new(stream);
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, service)
        .await
    {
        tracing::debug!("static connection: {e}");
    }
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
