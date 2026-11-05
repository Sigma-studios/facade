//! HTTP API bodies, shared by the server and the CLI.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::proto::{AppState, AppStats, BuildState, ImageLayer, NodeStats, StorageReport, Traffic};
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
    /// The system it runs on, as its agent last said (`None`: an agent from
    /// before platforms, or never connected).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<crate::platform::PlatformInfo>,
    /// Pending system updates and reboot, as the node last said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upkeep: Option<crate::events::Upkeep>,
    /// High and critical vulnerabilities in what it runs (`None`: not checked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vulns: Option<crate::vulns::VulnSummary>,
    /// Disk errors the kernel reported in the last day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_errors: Option<crate::events::DiskTrouble>,
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
    /// From the node's last storage report (every 15 minutes).
    #[serde(default)]
    pub disk: Option<AppDiskView>,
    /// CPU throttling and disk activity over the last five minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent: Option<crate::resources::ResourcePoint>,
}

/// A volume's size over time: `(at, bytes)`, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VolumeSizes {
    pub volume: String,
    pub points: Vec<(u64, u64)>,
}

/// What an app takes on its node's disk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AppDiskView {
    pub at: u64,
    /// The image it runs, plus what the rollback images add.
    pub images: u64,
    pub rollback_images: u32,
    /// Written inside the container, outside volumes: lost on redeploy.
    pub written: u64,
    /// Static site files.
    pub files: u64,
    /// Its volumes; a stack volume counts for each service mounting it.
    pub volumes: u64,
    /// Its volumes' backups, kept on the server's disk (whichever node the app
    /// runs on): not in `total`.
    #[serde(default)]
    pub backups: u64,
    /// The largest layers of the image it runs, largest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<ImageLayer>,
    /// The image's other layers holding files: how many, and what they take.
    #[serde(default)]
    pub smaller_layers: u32,
    #[serde(default)]
    pub smaller_layers_size: u64,
}

impl AppDiskView {
    pub fn total(&self) -> u64 {
        self.images + self.written + self.files + self.volumes
    }
}

/// Above this, what an app writes inside its container gets flagged: it belongs in
/// a volume.
pub const WRITTEN_WARNING: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvView {
    pub key: String,
    /// `None` when the variable is sensitive and was not revealed.
    pub value: Option<String>,
    pub sensitive: bool,
    /// Handed to the app's builds as a secret, as well as to its container.
    #[serde(default)]
    pub build: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetEnv {
    pub value: String,
    #[serde(default = "default_sensitive")]
    pub sensitive: bool,
    /// Hand it to builds too; `None` keeps the variable's current setting (off
    /// for a new one), so changing a value never drops it from builds by accident.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<bool>,
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

/// `GET /api/deployments?app=web,api&status=failed&limit=50`: the latest
/// deployments of the apps the caller sees, and how many match. Every
/// deployment is kept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentsPage {
    pub total: u64,
    pub deployments: Vec<DeploymentView>,
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
    /// The node's apps' traffic added up, counted by the server from what each
    /// report adds (no latency); `None` when none of its apps' is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic: Option<Traffic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppMetric {
    pub at: u64,
    pub stats: AppStats,
}

/// Resource use over a span of time starting at `at`: averages and peaks. CPU is
/// in percent (of one core for an app, of the whole machine for a node), memory
/// and disk in bytes; only nodes report their disk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsagePoint {
    pub at: u64,
    pub cpu: f32,
    pub cpu_peak: f32,
    pub mem: u64,
    pub mem_peak: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<u64>,
    /// Network traffic over the point's period, in bytes per second received
    /// and sent (a node's cards, an app's container); `None` where it is not
    /// known (the first point, a reboot or a new container, nothing counted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_rx: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_tx: Option<f64>,
    /// Requests through the entry point per second, and those answered 4xx and
    /// 5xx; `None` where not known (no domain, a new entry point's first
    /// period). A node's add up its apps'.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors_4xx: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors_5xx: Option<f64>,
    /// Median and 95th percentile time to response headers, in milliseconds,
    /// over the period's requests (`None` without any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_ms: Option<f64>,
    /// Client connections open through the entry point, on average.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<f64>,
    /// CPU throttling, disk activity, pressure (see [`crate::resources`]).
    #[serde(
        default,
        skip_serializing_if = "crate::resources::ResourcePoint::is_empty"
    )]
    pub resources: crate::resources::ResourcePoint,
}

