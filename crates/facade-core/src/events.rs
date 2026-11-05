//! Activity: what happened to apps and nodes, one entry each, newest first.
//!
//! Entries come from three places: the agents (containers crashing, being
//! killed for memory, turning unhealthy; kernel and SSH logs; a node's
//! upkeep), the server's own actions with the account behind them (deploys,
//! restarts, variable and settings changes), and its watchers (alerts,
//! certificates, DNS records, auto-deploy).

use serde::{Deserialize, Serialize};

pub use crate::platform::Checked;
use uuid::Uuid;

/// How an entry reads: grey, green, orange or red.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Info,
    Ok,
    Warn,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Ok => "ok",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "ok" => Severity::Ok,
            "warn" => Severity::Warn,
            "error" => Severity::Error,
            _ => Severity::Info,
        }
    }
}

/// Where an entry shows: an app's Activity, a node's (and the app's, when one
/// is named: a kernel OOM kill inside its container), or the global list only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    #[default]
    App,
    Node,
    Global,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::App => "app",
            Scope::Node => "node",
            Scope::Global => "global",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "node" => Scope::Node,
            "global" => Scope::Global,
            _ => Scope::App,
        }
    }
}

/// What happened. Kinds a newer server sends read as `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    // ---- deploys
    /// Asked for by someone (or by auto-deploy, see `AutoDeploy`).
    DeployStarted,
    Deployed,
    DeployFailed,
    DeployCancelled,
    Rollback,
    /// A change at the source noticed, and the deploy it started.
    AutoDeploy,
    // ---- restarts and crashes
    /// The container stopped without facade stopping it.
    Crash,
    /// The kernel killed a process for memory: the app's container (at its
    /// limit), or anything on the node.
    OomKill,
    Restart,
    Stop,
    Start,
    // ---- health
    Unhealthy,
    Healthy,
    // ---- changes
    Created,
    Deleted,
    Settings,
    Env,
    Domain,
    AutoDeploySwitch,
    Access,
    // ---- alerts
    AlertFired,
    AlertResolved,
    // ---- logins
    SshLogin,
    // ---- certificates and DNS
    CertIssued,
    CertFailed,
    DnsRecord,
    // ---- system and maintenance
    Reboot,
    AgentRestart,
    ServerStart,
    FacadeUpdate,
    Upkeep,
    DiskError,
    #[serde(other)]
    Other,
}

/// What the Activity filters offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Deploys,
    Restarts,
    Health,
    Changes,
    Alerts,
    Logins,
    Certificates,
    System,
}

impl Category {
    pub const ALL: [Category; 8] = [
        Category::Deploys,
        Category::Restarts,
        Category::Health,
        Category::Changes,
        Category::Alerts,
        Category::Logins,
        Category::Certificates,
        Category::System,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Category::Deploys => "deploys",
            Category::Restarts => "restarts",
            Category::Health => "health",
            Category::Changes => "changes",
            Category::Alerts => "alerts",
            Category::Logins => "logins",
            Category::Certificates => "certificates",
            Category::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Category::Deploys => "Deploys",
            Category::Restarts => "Restarts & crashes",
            Category::Health => "Health",
            Category::Changes => "Changes",
            Category::Alerts => "Alerts",
            Category::Logins => "Logins",
            Category::Certificates => "Certificates & DNS",
            Category::System => "System & upkeep",
        }
    }
}

