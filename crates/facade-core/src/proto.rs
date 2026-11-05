//! The server ↔ agent protocol.
//!
//! Agents always dial the server (so nodes behind NAT work), over TLS pinned to the
//! server's certificate fingerprint. There are two kinds of connection, both opened by
//! the agent and introduced by a single JSON [`Hello`] line:
//!
//! * one **control** connection per agent, carrying newline-delimited JSON
//!   [`ServerMsg`] / [`AgentMsg`] in both directions;
//! * short-lived **data** connections, dialed when the server asks for a tunnel
//!   (a log stream, a volume archive). After the hello line they carry raw bytes.

use std::collections::BTreeMap;

use base64::Engine;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::spec::AppSpec;

/// 4: workloads carry stop settings, a process limit, cgroup delegation and volume
/// sizes, and agents run commands in containers (backup hooks).
/// 5: workloads name the variables their builds get (`build_env`), which an older
/// agent would drop without a word.
/// 6: agents run commands for `facade exec` over a data connection
/// ([`AgentRequest::ExecStream`]); the server asks only agents speaking 6.
/// 7: nothing new on the wire: a feature that came with it was taken out, and
/// the number stays so that agents already at 7 are not refused.
/// 8: agents send what is installed for the vulnerability check
/// ([`AgentRequest::Inventory`]); the server asks only agents speaking 8.
pub const PROTOCOL_VERSION: u32 = 8;
/// The first protocol with [`AgentRequest::ExecStream`].
pub const EXEC_STREAM_PROTOCOL: u32 = 6;
/// The oldest agent protocol the server still talks to. Agents in between are
/// sent the server's binary and update themselves.
pub const MIN_AGENT_PROTOCOL: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Hello {
    Control {
        node: String,
        secret: String,
        protocol: u32,
        agent_version: String,
        hostname: String,
        /// SHA-256 of the agent's executable, to tell builds apart.
        #[serde(default)]
        binary: Option<String>,
        /// CPU architecture the agent runs on.
        #[serde(default)]
        arch: Option<String>,
        /// When the agent's process started, and the machine booted (seconds
        /// since the epoch): the server tells restarts and reboots from them.
        #[serde(default)]
        started: Option<u64>,
        #[serde(default)]
        boot: Option<u64>,
        /// The commit its binary was built from, when known.
        #[serde(default)]
        commit: Option<String>,
        /// The system it runs on; `None` from agents before platforms.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        platform: Option<crate::platform::PlatformInfo>,
    },
    Data {
        node: String,
        secret: String,
        tunnel: Uuid,
    },
    /// Download an uploaded executable, or the server's own binary (by its
    /// SHA-256). The server answers `ok <size>\n` then the bytes, or
    /// `error <message>\n`.
    Fetch {
        node: String,
        secret: String,
        sha256: String,
    },
}

/// Everything an agent needs to run one app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workload {
    pub spec: AppSpec,
    /// Resolved environment, secrets included. Only ever sent over the pinned TLS link.
    pub env: Vec<(String, String)>,
    /// The variables of `env` its builds get too, as BuildKit secrets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub build_env: Vec<String>,
    /// Stands for those variables' values (a keyed hash, as image tags and build
    /// arguments show it): built images are keyed by it, so a new value rebuilds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_digest: Option<String>,
    /// Bumped by every explicit deploy; a new revision recreates the container.
    pub revision: u64,
    /// The commit git sources are built from, resolved when the app was deployed.
    /// Builds are keyed by it, so a rebuild (on another node, after a cleanup) gets
    /// the same code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// How to fetch the repository, when a credential covers it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_auth: Option<GitAuth>,
    /// Identifies what a stack service's image is built from (its build context's
    /// content at the commit), so services whose code did not change keep their
    /// image across commits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_key: Option<String>,
    /// Extra host names the app answers to on its private network, asked for by
    /// apps linking to it under another name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Logins for private container registries, used by pulls and by builds (a
    /// Dockerfile's `FROM` can name any registry): those of the app's owner.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registries: Vec<RegistryLogin>,
}

