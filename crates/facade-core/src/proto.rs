//! The server ↔ agent protocol.
//!
//! Agents always dial the server (so nodes behind NAT work), over TLS pinned to the
//! server's certificate fingerprint. There are two kinds of connection, both opened by
//! the agent and introduced by a single JSON [`Hello`] line:
//!
//! * one **control** connection per agent, carrying newline-delimited JSON
//!   [`ServerMsg`] / [`AgentMsg`] in both directions;
//! * short-lived **data** connections, dialed when the server asks for a tunnel
//!   (HTTP traffic for an app, a log stream). After the hello line they carry raw bytes.

use base64::Engine;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::spec::AppSpec;

pub const PROTOCOL_VERSION: u32 = 1;

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
    Remove {
        app: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AgentMsg {
    Response {
        id: u64,
        #[serde(default)]
        error: Option<String>,
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