impl EventKind {
    pub const ALL: [EventKind; 33] = [
        EventKind::DeployStarted,
        EventKind::Deployed,
        EventKind::DeployFailed,
        EventKind::DeployCancelled,
        EventKind::Rollback,
        EventKind::AutoDeploy,
        EventKind::Crash,
        EventKind::OomKill,
        EventKind::Restart,
        EventKind::Stop,
        EventKind::Start,
        EventKind::Unhealthy,
        EventKind::Healthy,
        EventKind::Created,
        EventKind::Deleted,
        EventKind::Settings,
        EventKind::Env,
        EventKind::Domain,
        EventKind::AutoDeploySwitch,
        EventKind::Access,
        EventKind::AlertFired,
        EventKind::AlertResolved,
        EventKind::SshLogin,
        EventKind::CertIssued,
        EventKind::CertFailed,
        EventKind::DnsRecord,
        EventKind::Reboot,
        EventKind::AgentRestart,
        EventKind::ServerStart,
        EventKind::FacadeUpdate,
        EventKind::Upkeep,
        EventKind::DiskError,
        EventKind::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::DeployStarted => "deploy_started",
            EventKind::Deployed => "deployed",
            EventKind::DeployFailed => "deploy_failed",
            EventKind::DeployCancelled => "deploy_cancelled",
            EventKind::Rollback => "rollback",
            EventKind::AutoDeploy => "auto_deploy",
            EventKind::Crash => "crash",
            EventKind::OomKill => "oom_kill",
            EventKind::Restart => "restart",
            EventKind::Stop => "stop",
            EventKind::Start => "start",
            EventKind::Unhealthy => "unhealthy",
            EventKind::Healthy => "healthy",
            EventKind::Created => "created",
            EventKind::Deleted => "deleted",
            EventKind::Settings => "settings",
            EventKind::Env => "env",
            EventKind::Domain => "domain",
            EventKind::AutoDeploySwitch => "auto_deploy_switch",
            EventKind::Access => "access",
            EventKind::AlertFired => "alert_fired",
            EventKind::AlertResolved => "alert_resolved",
            EventKind::SshLogin => "ssh_login",
            EventKind::CertIssued => "cert_issued",
            EventKind::CertFailed => "cert_failed",
            EventKind::DnsRecord => "dns_record",
            EventKind::Reboot => "reboot",
            EventKind::AgentRestart => "agent_restart",
            EventKind::ServerStart => "server_start",
            EventKind::FacadeUpdate => "facade_update",
            EventKind::Upkeep => "upkeep",
            EventKind::DiskError => "disk_error",
            EventKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .unwrap_or(EventKind::Other)
    }

    pub fn category(self) -> Category {
        use EventKind::*;
        match self {
            DeployStarted | Deployed | DeployFailed | DeployCancelled | Rollback | AutoDeploy => {
                Category::Deploys
            }
            Crash | OomKill | Restart | Stop | Start => Category::Restarts,
            Unhealthy | Healthy => Category::Health,
            Created | Deleted | Settings | Env | Domain | AutoDeploySwitch | Access | Other => {
                Category::Changes
            }
            AlertFired | AlertResolved => Category::Alerts,
            SshLogin => Category::Logins,
            CertIssued | CertFailed | DnsRecord => Category::Certificates,
            Reboot | AgentRestart | ServerStart | FacadeUpdate | Upkeep | DiskError => {
                Category::System
            }
        }
    }
}

/// What an entry carries beyond its summary.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventDetails {
    /// Lines to show as a log: a crash's last output (with Docker's
    /// timestamps), the kernel's lines about a disk, a health check's output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<String>,
    /// Names: variables changed (never their values), settings changed,
    /// packages to update.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    /// Why a container stopped, in a few words (`out of memory`, `exit code
    /// 1`): what the Restarts tile says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// Bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit: Option<u64>,
    /// The program the kernel killed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The alert's key (`app:blog:down`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert: Option<String>,
    /// Where an SSH login came from, and how it authenticated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// A disk (`sda`) or filesystem (`sda1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

impl EventDetails {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One Activity entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventView {
    pub id: i64,
    /// When it happened (its last time, for a repeated one).
    pub at: u64,
    /// How many times in a row (a crash loop is one entry), since `first_at`.
    #[serde(default = "one")]
    pub count: u32,
    #[serde(default)]
    pub first_at: u64,
    pub kind: EventKind,
    pub severity: Severity,
    /// Bad or unusual: what "problems only" keeps.
    pub problem: bool,
    pub scope: Scope,
    pub app: Option<String>,
    pub node: Option<String>,
    pub summary: String,
    /// The account behind it, or what acted (`auto-deploy`).
    pub actor: Option<String>,
    #[serde(default)]
    pub details: EventDetails,
    /// It has log lines, left out of lists (`GET /api/events/{id}` has them).
    #[serde(default)]
    pub has_logs: bool,
}

fn one() -> u32 {
    1
}

/// A page of entries, newest first.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventsPage {
    pub events: Vec<EventView>,
    /// Older entries match too: ask again with `before` the last id.
    pub more: bool,
}

/// What an agent saw happen on its node, sent to the server as it happens
/// (kept while the server is away).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeEvent {
    pub at: u64,
    pub kind: EventKind,
    pub severity: Severity,
    pub problem: bool,
    /// The app it is about, if any.
    #[serde(default)]
    pub app: Option<String>,
    /// Show it on the node's Activity too (with an app: a kernel OOM kill).
    #[serde(default)]
    pub on_node: bool,
    pub summary: String,
    #[serde(default)]
    pub details: EventDetails,
}