impl UsagePoint {
    /// The point's traffic figures, from what the counters grew by over it.
    pub fn set_traffic(&mut self, span: &TrafficSpan) {
        self.requests = Some(span.per_sec(span.requests));
        self.errors_4xx = Some(span.per_sec(span.client_errors));
        self.errors_5xx = Some(span.per_sec(span.server_errors));
        self.p50_ms = span.percentile_ms(0.5);
        self.p95_ms = span.percentile_ms(0.95);
    }
}

/// Bytes per second between two readings of a counter of bytes since boot;
/// `None` when either is missing (0) or the counter went back (a reboot).
pub fn byte_rate(before: (u64, u64), after: (u64, u64)) -> Option<f64> {
    let ((t0, b0), (t1, b1)) = (before, after);
    (b0 > 0 && b1 >= b0 && t1 > t0).then(|| (b1 - b0) as f64 / (t1 - t0) as f64)
}

/// What an app's traffic counters grew by between two readings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrafficSpan {
    pub secs: u64,
    pub requests: u64,
    pub client_errors: u64,
    pub server_errors: u64,
    pub latency: Vec<u64>,
}

impl TrafficSpan {
    /// Between readings `before` and `after` (with their times); `None` when the
    /// counters started again in between (a new entry point) or time did not pass.
    pub fn between(before: (u64, &Traffic), after: (u64, &Traffic)) -> Option<Self> {
        let ((t0, a), (t1, b)) = (before, after);
        if a.since != b.since || t1 <= t0 || b.requests < a.requests {
            return None;
        }
        let latency = if a.latency.len() == b.latency.len() {
            b.latency
                .iter()
                .zip(&a.latency)
                .map(|(b, a)| b.saturating_sub(*a))
                .collect()
        } else {
            Vec::new()
        };
        Some(Self {
            secs: t1 - t0,
            requests: b.requests - a.requests,
            client_errors: b.client_errors.saturating_sub(a.client_errors),
            server_errors: b.server_errors.saturating_sub(a.server_errors),
            latency,
        })
    }

    /// The span from the first reading at most `window` seconds before the last
    /// one counting since the same moment, to that last one.
    pub fn recent<'a>(
        mut readings: impl DoubleEndedIterator<Item = (u64, &'a Traffic)> + Clone,
        window: u64,
    ) -> Option<Self> {
        let (t1, last) = readings.clone().next_back()?;
        let (t0, first) = readings.find(|(at, t)| at + window >= t1 && t.since == last.since)?;
        Self::between((t0, first), (t1, last))
    }

    pub fn per_sec(&self, count: u64) -> f64 {
        count as f64 / self.secs.max(1) as f64
    }

    /// The `q` quantile (0.5 for the median) of response times, in milliseconds,
    /// interpolated within its bucket; `None` without requests.
    pub fn percentile_ms(&self, q: f64) -> Option<f64> {
        latency_percentile(&self.latency, q)
    }
}

/// The `q` quantile of a histogram of response times (per
/// [`crate::proto::LATENCY_BOUNDS_MS`]), in milliseconds.
pub fn latency_percentile(histogram: &[u64], q: f64) -> Option<f64> {
    use crate::proto::LATENCY_BOUNDS_MS as BOUNDS;
    let total: u64 = histogram.iter().sum();
    if total == 0 {
        return None;
    }
    let target = q.clamp(0.0, 1.0) * total as f64;
    let mut below = 0.0;
    for (i, &count) in histogram.iter().enumerate() {
        let count = count as f64;
        if count > 0.0 && below + count >= target {
            let low = if i == 0 { 0.0 } else { BOUNDS[i - 1] };
            // Slower than the last bound: say the last bound.
            let Some(&high) = BOUNDS.get(i) else {
                return Some(low);
            };
            return Some(low + (high - low) * ((target - below) / count));
        }
        below += count;
    }
    BOUNDS.last().copied()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertView {
    pub key: String,
    pub message: String,
    pub since: u64,
    /// `None` while still firing.
    pub resolved_at: Option<u64>,
}

/// Something wrong right now (`GET /api/problems`): it is listed while it lasts
/// and goes away by itself once fixed. Alerts and the Activity keep the history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    /// Stable while it lasts: `app:blog:down`, `dns:blog.example.com`.
    pub key: String,
    pub severity: ProblemSeverity,
    pub message: String,
    /// What it concerns; several for a domain of an app, none for the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Since when it is known to be wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    /// Web UI path of where it gets fixed (`/apps/blog?tab=logs`).
    pub fix: String,
    /// That place, in a word or two (`Logs`, `Domains`).
    pub fix_label: String,
    /// What to do, when it is plain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Critical sorts first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemSeverity {
    /// Down or broken now.
    Critical,
    /// Will break, or works worse, unless looked at.
    Warning,
}

