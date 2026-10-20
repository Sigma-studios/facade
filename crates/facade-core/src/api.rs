//! HTTP API bodies, shared by the server and the CLI.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::proto::{AppState, AppStats, NodeStats};
use crate::spec::AppSpec;

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
    pub address: Option<String>,
    pub agent_version: Option<String>,
    pub last_seen: Option<u64>,
    pub stats: Option<NodeStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateNode {
    pub name: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
