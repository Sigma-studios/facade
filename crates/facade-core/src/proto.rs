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

use base64::Engine;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::spec::AppSpec;

pub const PROTOCOL_VERSION: u32 = 3;
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
    Report(NodeReport),
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
    /// The facade app the socket belongs to; `facade` for facade itself.
    #[serde(default)]
    pub app: Option<String>,
}

/// What Docker keeps on a node's disk, and the last cleanup.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StorageReport {
    pub at: u64,
    pub images: u64,
    pub build_cache: u64,
    /// Build cache Docker could drop right away.
    pub build_cache_reclaimable: u64,
    pub volumes: Vec<VolumeUsage>,
    pub last_cleanup: Option<Cleanup>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeUsage {
    /// Docker volume name, `facade-<app>-<volume>`.
    pub name: String,
    pub app: String,
    pub volume: String,
    pub size: Option<u64>,
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
