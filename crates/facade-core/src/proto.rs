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

pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Hello {
    Control {
        node: String,
        secret: String,
        protocol: u32,
        agent_version: String,
        hostname: String,
    },
    Data {
        node: String,
        secret: String,
        tunnel: Uuid,
    },
    /// Download an uploaded executable. The server answers `ok <size>\n` then the
    /// bytes, or `error <message>\n`.
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
    /// Bumped by every explicit deploy. A new revision forces a fresh build.
    pub revision: u64,
    /// Logins for private registries and git hosts, used by pulls, clones and
    /// builds. Every configured one is sent: a Dockerfile's `FROM` can name any
    /// registry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<Credential>,
}

impl Workload {
    /// What the running container depends on: credentials only matter while
    /// fetching, so changing them does not restart anything.
    pub fn run_identity(&self) -> (&AppSpec, &Vec<(String, String)>, u64) {
        (&self.spec, &self.env, self.revision)
    }

    pub fn credential(&self, kind: CredentialKind, host: &str) -> Option<&Credential> {
        self.credentials
            .iter()
            .find(|c| c.kind == kind && c.host.eq_ignore_ascii_case(host))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// A container registry (`ghcr.io`, `docker.io`).
    Registry,
    /// A git host, for HTTPS clones (`github.com`).
    Git,
}

impl std::fmt::Display for CredentialKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CredentialKind::Registry => "registry",
            CredentialKind::Git => "git",
        })
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Credential {
    pub kind: CredentialKind,
    pub host: String,
    pub username: String,
    /// Token or password.
    pub secret: String,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Credential({} {} as {})",
            self.kind, self.host, self.username
        )
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
    /// Desired state of one app. Rebuilds when the revision changed.
    Apply {
        workload: Workload,
        deployment: Option<Uuid>,
    },
    /// Remove the app's container (and with `delete_volumes`, its data).
    Remove {
        app: String,
        #[serde(default)]
        delete_volumes: bool,
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
    /// Every facade volume on the node, with sizes. Answers `Vec<VolumeInfo>`.
    Volumes,
    /// What listens on the node right now. Answers `Vec<Listener>`.
    Ports,
    /// Dial a data connection and write a gzipped tar of the volume to it. A running
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
    },
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
    DeployLog {
        deployment: Uuid,
        line: String,
    },
    DeployDone {
        deployment: Uuid,
        error: Option<String>,
        /// The commit that was built, for sources from git.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
    },
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

/// A volume on a node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub app: String,
    pub name: String,
    /// Bytes used, when it could be measured.
    pub size: Option<u64>,
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
