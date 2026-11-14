use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

/// `/etc/facade/server.toml`. Every field has a default, so an empty file works for
/// a single-node setup.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub data_dir: PathBuf,
    /// Address other nodes use to reach this server (IP or hostname, no port).
    /// Required before adding remote nodes.
    pub public_host: Option<String>,
    /// Agents connect here.
    pub node_listen: SocketAddr,
    /// API with the server's self-signed certificate (CLI pins its fingerprint).
    pub api_listen: SocketAddr,
    /// Plain-HTTP API on loopback, which this machine's entry point forwards
    /// `api_domain` to.
    pub api_loopback: SocketAddr,
    /// Where this machine serves its apps.
    pub http_listen: SocketAddr,
    pub https_listen: SocketAddr,
    /// `embedded`: the entry point runs inside `facaded server`. `external`: in
    /// `facaded ingress` (its own service), so updating the server never touches
    /// traffic.
    pub ingress: IngressSetting,
    /// Control socket of `facaded ingress`.
    pub ingress_control: PathBuf,
    /// Name of the node the server itself runs apps on.
    pub local_node: String,
    /// Serve the API on this domain through the ingress too, with a real certificate.
    pub api_domain: Option<String>,
    pub acme: AcmeConfig,
    pub backups: BackupConfig,
    pub update: UpdateConfig,
    pub auto_deploy: AutoDeployConfig,
    pub checks: ChecksConfig,
    pub alerts: AlertsConfig,
    pub vulns: VulnsConfig,
    /// The file this was read from (handed to the binary checking an update).
    #[serde(skip)]
    pub path: PathBuf,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            data_dir: "/var/lib/facade".into(),
            public_host: None,
            node_listen: "0.0.0.0:7700".parse().unwrap(),
            api_listen: "0.0.0.0:7701".parse().unwrap(),
            api_loopback: "127.0.0.1:7702".parse().unwrap(),
            http_listen: "0.0.0.0:80".parse().unwrap(),
            https_listen: "0.0.0.0:443".parse().unwrap(),
            ingress: IngressSetting::Embedded,
            ingress_control: DEFAULT_INGRESS_CONTROL.into(),
            local_node: "main".into(),
            api_domain: None,
            acme: AcmeConfig::default(),
            backups: BackupConfig::default(),
            update: UpdateConfig::default(),
            auto_deploy: AutoDeployConfig::default(),
            checks: ChecksConfig::default(),
            vulns: VulnsConfig::default(),
            alerts: AlertsConfig::default(),
            path: "/etc/facade/server.toml".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AcmeConfig {
    pub enabled: bool,
    /// Contact address for Let's Encrypt (expiry notices).
    pub email: Option<String>,
    /// Use Let's Encrypt's staging environment (untrusted certs, generous limits).
    pub staging: bool,
    /// Another ACME directory URL than Let's Encrypt's.
    pub directory: Option<String>,
    /// CA certificate to trust for `directory` (test CAs such as Pebble).
    pub root_ca: Option<PathBuf>,
    /// How long to let DNS challenge records spread before validation.
    pub dns_propagation_secs: u64,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            email: None,
            staging: false,
            directory: None,
            root_ca: None,
            dns_propagation_secs: 60,
        }
    }
}

pub const DEFAULT_INGRESS_CONTROL: &str = "/run/facade/ingress.sock";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IngressSetting {
    Embedded,
    External,
}

/// Copies of every app volume, and of the server's own database and key, kept in
/// `<data_dir>/backups`. They sit on the server's disk: `offsite` copies them
/// elsewhere, to survive losing the machine.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackupConfig {
    pub enabled: bool,
    pub every_hours: u64,
    /// Backups kept per volume (and of the server itself).
    pub keep: usize,
    /// `[backups.offsite]`: a copy of the backups somewhere else, encrypted.
    pub offsite: Option<OffsiteConfig>,
    /// `[backups.replique]`: report each offsite copy to a Réplique server.
    pub replique: Option<RepliqueConfig>,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            every_hours: 24,
            keep: 7,
            offsite: None,
            replique: None,
        }
    }
}