impl Workload {
    pub fn registry(&self, host: &str) -> Option<&RegistryLogin> {
        self.registries
            .iter()
            .find(|r| r.host.eq_ignore_ascii_case(host))
    }
}

/// A login for a container registry (`ghcr.io`, `docker.io`).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct RegistryLogin {
    pub host: String,
    pub username: String,
    /// Token or password.
    pub secret: String,
}

impl std::fmt::Debug for RegistryLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RegistryLogin({} as {})", self.host, self.username)
    }
}

impl Workload {
    /// `(key, value)` of each variable its builds get, as resolved in `env`.
    pub fn build_vars(&self) -> Vec<(&str, &str)> {
        self.build_env
            .iter()
            .filter_map(|key| {
                self.env
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(k, v)| (k.as_str(), v.as_str()))
            })
            .collect()
    }
}

/// A credential for fetching a private repository.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitAuth {
    /// An SSH private key (OpenSSH format); the repository is fetched over SSH.
    SshKey { private_key: String },
    /// An access token, sent over HTTPS to URLs under `scope` only.
    Token {
        scope: String,
        username: String,
        token: String,
    },
}

impl std::fmt::Debug for GitAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitAuth::SshKey { .. } => f.write_str("SshKey(..)"),
            GitAuth::Token { scope, .. } => write!(f, "Token({scope}, ..)"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // short-lived, serialized immediately
pub enum ServerMsg {
    Request {
        id: u64,
        req: AgentRequest,
    },
    Ping,
    /// Sent instead of anything else when the hello is refused; then the server hangs up.
    Rejected {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // short-lived, serialized immediately
pub enum AgentRequest {
    /// The complete desired state of the node. Anything facade-managed that is not
    /// listed is removed. Sent on every (re)connection.
    Sync {
        workloads: Vec<Workload>,
        /// The server takes [`AgentEvent::Activity`]. An older one would drop
        /// the connection on an event it does not know, so agents keep theirs
        /// until a server says so.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        activity: bool,
    },
    /// Desired state of one app. Builds when the commit (or source) changed.
    Apply {
        workload: Workload,
        deployment: Option<Uuid>,
        /// Build again even if an image of this commit exists.
        #[serde(default)]
        rebuild: bool,
        #[serde(default)]
        phase: ApplyPhase,
    },
    Remove {
        app: String,
        /// Delete its volumes too. Otherwise they stay on the node.
        #[serde(default)]
        volumes: bool,
    },
    /// What listens on the node right now. Answers `Vec<Listener>`.
    Ports,
    /// Replace the agent's executable with the server's (fetched by its SHA-256),
    /// hand the node's entry point over to it, and restart.
    Upgrade {
        sha256: String,
    },
    /// What is installed, for the vulnerability check: answers
    /// `Checked<Option<Inventory>>`, `None` when its hash is still `known`.
    Inventory {
        #[serde(default)]
        known: Option<String>,
    },
    /// Dial a data connection and write a gzipped tar of the volume (by its Docker
    /// name) to it. A running
    /// app is paused meanwhile, so files are copied as they were at one instant.
    VolumeExport {
        tunnel: Uuid,
        app: String,
        volume: String,
    },
    /// Dial a data connection, read `<size>\n` and then that many bytes of a tar
    /// (gzipped or not), replace the volume's contents with it, and answer `ok\n`
    /// or `error <message>\n` on the same connection. The app is stopped meanwhile.
    VolumeImport {
        tunnel: Uuid,
        app: String,
        volume: String,
        /// How to create the volume if the app never ran yet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        create: Option<NewVolume>,
    },
    /// Stop a deployment that is queued or building.
    Cancel {
        deployment: Uuid,
    },
    /// Free disk space now; `aggressive` also drops the whole build cache and the
    /// images kept for rollbacks.
    Prune {
        aggressive: bool,
    },
    /// Delete a facade volume no app uses any more.
    RemoveVolume {
        name: String,
    },
    Restart {
        app: String,
    },
    /// Certificates and extra routes for the node's HTTP(S) entry point. Replaces
    /// whatever was sent before.
    Ingress {
        certs: Vec<CertBundle>,
        proxies: Vec<ProxyRoute>,
    },
    /// Answer (or stop answering) an ACME HTTP-01 challenge on port 80.
    Challenge {
        token: String,
        key_authorization: Option<String>,
    },
    /// Dial a data connection for `tunnel` and write the app's logs to it.
    Logs {
        tunnel: Uuid,
        app: String,
        tail: u32,
        follow: bool,
    },
    /// Run `command` with `sh -c` in the app's running container. The answer comes
    /// once it exits; a non-zero exit is an error carrying its output.
    Exec {
        app: String,
        command: String,
        timeout_secs: u32,
    },
    /// Dial a data connection for `tunnel` and run `request` in the app's
    /// container, with its input and output in [`crate::exec::Frame`]s on it.
    ExecStream {
        tunnel: Uuid,
        app: String,
        request: crate::exec::ExecRequest,
    },
}

/// Stacks deploy in two phases: every service is built first (nothing changes if
/// one fails), then replaced in dependency order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyPhase {
    /// Build (or pull) and replace the container.
    #[default]
    All,
    /// Only build or pull the image; the desired state does not change.
    Build,
    /// Replace the container with the image built before (no new pull).
    Release,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AgentMsg {
    Response {
        id: u64,
        #[serde(default)]
        error: Option<String>,
        /// The answer, for requests that have one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    Event {
        event: AgentEvent,
    },
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    Report(Box<NodeReport>),
    /// The node's packages changed (an upgrade, a rebuild): the server asks
    /// for the new inventory.
    InventoryChanged {
        hash: String,
    },
    AppState {
        app: String,
        state: AppState,
        message: Option<String>,
    },
    /// The deployment left the build queue and is being worked on.
    DeployStarted {
        deployment: Uuid,
    },
    /// The commit a deployment checked out.
    DeployCommit {
        deployment: Uuid,
        commit: String,
        title: String,
    },
    DeployLog {
        deployment: Uuid,
        line: String,
    },
    DeployDone {
        deployment: Uuid,
        error: Option<String>,
        #[serde(default)]
        cancelled: bool,
    },
    /// The build phase of a stack deployment is over (an error ends the deployment).
    BuildDone {
        deployment: Uuid,
        error: Option<String>,
    },
    /// The node's build queue, sent whenever it changes.
    Builds {
        builds: Vec<BuildEntry>,
    },
    /// Something happened on the node (an app crashed, the kernel killed a
    /// process, someone logged in): an Activity entry. Only sent to servers
    /// asking for them (see [`AgentRequest::Sync`]).
    Activity {
        event: Box<crate::events::NodeEvent>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildState {
    Queued,
    Building,
}

/// One build waiting for, or holding, the node's build slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildEntry {
    pub app: String,
    pub deployment: Option<Uuid>,
    pub commit: Option<String>,
    pub state: BuildState,
    /// When it entered its current state.
    pub since: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppState {
    Pending,
    Building,
    Running,
    Stopped,
    /// The container exited or keeps restarting.
    Crashed,
    /// The last build or start failed.
    Failed,
    Unknown,
}

impl std::fmt::Display for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            AppState::Pending => "pending",
            AppState::Building => "building",
            AppState::Running => "running",
            AppState::Stopped => "stopped",
            AppState::Crashed => "crashed",
            AppState::Failed => "failed",
            AppState::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeReport {
    pub node: NodeStats,
    pub apps: Vec<AppStats>,
    /// Refreshed every few minutes, so only in some reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageReport>,
    /// Every listening socket on the machine, facade's or not.
    #[serde(default)]
    pub listeners: Vec<Listener>,
    /// Addresses the HTTP(S) entry point wants but cannot bind (still retrying).
    #[serde(default)]
    pub ingress_errors: Vec<String>,
    /// Pending system updates and reboot, read every few hours: in the first
    /// report of a connection, then when it changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upkeep: Option<crate::events::Upkeep>,
    /// Failed requests and visitors per app, from the entry point (see
    /// [`EdgeReading`]); empty from older agents and entry points.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub edge: BTreeMap<String, EdgeReading>,
}

/// A TCP socket in LISTEN state or a bound UDP socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listener {
    pub protocol: crate::spec::Protocol,
    pub address: String,
    pub port: u16,
    /// Process name, when it could be found.
    #[serde(default)]
    pub process: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    /// The facade app the socket belongs to; `facade` for facade itself. A TLS
    /// port of the entry point belongs to the app it passes connections to.
    #[serde(default)]
    pub app: Option<String>,
    /// What holds it, in a few words, when the agent knows: `façade entry
    /// point`, `container grafana`, `SSH (system)`. Older agents send none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Listener {
    /// Who holds the socket, for messages: `app blog`, `facade itself`,
    /// `docker-proxy (pid 412, container grafana)`.
    pub fn holder(&self) -> String {
        match (&self.app, &self.process, self.pid, &self.description) {
            (Some(app), ..) if app == "facade" => "facade itself".into(),
            (Some(app), ..) => format!("app {app}"),
            (None, Some(p), Some(pid), Some(what)) => format!("{p} (pid {pid}, {what})"),
            (None, Some(p), Some(pid), None) => format!("{p} (pid {pid})"),
            (None, Some(p), None, _) => p.clone(),
            (None, None, _, Some(what)) => what.clone(),
            _ => "another program".into(),
        }
    }
}

/// What Docker and facade keep on a node's disk, and the last cleanup.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StorageReport {
    pub at: u64,
    pub images: u64,
    pub build_cache: u64,
    /// Build cache Docker could drop right away.
    pub build_cache_reclaimable: u64,
    /// What containers wrote outside their volumes.
    #[serde(default)]
    pub containers: u64,
    /// Every Docker volume of the node, facade's or not. Volumes with a size
    /// count for what their filesystem's image file takes on the node's disk
    /// (Docker leaves them out of its own figure).
    #[serde(default)]
    pub volumes_size: u64,
    /// facade's own files: static sites, certificates, build folders (and on the
    /// server's node, its database and uploads). Sized volumes' files are volume
    /// data, left to `volumes_size`; backups are `backups` (older agents count
    /// them here).
    #[serde(default)]
    pub facade_files: u64,
    /// Volume backups and copies of the server's database: on the server's node
    /// only, where they are kept (`<data dir>/backups`).
    #[serde(default)]
    pub backups: u64,
    /// What each app's volume backups take, by app (on the server's node): its
    /// full copies, and the chunks of its incremental backups, a chunk held by
    /// several apps split between them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub app_backups: BTreeMap<String, u64>,
    /// Docker's folders on disk, volumes aside: images, build cache, containers
    /// and their logs, measured. `images` and `build_cache` are Docker's own
    /// figures, which share layers and overlap; this is what they take together.
    /// `None` when the agent cannot reach the folders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_on_disk: Option<u64>,
    /// The node's filesystem when the report was made, so the parts add up.
    #[serde(default)]
    pub disk_used: u64,
    #[serde(default)]
    pub disk_total: u64,
    pub volumes: Vec<VolumeUsage>,
    /// Per app, what is not in volumes (those are listed above).
    #[serde(default)]
    pub apps: Vec<AppDisk>,
    pub last_cleanup: Option<Cleanup>,
}

impl StorageReport {
    /// Used space neither Docker nor facade accounts for: the system and other
    /// programs.
    pub fn other(&self) -> u64 {
        let docker = self
            .docker_on_disk
            .unwrap_or(self.images + self.build_cache + self.containers);
        self.disk_used
            .saturating_sub(docker + self.volumes_size + self.facade_files + self.backups)
    }

    /// Images and build cache together, as measured on disk: Docker's figures
    /// for each overlap.
    pub fn images_and_cache(&self) -> Option<u64> {
        self.docker_on_disk
            .map(|d| d.saturating_sub(self.containers))
    }
}

/// One app's footprint on its node, volumes aside.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AppDisk {
    pub app: String,
    /// The image it runs, plus what the images kept for rollbacks add to it.
    pub images: u64,
    /// Rollback images kept besides the running one.
    pub rollback_images: u32,
    /// Written inside the container, outside volumes: lost on redeploy.
    pub written: u64,
    /// Static site files.
    pub files: u64,
    /// The largest layers of the image it runs, largest first. Older agents
    /// send none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<ImageLayer>,
    /// The image's other layers holding files: how many, and what they take.
    #[serde(default)]
    pub smaller_layers: u32,
    #[serde(default)]
    pub smaller_layers_size: u64,
}

/// One layer of an image, from its history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageLayer {
    pub size: u64,
    /// The Dockerfile step that made it, shortened (`RUN apt-get install …`).
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeUsage {
    /// Docker volume name, `facade-<app>-<volume>`.
    pub name: String,
    pub app: String,
    pub volume: String,
    /// What its files take; for a volume with a size, the space used in its
    /// filesystem.
    pub size: Option<u64>,
    /// Bytes it may hold, for volumes with a size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Mounted by a container right now.
    pub in_use: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cleanup {
    pub at: u64,
    pub freed: u64,
    pub aggressive: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeStats {
    /// `x86_64` or `aarch64`.
    #[serde(default)]
    pub arch: String,
    pub cpus: usize,
    pub cpu_percent: f32,
    pub load1: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
    pub uptime: u64,
    /// Bytes received and sent by the node's network interfaces since it booted
    /// (0 from agents that do not count them).
    #[serde(default)]
    pub net_rx: u64,
    #[serde(default)]
    pub net_tx: u64,
    /// Pressure, disk activity and time waiting on disk (older agents send none).
    #[serde(
        default,
        skip_serializing_if = "crate::resources::NodeResources::is_empty"
    )]
    pub resources: crate::resources::NodeResources,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppStats {
    pub app: String,
    pub state: AppState,
    pub cpu_percent: f32,
    pub mem_used: u64,
    pub mem_limit: u64,
    pub net_rx: u64,
    pub net_tx: u64,
    pub restarts: u64,
    /// What the node's entry point counted for the app; `None` when nothing
    /// routes to it (no domain) or the entry point cannot tell (an older one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic: Option<Traffic>,
    /// CPU throttling, disk activity, processes, health checks (older agents
    /// send none).
    #[serde(
        default,
        skip_serializing_if = "crate::resources::AppResources::is_empty"
    )]
    pub resources: crate::resources::AppResources,
}

/// Upper bounds, in milliseconds, of the buckets response times are counted
/// in (a last bucket takes anything slower). Readings are only compared
/// bucket by bucket, so the bounds must not change.
pub const LATENCY_BOUNDS_MS: [f64; 27] = [
    1.0, 1.5, 2.0, 3.0, 5.0, 7.5, 10.0, 15.0, 20.0, 30.0, 50.0, 75.0, 100.0, 150.0, 200.0, 300.0,
    500.0, 750.0, 1000.0, 1500.0, 2000.0, 3000.0, 5000.0, 7500.0, 10000.0, 15000.0, 30000.0,
];
/// Buckets in a latency histogram: one per bound, and one past the last.
pub const LATENCY_BUCKETS: usize = LATENCY_BOUNDS_MS.len() + 1;

/// An app's traffic through its node's entry point: counters since `since`,
/// and the connections open now.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Traffic {
    /// When the entry point started counting for the app (seconds since the
    /// epoch). A new entry point (after an update) starts again from zero:
    /// readings with different `since` do not compare.
    pub since: u64,
    /// HTTP requests answered (a WebSocket counts once, for its upgrade).
    pub requests: u64,
    /// Of which answered 4xx, and 5xx (the entry point's own "unavailable"
    /// answers included: visitors see those as errors too).
    pub client_errors: u64,
    pub server_errors: u64,
    /// Requests by time to the response's headers, per [`LATENCY_BOUNDS_MS`].
    #[serde(default)]
    pub latency: Vec<u64>,
    /// Client connections open now: kept-alive HTTP, WebSockets, TLS ports.
    #[serde(default)]
    pub open: u64,
    /// Bytes received from clients and sent to them, as on the wire (TLS
    /// included): what a hosting provider's traffic quota counts. A kept-alive
    /// connection counts for the app its first request went to (0 from older
    /// entry points).
    #[serde(default)]
    pub bytes_in: u64,
    #[serde(default)]
    pub bytes_out: u64,
}