/// A node's pending system upkeep, read every few hours. facade reports it and
/// never installs anything: the system does (unattended-upgrades, a rebuild).
///
/// Each part is [`Checked`]: "nothing to do" and "this node cannot tell" read
/// differently. On the wire it also carries the fields servers before these
/// had (and reads them from agents before these).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "UpkeepWire", into = "UpkeepWire")]
pub struct Upkeep {
    pub at: u64,
    pub updates: Checked<Updates>,
    pub reboot: Checked<Reboot>,
}

/// Pending system updates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Updates {
    /// A package manager's count (apt).
    Packages {
        count: u32,
        /// Of which security updates.
        security: u32,
        /// Their names (at most a hundred).
        #[serde(default)]
        names: Vec<String>,
    },
    /// A system built from a channel (NixOS): how far behind it its build is.
    Channel(ChannelState),
}

/// Where a NixOS node's nixpkgs stands. The agent says what it runs; the
/// server, which knows the channel's head, adds how far behind it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelState {
    /// `26.05`.
    pub release: String,
    /// The nixpkgs commit the running system was built from.
    pub revision: String,
    /// The branch it follows: `nixos-26.05`.
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind_commits: Option<u32>,
    /// The release no longer gets security fixes.
    #[serde(default)]
    pub eol: bool,
}

/// Whether the node needs a reboot to run what is installed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reboot {
    pub required: bool,
    /// What asks for it: packages (`linux-image-…`), or `kernel 6.12.40 → 6.12.44`.
    #[serde(default)]
    pub reasons: Vec<String>,
}

