//! HTTP API bodies, shared by the server and the CLI.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::proto::{AppState, AppStats, BuildState, NodeStats, StorageReport};
use crate::spec::{AppSpec, Build};
use crate::stack::StackSpec;

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
    /// Docker's disk usage, volumes and the last cleanup.
    #[serde(default)]
    pub storage: Option<StorageReport>,
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
    /// The commit the app was last deployed from (git sources).
    #[serde(default)]
    pub commit: Option<String>,
    pub stats: Option<AppStats>,
    /// The member whose resources and logins the app uses; `None` for the admins.
    #[serde(default)]
    pub owner: Option<String>,
    /// Other members who can manage it.
    #[serde(default)]
    pub access: Vec<String>,
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
    /// Waiting for the node's build slot.
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl DeploymentStatus {
    pub fn is_finished(self) -> bool {
        !matches!(self, DeploymentStatus::Queued | DeploymentStatus::Running)
    }
}

impl std::fmt::Display for DeploymentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DeploymentStatus::Queued => "queued",
            DeploymentStatus::Running => "running",
            DeploymentStatus::Succeeded => "succeeded",
            DeploymentStatus::Failed => "failed",
            DeploymentStatus::Cancelled => "cancelled",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentView {
    pub id: Uuid,
    pub app: String,
    pub revision: u64,
    pub status: DeploymentStatus,
    /// When it was asked for.
    pub created_at: u64,
    /// When the node started on it (it may wait in the build queue first).
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub error: Option<String>,
    /// Full hash of the commit built (git sources).
    #[serde(default)]
    pub commit: Option<String>,
    /// Its subject line, once the node has checked it out.
    #[serde(default)]
    pub commit_title: Option<String>,
    /// Only filled when a single deployment is fetched.
    #[serde(default)]
    pub log: String,
}

/// `POST /api/apps/{name}/deploy`; an empty body deploys the branch's latest commit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeployRequest {
    /// A full commit hash, a branch or a tag to deploy instead.
    #[serde(default)]
    pub commit: Option<String>,
    /// Build again even if an image of this commit is still on the node.
    #[serde(default)]
    pub rebuild: bool,
}

/// What every node is building, and recent deployments of all apps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildsView {
    pub queue: Vec<QueuedBuild>,
    pub recent: Vec<DeploymentView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedBuild {
    pub node: String,
    pub app: String,
    pub deployment: Option<Uuid>,
    pub commit: Option<String>,
    pub state: BuildState,
    pub since: u64,
    /// 0 while building; 1 for the next in line, and so on.
    pub position: usize,
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
    /// The git credential used to fetch it, if any.
    #[serde(default)]
    pub credential: Option<String>,
    /// Compose files found (`docker-compose.yml`, …), each usable as a stack.
    #[serde(default)]
    pub compose_files: Vec<String>,
}

/// Scan errors caused by a private repository (or a wrong URL) start with this, so
/// clients can offer to add a credential.
pub const NO_ACCESS: &str = "no access to";

// ---------------------------------------------------------------- git credentials

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitCredentialKind {
    /// An SSH key facade generated (or was given): a deploy key for one repository.
    SshKey,
    /// An access token (GitHub fine-grained token, GitLab project token, …) for
    /// every repository under its scope.
    Token,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCredentialView {
    /// Whose it is: a member's name, or `None` for the admins'. Builds of an app
    /// use its owner's credentials.
    #[serde(default)]
    pub owner: Option<String>,
    pub name: String,
    pub kind: GitCredentialKind,
    /// Repositories it covers: `github.com/owner/repo`, `github.com/owner`, …
    pub scope: String,
    pub username: Option<String>,
    /// For SSH keys: the public half, to register on the forge.
    pub public_key: Option<String>,
    /// For SSH keys scoped to one repository on a known forge: where to add it.
    pub add_key_url: Option<String>,
    pub created_at: u64,
    /// Apps whose repository it covers.
    pub apps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateGitCredential {
    pub name: String,
    /// A repository URL, or a prefix such as `github.com/my-org`.
    pub scope: String,
    pub secret: NewGitSecret,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NewGitSecret {
    /// `None` generates an ed25519 key.
    SshKey {
        #[serde(default)]
        private_key: Option<String>,
    },
    Token {
        /// Defaults to `x-access-token`, which GitHub and GitLab accept with tokens.
        #[serde(default)]
        username: Option<String>,
        token: String,
    },
}

// ---------------------------------------------------------------- databases & import

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseKind {
    Postgres,
    Mysql,
    Mariadb,
    Redis,
}

impl std::str::FromStr for DatabaseKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "pg" => Ok(DatabaseKind::Postgres),
            "mysql" => Ok(DatabaseKind::Mysql),
            "mariadb" => Ok(DatabaseKind::Mariadb),
            "redis" | "valkey" => Ok(DatabaseKind::Redis),
            _ => Err(format!("`{s}`: use postgres, mysql, mariadb or redis")),
        }
    }
}