/// Where the offsite copy goes (see `server::offsite`). One flat table rather
/// than one per provider, so that a setting of another provider is named in the
/// error instead of being an "unknown field" somewhere.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OffsiteConfig {
    pub provider: Option<OffsiteProvider>,
    /// sftp: the server, its port (22; a Hetzner Storage Box takes 23 too) and
    /// the login.
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    /// sftp: the private key to log in with (OpenSSH format). Generated there
    /// when missing, so that its public half can be installed on the target.
    pub key_file: Option<PathBuf>,
    /// sftp: the server's host key (`ssh-ed25519 AAAA…`) or its fingerprint
    /// (`SHA256:…`). Without it, the key seen at the first connection is kept and
    /// required from then on.
    pub host_key: Option<String>,
    /// sftp and local: the directory the copy lives in (relative paths are from
    /// the login's home on sftp). s3: a prefix inside the bucket.
    pub path: Option<String>,
    /// s3: the endpoint (`https://s3.eu-central-1.amazonaws.com`,
    /// `https://fsn1.your-objectstorage.com`, `http://127.0.0.1:9000`…), the
    /// bucket and its region.
    pub endpoint: Option<String>,
    pub bucket: Option<String>,
    pub region: Option<String>,
    /// s3: files holding the access key id and the secret key.
    pub access_key_file: Option<PathBuf>,
    pub secret_key_file: Option<PathBuf>,
    /// Backups kept offsite per volume (and of the server itself); `[backups]
    /// keep` when absent. Never fewer than that.
    pub keep: Option<usize>,
    /// How often a sample of the offsite chunks is read back and checked.
    pub verify_every_hours: u64,
    /// How many chunks each of those checks reads back.
    pub verify_sample: usize,
    /// A copy older than this is a problem (twice the backup interval when
    /// absent: one missed run goes unremarked, a failed one has its own alert).
    pub stale_after_hours: Option<u64>,
}

impl Default for OffsiteConfig {
    fn default() -> Self {
        Self {
            provider: None,
            host: None,
            port: None,
            user: None,
            key_file: None,
            host_key: None,
            path: None,
            endpoint: None,
            bucket: None,
            region: None,
            access_key_file: None,
            secret_key_file: None,
            keep: None,
            verify_every_hours: 24,
            verify_sample: 64,
            stale_after_hours: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OffsiteProvider {
    Sftp,
    S3,
    Local,
}

/// A Réplique server receiving a report of each offsite copy and check, as one
/// more device's backup job.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepliqueConfig {
    /// `https://replique.example.com`
    pub url: String,
    /// A file holding the device's report token (`rpl_…`), from Réplique's
    /// website.
    pub token_file: PathBuf,
    /// The job's name on Réplique.
    #[serde(default = "RepliqueConfig::default_job")]
    pub job: String,
}

impl RepliqueConfig {
    fn default_job() -> String {
        "offsite".into()
    }
}

/// Where updates of facade itself come from: a GitHub release that CI replaces
/// with each build of the main branch (`.github/workflows/build.yml`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    /// `owner/name` on GitHub. A fork publishing its own builds goes here.
    pub repository: String,
    /// The release tag holding the builds.
    pub release: String,
    /// Where the release's files are downloaded from instead of GitHub (a
    /// mirror): `<url>/manifest.json` and the tarballs next to it.
    pub url: Option<String>,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            repository: "Sigma-studios/facade".into(),
            release: "edge".into(),
            url: None,
        }
    }
}

/// Apps and stacks with auto-deploy on: how often their source is checked when
/// no webhook reports changes.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutoDeployConfig {
    /// Seconds between two checks of a source without a working webhook (ten
    /// times longer with one, as a safety net).
    pub poll_secs: u64,
    /// GitHub's REST API, where repository webhooks are created.
    pub github_api: String,
}

impl Default for AutoDeployConfig {
    fn default() -> Self {
        Self {
            poll_secs: 180,
            github_api: "https://api.github.com".into(),
        }
    }
}