impl Upkeep {
    /// `7 updates · reboot required`, or `None` when there is nothing to do (or
    /// nothing known: see [`Upkeep::unknown`]).
    pub fn badge(&self) -> Option<String> {
        let mut parts = vec![];
        match self.updates.known() {
            Some(Updates::Packages {
                count, security, ..
            }) => match (*count, *security) {
                (0, _) => {}
                (n, 0) => parts.push(format!("{n} update{}", plural(n))),
                (n, s) => parts.push(format!("{n} update{} ({s} security)", plural(n))),
            },
            Some(Updates::Channel(c)) => {
                if c.eol {
                    parts.push(format!("NixOS {} past end of life", c.release));
                } else if let Some(days) = c.behind_days.filter(|d| *d > 0) {
                    parts.push(format!(
                        "nixpkgs {days} day{} behind {}",
                        plural(days),
                        c.branch
                    ));
                }
            }
            None => {}
        }
        if self.reboot_required() {
            parts.push("reboot required".into());
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// What the node could not check: `updates: not available: …`.
    pub fn unknown(&self) -> Vec<String> {
        let mut out = vec![];
        if let Some(why) = self.updates.why_unknown() {
            out.push(format!("updates: {why}"));
        }
        if let Some(why) = self.reboot.why_unknown() {
            out.push(format!("reboot: {why}"));
        }
        out
    }

    pub fn reboot_required(&self) -> bool {
        self.reboot.known().is_some_and(|r| r.required)
    }

    /// What asks for a reboot.
    pub fn reboot_reasons(&self) -> &[String] {
        self.reboot.known().map_or(&[], |r| &r.reasons)
    }

    /// Security updates waiting (package managers only).
    pub fn security(&self) -> u32 {
        match self.updates.known() {
            Some(Updates::Packages { security, .. }) => *security,
            _ => 0,
        }
    }

    /// Worth an alert: a reboot waits, security updates do, or the system's
    /// release gets no fixes any more.
    pub fn needs_attention(&self) -> bool {
        self.reboot_required()
            || self.security() > 0
            || matches!(self.updates.known(), Some(Updates::Channel(c)) if c.eol)
    }
}

fn plural(n: u32) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[derive(Default, Serialize, Deserialize)]
struct UpkeepWire {
    at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<Checked<Updates>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reboot: Option<Checked<Reboot>>,
    // What agents and servers before `Checked` send and read.
    #[serde(default)]
    updates: Option<u32>,
    #[serde(default)]
    security: Option<u32>,
    #[serde(default)]
    packages: Vec<String>,
    #[serde(default)]
    reboot_required: bool,
    #[serde(default)]
    reboot_packages: Vec<String>,
}

impl From<UpkeepWire> for Upkeep {
    fn from(w: UpkeepWire) -> Self {
        let updates = w.pending.unwrap_or_else(|| match w.updates {
            Some(count) => Checked::Known(Updates::Packages {
                count,
                security: w.security.unwrap_or(0),
                names: w.packages,
            }),
            None => Checked::Unsupported("the node's agent does not say (update it)".into()),
        });
        let reboot = w.reboot.unwrap_or(Checked::Known(Reboot {
            required: w.reboot_required,
            reasons: w.reboot_packages,
        }));
        Upkeep {
            at: w.at,
            updates,
            reboot,
        }
    }
}

impl From<Upkeep> for UpkeepWire {
    fn from(u: Upkeep) -> Self {
        let mut w = UpkeepWire {
            at: u.at,
            ..Default::default()
        };
        if let Some(Updates::Packages {
            count,
            security,
            names,
        }) = u.updates.known()
        {
            w.updates = Some(*count);
            w.security = Some(*security);
            w.packages = names.clone();
        }
        if let Some(r) = u.reboot.known() {
            w.reboot_required = r.required;
            w.reboot_packages = r.reasons.clone();
        }
        w.pending = Some(u.updates);
        w.reboot = Some(u.reboot);
        w
    }
}

/// The kernel reported disk errors on the node lately (a day).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskTrouble {
    pub at: u64,
    pub summary: String,
    /// The entry saying so.
    pub event: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_and_unknown_ones_read_as_other() {
        for kind in EventKind::ALL {
            assert_eq!(EventKind::parse(kind.as_str()), kind);
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
        }
        let later: EventKind = serde_json::from_str("\"time_travel\"").unwrap();
        assert_eq!(later, EventKind::Other);
        assert_eq!(EventKind::parse("time_travel"), EventKind::Other);
    }

    #[test]
    fn upkeep_badges() {
        let mut u = Upkeep::default();
        assert_eq!(u.badge(), None);
        assert!(!u.needs_attention());
        assert_eq!(u.unknown().len(), 2);
        u.updates = Checked::Known(Updates::Packages {
            count: 7,
            security: 0,
            names: vec![],
        });
        assert_eq!(u.badge().as_deref(), Some("7 updates"));
        u.updates = Checked::Known(Updates::Packages {
            count: 7,
            security: 2,
            names: vec![],
        });
        u.reboot = Checked::Known(Reboot {
            required: true,
            reasons: vec![],
        });
        assert_eq!(
            u.badge().as_deref(),
            Some("7 updates (2 security) · reboot required")
        );
        assert!(u.needs_attention());
        assert!(u.unknown().is_empty());
        u.updates = Checked::Known(Updates::Channel(ChannelState {
            release: "26.05".into(),
            branch: "nixos-26.05".into(),
            behind_days: Some(12),
            ..Default::default()
        }));
        u.reboot = Checked::Known(Reboot::default());
        assert_eq!(
            u.badge().as_deref(),
            Some("nixpkgs 12 days behind nixos-26.05")
        );
        assert!(!u.needs_attention());
    }

    #[test]
    fn upkeep_reads_and_writes_the_old_fields() {
        // From an agent before `Checked`.
        let old: Upkeep = serde_json::from_str(
            r#"{"at":5,"updates":7,"security":2,"packages":["openssl"],
                "reboot_required":true,"reboot_packages":["linux-image-7.0.0-43-generic"]}"#,
        )
        .unwrap();
        assert_eq!(
            old.updates,
            Checked::Known(Updates::Packages {
                count: 7,
                security: 2,
                names: vec!["openssl".into()]
            })
        );
        assert!(old.reboot_required());
        assert_eq!(old.reboot_reasons(), ["linux-image-7.0.0-43-generic"]);
        // From an agent that could not run apt.
        let silent: Upkeep = serde_json::from_str(r#"{"at":5}"#).unwrap();
        assert!(silent.updates.why_unknown().is_some());
        assert_eq!(silent.reboot, Checked::Known(Reboot::default()));
        // Round trip, and what an older server reads of a new one.
        let json = serde_json::to_value(&old).unwrap();
        assert_eq!(json["updates"], 7);
        assert_eq!(json["reboot_required"], true);
        assert_eq!(serde_json::from_value::<Upkeep>(json).unwrap(), old);
    }
}