/// `POST /api/databases`: an app running a database image with a volume, a
/// generated password and a connection URL other apps can reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateDatabase {
    pub name: String,
    pub kind: DatabaseKind,
    /// Image tag, e.g. `17` (default: the template's).
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub memory_mb: Option<u64>,
    /// Apps to link to it; each gets the connection URL in its environment.
    #[serde(default)]
    pub link: Vec<String>,
    /// Variable the linked apps get the URL in (default `DATABASE_URL`, or
    /// `REDIS_URL` for Redis).
    #[serde(default)]
    pub env_var: Option<String>,
}

// ---------------------------------------------------------------- stacks

/// A stack, with the apps running its services.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackView {
    pub spec: StackSpec,
    /// Commit of the last successful deploy.
    pub commit: Option<String>,
    pub services: Vec<AppView>,
    /// The deployment in progress, if any.
    pub deploying: Option<Uuid>,
}

/// `POST /api/stacks/plan`: what deploying a stack would do, without doing it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackPlanRequest {
    pub spec: StackSpec,
    /// A full commit hash, branch or tag (default: the stack's branch).
    #[serde(default)]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackPlan {
    pub commit: Option<String>,
    pub commit_title: Option<String>,
    /// In deploy order.
    pub services: Vec<PlannedService>,
    /// Services running now that the stack no longer has.
    pub removed: Vec<String>,
    /// What the compose file asks for that facade does not do.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedService {
    pub service: String,
    pub app: AppSpec,
    pub depends_on: Vec<String>,
    /// Variables from the compose file; sensitive values hidden.
    pub env: Vec<EnvView>,
    /// Variables still to set (`facade env set <stack>.<service> KEY`).
    pub missing: Vec<String>,
    pub change: ServiceChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceChange {
    New,
    Changed,
    Unchanged,
}

/// `POST /api/stacks/{name}/deploy`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StackDeployRequest {
    /// A full commit hash, a branch or a tag (default: the stack's branch).
    #[serde(default)]
    pub commit: Option<String>,
    /// Build again without Docker's cache.
    #[serde(default)]
    pub rebuild: bool,
    /// Replace every service, changed or not (and pull images again).
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackDeploymentView {
    pub id: Uuid,
    pub stack: String,
    pub status: DeploymentStatus,
    pub commit: Option<String>,
    pub commit_title: Option<String>,
    pub created_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub error: Option<String>,
    /// The deployment of each service it replaced.
    pub services: Vec<StackDeploymentService>,
    /// Only filled when a single deployment is fetched.
    #[serde(default)]
    pub log: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackDeploymentService {
    pub service: String,
    pub deployment: Uuid,
}

/// A volume on a node, as reported by its agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeView {
    pub node: String,
    pub name: String,
    /// The app, or the stack, it belongs to.
    pub app: String,
    pub volume: String,
    pub size: Option<u64>,
    /// Bytes it may hold, for volumes with a size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    pub in_use: bool,
    /// Still declared by the app's spec (on this node).
    pub declared: bool,
    /// Newest backup of it, as a Unix time.
    #[serde(default)]
    pub last_backup: Option<u64>,
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
    /// Suggested command, e.g. to listen on every interface instead of loopback.
    #[serde(default)]
    pub command: Vec<String>,
    /// Things to know before deploying it.
    #[serde(default)]
    pub notes: Vec<String>,
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

/// Resource use over a span of time starting at `at`: averages and peaks. CPU is
/// in percent (of one core for an app, of the whole machine for a node), memory
/// and disk in bytes; only nodes report their disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsagePoint {
    pub at: u64,
    pub cpu: f32,
    pub cpu_peak: f32,
    pub mem: u64,
    pub mem_peak: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<u64>,
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

/// Who is logged in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoAmI {
    pub username: String,
    pub role: String,
    /// Still on the password generated at install (or reset by an admin).
    #[serde(default)]
    pub must_change_password: bool,
    /// Shared by every app the user owns; `None` is no limit.
    #[serde(default)]
    pub memory_cap_mb: Option<u64>,
    #[serde(default)]
    pub cpu_cap: Option<f64>,
    /// What the user's running apps reserve, against those caps.
    #[serde(default)]
    pub memory_allocated_mb: u64,
    #[serde(default)]
    pub cpus_allocated: f64,
}