/// The outside check: every app domain loaded from the server the way a
/// visitor would (see `server::reach`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChecksConfig {
    pub enabled: bool,
    /// Seconds between two rounds.
    pub every_secs: u64,
    /// How long a domain has to answer (headers), connecting included.
    pub timeout_secs: u64,
    /// Domains checked at once.
    pub concurrency: usize,
    /// Addresses to use instead of DNS for some names (tests, split horizons).
    pub resolve: std::collections::BTreeMap<String, std::net::IpAddr>,
    /// A CA certificate to trust besides the public ones (test CAs).
    pub root_ca: Option<PathBuf>,
}

/// `[vulns]`: the vulnerability check of what the nodes run.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VulnsConfig {
    pub enabled: bool,
    /// The least severe finding with no fix that raises an alert: `critical`,
    /// `high`, `medium` or `low`. Not `high` by default: a kernel always has
    /// dozens of high ones waiting for a fix (drivers a server never loads,
    /// mostly), which would bury the critical ones.
    pub alert_min: String,
    /// Where Ubuntu's OVAL files are (`com.ubuntu.<codename>.cve.oval.xml.bz2`),
    /// for a mirror or offline use.
    pub ubuntu_oval_url: String,
    /// OSV's archive of upstream kernel vulnerabilities, matched against the
    /// running kernel of NixOS nodes.
    pub kernel_osv_url: String,
}

impl Default for VulnsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            alert_min: "critical".into(),
            ubuntu_oval_url: "https://security-metadata.canonical.com/oval".into(),
            kernel_osv_url: "https://osv-vulnerabilities.storage.googleapis.com/Linux/all.zip"
                .into(),
        }
    }
}

impl Default for ChecksConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            every_secs: 60,
            timeout_secs: 10,
            concurrency: 8,
            resolve: Default::default(),
            root_ca: None,
        }
    }
}

/// Discord notifications. The webhook and who to tag live in the database
/// (`facade alerts config`); only when the weekly summary goes out is here.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AlertsConfig {
    /// The day of the week the summary of the week before is posted (UTC).
    pub weekly_day: Weekday,
    /// The hour it is posted at (UTC, 0-23).
    pub weekly_hour: u8,
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            weekly_day: Weekday::Monday,
            weekly_hour: 7,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Weekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl Weekday {
    pub const ALL: [Weekday; 7] = [
        Weekday::Monday,
        Weekday::Tuesday,
        Weekday::Wednesday,
        Weekday::Thursday,
        Weekday::Friday,
        Weekday::Saturday,
        Weekday::Sunday,
    ];

    /// Days after Monday.
    pub fn index(self) -> u64 {
        Self::ALL.iter().position(|d| *d == self).unwrap_or(0) as u64
    }

    pub fn name(self) -> &'static str {
        match self {
            Weekday::Monday => "Monday",
            Weekday::Tuesday => "Tuesday",
            Weekday::Wednesday => "Wednesday",
            Weekday::Thursday => "Thursday",
            Weekday::Friday => "Friday",
            Weekday::Saturday => "Saturday",
            Weekday::Sunday => "Sunday",
        }
    }
}

impl ServerConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            tracing::warn!("{} not found, using defaults", path.display());
            return Ok(Self {
                path: path.to_path_buf(),
                ..Self::default()
            });
        }
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Self =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        config.path = path.to_path_buf();
        facade_core::validate_name("local_node", &config.local_node)?;
        if config.alerts.weekly_hour > 23 {
            anyhow::bail!("[alerts] weekly_hour must be 0 to 23 (UTC)");
        }
        if let Some(offsite) = &config.backups.offsite {
            crate::server::offsite::Settings::from_config(offsite, &config.backups)
                .context("[backups.offsite]")?;
        }
        if let Some(replique) = &config.backups.replique {
            crate::server::offsite::replique::check_config(replique)
                .context("[backups.replique]")?;
        }
        Ok(config)
    }

    /// Where the server's own agent dials.
    pub fn loopback_node_addr(&self) -> SocketAddr {
        let mut addr = self.node_listen;
        if addr.ip().is_unspecified() {
            addr.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
        }
        addr
    }
}
