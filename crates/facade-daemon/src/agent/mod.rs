//! The agent: keeps one control connection to the server, converges Docker to the
//! desired workloads, and dials data connections on request.

mod build;
mod maintenance;
mod ports;
mod queue;
mod routes;
mod runtime;
mod stats;
mod volumes;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use facade_core::proto::{
    AgentEvent, AgentMsg, AgentRequest, AppState, ApplyPhase, CertBundle, Cleanup, Hello,
    JoinToken, NodeReport, PROTOCOL_VERSION, ProxyRoute, ServerMsg, StorageReport, Workload,
};
use futures_util::future::AbortHandle;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::framing::{JsonLines, write_json};
use crate::tls;
use queue::BuildQueue;
pub use routes::IngressMode;
use runtime::Runtime;
pub use runtime::{planned_image, run_key_matches};

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
    ingress: IngressMode,
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
    /// What the server sent for the entry point.
    certs: Mutex<Vec<CertBundle>>,
    proxies: Mutex<Vec<ProxyRoute>>,
    challenges: Mutex<std::collections::BTreeMap<String, String>>,
    link: routes::Link,
    /// One build at a time; the others wait here.
    queue: Arc<BuildQueue>,
    /// Deployments still queued or building, which a cancel can stop.
    cancellable: Mutex<HashMap<Uuid, Cancellable>>,
    /// Latest storage report, and whether the server still has to get it.
    storage: Mutex<(Option<StorageReport>, bool)>,
    last_cleanup: Mutex<Option<Cleanup>>,
}

struct Cancellable {
    app: String,
    /// The workload's hash, marked as failed so healing does not build it again.
    hash: String,
    abort: AbortHandle,
}

impl Agent {
    pub async fn new(
        token: JoinToken,
        data_dir: PathBuf,
        ingress: IngressMode,
    ) -> anyhow::Result<Arc<Self>> {
        let link = routes::Link::new(ingress, &data_dir)?;
        let runtime = Runtime::new(data_dir, token.clone()).await?;
        Ok(Arc::new(Self {
            token,
            runtime,
            workloads: Default::default(),
            locks: Default::default(),
            outbox: Default::default(),
            failed: Default::default(),
            certs: Default::default(),
            proxies: Default::default(),
            challenges: Default::default(),
            link,
            queue: BuildQueue::new(1),
            cancellable: Default::default(),
            storage: Default::default(),
            last_cleanup: Default::default(),
        }))
    }

    fn set_storage(&self, report: StorageReport) {
        *self.storage.lock() = (Some(report), true);
    }