/// What an entry point gathered for one app beyond its counters: requests that
/// failed, grouped, and who visited. Reports carry only what changed since the
/// previous one on the same connection to the server, the first one everything.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EdgeReading {
    /// When the entry point started counting for the app, as [`Traffic::since`].
    pub since: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ErrorTally>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visitors: Vec<VisitorSketch>,
}

/// Requests answered 4xx or 5xx with the same status, method and path (query
/// left out, ids and hashes replaced by placeholders), since `first_seen`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorTally {
    pub status: u16,
    pub method: String,
    pub path: String,
    pub count: u64,
    /// When the entry point started counting these (seconds since the epoch):
    /// one forgotten to make room and seen again starts again from there.
    pub first_seen: u64,
    pub last_seen: u64,
}

/// A HyperLogLog sketch of the visitors of one day (UTC; `day` counts days since
/// the epoch): per-register maxima, merged by taking the larger of each.
/// No address is kept, only these maxima of keyed hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisitorSketch {
    pub day: u64,
    /// The registers, base64-encoded.
    pub registers: String,
}

/// A certificate and its key, for the names it covers (`*.example.com` included).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CertBundle {
    pub names: Vec<String>,
    pub cert_pem: String,
    pub key_pem: String,
}

/// A host name the node forwards to a local plain-HTTP port (the server's API).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyRoute {
    pub domain: String,
    pub port: u16,
}