impl Problem {
    /// What it is about first: `("domain", "blog.example.com")`, then an app, a
    /// stack, a node; `("server", "")` for the rest.
    pub fn subject(&self) -> (&'static str, &str) {
        if let Some(d) = &self.domain {
            ("domain", d)
        } else if let Some(a) = &self.app {
            ("app", a)
        } else if let Some(s) = &self.stack {
            ("stack", s)
        } else if let Some(n) = &self.node {
            ("node", n)
        } else {
            ("server", "")
        }
    }
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
    /// The outside check's last result; `None` until checked (or from servers
    /// without it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reach: Option<crate::insights::Reach>,
}

/// Public suffixes of more than one label that people register under, so
/// `shop.example.co.uk` belongs to `example.co.uk`. Not the whole list: the
/// common ones, separated by spaces.
const SUFFIXES: &str = "\
    co.uk org.uk me.uk ltd.uk plc.uk net.uk ac.uk gov.uk com.au net.au org.au edu.au gov.au \
    co.nz net.nz org.nz co.jp ne.jp or.jp ac.jp co.kr or.kr com.br net.br org.br com.mx com.ar \
    com.tr com.cn net.cn org.cn com.tw com.hk com.sg com.my co.in net.in org.in co.id co.il \
    co.za org.za com.ua com.pl co.at or.at com.es com.pt co.it eu.org github.io gitlab.io \
    pages.dev netlify.app vercel.app fly.dev herokuapp.com duckdns.org";

/// The registered domain a host name belongs to: `www.example.com` →
/// `example.com`. Addresses stay as they are. The Domains page groups names by
/// it (`/domains?domain=example.com`).
pub fn main_domain(host: &str) -> String {
    let host = host.trim_end_matches('.').to_lowercase();
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let n = labels.len();
    let keep = if n >= 3
        && SUFFIXES
            .split_whitespace()
            .any(|s| s == labels[n - 2..].join("."))
    {
        3
    } else {
        2
    };
    labels[n.saturating_sub(keep)..].join(".")
}

/// Where alerts go. Read with the webhook partly hidden.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotifySettings {
    pub discord_webhook: Option<String>,
    /// Tagged on problems (not on resolutions): `@here`, `@everyone`, `user:<id>`, `role:<id>`.
    pub mentions: Vec<String>,
    /// When a message last reached Discord, since the server started or the
    /// webhook changed.
    #[serde(default)]
    pub last_sent: Option<u64>,
    /// The last delivery that failed, and why; cleared by the next success.
    #[serde(default)]
    pub last_error: Option<NotifyError>,
    /// The weekly summary is switched on (it goes out only with a webhook).
    #[serde(default)]
    pub weekly: bool,
    /// When it goes out, like `Monday 07:00 UTC`; `None` from servers without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_schedule: Option<String>,
    /// The next time it goes out, if it is on and there is a webhook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_next: Option<u64>,
    /// When it last went out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_sent: Option<u64>,
}

/// The weekly summary as it would be posted: a title, then one Discord embed
/// per part (a long one is cut at line ends).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WeeklyPreview {
    pub title: String,
    pub parts: Vec<String>,
    /// The days covered, `[from, to)` (UTC days since the epoch).
    pub from_day: u64,
    pub to_day: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotifyError {
    pub at: u64,
    pub error: String,
}