    pub async fn run(self: Arc<Self>) {
        // Apps stay reachable while the server is away.
        self.start_routing();
        tokio::spawn(maintenance::run(self.clone()));
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
                binary: crate::selfbin::current().map(|b| b.sha256.clone()),
                arch: Some(std::env::consts::ARCH.into()),
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
        // The build queue, now and whenever it changes.
        let builds = tokio::spawn({
            let tx = tx.clone();
            let mut rx = self.queue.subscribe();
            async move {
                loop {
                    let builds = rx.borrow_and_update().clone();
                    let event = AgentEvent::Builds { builds };
                    if tx.send(AgentMsg::Event { event }).await.is_err() {
                        break;
                    }
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            }
        });
        self.storage.lock().1 = true;
        let _guard = AbortOnDrop(vec![writer, reporter, healer, builds]);

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
    /// reports through events.
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
                rebuild,
                phase,
            } => {
                let app = workload.spec.name.clone();
                let hash = workload_hash(&workload);
                let task: futures_util::future::BoxFuture<'static, ()> = match phase {
                    // Only the image: what runs, and should run, stays as it is.
                    ApplyPhase::Build => {
                        Box::pin(self.clone().prepare(workload, deployment, rebuild))
                    }
                    ApplyPhase::All | ApplyPhase::Release => {
                        self.workloads
                            .lock()
                            .insert(workload.spec.name.clone(), workload.clone());
                        let pull = phase == ApplyPhase::All;
                        Box::pin(self.clone().converge(workload, deployment, rebuild, pull))
                    }
                };
                match deployment {
                    Some(id) => {
                        let (task, abort) = futures_util::future::abortable(task);
                        self.cancellable
                            .lock()
                            .insert(id, Cancellable { app, hash, abort });
                        tokio::spawn(task);
                    }
                    None => {
                        tokio::spawn(task);
                    }
                }
            }
            AgentRequest::Cancel { deployment } => {
                let Some(c) = self.cancellable.lock().remove(&deployment) else {
                    // Finished, or past its build: the server sees which.
                    return Ok(None);
                };
                c.abort.abort();
                self.failed.lock().insert(c.app.clone(), c.hash);
                self.emit(AgentEvent::DeployLog {
                    deployment,
                    line: "cancelled".into(),
                });
                self.emit(AgentEvent::DeployDone {
                    deployment,
                    error: Some("cancelled".into()),
                    cancelled: true,
                });
                // The version that ran before keeps running.
                let workload = self.workloads.lock().get(&c.app).cloned();
                let state = match workload {
                    Some(w) => self.runtime.app_stats(&[w]).await[0].state,
                    None => AppState::Unknown,
                };
                self.emit(AgentEvent::AppState {
                    app: c.app,
                    state,
                    message: Some("deployment cancelled".into()),
                });
            }
            AgentRequest::Remove { app, volumes } => {
                self.workloads.lock().remove(&app);
                let lock = self.lock_for(&app);
                let _held = lock.lock().await;
                self.runtime.remove(&app, volumes).await?;
                self.refresh_routes().await;
            }
            AgentRequest::Prune { aggressive } => {
                let agent = self.clone();
                tokio::spawn(async move {
                    maintenance::cleanup(&agent, aggressive).await;
                    maintenance::refresh_report(&agent).await;
                });
            }
            AgentRequest::RemoveVolume { name } => {
                self.runtime.remove_volume(&name).await?;
                let agent = self.clone();
                tokio::spawn(async move { maintenance::refresh_report(&agent).await });
            }
            AgentRequest::Restart { app } => self.runtime.restart(&app).await?,
            AgentRequest::Ingress { certs, proxies } => {
                *self.certs.lock() = certs;
                *self.proxies.lock() = proxies;
                self.refresh_routes().await;
            }
            AgentRequest::Challenge {
                token,
                key_authorization,
            } => {
                {
                    let mut challenges = self.challenges.lock();
                    match key_authorization {
                        Some(key) => challenges.insert(token, key),
                        None => challenges.remove(&token),
                    };
                }
                self.refresh_routes().await;
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
            AgentRequest::Ports => {
                return Ok(Some(serde_json::to_value(self.listeners().await)?));
            }
            AgentRequest::Upgrade { sha256 } => self.upgrade(sha256).await?,
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
                create,
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
                            .import_volume(&app, &volume, create.as_ref(), upload, size)
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

    /// The build phase of a stack deployment: the image only.
    async fn prepare(self: Arc<Self>, workload: Workload, deployment: Option<Uuid>, rebuild: bool) {
        let app = workload.spec.name.clone();
        let lock = self.lock_for(&app);
        let _held = lock.lock().await;
        let log = DeployLog {
            agent: self.clone(),
            deployment,
            app: app.clone(),
            started: AtomicBool::new(false),
        };
        let result = self.runtime.prepare(&workload, &log, rebuild).await;
        log.build_done();
        if let Err(e) = &result {
            log.line(format!("error: {e:#}"));
        }
        // The running version is untouched.
        let current = self.workloads.lock().get(&app).cloned();
        let state = match current {
            Some(w) => self.runtime.app_stats(&[w]).await[0].state,
            None => AppState::Pending,
        };
        self.emit(AgentEvent::AppState {
            app,
            state,
            message: None,
        });
        if let Some(deployment) = deployment {
            self.emit(AgentEvent::BuildDone {
                deployment,
                error: result.err().map(|e| format!("{e:#}")),
            });
        }
    }

    /// `pull`: pull images again (an explicit deploy) rather than use the one the
    /// build phase pulled.
    async fn converge(
        self: Arc<Self>,
        workload: Workload,
        deployment: Option<Uuid>,
        rebuild: bool,
        pull: bool,
    ) {
        let app = workload.spec.name.clone();
        let lock = self.lock_for(&app);
        let _held = lock.lock().await;
        let log = DeployLog {
            agent: self.clone(),
            deployment,
            app: app.clone(),
            started: AtomicBool::new(false),
        };
        let result = self.runtime.converge(&workload, &log, rebuild, pull).await;
        // A new container listens on a new local port.
        self.refresh_routes().await;
        log.build_done();
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
        if deployment.is_some() && result.is_ok() {
            // New volumes show up without waiting for the periodic report.
            let agent = self.clone();
            tokio::spawn(async move { maintenance::refresh_report(&agent).await });
        }
        if let Some(deployment) = deployment {
            self.emit(AgentEvent::DeployDone {
                deployment,
                error: result.err().map(|e| format!("{e:#}")),
                cancelled: false,
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
            self.clone().converge(workload, None, false, false).await;
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
                    self.clone().converge(workload, None, false, false).await;
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
            let storage = {
                let mut storage = self.storage.lock();
                std::mem::take(&mut storage.1)
                    .then(|| storage.0.clone())
                    .flatten()
            };
            let listeners = self.listeners().await;
            let report = NodeReport {
                node: sampler.sample(),
                apps: self.runtime.app_stats(&workloads).await,
                storage,
                listeners,
                ingress_errors: self.link.errors(),
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

    async fn listeners(&self) -> Vec<facade_core::proto::Listener> {
        match self.runtime.port_owners().await {
            Ok((containers, published)) => {
                tokio::task::spawn_blocking(move || ports::scan(&containers, &published))
                    .await
                    .unwrap_or_default()
            }
            Err(e) => {
                warn!("listing containers for port owners: {e:#}");
                vec![]
            }
        }
    }

    /// Installs the server's binary in place of this one, hands the entry point
    /// over to it, and exits so the service manager starts the new one. Apps keep
    /// running throughout: they belong to Docker.
    async fn upgrade(&self, sha256: String) -> anyhow::Result<()> {
        if crate::selfbin::current().is_some_and(|b| b.sha256 == sha256) {
            return Ok(());
        }
        let path = crate::selfbin::installed_path()?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "facaded".into());
        let tmp = path.with_file_name(format!(".{name}.new"));
        fetch_artifact(&self.token, &sha256, &tmp)
            .await
            .context("downloading the server's binary")?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        // The previous build stays next to it, for going back by hand.
        if let Err(e) = std::fs::copy(&path, path.with_file_name(format!("{name}.prev"))) {
            warn!("keeping the previous binary: {e}");
        }
        std::fs::rename(&tmp, &path).with_context(|| format!("installing {}", path.display()))?;
        info!(
            "installed facaded {} at {}; handing over the entry point and restarting",
            crate::selfbin::short(&sha256),
            path.display()
        );
        self.link.request_upgrade();
        tokio::spawn(async {
            // Time to answer the server and pass the request to the entry point.
            tokio::time::sleep(Duration::from_secs(2)).await;
            // EX_TEMPFAIL: restarted under Restart=always and Restart=on-failure.
            std::process::exit(75);
        });
        Ok(())
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
    started: AtomicBool,
}

impl DeployLog {
    /// The deployment is out of the queue and being worked on.
    pub fn begin(&self) {
        if let Some(deployment) = self.deployment
            && !self.started.swap(true, Ordering::Relaxed)
        {
            self.agent.emit(AgentEvent::DeployStarted { deployment });
        }
    }

    /// Waits for the node's build slot, saying so if others are ahead.
    pub async fn build_slot(&self, commit: Option<String>) -> queue::Slot {
        let ahead = self.agent.queue.subscribe().borrow().len();
        if ahead > 0 {
            self.line(format!(
                "waiting for {ahead} other build{} on this node",
                if ahead == 1 { "" } else { "s" }
            ));
        }
        let slot = self
            .agent
            .queue
            .acquire(&self.app, self.deployment, commit)
            .await;
        self.begin();
        slot
    }

    /// Past the build: from here on, cancelling would leave the app half-replaced.
    pub fn build_done(&self) {
        if let Some(deployment) = self.deployment {
            self.agent.cancellable.lock().remove(&deployment);
        }
    }

    pub fn commit(&self, sha: &str, title: &str) {
        if let Some(deployment) = self.deployment {
            self.agent.emit(AgentEvent::DeployCommit {
                deployment,
                commit: sha.to_string(),
                title: title.to_string(),
            });
        }
    }

    pub fn line(&self, line: impl Into<String>) {
        let line = line.into();
        tracing::debug!(app = %self.app, "{line}");
        if let Some(deployment) = self.deployment {
            self.agent.emit(AgentEvent::DeployLog { deployment, line });
        }
    }

    pub fn state_with(&self, state: AppState, message: Option<String>) {
        self.agent.emit(AgentEvent::AppState {
            app: self.app.clone(),
            state,
            message,
        });
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