/// What `facaded agent --join` takes: where the server is, how to recognise it, and
/// who this node is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinToken {
    /// `host:port` of the server's node listener.
    pub server: String,
    /// SHA-256 of the server's certificate, hex.
    pub fingerprint: String,
    pub node: String,
    pub secret: String,
}

impl JoinToken {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("serializable");
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    }

    pub fn decode(s: &str) -> Result<Self, String> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s.trim())
            .map_err(|e| format!("join token is not valid base64: {e}"))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("join token is malformed: {e}"))
    }
}

/// The labels of a facade volume created ahead of its app's first deploy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewVolume {
    /// The volume's name in the app's spec (`data`).
    pub name: String,
    /// The stack it belongs to, for the volumes a stack's services share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1_000_000_000;

    #[test]
    fn hellos_from_agents_before_platforms_still_read() {
        let old = r#"{"t":"control","node":"eu-2","secret":"s","protocol":7,"agent_version":"0.1.0",
            "hostname":"h","binary":"ab","arch":"x86_64","started":1,"boot":2,"commit":"c"}"#;
        let hello: Hello = serde_json::from_str(old).unwrap();
        let Hello::Control {
            platform, protocol, ..
        } = hello
        else {
            panic!("not a control hello");
        };
        assert_eq!(protocol, 7);
        assert_eq!(platform, None);
        // What the server asks agents speaking 8.
        let request: AgentRequest =
            serde_json::from_str(r#"{"op":"inventory","known":"abc"}"#).unwrap();
        assert!(matches!(request, AgentRequest::Inventory { known: Some(k) } if k == "abc"));
    }

    #[test]
    fn measured_docker_keeps_parts_within_the_disk() {
        // Docker's figures overlap: 20 + 19 GB on a disk with 32.7 GB used.
        let mut report = StorageReport {
            images: 20 * GB,
            build_cache: 19 * GB,
            containers: GB / 2,
            volumes_size: GB,
            facade_files: GB / 4,
            disk_used: 32 * GB,
            disk_total: 72 * GB,
            ..Default::default()
        };
        assert_eq!(report.other(), 0);
        assert_eq!(report.images_and_cache(), None);

        report.docker_on_disk = Some(27 * GB);
        assert_eq!(report.images_and_cache(), Some(27 * GB - GB / 2));
        assert_eq!(report.other(), 32 * GB - 27 * GB - GB - GB / 4);

        report.backups = 2 * GB;
        assert_eq!(report.other(), 32 * GB - 27 * GB - GB - GB / 4 - 2 * GB);
    }
}