/// Fields left out are unchanged; an empty webhook removes it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotifyUpdate {
    #[serde(default)]
    pub discord_webhook: Option<String>,
    #[serde(default)]
    pub mentions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<bool>,
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

    fn reading(since: u64, requests: u64, errors: u64, latency: Vec<u64>) -> Traffic {
        Traffic {
            since,
            requests,
            client_errors: 0,
            server_errors: errors,
            latency,
            ..Default::default()
        }
    }

    #[test]
    fn traffic_spans_restart_with_their_counters() {
        let buckets = crate::proto::LATENCY_BUCKETS;
        let mut lat = vec![0; buckets];
        let a = reading(100, 10, 1, lat.clone());
        // 100 requests between 50 and 75 ms (bucket 11), 10 between 0 and 1 ms.
        lat[11] = 100;
        lat[0] = 10;
        let b = reading(100, 120, 21, lat);
        let span = TrafficSpan::between((1000, &a), (1010, &b)).unwrap();
        assert_eq!(
            (span.secs, span.requests, span.server_errors),
            (10, 110, 20)
        );
        assert_eq!(span.per_sec(span.requests), 11.0);
        let p50 = span.percentile_ms(0.5).unwrap();
        assert!((50.0..=75.0).contains(&p50), "{p50}");
        assert!(span.percentile_ms(0.05).unwrap() <= 1.0);
        assert!(span.percentile_ms(0.95).unwrap() > p50);
        // A new entry point: its counters do not compare with the old one's.
        let c = reading(1005, 3, 0, vec![0; buckets]);
        assert_eq!(TrafficSpan::between((1010, &b), (1020, &c)), None);
        // Nor do counters that went back.
        assert_eq!(TrafficSpan::between((1010, &b), (1020, &a)), None);
        // The window starts after the restart.
        let d = reading(1005, 13, 0, vec![0; buckets]);
        let readings = [(1000, &a), (1010, &b), (1020, &c), (1030, &d)];
        let recent = TrafficSpan::recent(readings.iter().copied(), 300).unwrap();
        assert_eq!((recent.secs, recent.requests), (10, 10));
        assert_eq!(recent.percentile_ms(0.5), None, "no latency counted");
    }

    #[test]
    fn slow_requests_read_as_the_last_bound() {
        let mut lat = vec![0; crate::proto::LATENCY_BUCKETS];
        *lat.last_mut().unwrap() = 5;
        assert_eq!(latency_percentile(&lat, 0.5), Some(30_000.0));
        assert_eq!(latency_percentile(&[], 0.5), None);
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

/// Updating facade itself to the latest build published from its repository's
/// main branch (Settings → Updates, `facade server update`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerUpdate {
    /// GitHub repository the builds come from (`owner/name`).
    pub repository: String,
    pub current: RunningBuild,
    /// The published build, as of the last check.
    #[serde(default)]
    pub latest: Option<PublishedBuild>,
    #[serde(default)]
    pub checked_at: Option<u64>,
    #[serde(default)]
    pub check_error: Option<String>,
    /// `latest` is another build than the running one.
    #[serde(default)]
    pub available: bool,
    /// Commits between the running build and `latest`, newest first (at most 50).
    /// Empty when that is unknown (a local build).
    #[serde(default)]
    pub commits: Vec<CommitView>,
    /// Downloaded and checked, ready to install.
    #[serde(default)]
    pub prepared: Option<PreparedUpdate>,
    /// The update in progress, or the last one.
    #[serde(default)]
    pub run: Option<UpdateRun>,
    #[serde(default)]
    pub nodes: Vec<NodeBuild>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningBuild {
    /// Commit the server's facaded was built from; `None` for a build without one.
    pub commit: Option<String>,
    pub version: String,
    /// CPU architecture (`x86_64`, `aarch64`).
    pub arch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishedBuild {
    pub commit: String,
    pub subject: String,
    /// Unix time of the commit.
    pub time: u64,
    /// CPU architectures it is built for.
    pub arches: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedUpdate {
    pub commit: String,
    /// What installing it does to the apps on the server's node.
    pub plan: Vec<PlannedChange>,
}

/// Output of `facaded upgrade-check --json`, run with the binary to install.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedChange {
    pub app: String,
    pub change: AppChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppChange {
    Unchanged,
    /// Its container is recreated (a few seconds down).
    Restart,
    /// Its image is rebuilt (git: at the deployed commit) or pulled.
    Rebuild,
    /// Not running on this node now; nothing to compare.
    NotRunning,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRun {
    /// Commit being installed.
    pub to: String,
    pub from: Option<String>,
    pub state: UpdateState,
    /// The step under way, or how it ended.
    pub message: String,
    pub started: u64,
    #[serde(default)]
    pub finished: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    Running,
    Done,
    /// The new build did not come up healthy; the previous one is back.
    RolledBack,
    Failed,
}

/// Whether a node runs the server's build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeBuild {
    pub name: String,
    pub connected: bool,
    pub arch: Option<String>,
    pub state: NodeBuildState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeBuildState {
    Current,
    /// Runs another build; it is sent the server's when it connects.
    Updating,
    /// Runs another build, and the server has none for its CPU: update it by hand
    /// (or update the server from a published build, which brings them all).
    Manual,
    /// It has not said which build it runs.
    Unknown,
}

/// The build a prepare or install is meant for, as last shown to the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateTarget {
    pub commit: String,
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
    /// What a full copy of it takes, compressed: for an incremental backup, its
    /// chunks, shared with other backups or not.
    pub size: u64,
    /// What it added to the backup store (the chunks no other backup had); none
    /// for a full copy (`.tar.gz`, from before incremental backups).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<u64>,
}

/// `GET /api/backups/store`: what volume backups take on the server's disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackupStoreView {
    /// Chunks of incremental backups, each stored once: how many, and what they
    /// take.
    pub chunks: u64,
    pub chunk_bytes: u64,
    /// Incremental backups, and what they would take as separate full copies.
    pub snapshots: u64,
    pub snapshot_bytes: u64,
    /// Full copies (`.tar.gz`, from before incremental backups), and what they
    /// take.
    pub full_copies: u64,
    pub full_copy_bytes: u64,
}

impl BackupStoreView {
    /// What volume backups take on disk.
    pub fn on_disk(&self) -> u64 {
        self.chunk_bytes + self.full_copy_bytes
    }
}

/// `POST /api/backups/check`: every chunk incremental backups hold, read back.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackupCheckReport {
    pub store: BackupStoreView,
    /// Chunks found intact.
    pub verified: u64,
    /// Missing or damaged chunks (with the backups holding them), unreadable
    /// manifests.
    pub problems: Vec<String>,
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

/// What a deploy of an image app would pull (`POST /api/apps/{name}/image-updates`):
/// the digest its tag points to at the registry, against the one the last
/// successful deploy pulled. Nothing is pulled to find out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUpdateCheck {
    /// The image and its tag (`ghcr.io/me/blog:latest`).
    pub image: String,
    /// The digest the tag points to now.
    pub latest: String,
    /// The digest the last successful deploy pulled; `None` when its log does
    /// not say (the image was already on the node, or never deployed).
    pub deployed: Option<String>,
}

impl ImageUpdateCheck {
    /// `None` when what runs is unknown.
    pub fn outdated(&self) -> Option<bool> {
        self.deployed.as_ref().map(|d| *d != self.latest)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitView {
    pub hash: String,
    pub subject: String,
    pub author: String,
    /// Unix time.
    pub time: u64,
}

// ---------------------------------------------------------------- auto-deploy

/// Whether an app (or stack) deploys by itself when its source changes, and how
/// a change is noticed (`GET /api/apps/{name}/auto-deploy`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoDeployView {
    pub enabled: bool,
    /// What is watched: `github.com/me/shop (main)`, `ghcr.io/me/blog:latest`.
    /// `None` for sources that cannot change by themselves (uploaded executables).
    pub watching: Option<String>,
    pub method: AutoDeployMethod,
    /// Seconds between two checks of the source (longer while a webhook works).
    pub poll_secs: u64,
    /// What stands between polling and a webhook, or why nothing is watched.
    #[serde(default)]
    pub note: Option<String>,
    /// The GitHub repository whose webhook reports changes, once facade set it up.
    #[serde(default)]
    pub webhook_repo: Option<String>,
    /// Unix time of the last delivery from that webhook.
    #[serde(default)]
    pub webhook_delivered_at: Option<u64>,
    #[serde(default)]
    pub checked_at: Option<u64>,
    /// The commit or image digest last seen at the source.
    #[serde(default)]
    pub seen: Option<String>,
    #[serde(default)]
    pub triggered_at: Option<u64>,
    /// What the last automatic deploy deployed: a commit, or an image digest.
    #[serde(default)]
    pub triggered: Option<String>,
    /// `webhook` or `poll`.
    #[serde(default)]
    pub triggered_by: Option<String>,
    /// Why the last check or automatic deploy failed.
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoDeployMethod {
    Off,
    Polling,
    Webhook,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetAutoDeploy {
    pub enabled: bool,
}

/// One app or stack with auto-deploy on (`GET /api/auto-deploy`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoDeployTarget {
    /// `app` or `stack`.
    pub kind: String,
    pub name: String,
    #[serde(flatten)]
    pub status: AutoDeployView,
}

/// What a webhook set up by hand needs (`GET /api/hooks/github`, admins).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookInfo {
    /// `None` until the server has a domain (`facade server set --api-domain`).
    pub url: Option<String>,
    pub secret: String,
}
