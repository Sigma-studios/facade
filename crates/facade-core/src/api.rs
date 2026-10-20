//! HTTP API bodies, shared by the server and the CLI.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::proto::{AppState, AppStats, CredentialKind, NodeStats};
use crate::spec::{AppSpec, Build};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangePassword {
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeView {
    pub name: String,
    pub connected: bool,
    /// The server's own node.
    pub local: bool,
    pub hostname: Option<String>,
    /// Where the agent last connected from.
    pub address: Option<String>,
    /// The address domains of apps on this node point to.
    pub public_ip: Option<String>,
    /// `public_ip` was set by hand rather than detected.
    #[serde(default)]
    pub public_ip_manual: bool,
    pub agent_version: Option<String>,
    pub last_seen: Option<u64>,
    pub stats: Option<NodeStats>,
    /// Addresses the node's HTTP(S) entry point cannot bind (another program holds
    /// them); it keeps retrying.
    #[serde(default)]
    pub ingress_errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateNode {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetNodeIp {
    /// `None` goes back to the detected address.
    pub public_ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedNode {
    pub name: String,
    pub join_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppView {
    pub spec: AppSpec,
    pub state: AppState,
    pub message: Option<String>,
    pub revision: u64,
    pub stats: Option<AppStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvView {
    pub key: String,
    /// `None` when the variable is sensitive and was not revealed.
    pub value: Option<String>,
    pub sensitive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetEnv {
    pub value: String,
    #[serde(default = "default_sensitive")]
    pub sensitive: bool,
}

/// Several env changes applied with a single restart.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvPatch {
    #[serde(default)]
    pub set: BTreeMap<String, SetEnv>,
    #[serde(default)]
    pub unset: Vec<String>,
}

fn default_sensitive() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentView {
    pub id: Uuid,
    pub app: String,
    pub revision: u64,
    pub status: DeploymentStatus,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub error: Option<String>,
    /// Only filled when a single deployment is fetched.
    #[serde(default)]
    pub log: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusView {
    pub nodes: Vec<NodeView>,
    pub apps: Vec<AppView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyReport {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
    /// `app: KEY` for sensitive variables declared without a value and not set yet.
    pub missing_secrets: Vec<String>,
    /// Nodes declared in the manifest that did not exist; start their agents with these.
    pub new_nodes: Vec<CreatedNode>,
    /// Apps whose executable is not on this server yet: upload it again.
    #[serde(default)]
    pub missing_uploads: Vec<String>,
}

/// An uploaded executable, ready to be used as `Source::Executable`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactView {
    pub sha256: String,
    pub filename: String,
    pub arch: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectRequest {
    pub repo: String,
    #[serde(default)]
    pub branch: Option<String>,
}

/// What a repository scan found.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectReport {
    /// Short hash and subject of the commit that was scanned.
    pub commit: String,
    /// Best first.
    pub candidates: Vec<Candidate>,
}

/// One way to build and run something found in a repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// Which detector found it: `rust`, `dockerfile`, `static`, …
    pub detector: String,
    pub title: String,
    pub detail: String,
    pub build: Build,
    /// Port the code seems to listen on.
    pub http_port: Option<u16>,
    /// Environment variables the code reads.
    pub env: Vec<String>,
    /// Suggested `args`, e.g. to listen on every interface instead of loopback.
    #[serde(default)]
    pub args: Vec<String>,
    /// Things to know before deploying it.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// A registry or git login. The secret is never sent back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialView {
    pub kind: CredentialKind,
    pub host: String,
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetCredential {
    pub username: String,
    pub secret: String,
}

/// A volume as the server sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeView {
    pub app: String,
    pub name: String,
    pub node: String,
    /// Mount point, if the app still declares this volume.
    pub path: Option<String>,
    pub size: Option<u64>,
    /// Newest backup, as a Unix time.
    pub last_backup: Option<u64>,
}

/// One stored backup of a volume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupView {
    pub app: String,
    pub volume: String,
    /// Unix time it was taken; also its id.
    pub id: u64,
    pub size: u64,
}

/// `POST /api/backups`: back up now. Empty means every app with volumes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackupRequest {
    #[serde(default)]
    pub app: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupReport {
    pub done: Vec<BackupView>,
    /// `app/volume: reason`
    pub failed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeMetric {
    pub at: u64,
    pub stats: NodeStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppMetric {
    pub at: u64,
    pub stats: AppStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertView {
    pub key: String,
    pub message: String,
    pub since: u64,
    /// `None` while still firing.
    pub resolved_at: Option<u64>,
}

/// The DNS provider facade manages records (and wildcard certificates) with.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DnsSettings {
    /// `porkbun`, or `None` when records are created by hand.
    pub provider: Option<String>,
    /// Domains the provider lets facade manage.
    pub zones: Vec<String>,
    /// Why the provider could not be reached, if it could not.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DnsUpdate {
    /// `porkbun`, or empty to stop managing DNS.
    pub provider: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub secret_key: String,
}

/// Everything facade knows about one routed host name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainStatus {
    pub domain: String,
    /// `None` for the server's own API domain.
    pub app: Option<String>,
    pub node: String,
    /// The address the name must point to.
    pub expected_ip: Option<String>,
    /// What the name resolves to right now.
    pub resolved: Vec<String>,
    /// The record is kept in sync by the DNS provider.
    pub managed: bool,
    pub dns_ok: bool,
    pub dns_error: Option<String>,
    /// Name of the certificate covering it (`*.example.com` or the domain itself).
    pub certificate: Option<String>,
    pub cert_expires: Option<u64>,
    pub cert_error: Option<String>,
}

/// Where alerts go. Read with the webhook partly hidden.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotifySettings {
    pub discord_webhook: Option<String>,
    /// Tagged on problems (not on resolutions): `@here`, `@everyone`, `user:<id>`, `role:<id>`.
    pub mentions: Vec<String>,
}

/// Fields left out are unchanged; an empty webhook removes it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotifyUpdate {
    #[serde(default)]
    pub discord_webhook: Option<String>,
    #[serde(default)]
    pub mentions: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mention {
    Here,
    Everyone,
    User(u64),
    Role(u64),
}

impl std::str::FromStr for Mention {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let id = |v: &str| {
            v.trim()
                .parse::<u64>()
                .map_err(|_| format!("`{s}`: expected a numeric Discord id"))
        };
        match s.trim() {
            "@here" | "here" => Ok(Mention::Here),
            "@everyone" | "everyone" => Ok(Mention::Everyone),
            other => {
                if let Some(v) = other.strip_prefix("user:") {
                    Ok(Mention::User(id(v)?))
                } else if let Some(v) = other.strip_prefix("role:") {
                    Ok(Mention::Role(id(v)?))
                } else if let Some(v) = other.strip_prefix("<@&").and_then(|v| v.strip_suffix('>'))
                {
                    Ok(Mention::Role(id(v)?))
                } else if let Some(v) = other.strip_prefix("<@").and_then(|v| v.strip_suffix('>')) {
                    Ok(Mention::User(id(v.trim_start_matches('!'))?))
                } else {
                    Err(format!(
                        "`{s}`: use @here, @everyone, user:<id> or role:<id>"
                    ))
                }
            }
        }
    }
}

impl std::fmt::Display for Mention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mention::Here => f.write_str("@here"),
            Mention::Everyone => f.write_str("@everyone"),
            Mention::User(id) => write!(f, "user:{id}"),
            Mention::Role(id) => write!(f, "role:{id}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mentions() {
        assert_eq!("@here".parse(), Ok(Mention::Here));
        assert_eq!("user:42".parse(), Ok(Mention::User(42)));
        assert_eq!("<@!42>".parse(), Ok(Mention::User(42)));
        assert_eq!("<@&7>".parse(), Ok(Mention::Role(7)));
        assert!("bob".parse::<Mention>().is_err());
    }
}