/// `admin`: everything. `member`: only the apps they own or were given, and
/// nothing global.
pub const ROLES: [&str; 2] = ["admin", "member"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserView {
    pub username: String,
    pub role: String,
    pub memory_cap_mb: Option<u64>,
    pub cpu_cap: Option<f64>,
    pub memory_allocated_mb: u64,
    pub cpus_allocated: f64,
    /// Apps the user owns.
    pub apps: Vec<String>,
    /// Apps the user was given access to.
    pub shared_apps: Vec<String>,
    pub must_change_password: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateUser {
    pub username: String,
    #[serde(default = "member")]
    pub role: String,
    #[serde(default)]
    pub memory_cap_mb: Option<u64>,
    #[serde(default)]
    pub cpu_cap: Option<f64>,
}

fn member() -> String {
    "member".into()
}

/// Replaces a user's role and caps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateUser {
    pub role: String,
    #[serde(default)]
    pub memory_cap_mb: Option<u64>,
    #[serde(default)]
    pub cpu_cap: Option<f64>,
}

/// A password made up by the server: shown once, to be changed at first login.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OneTimePassword {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetOwner {
    /// `None`: the admins.
    pub owner: Option<String>,
}

/// How the server itself is reached. Changed from the CLI or web UI; the
/// config file only gives the values used before the first change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    /// Domain serving the web UI and API with a real certificate, through the
    /// server's own HTTPS entry point.
    pub api_domain: Option<String>,
    /// Whether `api_domain` has a certificate (so it works in a browser).
    pub api_domain_ready: bool,
    /// Contact address given to Let's Encrypt (expiry notices).
    pub acme_email: Option<String>,
    /// Address of the direct API port (self-signed certificate).
    pub api_listen: String,
    /// The direct API port only accepts connections from the server itself
    /// (SSH tunnels), so the API is public only through `api_domain`.
    pub direct_api_local_only: bool,
}

/// Fields left out are unchanged; an empty string removes a value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerSettingsUpdate {
    #[serde(default)]
    pub api_domain: Option<String>,
    #[serde(default)]
    pub acme_email: Option<String>,
    #[serde(default)]
    pub direct_api_local_only: Option<bool>,
}

/// A container registry login. The secret is never sent back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryLoginView {
    pub host: String,
    pub username: String,
    /// Whose it is: a member's name, or `None` for the admins'. Pulls and builds of
    /// an app use its owner's logins.
    #[serde(default)]
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRegistryLogin {
    pub username: String,
    pub secret: String,
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

/// What a deploy would bring: the branch's latest commit compared to the deployed one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCheck {
    /// The branch deployments build, as configured (`None`: the default branch).
    pub branch: Option<String>,
    /// Commit of the last successful deployment.
    pub deployed: Option<String>,
    pub latest: CommitView,
    /// Commits on the branch since the deployed one. `None` when that is unknown
    /// (never deployed from git, or the deployed commit is no longer on the branch).
    pub behind: Option<u32>,
    /// Those commits, newest first (at most 50); the latest few when `behind` is unknown.
    pub commits: Vec<CommitView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitView {
    pub hash: String,
    pub subject: String,
    pub author: String,
    /// Unix time.
    pub time: u64,
}
