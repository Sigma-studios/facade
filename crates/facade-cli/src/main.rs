//! `facade`: command-line client for a facade server. Every command takes `--json`
//! for machine-readable output, so scripts and AI agents can drive it.

mod client;
mod output;

use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use facade_core::api::*;
use facade_core::proto::Listener;
use facade_core::stack::{ServiceSpec, StackSpec};
use facade_core::{AppSpec, Build, Limits, Link, PortMap, Source, TlsPort, Volume};
use futures_util::StreamExt;
use reqwest::Method;

use client::{Api, Saved};
use output::{bytes, print_json, table};

#[derive(Parser)]
#[command(name = "facade", version, about = "Manage apps on your facade servers")]
struct Cli {
    /// Print JSON instead of tables.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once
enum Command {
    /// Log in to a server, e.g. `facade login https://203.0.113.10:7701`.
    Login {
        server: String,
        #[arg(short, long, default_value = "admin")]
        username: String,
        /// Read the password from stdin instead of prompting.
        #[arg(long)]
        password_stdin: bool,
        /// Expected certificate fingerprint (printed in the server log).
        #[arg(long)]
        fingerprint: Option<String>,
        /// Trust the presented certificate without asking.
        #[arg(long, short)]
        yes: bool,
    },
    Logout,
    /// Change your password.
    Passwd,
    /// Nodes and apps at a glance.
    Status,
    /// Firing and recently resolved alerts; `config` and `test` for Discord notifications.
    Alerts {
        #[command(subcommand)]
        action: Option<AlertsCommand>,
    },
    #[command(subcommand)]
    Node(NodeCommand),
    #[command(subcommand)]
    App(AppCommand),
    #[command(subcommand)]
    Env(EnvCommand),
    #[command(subcommand)]
    Domain(DomainCommand),
    /// Every routed domain: where it must point, whether it does, and its certificate.
    Domains,
    /// Show or set the DNS provider facade manages records with.
    #[command(subcommand)]
    Dns(DnsCommand),
    /// Build (if needed) and (re)start an app from the latest commit of its branch,
    /// following the output.
    Deploy {
        app: String,
        /// Deploy this commit (full hash), branch or tag instead.
        #[arg(long)]
        commit: Option<String>,
        /// Build again without Docker's layer cache, even if this commit was built.
        #[arg(long)]
        rebuild: bool,
        /// Return immediately instead of following the build.
        #[arg(long)]
        detach: bool,
    },
    /// Deploy the commit of an earlier successful deployment (by default, the one
    /// before the current commit).
    Rollback {
        app: String,
        /// Deployment ID to go back to (see `facade deployments <app>`).
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        detach: bool,
    },
    /// Stop a queued or building deployment: a deployment ID, or an app name for
    /// its current one.
    Cancel {
        target: String,
    },
    /// What each node is building and what waits, and recent deployments.
    Builds,
    Restart {
        app: String,
    },
    /// Enable and run a stopped app.
    Start {
        app: String,
    },
    /// Stop an app, keeping its configuration.
    Stop {
        app: String,
    },
    /// Show container logs.
    Logs {
        app: String,
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 200)]
        tail: u32,
    },
    /// Recent deployments of an app, or one deployment's log.
    Deployments {
        app: String,
        /// Show this deployment's full log.
        #[arg(long)]
        id: Option<String>,
    },
    /// List what could be deployed from a git repository.
    Scan {
        repo: String,
        #[arg(long)]
        branch: Option<String>,
    },
    /// CPU / memory history of an app (last hour).
    Metrics {
        app: String,
    },
    /// Credentials for private repositories: deploy keys and access tokens.
    #[command(subcommand)]
    Git(GitCommand),
    /// Databases: an official image with a volume, a generated password and a
    /// connection URL for the apps it is linked to.
    #[command(subcommand)]
    Db(DbCommand),
    /// Volumes on the nodes: list, download, replace with an archive, delete leftovers.
    #[command(subcommand)]
    Volume(VolumeCommand),
    /// Volume backups (taken on a schedule; see [backups] in server.toml).
    #[command(subcommand)]
    Backup(BackupCommand),
    /// Logins for private container registries (image pulls, Dockerfile FROM).
    #[command(subcommand)]
    Registry(RegistryCommand),
    /// Accounts: admins manage everything, members only their apps (admins only).
    #[command(subcommand)]
    User(UserCommand),
    /// Who you are logged in as, and your resource cap.
    Whoami,
    /// How this server is reached: its web UI domain, ACME contact, direct API port.
    Server {
        #[command(subcommand)]
        action: Option<ServerCommand>,
    },
    /// Fetch an app's branch and list the commits a deploy would bring.
    Updates {
        app: String,
    },
    /// Stacks: services deployed together from one repository (a docker-compose
    /// file, or services defined here), on one node, on a network of their own.
    #[command(subcommand)]
    Stack(StackCommand),
    /// Print the whole configuration as TOML (sensitive values are left out).
    Export {
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Create or update nodes and apps from a TOML manifest (as produced by `export`).
    Apply {
        #[arg(short, long)]
        file: PathBuf,
    },
}

#[derive(Subcommand)]
enum AlertsCommand {
    /// Show or change where alerts are sent. With no flags, shows the settings.
    Config {
        /// Discord webhook URL (channel settings → Integrations → Webhooks). "" removes it.
        #[arg(long)]
        webhook: Option<String>,
        /// Who to tag on problems: @here, @everyone, user:<id>, role:<id> (repeatable;
        /// replaces the current list).
        #[arg(long = "mention")]
        mentions: Vec<String>,
        /// Tag nobody.
        #[arg(long, conflicts_with = "mentions")]
        no_mentions: bool,
    },
    /// Send a test notification.
    Test,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once
enum StackCommand {
    #[command(alias = "ls")]
    List,
    /// Services, their state, and recent deployments.
    Show { name: String },
    /// Create a stack. Its services come from a compose file of the repository
    /// (found on its own, or --file), read again at every deploy; with --no-file,
    /// you define them with `facade stack service add`.
    Create {
        name: String,
        #[arg(long)]
        git: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        /// Compose file in the repository.
        #[arg(long, conflicts_with = "no_file")]
        file: Option<String>,
        /// Define the services here rather than in a file.
        #[arg(long)]
        no_file: bool,
        /// Node for every service (default: the server's own).
        #[arg(long)]
        node: Option<String>,
        /// Create without asking.
        #[arg(long, short)]
        yes: bool,
    },
    /// Change the repository, branch, file or node (applied by the next deploy).
    Update {
        name: String,
        #[arg(long)]
        git: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long, conflicts_with = "no_file")]
        file: Option<String>,
        #[arg(long)]
        no_file: bool,
        #[arg(long)]
        node: Option<String>,
    },
    /// What a deploy would do now: the services, and which ones changed.
    Plan {
        name: String,
        #[arg(long)]
        commit: Option<String>,
    },
    /// Delete the stack and its services.
    #[command(alias = "rm")]
    Remove {
        name: String,
        /// Delete its volumes (its data) too.
        #[arg(long)]
        volumes: bool,
        #[arg(long, short)]
        yes: bool,
    },
    /// Services of a stack: define them (stacks without a file), or change what
    /// the file says about them (domains, limits, release command, …).
    #[command(subcommand)]
    Service(ServiceCommand),
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once
enum ServiceCommand {
    /// Add a service (or settings for one of the file's).
    Add {
        stack: String,
        service: String,
        #[command(flatten)]
        args: ServiceArgs,
    },
    /// Change a service's settings; only the flags given change.
    Update {
        stack: String,
        service: String,
        #[command(flatten)]
        args: ServiceArgs,
    },
    /// Remove a service (or the settings given here for one of the file's).
    #[command(alias = "rm")]
    Remove { stack: String, service: String },
}

#[derive(Args, Default)]
struct ServiceArgs {
    /// Run a registry image.
    #[arg(long, conflicts_with_all = ["dockerfile", "rust_bin", "database"])]
    image: Option<String>,
    /// Build with this Dockerfile of the stack's repository.
    #[arg(long)]
    dockerfile: Option<String>,
    /// Build context in the repository (default: the Dockerfile's directory).
    #[arg(long)]
    context: Option<String>,
    /// Build this cargo binary of the repository.
    #[arg(long, conflicts_with_all = ["dockerfile", "database"])]
    rust_bin: Option<String>,
    #[arg(long, requires = "rust_bin")]
    rust_package: Option<String>,
    #[arg(long, requires = "rust_bin", value_delimiter = ',')]
    features: Vec<String>,
    #[arg(long, requires = "rust_bin")]
    no_default_features: bool,
    /// A database: postgres, mysql, mariadb or redis (image, volume, password and
    /// DATABASE_URL / REDIS_URL for the other services to use).
    #[arg(long, conflicts_with = "dockerfile")]
    database: Option<String>,
    /// Database image tag.
    #[arg(long, requires = "database")]
    version: Option<String>,
    /// Replace the image's command ("" restores it).
    #[arg(long, allow_hyphen_values = true)]
    command: Option<String>,
    /// Run before each deploy replaces the service ("" removes it).
    #[arg(long, allow_hyphen_values = true)]
    release: Option<String>,
    /// Port HTTP for its domains goes to (0 removes it).
    #[arg(long)]
    port: Option<u16>,
    /// Route this domain to the service (repeatable; replaces the list).
    #[arg(long = "domain")]
    domains: Vec<String>,
    #[arg(long, conflicts_with = "domains")]
    no_domains: bool,
    /// Publish raw ports on the node's public address (repeatable).
    #[arg(long = "publish", short = 'p')]
    ports: Vec<PortMap>,
    /// Stack volume to mount: name:/path, or name:/path:20G with a size
    /// (repeatable; shared by the services naming it).
    #[arg(long = "volume", short = 'v')]
    volumes: Vec<Volume>,
    /// Services to start (and replace) before this one (repeatable).
    #[arg(long = "depends-on")]
    depends_on: Vec<String>,
    /// Memory limit in MB.
    #[arg(long)]
    memory: Option<u64>,
    #[arg(long)]
    cpus: Option<f64>,
    /// Most processes and threads at once (0 restores the default, 4096).
    #[arg(long)]
    pids: Option<u32>,
    /// Seconds the service gets to shut down when stopped or replaced (0 restores
    /// the default, 10).
    #[arg(long)]
    stop_timeout: Option<u32>,
    /// Signal asking the service to stop, e.g. SIGINT ("" restores the image's).
    #[arg(long)]
    stop_signal: Option<String>,
    /// How to tell the service is ready, run with `sh -c` in its container; the
    /// services depending on it start once it passes ("" uses the default).
    #[arg(long, allow_hyphen_values = true)]
    health: Option<String>,
}

/// `--health "cmd"`: a shell check, or none for "".
fn health_check(command: &str) -> Option<facade_core::HealthCheck> {
    let command = command.trim();
    (!command.is_empty()).then(|| facade_core::HealthCheck::shell(command))
}

impl ServiceArgs {
    /// Applies the flags given to `spec`.
    fn apply(&self, spec: &mut ServiceSpec) -> anyhow::Result<()> {
        if let Some(image) = &self.image {
            spec.image = Some(image.clone());
            spec.build = None;
        }
        if let Some(dockerfile) = &self.dockerfile {
            let context = self.context.clone().unwrap_or_else(|| {
                std::path::Path::new(dockerfile)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| ".".into())
            });
            spec.build = Some(Build::Dockerfile {
                dockerfile: dockerfile.clone(),
                context,
            });
            spec.image = None;
        } else if let Some(context) = &self.context {
            match &mut spec.build {
                Some(Build::Dockerfile { context: c, .. }) => *c = context.clone(),
                _ => bail!("--context goes with --dockerfile"),
            }
        }
        if let Some(bin) = &self.rust_bin {
            spec.build = Some(Build::Rust {
                package: self.rust_package.clone(),
                bin: bin.clone(),
                features: self.features.clone(),
                no_default_features: self.no_default_features,
            });
            spec.image = None;
        }
        if let Some(command) = &self.command {
            let words = facade_core::split_command(command)?;
            spec.command = (!words.is_empty()).then_some(words);
        }
        if let Some(release) = &self.release {
            spec.release_command = Some(release.trim().to_string()).filter(|r| !r.is_empty());
        }
        if let Some(port) = self.port {
            spec.http_port = (port != 0).then_some(port);
        }
        if !self.domains.is_empty() || self.no_domains {
            spec.domains = Some(self.domains.iter().map(|d| d.to_lowercase()).collect());
        }
        if !self.ports.is_empty() {
            spec.ports = Some(self.ports.clone());
        }
        if !self.volumes.is_empty() {
            spec.volumes = Some(self.volumes.clone());
        }
        if !self.depends_on.is_empty() {
            spec.depends_on = Some(self.depends_on.clone());
        }
        if let Some(mb) = self.memory {
            spec.limits.memory_mb = (mb != 0).then_some(mb);
        }
        if let Some(cpus) = self.cpus {
            spec.limits.cpus = (cpus != 0.0).then_some(cpus);
        }
        if let Some(pids) = self.pids {
            spec.limits.pids = (pids != 0).then_some(pids);
        }
        if let Some(secs) = self.stop_timeout {
            spec.stop_timeout_secs = (secs != 0).then_some(secs);
        }
        if let Some(signal) = &self.stop_signal {
            spec.stop_signal = stop_signal(signal);
        }
        if let Some(health) = &self.health {
            spec.healthcheck = health_check(health);
        }
        Ok(())
    }
}

/// `--stop-signal sigint`: `SIGINT`, or the image's own for "".
fn stop_signal(signal: &str) -> Option<String> {
    let signal = signal.trim().to_ascii_uppercase();
    match signal.as_str() {
        "" => None,
        s if s.starts_with("SIG") || s.parse::<u8>().is_ok() => Some(signal),
        _ => Some(format!("SIG{signal}")),
    }
}

#[derive(Subcommand)]
enum GitCommand {
    /// List credentials and the apps they cover.
    #[command(alias = "ls")]
    List,
    /// Generate a deploy key for one repository and print its public half, to add
    /// to the repository (GitHub: Settings → Deploy keys).
    Key {
        name: String,
        /// The repository, e.g. https://github.com/me/app.
        #[arg(long)]
        repo: String,
        /// Use this private key (OpenSSH format, file path) instead of generating one.
        #[arg(long)]
        private_key: Option<PathBuf>,
    },
    /// Store an access token (GitHub fine-grained token with read access to
    /// contents, GitLab project/group token, …) for every repository under a scope.
    /// The token is read from stdin or prompted for.
    Token {
        name: String,
        /// Repositories it covers: github.com/my-org, github.com/me/app, …
        #[arg(long)]
        scope: String,
        /// HTTPS user name sent with the token (default x-access-token).
        #[arg(long)]
        username: Option<String>,
    },
    #[command(alias = "rm")]
    Remove { name: String },
}

#[derive(Subcommand)]
enum DbCommand {
    /// Create (and start) a database app.
    Create {
        name: String,
        /// postgres, mysql, mariadb or redis.
        #[arg(long, default_value = "postgres")]
        kind: String,
        /// Image tag (default: a recent major version).
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        node: Option<String>,
        /// Memory limit in MB.
        #[arg(long)]
        memory: Option<u64>,
        /// App to connect to it (repeatable): linked, and given the URL in
        /// DATABASE_URL (REDIS_URL for Redis).
        #[arg(long = "link")]
        links: Vec<String>,
        /// Variable the linked apps get the URL in.
        #[arg(long)]
        env_var: Option<String>,
    },
}

#[derive(Subcommand)]
enum BackupCommand {
    /// Stored backups, newest first.
    #[command(alias = "ls")]
    List { app: Option<String> },
    /// Back up now: one app's volumes, or every app's.
    Run { app: Option<String> },
    /// Download a backup.
    Get {
        app: String,
        volume: String,
        id: u64,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Replace a volume's contents with a backup.
    Restore {
        app: String,
        volume: String,
        id: u64,
        #[arg(long, short)]
        yes: bool,
    },
    /// Download the server's database and master key (everything facade knows,
    /// secrets included: keep it safe).
    Server {
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum UserCommand {
    #[command(alias = "ls")]
    List,
    /// Create an account; prints a one-time password to hand over (it must be
    /// changed at first login).
    Add {
        name: String,
        /// An admin instead of a member.
        #[arg(long)]
        admin: bool,
        /// Memory shared by all the member's apps, in MB.
        #[arg(long)]
        memory: Option<u64>,
        /// CPUs shared by all the member's apps.
        #[arg(long)]
        cpus: Option<f64>,
    },
    /// Change a role or caps; only the flags given are changed (0 removes a cap).
    Set {
        name: String,
        #[arg(long, value_parser = ["admin", "member"])]
        role: Option<String>,
        #[arg(long)]
        memory: Option<u64>,
        #[arg(long)]
        cpus: Option<f64>,
    },
    /// Replace a password with a one-time one (logs the user out).
    ResetPassword { name: String },
    /// Delete an account; its apps go back to the admins.
    #[command(alias = "rm")]
    Remove {
        name: String,
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ServerCommand {
    /// Change settings; only the flags given are changed.
    Set {
        /// Serve the web UI and API at this domain with a real certificate (point
        /// its DNS at this server). "" removes it.
        #[arg(long)]
        api_domain: Option<String>,
        /// Contact address for Let's Encrypt (expiry notices). "" removes it.
        #[arg(long)]
        acme_email: Option<String>,
        /// Make the direct API port (7701) accept only connections from the
        /// server itself (SSH tunnels), once the API domain has its certificate.
        #[arg(long, conflicts_with = "direct_api_open")]
        direct_api_local_only: bool,
        /// Accept connections to the direct API port from anywhere again.
        #[arg(long)]
        direct_api_open: bool,
    },
    /// Update facade to the latest build of its main branch: the server
    /// downloads it, says which apps it would restart, installs it after a yes
    /// and rolls back if it does not come up. Other nodes follow by themselves.
    Update {
        /// Only show what is published and which commits it brings.
        #[arg(long)]
        check: bool,
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum RegistryCommand {
    /// Saved logins (secrets are never shown).
    #[command(alias = "ls")]
    List,
    /// Log in to a container registry, e.g. `facade registry login ghcr.io --user me`
    /// with a token that can read packages. Used for image pulls and Dockerfile FROM;
    /// an app uses its owner's logins.
    Login {
        host: String,
        #[arg(long, short)]
        user: String,
        /// Read the token from stdin instead of prompting.
        #[arg(long)]
        token_stdin: bool,
    },
    #[command(alias = "rm")]
    Remove { host: String },
}

#[derive(Subcommand)]
enum VolumeCommand {
    /// Volumes on every node, their size and the app they belong to.
    #[command(alias = "ls")]
    List,
    /// Download a volume as a .tar.gz (the app is paused while it is copied).
    Export {
        app: String,
        volume: String,
        /// Defaults to <app>-<volume>-<time>.tar.gz.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Replace a volume's contents with a .tar or .tar.gz (the app is stopped while
    /// its files are replaced). Use it to bring existing data into facade.
    Import {
        app: String,
        volume: String,
        file: PathBuf,
        #[arg(long, short)]
        yes: bool,
    },
    /// Delete a volume no app declares any more (its data is lost).
    #[command(alias = "rm")]
    Remove {
        node: String,
        /// Docker volume name, as listed (facade-<app>-<volume>).
        name: String,
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum NodeCommand {
    #[command(alias = "ls")]
    List,
    /// Register a node and print the command that starts its agent.
    Add { name: String },
    /// Issue a new join token (invalidates the old one).
    Token { name: String },
    #[command(alias = "rm")]
    Remove { name: String },
    /// CPU / memory / disk history (last hour).
    Metrics { name: String },
    /// What takes the node's disk: Docker, facade, the rest, and each app.
    Disk { name: String },
    /// Set the public IP domains of this node's apps point to (`auto` to detect it).
    Ip { name: String, ip: String },
    /// Every listening port on the node, and which program or app holds it.
    Ports { name: String },
    /// Free disk space now: old build cache and unused images. Also runs on its own
    /// every 6 hours, and when the disk is 85 % full.
    Prune {
        name: String,
        /// Also drop the whole build cache and the images kept for rollbacks.
        #[arg(long)]
        aggressive: bool,
    },
}

#[derive(Subcommand)]
enum AppCommand {
    #[command(alias = "ls")]
    List,
    Show {
        name: String,
    },
    /// Create an app (it is deployed by `facade deploy`).
    Create {
        name: String,
        #[command(flatten)]
        spec: SpecArgs,
    },
    /// Change an app's settings; only the flags given are changed.
    Update {
        name: String,
        #[command(flatten)]
        spec: SpecArgs,
        /// With --node: copy the app's volumes to the new node (and delete them
        /// from the old one once copied).
        #[arg(long)]
        move_data: bool,
    },
    /// Give an app to a member (their cap, their logins), or `--admins` back.
    Owner {
        app: String,
        user: Option<String>,
        #[arg(long, conflicts_with = "user")]
        admins: bool,
    },
    /// Let a member manage an app (admins only).
    Grant {
        app: String,
        user: String,
    },
    /// Take that back.
    Revoke {
        app: String,
        user: String,
    },
    #[command(alias = "rm")]
    Remove {
        name: String,
        /// Delete its volumes (its data) too; otherwise they stay on the node.
        #[arg(long)]
        volumes: bool,
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Args, Default)]
struct SpecArgs {
    /// Node to run on (default: the server's own node).
    #[arg(long)]
    node: Option<String>,
    /// Run a registry image.
    #[arg(long, conflicts_with_all = ["git", "executable"])]
    image: Option<String>,
    /// Upload and run a Linux executable (built for the node's CPU).
    #[arg(long, conflicts_with = "git")]
    executable: Option<PathBuf>,
    /// Argument passed to the executable (repeatable). Other apps take --command.
    #[arg(long = "arg", allow_hyphen_values = true)]
    args: Vec<String>,
    /// Build from a git repository. Without --dockerfile, --rust-bin or --static, the
    /// repository is scanned and the best match is used (see `facade scan`).
    #[arg(long)]
    git: Option<String>,
    #[arg(long)]
    branch: Option<String>,
    /// Dockerfile path in the repository (default build kind). With --static, it
    /// builds the site: the files of its last stage are served.
    #[arg(long)]
    dockerfile: Option<String>,
    /// Docker build context in the repository.
    #[arg(long)]
    context: Option<String>,
    /// Build this cargo binary with a generated Dockerfile.
    #[arg(long, conflicts_with_all = ["dockerfile", "static_dir"])]
    rust_bin: Option<String>,
    #[arg(long, requires = "rust_bin")]
    rust_package: Option<String>,
    /// Cargo features, comma separated.
    #[arg(long, requires = "rust_bin", value_delimiter = ',')]
    features: Vec<String>,
    #[arg(long, requires = "rust_bin")]
    no_default_features: bool,
    /// Serve this directory as a static site: of the repository, or with
    /// --dockerfile, of what the build outputs (`.` for all of it).
    #[arg(long = "static")]
    static_dir: Option<String>,
    /// Build the Rust web app in this directory with trunk (its index.html has
    /// `<link data-trunk rel="rust">`) and serve the result as a static site.
    #[arg(long, conflicts_with_all = ["dockerfile", "static_dir", "rust_bin"])]
    trunk: Option<String>,
    /// Run a Python program: a script (server.py), `-m module` or a command the
    /// project installs. Dependencies come from requirements.txt / pyproject.toml.
    #[arg(long, allow_hyphen_values = true, conflicts_with_all = ["dockerfile", "static_dir", "rust_bin"])]
    python: Option<String>,
    /// Python version for --python (default: the current stable one).
    #[arg(long, requires = "python")]
    python_version: Option<String>,
    /// Port the app listens on for HTTP.
    #[arg(long)]
    port: Option<u16>,
    /// Route this domain to the app (repeatable; the first is the main one).
    #[arg(long = "domain")]
    domains: Vec<String>,
    /// Redirect this domain to the main domain, e.g. www.example.com (repeatable).
    #[arg(long = "redirect")]
    redirects: Vec<String>,
    #[arg(long, conflicts_with = "redirects")]
    no_redirects: bool,
    /// Publish raw ports: `3478:3478/udp`, `49160-49260:49160-49260/udp` (repeatable).
    #[arg(long = "publish", short = 'p')]
    ports: Vec<PortMap>,
    /// Take TLS off a public TCP port with the app's certificate and pass the
    /// connection on to a port of the app: `993:1143` (repeatable; replaces the list).
    #[arg(long = "tls-port")]
    tls_ports: Vec<TlsPort>,
    #[arg(long, conflicts_with = "tls_ports")]
    no_tls_ports: bool,
    /// Share the node's network (e.g. for TURN relays).
    #[arg(long)]
    host_network: Option<bool>,
    /// Memory limit in MB (0 removes it).
    #[arg(long)]
    memory: Option<u64>,
    /// CPU limit in cores (0 removes it).
    #[arg(long)]
    cpus: Option<f64>,
    /// Most processes and threads at once (0 restores the default, 4096). Each
    /// thread counts: a JVM or a process supervisor needs more.
    #[arg(long)]
    pids: Option<u32>,
    /// Seconds the app gets to shut down cleanly when stopped, restarted or
    /// replaced by a deploy, before it is killed (0 restores the default, 10).
    #[arg(long)]
    stop_timeout: Option<u32>,
    /// Signal asking the app to stop, e.g. SIGINT ("" restores the image's).
    #[arg(long)]
    stop_signal: Option<String>,
    /// Let the app divide its limits between its own processes: /sys/fs/cgroup
    /// becomes writable in its container (cgroup v2 nodes).
    #[arg(long)]
    delegate_cgroups: Option<bool>,
    /// Run in the container before its volumes are backed up, so what is on disk
    /// is whole, e.g. "rcon-cli save-off && rcon-cli save-all flush" ("" removes it).
    #[arg(long, allow_hyphen_values = true)]
    backup_before: Option<String>,
    /// Run in the container after its volumes are backed up, e.g. "rcon-cli
    /// save-on" ("" removes it).
    #[arg(long, allow_hyphen_values = true)]
    backup_after: Option<String>,
    /// Replace the image's command, e.g. "./worker --queue emails" ("" restores it).
    #[arg(long, allow_hyphen_values = true)]
    command: Option<String>,
    /// Run before each deploy replaces the app, in a container of the new build
    /// (with `sh -c`), e.g. "./migrate up". A failure stops the deploy ("" removes it).
    #[arg(long, allow_hyphen_values = true)]
    release: Option<String>,
    /// Keep data in a volume: name:/path, or name:/path:20G for one that holds at
    /// most that much, on a filesystem of its own (repeatable; replaces the list).
    #[arg(long = "volume", short = 'v')]
    volumes: Vec<Volume>,
    #[arg(long, conflicts_with = "volumes")]
    no_volumes: bool,
    /// Reach another app of the node by name: app or app:alias (repeatable;
    /// replaces the list).
    #[arg(long = "link")]
    links: Vec<Link>,
    #[arg(long, conflicts_with = "links")]
    no_links: bool,
    /// How to tell the app is ready, run with `sh -c` in the container (exit 0 =
    /// ready), e.g. "curl -fs localhost:8080/health". Deploys wait for it ("" removes it).
    #[arg(long, allow_hyphen_values = true)]
    health: Option<String>,
}

impl SpecArgs {
    /// The source these flags describe, given the current one and an executable
    /// uploaded for `--executable`.
    fn source(
        &self,
        current: Option<&Source>,
        uploaded: Option<&ArtifactView>,
    ) -> anyhow::Result<Option<Source>> {
        let executable = uploaded.is_some()
            || (self.image.is_none()
                && self.git.is_none()
                && matches!(current, Some(Source::Executable { .. })));
        if !self.args.is_empty() && !executable {
            bail!(
                "--arg is for executables; give this app its whole command line instead, \
                 e.g. --command \"python server.py --port 8000\""
            );
        }
        if let Some(image) = &self.image {
            return Ok(Some(Source::Image {
                image: image.clone(),
            }));
        }
        if let Some(artifact) = uploaded {
            return Ok(Some(Source::Executable {
                sha256: artifact.sha256.clone(),
                filename: artifact.filename.clone(),
                arch: artifact.arch.clone(),
                args: self.args.clone(),
            }));
        }
        if let Some(Source::Executable {
            sha256,
            filename,
            arch,
            ..
        }) = current
            && !self.args.is_empty()
        {
            return Ok(Some(Source::Executable {
                sha256: sha256.clone(),
                filename: filename.clone(),
                arch: arch.clone(),
                args: self.args.clone(),
            }));
        }
        let touches_git = self.git.is_some()
            || self.branch.is_some()
            || self.dockerfile.is_some()
            || self.context.is_some()
            || self.rust_bin.is_some()
            || self.static_dir.is_some()
            || self.trunk.is_some()
            || self.python.is_some();
        if !touches_git {
            return Ok(None);
        }
        let (repo, branch, build) = match current {
            Some(Source::Git {
                repo,
                branch,
                build,
            }) => (Some(repo.clone()), branch.clone(), build.clone()),
            _ => (None, None, Build::default()),
        };
        let repo = self
            .git
            .clone()
            .or(repo)
            .context("--git <url> is required")?;
        let build = if let Some(bin) = &self.rust_bin {
            Build::Rust {
                package: self.rust_package.clone(),
                bin: bin.clone(),
                features: self.features.clone(),
                no_default_features: self.no_default_features,
            }
        } else if let Some(dir) = &self.trunk {
            Build::Trunk { dir: dir.clone() }
        } else if let Some(dir) = &self.static_dir {
            Build::Static {
                dir: dir.clone(),
                dockerfile: self.dockerfile.clone(),
                context: self.context.clone(),
            }
        } else if let Some(entry) = &self.python {
            Build::Python {
                version: self.python_version.clone(),
                entry: entry.clone(),
            }
        } else if self.dockerfile.is_some() || self.context.is_some() {
            let (df, ctx) = match build {
                Build::Dockerfile {
                    dockerfile,
                    context,
                } => (dockerfile, context),
                _ => ("Dockerfile".into(), ".".into()),
            };
            Build::Dockerfile {
                dockerfile: self.dockerfile.clone().unwrap_or(df),
                context: self.context.clone().unwrap_or(ctx),
            }
        } else {
            build
        };
        Ok(Some(Source::Git {
            repo,
            branch: self.branch.clone().or(branch),
            build,
        }))
    }

    /// Whether the build was left for the repository scan to choose.
    fn build_unspecified(&self) -> bool {
        self.dockerfile.is_none()
            && self.context.is_none()
            && self.rust_bin.is_none()
            && self.static_dir.is_none()
            && self.trunk.is_none()
            && self.python.is_none()
    }

    fn apply(&self, spec: &mut AppSpec, uploaded: Option<&ArtifactView>) -> anyhow::Result<()> {
        if let Some(node) = &self.node {
            spec.node = node.clone();
        }
        if let Some(source) = self.source(Some(&spec.source), uploaded)? {
            spec.source = source;
        }
        if let Some(port) = self.port {
            spec.http_port = (port != 0).then_some(port);
        }
        if !self.domains.is_empty() {
            spec.domains = self.domains.iter().map(|d| d.to_lowercase()).collect();
        }
        if !self.redirects.is_empty() {
            spec.redirects = self.redirects.iter().map(|d| d.to_lowercase()).collect();
        }
        if self.no_redirects {
            spec.redirects.clear();
        }
        if !self.ports.is_empty() {
            spec.ports = self.ports.clone();
        }
        if !self.tls_ports.is_empty() || self.no_tls_ports {
            spec.tls_ports = self.tls_ports.clone();
        }
        if let Some(host) = self.host_network {
            spec.host_network = host;
        }
        if let Some(mb) = self.memory {
            spec.limits.memory_mb = (mb != 0).then_some(mb);
        }
        if let Some(cpus) = self.cpus {
            spec.limits.cpus = (cpus != 0.0).then_some(cpus);
        }
        if let Some(pids) = self.pids {
            spec.limits.pids = (pids != 0).then_some(pids);
        }
        if let Some(secs) = self.stop_timeout {
            spec.stop_timeout_secs = (secs != 0).then_some(secs);
        }
        if let Some(signal) = &self.stop_signal {
            spec.stop_signal = stop_signal(signal);
        }
        if let Some(delegate) = self.delegate_cgroups {
            spec.delegate_cgroups = delegate;
        }
        let hook = |c: &String| Some(c.trim().to_string()).filter(|c| !c.is_empty());
        if self.backup_before.is_some() || self.backup_after.is_some() {
            let backup = spec.backup.get_or_insert_with(Default::default);
            if let Some(before) = &self.backup_before {
                backup.before = hook(before);
            }
            if let Some(after) = &self.backup_after {
                backup.after = hook(after);
            }
            if backup.before.is_none() && backup.after.is_none() {
                spec.backup = None;
            }
        }
        if let Some(command) = &self.command {
            spec.command = facade_core::split_command(command)?;
        }
        if let Some(release) = &self.release {
            spec.release_command = Some(release.trim().to_string()).filter(|r| !r.is_empty());
        }
        if !self.volumes.is_empty() || self.no_volumes {
            spec.volumes = self.volumes.clone();
        }
        if !self.links.is_empty() || self.no_links {
            spec.links = self.links.clone();
        }
        if let Some(health) = &self.health {
            spec.healthcheck = health_check(health);
        }
        Ok(())
    }
}

#[derive(Subcommand)]
enum EnvCommand {
    /// List variables; sensitive values are hidden unless --reveal.
    #[command(alias = "ls")]
    List {
        app: String,
        #[arg(long)]
        reveal: bool,
    },
    /// Set variables (`KEY=VALUE`, or `KEY` to read the value from stdin). Values are
    /// sensitive unless --public. Restarts the app once, or rebuilds it when its
    /// builds get a changed variable.
    Set {
        app: String,
        #[arg(required = true)]
        vars: Vec<String>,
        /// Mark as non-sensitive: shown in listings and included in exports.
        #[arg(long)]
        public: bool,
        /// Hand them to the app's builds too, as BuildKit secrets named like the
        /// variable (`RUN --mount=type=secret,id=KEY,env=KEY` in a Dockerfile; Rust
        /// builds facade generates see them in `env!`). Kept until --no-build.
        #[arg(long, conflicts_with = "no_build")]
        build: bool,
        /// Stop handing them to builds.
        #[arg(long)]
        no_build: bool,
    },
    Unset {
        app: String,
        #[arg(required = true)]
        keys: Vec<String>,
    },
}

#[derive(Subcommand)]
enum DnsCommand {
    /// Current provider and the domains it manages.
    Show,
    /// Manage DNS through Porkbun (keys from porkbun.com/account/api; turn on API
    /// access in each domain's settings).
    Porkbun {
        #[arg(long, env = "PORKBUN_API_KEY")]
        api_key: String,
        #[arg(long, env = "PORKBUN_SECRET_KEY")]
        secret_key: String,
    },
    /// Stop managing DNS; records are then created by hand.
    Off,
}

#[derive(Subcommand)]
enum DomainCommand {
    Add {
        app: String,
        domain: String,
    },
    #[command(alias = "rm")]
    Remove {
        app: String,
        domain: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let json = cli.json;
    match cli.command {
        Command::Login {
            server,
            username,
            password_stdin,
            fingerprint,
            yes,
        } => login(server, username, password_stdin, fingerprint, yes).await,
        command => {
            let api = Api::new(Saved::load()?)?;
            dispatch(&api, command, json).await
        }
    }
}

async fn login(
    server: String,
    username: String,
    password_stdin: bool,
    fingerprint: Option<String>,
    yes: bool,
) -> anyhow::Result<()> {
    let server = server.trim_end_matches('/').to_string();
    let server = if server.contains("://") {
        server
    } else {
        format!("https://{server}")
    };

    // A CA-signed certificate (api_domain) needs no pinning; otherwise trust on first use.
    let ca_ok = client::http_client(None)?
        .get(format!("{server}/api/health"))
        .send()
        .await
        .is_ok();
    let fingerprint = if ca_ok {
        None
    } else {
        let seen = client::probe_fingerprint(&server).await?;
        match fingerprint {
            Some(expected) if expected.to_lowercase().replace(':', "") == seen => {}
            Some(expected) => {
                bail!("fingerprint mismatch: expected {expected}, server presented {seen}")
            }
            None if yes => {}
            None => {
                eprintln!(
                    "The server presented a self-signed certificate with fingerprint\n  {seen}"
                );
                eprintln!(
                    "It is printed in the server log at startup (\"certificate fingerprint\")."
                );
                if !confirm("Trust it?")? {
                    bail!("aborted");
                }
            }
        }
        Some(seen)
    };

    let password = if password_stdin {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s.trim_end_matches(['\n', '\r']).to_string()
    } else {
        rpassword::prompt_password(format!("password for {username}: "))?
    };
    let resp = client::http_client(fingerprint.as_deref())?
        .post(format!("{server}/api/login"))
        .json(&LoginRequest { username, password })
        .send()
        .await?;
    let LoginResponse { token } = client::check(resp).await?.json().await?;
    let saved = Saved {
        server,
        fingerprint,
        token,
    };
    saved.save()?;
    eprintln!("logged in (saved to {})", client::config_path()?.display());
    if let Ok(me) = Api::new(saved)?.get::<WhoAmI>("/api/whoami").await
        && me.must_change_password
    {
        eprintln!("this is the password generated at install: change it with `facade passwd`");
    }
    Ok(())
}

fn confirm(question: &str) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("{question} (no terminal to ask on; pass --yes)");
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// `facade server update`.
async fn update_server(api: &Api, check: bool, yes: bool, json: bool) -> anyhow::Result<()> {
    let short = |h: &str| h.get(..7).unwrap_or(h).to_string();
    let status: ServerUpdate = api
        .json(api.request(Method::POST, "/api/server-update/check"))
        .await?;
    if json && (check || !status.available) {
        return print_json(&status);
    }
    if let Some(e) = &status.check_error {
        bail!("checking {} for a new build: {e}", status.repository);
    }
    let Some(latest) = status.latest.clone() else {
        bail!("{} has no published build", status.repository);
    };
    let running = match &status.current.commit {
        Some(c) => short(c),
        None => "a build without a commit".into(),
    };
    println!("running:    {running} (facade {})", status.current.version);
    println!(
        "published:  {} {}  ({})",
        short(&latest.commit),
        latest.subject,
        output::ago(latest.time)
    );
    if let Some(run) = &status.run {
        let state = match run.state {
            UpdateState::Running => "in progress",
            UpdateState::Done => "done",
            UpdateState::RolledBack => "rolled back",
            UpdateState::Failed => "failed",
        };
        println!(
            "last update: to {} {state} ({}): {}",
            short(&run.to),
            output::ago(run.started),
            run.message.lines().next().unwrap_or_default()
        );
    }
    for node in &status.nodes {
        match node.state {
            NodeBuildState::Current => {}
            NodeBuildState::Updating => {
                println!("node {}: updates itself when it connects", node.name)
            }
            NodeBuildState::Manual => println!(
                "node {}: runs another build, and the server has none for its CPU ({}): update it by hand",
                node.name,
                node.arch.as_deref().unwrap_or("unknown")
            ),
            NodeBuildState::Unknown => println!("node {}: build unknown", node.name),
        }
    }
    if !status.available {
        println!("up to date");
        return Ok(());
    }
    if !status.commits.is_empty() {
        println!("new commits:");
        for c in &status.commits {
            println!(
                "  {} {}  ({}, {})",
                short(&c.hash),
                c.subject,
                c.author,
                output::ago(c.time)
            );
        }
    }
    if check {
        println!("install it with `facade server update`");
        return Ok(());
    }

    eprintln!("downloading and checking {}…", short(&latest.commit));
    let target = UpdateTarget {
        commit: latest.commit.clone(),
    };
    let status: ServerUpdate = api
        .json(
            api.request(Method::POST, "/api/server-update/prepare")
                .json(&target),
        )
        .await?;
    let plan = status.prepared.map(|p| p.plan).unwrap_or_default();
    let mut affected = 0;
    for change in &plan {
        let what = match change.change {
            AppChange::Restart => "restarts (its container is recreated, a few seconds down)",
            AppChange::Rebuild => "is rebuilt at its deployed commit (or pulled), then restarts",
            AppChange::Unchanged | AppChange::NotRunning => continue,
        };
        affected += 1;
        println!("  {:<24} {what}", change.app);
    }
    if affected == 0 {
        println!("no running app is restarted by this update");
    }
    if !yes && !confirm("Install it?")? {
        bail!("aborted");
    }
    let _: ServerUpdate = api
        .json(
            api.request(Method::POST, "/api/server-update/install")
                .json(&target),
        )
        .await?;
    eprintln!("installing; the server restarts…");

    // The server goes away for a few seconds: keep asking.
    let mut said = String::new();
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let Ok(status) = api.get::<ServerUpdate>("/api/server-update").await else {
            continue;
        };
        let Some(run) = status.run.clone() else {
            continue;
        };
        if run.message != said {
            eprintln!("  {}", run.message);
            said = run.message.clone();
        }
        match run.state {
            UpdateState::Running => {}
            UpdateState::Done => {
                if json {
                    return print_json(&status);
                }
                println!("facade is now at {}", short(&run.to));
                return Ok(());
            }
            UpdateState::RolledBack | UpdateState::Failed => bail!("the update did not go through"),
        }
    }
    bail!("the update is still running after 5 minutes: see `facade server update --check`")
}

async fn dispatch(api: &Api, command: Command, json: bool) -> anyhow::Result<()> {
    match command {
        Command::Login { .. } => unreachable!(),
        Command::Logout => {
            api.send(api.request(Method::POST, "/api/logout")).await?;
            let mut saved = api.saved.clone();
            saved.token.clear();
            saved.save()?;
            eprintln!("logged out");
        }
        Command::Passwd => {
            let password = rpassword::prompt_password("new password: ")?;
            if password != rpassword::prompt_password("again: ")? {
                bail!("passwords differ");
            }
            api.send(
                api.request(Method::PUT, "/api/password")
                    .json(&ChangePassword { password }),
            )
            .await?;
            eprintln!("password changed; other sessions were logged out — log in again");
        }
        Command::Status => {
            let status: StatusView = api.get("/api/status").await?;
            if json {
                return print_json(&status);
            }
            print_nodes(&status.nodes);
            println!();
            print_apps(&status.apps);
        }
        Command::Alerts {
            action: Some(AlertsCommand::Test),
        } => {
            api.send(api.request(Method::POST, "/api/alerts/test"))
                .await?;
            eprintln!("test notification sent");
        }
        Command::Alerts {
            action:
                Some(AlertsCommand::Config {
                    webhook,
                    mentions,
                    no_mentions,
                }),
        } => {
            for mention in &mentions {
                mention.parse::<Mention>().map_err(|e| anyhow::anyhow!(e))?;
            }
            let update = NotifyUpdate {
                discord_webhook: webhook,
                mentions: if no_mentions {
                    Some(vec![])
                } else {
                    (!mentions.is_empty()).then_some(mentions)
                },
            };
            let settings: NotifySettings =
                if update.discord_webhook.is_none() && update.mentions.is_none() {
                    api.get("/api/alerts/settings").await?
                } else {
                    api.json(
                        api.request(Method::PUT, "/api/alerts/settings")
                            .json(&update),
                    )
                    .await?
                };
            if json {
                return print_json(&settings);
            }
            println!(
                "discord webhook: {}",
                settings.discord_webhook.as_deref().unwrap_or("(none)")
            );
            println!(
                "tags on problems: {}",
                if settings.mentions.is_empty() {
                    "(nobody)".to_string()
                } else {
                    settings.mentions.join(" ")
                }
            );
        }
        Command::Alerts { action: None } => {
            let alerts: Vec<AlertView> = api.get("/api/alerts").await?;
            if json {
                return print_json(&alerts);
            }
            if alerts.is_empty() {
                println!("no alerts");
            }
            table(
                &["STATE", "SINCE", "ALERT"],
                alerts.iter().map(|a| {
                    vec![
                        if a.resolved_at.is_some() {
                            "resolved"
                        } else {
                            "FIRING"
                        }
                        .to_string(),
                        output::ago(a.since),
                        a.message.clone(),
                    ]
                }),
            );
        }
        Command::Node(cmd) => node(api, cmd, json).await?,
        Command::App(cmd) => app(api, cmd, json).await?,
        Command::Env(cmd) => env(api, cmd, json).await?,
        Command::Domain(cmd) => {
            let (name, domain, add) = match cmd {
                DomainCommand::Add { app, domain } => (app, domain, true),
                DomainCommand::Remove { app, domain } => (app, domain, false),
            };
            let view: AppView = api.get(&format!("/api/apps/{name}")).await?;
            let mut spec = view.spec;
            let domain = domain.to_lowercase();
            if add {
                if !spec.domains.contains(&domain) {
                    spec.domains.push(domain.clone());
                }
            } else {
                spec.domains.retain(|d| *d != domain);
            }
            let view: AppView = api
                .json(
                    api.request(Method::PUT, &format!("/api/apps/{name}"))
                        .json(&spec),
                )
                .await?;
            if json {
                return print_json(&view);
            }
            if add {
                eprintln!(
                    "{domain} → {name}. Check its DNS and certificate with `facade domains`."
                );
            } else {
                eprintln!("{domain} removed from {name}");
            }
        }
        Command::Deploy {
            app,
            commit,
            rebuild,
            detach,
        } => {
            if stack_of(api, &app).await.is_some() {
                let request = StackDeployRequest {
                    commit,
                    rebuild,
                    all: false,
                };
                return deploy_stack(api, &app, &request, detach, json).await;
            }
            let request = DeployRequest { commit, rebuild };
            deploy(api, &app, &request, detach, json).await?;
        }
        Command::Rollback { app, to, detach } => {
            if let Some(stack) = stack_of(api, &app).await {
                let list: Vec<StackDeploymentView> =
                    api.get(&format!("/api/stacks/{app}/deployments")).await?;
                let target = match &to {
                    Some(id) => list
                        .iter()
                        .find(|d| d.id.to_string().starts_with(id.as_str()))
                        .with_context(|| format!("no deployment {id} of {app}"))?,
                    None => list
                        .iter()
                        .find(|d| {
                            d.status == DeploymentStatus::Succeeded
                                && d.commit.is_some()
                                && d.commit != stack.commit
                        })
                        .context("no earlier successful deployment of another commit")?,
                };
                let commit = target
                    .commit
                    .clone()
                    .context("that deployment has no commit")?;
                if !json {
                    eprintln!(
                        "rolling {app} back to {} {}",
                        short(&commit),
                        target.commit_title.as_deref().unwrap_or("")
                    );
                }
                let request = StackDeployRequest {
                    commit: Some(commit),
                    rebuild: false,
                    all: false,
                };
                return deploy_stack(api, &app, &request, detach, json).await;
            }
            let list: Vec<DeploymentView> =
                api.get(&format!("/api/apps/{app}/deployments")).await?;
            let current: AppView = api.get(&format!("/api/apps/{app}")).await?;
            let target = match &to {
                Some(id) => list
                    .iter()
                    .find(|d| d.id.to_string().starts_with(id.as_str()))
                    .with_context(|| format!("no deployment {id} of {app}"))?,
                None => list
                    .iter()
                    .find(|d| {
                        d.status == DeploymentStatus::Succeeded
                            && d.commit.is_some()
                            && d.commit != current.commit
                    })
                    .context("no earlier successful deployment of another commit")?,
            };
            let commit = target
                .commit
                .clone()
                .context("that deployment has no recorded commit")?;
            if !json {
                eprintln!(
                    "rolling {app} back to {} {}",
                    short(&commit),
                    target.commit_title.as_deref().unwrap_or("")
                );
            }
            let request = DeployRequest {
                commit: Some(commit),
                rebuild: false,
            };
            deploy(api, &app, &request, detach, json).await?;
        }
        Command::Cancel { target } => {
            if stack_of(api, &target).await.is_some() {
                let list: Vec<StackDeploymentView> = api
                    .get(&format!("/api/stacks/{target}/deployments"))
                    .await?;
                let id = list
                    .iter()
                    .find(|d| !d.status.is_finished())
                    .map(|d| d.id)
                    .with_context(|| format!("{target} has no deployment in progress"))?;
                api.send(api.request(Method::POST, &format!("/api/stack-deployments/{id}/cancel")))
                    .await?;
                eprintln!("deployment {id} of {target} cancelled");
                return Ok(());
            }
            let id = match uuid::Uuid::parse_str(&target) {
                Ok(id) => id,
                Err(_) => {
                    let list: Vec<DeploymentView> =
                        api.get(&format!("/api/apps/{target}/deployments")).await?;
                    list.iter()
                        .find(|d| !d.status.is_finished())
                        .map(|d| d.id)
                        .with_context(|| format!("{target} has no deployment in progress"))?
                }
            };
            api.send(api.request(Method::POST, &format!("/api/deployments/{id}/cancel")))
                .await?;
            eprintln!("deployment {id} cancelled");
        }
        Command::Builds => {
            let builds: BuildsView = api.get("/api/builds").await?;
            if json {
                return print_json(&builds);
            }
            if builds.queue.is_empty() {
                println!("nothing building");
            } else {
                table(
                    &["NODE", "APP", "STATE", "COMMIT", "SINCE", "DEPLOYMENT"],
                    builds.queue.iter().map(|b| {
                        vec![
                            b.node.clone(),
                            b.app.clone(),
                            match b.state {
                                facade_core::proto::BuildState::Building => "building".into(),
                                facade_core::proto::BuildState::Queued => {
                                    format!("queued #{}", b.position)
                                }
                            },
                            b.commit.as_deref().map(short).unwrap_or_default(),
                            output::ago(b.since),
                            b.deployment
                                .map(|d| d.to_string())
                                .unwrap_or_else(|| "(healing)".into()),
                        ]
                    }),
                );
            }
            println!();
            print_deployments(&builds.recent, true);
        }
        Command::Git(cmd) => git(api, cmd, json).await?,
        Command::Backup(cmd) => backup(api, cmd, json).await?,
        Command::Registry(cmd) => registry(api, cmd, json).await?,
        Command::User(cmd) => user(api, cmd, json).await?,
        Command::Whoami => {
            let me: WhoAmI = api.get("/api/whoami").await?;
            if json {
                return print_json(&me);
            }
            println!("{} ({})", me.username, me.role);
            if let Some(cap) = me.memory_cap_mb {
                println!(
                    "memory: {} of {cap} MB used by your apps",
                    me.memory_allocated_mb
                );
            }
            if let Some(cap) = me.cpu_cap {
                println!("cpus:   {} of {cap} used by your apps", me.cpus_allocated);
            }
            if me.must_change_password {
                println!("change your password: facade passwd");
            }
        }
        Command::Server {
            action: Some(ServerCommand::Update { check, yes }),
        } => update_server(api, check, yes, json).await?,
        Command::Server { action } => {
            let settings: ServerSettings = match action {
                None => api.get("/api/server-settings").await?,
                Some(ServerCommand::Update { .. }) => unreachable!(),
                Some(ServerCommand::Set {
                    api_domain,
                    acme_email,
                    direct_api_local_only,
                    direct_api_open,
                }) => {
                    let update = ServerSettingsUpdate {
                        api_domain,
                        acme_email,
                        direct_api_local_only: if direct_api_local_only {
                            Some(true)
                        } else if direct_api_open {
                            Some(false)
                        } else {
                            None
                        },
                    };
                    api.json(
                        api.request(Method::PUT, "/api/server-settings")
                            .json(&update),
                    )
                    .await?
                }
            };
            if json {
                return print_json(&settings);
            }
            match &settings.api_domain {
                Some(d) if settings.api_domain_ready => println!("web UI & API:  https://{d}"),
                Some(d) => println!(
                    "web UI & API:  https://{d} (waiting for its certificate: point its DNS here, see `facade domains`)"
                ),
                None => println!(
                    "web UI & API:  no domain (set one with `facade server set --api-domain …`)"
                ),
            }
            println!(
                "direct port:   https://{} ({})",
                settings.api_listen,
                if settings.direct_api_local_only {
                    "only from the server itself"
                } else {
                    "open, self-signed certificate"
                }
            );
            println!(
                "acme contact:  {}",
                settings.acme_email.as_deref().unwrap_or("(none)")
            );
        }
        Command::Updates { app } => {
            let check: UpdateCheck = api
                .json(api.request(Method::POST, &format!("/api/apps/{app}/updates")))
                .await?;
            if json {
                return print_json(&check);
            }
            let branch = check.branch.as_deref().unwrap_or("the default branch");
            let short = |h: &str| h.get(..7).unwrap_or(h).to_string();
            match check.behind {
                Some(0) => println!(
                    "{app} is up to date with {branch} ({})",
                    short(&check.latest.hash)
                ),
                Some(n) => println!(
                    "{app} is {n} commit{} behind {branch}:",
                    if n == 1 { "" } else { "s" }
                ),
                None => println!(
                    "{app}'s deployed commit is unknown{}; recent commits on {branch}:",
                    check
                        .deployed
                        .as_deref()
                        .map(|c| format!(" ({} is not on the branch)", short(c)))
                        .unwrap_or_default()
                ),
            }
            for c in &check.commits {
                println!(
                    "  {} {}  ({}, {})",
                    short(&c.hash),
                    c.subject,
                    c.author,
                    output::ago(c.time)
                );
            }
            if check.behind != Some(0) {
                println!("deploy them with `facade deploy {app}`");
            }
        }
        Command::Db(DbCommand::Create {
            name,
            kind,
            version,
            node,
            memory,
            links,
            env_var,
        }) => {
            let kind: DatabaseKind = kind.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            let view: AppView = api
                .json(
                    api.request(Method::POST, "/api/databases")
                        .json(&CreateDatabase {
                            name: name.clone(),
                            kind,
                            version,
                            node,
                            memory_mb: memory,
                            link: links.clone(),
                            env_var: env_var.clone(),
                        }),
                )
                .await?;
            if json {
                return print_json(&view);
            }
            eprintln!(
                "created {name} ({}) on {}, with its data in a volume",
                match &view.spec.source {
                    Source::Image { image } => image.as_str(),
                    _ => "?",
                },
                view.spec.node
            );
            let var = env_var.unwrap_or_else(|| kind.url_var().to_string());
            for app in &links {
                eprintln!(
                    "{app}: linked, {var}={}",
                    facade_core::env::reference(&name, kind.url_var())
                );
            }
            eprintln!(
                "other apps of {} connect with: facade app update <app> --link {name} && facade env set <app> {var}='{}' --public",
                view.spec.node,
                facade_core::env::reference(&name, kind.url_var())
            );
        }
        Command::Volume(VolumeCommand::Export {
            app,
            volume,
            output,
        }) => {
            let path = output.unwrap_or_else(|| {
                PathBuf::from(format!("{app}-{volume}-{}.tar.gz", facade_core::now()))
            });
            let resp = api
                .send(api.request(
                    Method::GET,
                    &format!("/api/apps/{app}/volumes/{volume}/archive"),
                ))
                .await?;
            let size = download(resp, &path).await?;
            eprintln!("saved {} ({})", path.display(), bytes(size));
        }
        Command::Volume(VolumeCommand::Import {
            app,
            volume,
            file,
            yes,
        }) => {
            if !yes
                && !confirm(&format!(
                    "Replace everything in {app}/{volume} with {}? ({app} is stopped meanwhile)",
                    file.display()
                ))?
            {
                bail!("aborted");
            }
            let f = tokio::fs::File::open(&file)
                .await
                .with_context(|| format!("opening {}", file.display()))?;
            let size = f.metadata().await?.len();
            eprintln!("uploading {} ({})", file.display(), bytes(size));
            api.send(
                api.request(
                    Method::PUT,
                    &format!("/api/apps/{app}/volumes/{volume}/archive"),
                )
                .header("content-length", size)
                .body(reqwest::Body::wrap_stream(
                    tokio_util::io::ReaderStream::new(f),
                )),
            )
            .await?;
            eprintln!("{app}/{volume} replaced");
        }
        Command::Volume(VolumeCommand::List) => {
            let volumes: Vec<VolumeView> = api.get("/api/volumes").await?;
            if json {
                return print_json(&volumes);
            }
            if volumes.is_empty() {
                println!("no volumes");
            }
            table(
                &["NODE", "VOLUME", "APP", "SIZE", "STATE", "LAST BACKUP"],
                volumes.iter().map(|v| {
                    vec![
                        v.node.clone(),
                        v.name.clone(),
                        v.app.clone(),
                        match (v.size, v.limit) {
                            (size, Some(limit)) => format!(
                                "{} of {}",
                                size.map(bytes).unwrap_or_else(|| "?".into()),
                                bytes(limit)
                            ),
                            (size, None) => size.map(bytes).unwrap_or_else(|| "?".into()),
                        },
                        if !v.declared {
                            "orphaned (no app declares it)".into()
                        } else if v.in_use {
                            "in use".into()
                        } else {
                            "declared, not mounted".into()
                        },
                        v.last_backup
                            .map(output::ago)
                            .unwrap_or_else(|| "never".into()),
                    ]
                }),
            );
        }
        Command::Volume(VolumeCommand::Remove { node, name, yes }) => {
            if !yes && !confirm(&format!("Delete volume {name} on {node} and all its data?"))? {
                bail!("aborted");
            }
            api.send(api.request(Method::DELETE, &format!("/api/nodes/{node}/volumes/{name}")))
                .await?;
            eprintln!("{name} deleted");
        }
        Command::Stack(cmd) => stack(api, cmd, json).await?,
        Command::Restart { app } => {
            for app in targets(api, &app).await? {
                api.send(api.request(Method::POST, &format!("/api/apps/{app}/restart")))
                    .await?;
                eprintln!("{app} restarted");
            }
        }
        Command::Start { app } => {
            for app in targets(api, &app).await? {
                api.send(api.request(Method::POST, &format!("/api/apps/{app}/start")))
                    .await?;
                eprintln!("{app} enabled");
            }
        }
        Command::Stop { app } => {
            for app in targets(api, &app).await? {
                api.send(api.request(Method::POST, &format!("/api/apps/{app}/stop")))
                    .await?;
                eprintln!("{app} stopped");
            }
        }
        Command::Logs { app, follow, tail } => {
            let resp = api
                .send(
                    api.request(Method::GET, &format!("/api/apps/{app}/logs"))
                        .query(&[("tail", tail.to_string()), ("follow", follow.to_string())]),
                )
                .await?;
            let mut body = resp.bytes_stream();
            let mut stdout = std::io::stdout().lock();
            while let Some(chunk) = body.next().await {
                stdout.write_all(&chunk?)?;
                stdout.flush()?;
            }
        }
        Command::Deployments { app, id } => {
            if let Some(id) = id {
                let d: DeploymentView = api.get(&format!("/api/deployments/{id}")).await?;
                if json {
                    return print_json(&d);
                }
                print!("{}", d.log);
                println!(
                    "-- {}{}{}",
                    d.status,
                    d.commit
                        .as_deref()
                        .map(|c| format!(" at {}", short(c)))
                        .unwrap_or_default(),
                    d.error.map(|e| format!(": {e}")).unwrap_or_default()
                );
                return Ok(());
            }
            let list: Vec<DeploymentView> =
                api.get(&format!("/api/apps/{app}/deployments")).await?;
            if json {
                return print_json(&list);
            }
            print_deployments(&list, false);
        }
        Command::Domains => {
            let domains: Vec<DomainStatus> = api.get("/api/domains").await?;
            if json {
                return print_json(&domains);
            }
            if domains.is_empty() {
                println!("no domains; add one with `facade domain add <app> <domain>`");
            }
            table(
                &["DOMAIN", "APP", "NODE", "DNS", "CERTIFICATE"],
                domains.iter().map(|d| {
                    let ip = d.expected_ip.clone().unwrap_or_else(|| "?".into());
                    let dns = if let Some(e) = &d.dns_error {
                        format!("error: {e}")
                    } else if d.dns_ok {
                        format!("ok ({ip})")
                    } else if d.managed {
                        format!("managed, propagating ({ip})")
                    } else {
                        format!("create an A record → {ip}")
                    };
                    let cert = match (&d.certificate, &d.cert_error) {
                        (_, Some(e)) => format!("error: {e}"),
                        (Some(name), None) => format!(
                            "{name}, valid until {}",
                            output::date(d.cert_expires.unwrap_or(0))
                        ),
                        (None, None) => "pending".into(),
                    };
                    vec![
                        d.domain.clone(),
                        d.app.clone().unwrap_or_else(|| "(facade)".into()),
                        d.node.clone(),
                        dns,
                        cert,
                    ]
                }),
            );
        }
        Command::Dns(cmd) => {
            let settings: DnsSettings = match cmd {
                DnsCommand::Show => api.get("/api/dns").await?,
                DnsCommand::Porkbun {
                    api_key,
                    secret_key,
                } => {
                    api.json(api.request(Method::PUT, "/api/dns").json(&DnsUpdate {
                        provider: "porkbun".into(),
                        api_key,
                        secret_key,
                    }))
                    .await?
                }
                DnsCommand::Off => {
                    api.json(
                        api.request(Method::PUT, "/api/dns")
                            .json(&DnsUpdate::default()),
                    )
                    .await?
                }
            };
            if json {
                return print_json(&settings);
            }
            match &settings.provider {
                Some(p) => println!("provider: {p}"),
                None => {
                    println!("provider: none (create DNS records by hand; see `facade domains`)")
                }
            }
            if !settings.zones.is_empty() {
                println!("domains:  {}", settings.zones.join(", "));
            }
            if let Some(e) = &settings.error {
                println!("error:    {e}");
            }
        }
        Command::Scan { repo, branch } => {
            let repo = repo.as_str();
            let report = scan(api, repo, branch.as_deref()).await?;
            if json {
                return print_json(&report);
            }
            println!("commit {}", report.commit);
            if let Some(credential) = &report.credential {
                println!("fetched with credential {credential}");
            }
            if report.candidates.is_empty() {
                println!("nothing deployable found");
            }
            for (i, c) in report.candidates.iter().enumerate() {
                println!("{}. {}\n   {}", i + 1, c.title, c.detail);
                if !c.command.is_empty() {
                    println!(
                        "   suggested command: {}",
                        facade_core::join_command(&c.command)
                    );
                }
                for note in &c.notes {
                    println!("   note: {note}");
                }
                if !c.env.is_empty() {
                    println!("   reads {}", c.env.join(", "));
                }
            }
            for file in &report.compose_files {
                println!(
                    "compose file {file}: `facade import {repo} --file {file}` creates one app per service"
                );
            }
        }
        Command::Metrics { app } => {
            let metrics: Vec<AppMetric> = api.get(&format!("/api/apps/{app}/metrics")).await?;
            if json {
                return print_json(&metrics);
            }
            table(
                &["AT", "STATE", "CPU", "MEMORY", "NET IN", "NET OUT"],
                metrics.iter().rev().take(40).map(|m| {
                    vec![
                        output::ago(m.at),
                        m.stats.state.to_string(),
                        format!("{:.1}%", m.stats.cpu_percent),
                        bytes(m.stats.mem_used),
                        bytes(m.stats.net_rx),
                        bytes(m.stats.net_tx),
                    ]
                }),
            );
        }
        Command::Export { output } => {
            let toml = api
                .send(api.request(Method::GET, "/api/export"))
                .await?
                .text()
                .await?;
            match output {
                Some(path) => {
                    std::fs::write(&path, toml)?;
                    eprintln!("written to {}", path.display());
                }
                None => print!("{toml}"),
            }
        }
        Command::Apply { file } => {
            let body = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            facade_core::manifest::Manifest::parse(&body)
                .map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
            let report: ApplyReport = api
                .json(
                    api.request(Method::POST, "/api/apply")
                        .header("content-type", "application/toml")
                        .body(body),
                )
                .await?;
            if json {
                return print_json(&report);
            }
            for (label, list) in [
                ("created", &report.created),
                ("updated", &report.updated),
                ("unchanged", &report.unchanged),
            ] {
                if !list.is_empty() {
                    println!("{label}: {}", list.join(", "));
                }
            }
            for node in &report.new_nodes {
                println!("new node {}: on that machine run", node.name);
                println!("  facaded agent --join {}", node.join_token);
            }
            if !report.missing_secrets.is_empty() {
                println!("sensitive variables still to set (facade env set <app> KEY):");
                for missing in &report.missing_secrets {
                    println!("  {missing}");
                }
            }
            if !report.created.is_empty() {
                println!("new apps are not deployed yet: facade deploy <app>");
            }
        }
    }
    Ok(())
}

async fn deploy(
    api: &Api,
    app: &str,
    request: &DeployRequest,
    detach: bool,
    json: bool,
) -> anyhow::Result<()> {
    let deployment: DeploymentView = api
        .json(
            api.request(Method::POST, &format!("/api/apps/{app}/deploy"))
                .json(request),
        )
        .await?;
    if json && detach {
        return print_json(&deployment);
    }
    if detach {
        eprintln!(
            "deployment {} started{}",
            deployment.id,
            deployment
                .commit
                .as_deref()
                .map(|c| format!(" (commit {})", short(c)))
                .unwrap_or_default()
        );
        return Ok(());
    }
    if !json && let Some(commit) = &deployment.commit {
        eprintln!("deploying {app} at {}", short(commit));
    }
    follow_deployment(api, deployment.id, json).await
}

/// The first 12 characters of a commit hash.
fn short(commit: &str) -> String {
    commit.chars().take(12).collect()
}

async fn follow_deployment(api: &Api, id: uuid::Uuid, json: bool) -> anyhow::Result<()> {
    let mut printed = 0;
    let mut said_queued = false;
    loop {
        let d: DeploymentView = api.get(&format!("/api/deployments/{id}")).await?;
        if !json && d.log.len() > printed {
            print!("{}", &d.log[printed..]);
            std::io::stdout().flush()?;
            printed = d.log.len();
        }
        match d.status {
            DeploymentStatus::Queued => {
                if !json && !said_queued && printed == 0 {
                    eprintln!("queued: waiting for the node's build slot (`facade builds`)");
                    said_queued = true;
                }
                tokio::time::sleep(Duration::from_millis(1000)).await
            }
            DeploymentStatus::Running => tokio::time::sleep(Duration::from_millis(700)).await,
            DeploymentStatus::Succeeded => {
                if json {
                    return print_json(&d);
                }
                eprintln!(
                    "deployed {} (revision {}{})",
                    d.app,
                    d.revision,
                    d.commit
                        .as_deref()
                        .map(|c| format!(", commit {}", short(c)))
                        .unwrap_or_default()
                );
                return Ok(());
            }
            DeploymentStatus::Cancelled => {
                if json {
                    print_json(&d)?;
                }
                bail!("deployment cancelled; the previous version keeps running");
            }
            DeploymentStatus::Failed => {
                if json {
                    print_json(&d)?;
                }
                bail!(
                    "deployment failed: {}",
                    d.error.unwrap_or_else(|| "unknown error".into())
                );
            }
        }
    }
}

fn print_deployments(list: &[DeploymentView], with_app: bool) {
    let mut headers = vec!["ID", "REV", "STATUS", "COMMIT", "CREATED", "TOOK", "ERROR"];
    if with_app {
        headers.insert(1, "APP");
    }
    table(
        &headers,
        list.iter().map(|d| {
            let mut row = vec![
                d.id.to_string(),
                d.revision.to_string(),
                d.status.to_string(),
                match (&d.commit, &d.commit_title) {
                    (Some(c), Some(t)) => format!("{} {}", short(c), truncate(t, 40)),
                    (Some(c), None) => short(c),
                    _ => String::new(),
                },
                output::ago(d.created_at),
                match (d.started_at, d.finished_at) {
                    (Some(s), Some(f)) => format!("{}s", f.saturating_sub(s)),
                    _ => String::new(),
                },
                d.error
                    .clone()
                    .unwrap_or_default()
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string(),
            ];
            if with_app {
                row.insert(1, d.app.clone());
            }
            row
        }),
    );
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

/// The stack named `name`, if it is one.
async fn stack_of(api: &Api, name: &str) -> Option<StackView> {
    if name.contains('.') || uuid::Uuid::parse_str(name).is_ok() {
        return None;
    }
    api.get(&format!("/api/stacks/{name}")).await.ok()
}

/// An app, or every service of a stack.
async fn targets(api: &Api, name: &str) -> anyhow::Result<Vec<String>> {
    Ok(match stack_of(api, name).await {
        Some(stack) => stack.services.into_iter().map(|a| a.spec.name).collect(),
        None => vec![name.to_string()],
    })
}

async fn deploy_stack(
    api: &Api,
    name: &str,
    request: &StackDeployRequest,
    detach: bool,
    json: bool,
) -> anyhow::Result<()> {
    let d: StackDeploymentView = api
        .json(
            api.request(Method::POST, &format!("/api/stacks/{name}/deploy"))
                .json(request),
        )
        .await?;
    if detach {
        if json {
            return print_json(&d);
        }
        eprintln!("deployment {} of {name} started", d.id);
        return Ok(());
    }
    // The stack's own log, and each service's build and start output.
    let mut printed = 0;
    let mut service_printed: BTreeMap<uuid::Uuid, usize> = BTreeMap::new();
    loop {
        let d: StackDeploymentView = api.get(&format!("/api/stack-deployments/{}", d.id)).await?;
        if !json {
            if d.log.len() > printed {
                print!("{}", &d.log[printed..]);
                printed = d.log.len();
            }
            for s in &d.services {
                let Ok(sd) = api
                    .get::<DeploymentView>(&format!("/api/deployments/{}", s.deployment))
                    .await
                else {
                    continue;
                };
                let done = service_printed.entry(s.deployment).or_default();
                if sd.log.len() > *done {
                    for line in sd.log[*done..].lines() {
                        println!("  {} | {line}", s.service);
                    }
                    *done = sd.log.len();
                }
            }
            std::io::stdout().flush()?;
        }
        match d.status {
            DeploymentStatus::Queued | DeploymentStatus::Running => {
                tokio::time::sleep(Duration::from_millis(800)).await
            }
            DeploymentStatus::Succeeded => {
                if json {
                    return print_json(&d);
                }
                eprintln!(
                    "deployed {name}{}",
                    d.commit
                        .as_deref()
                        .map(|c| format!(" at {}", short(c)))
                        .unwrap_or_default()
                );
                return Ok(());
            }
            DeploymentStatus::Cancelled => {
                if json {
                    print_json(&d)?;
                }
                bail!("deployment cancelled");
            }
            DeploymentStatus::Failed => {
                if json {
                    print_json(&d)?;
                }
                bail!(
                    "deployment failed: {}",
                    d.error.unwrap_or_else(|| "unknown error".into())
                );
            }
        }
    }
}

fn print_stack_plan(plan: &StackPlan) {
    if let Some(commit) = &plan.commit {
        println!(
            "at {} {}",
            short(commit),
            plan.commit_title.as_deref().unwrap_or("")
        );
    }
    for s in &plan.services {
        let app = &s.app;
        let source = match &app.source {
            Source::Image { image } => format!("image {image}"),
            Source::Git {
                build:
                    Build::Dockerfile {
                        dockerfile,
                        context,
                    },
                ..
            } => format!("build {dockerfile} (context {context})"),
            Source::Git {
                build: Build::Rust { bin, .. },
                ..
            } => format!("cargo binary {bin}"),
            _ => String::new(),
        };
        let change = match s.change {
            ServiceChange::New => "new",
            ServiceChange::Changed => "changed",
            ServiceChange::Unchanged => "unchanged",
        };
        println!("  {} ({change}): {source}", s.service);
        if let Some(port) = app.http_port {
            println!("      http port {port}");
        }
        for domain in &app.domains {
            println!("      domain {domain}");
        }
        for v in &app.volumes {
            println!("      volume {v}");
        }
        if !s.depends_on.is_empty() {
            println!("      after {}", s.depends_on.join(", "));
        }
        if let Some(release) = &app.release_command {
            println!("      release {release}");
        }
        if !s.env.is_empty() {
            println!("      {} variables from the file", s.env.len());
        }
        if !s.missing.is_empty() {
            println!(
                "      to set: {} (facade env set {} KEY)",
                s.missing.join(", "),
                app.name
            );
        }
    }
    if !plan.removed.is_empty() {
        println!("  removed: {}", plan.removed.join(", "));
    }
    for note in &plan.notes {
        println!("  note: {note}");
    }
}

async fn stack(api: &Api, cmd: StackCommand, json: bool) -> anyhow::Result<()> {
    match cmd {
        StackCommand::List => {
            let stacks: Vec<StackView> = api.get("/api/stacks").await?;
            if json {
                return print_json(&stacks);
            }
            if stacks.is_empty() {
                println!("no stacks; create one with `facade stack create <name> --git <url>`");
            }
            table(
                &["STACK", "NODE", "SOURCE", "COMMIT", "SERVICES"],
                stacks.iter().map(|s| {
                    let running = s
                        .services
                        .iter()
                        .filter(|a| a.state == facade_core::proto::AppState::Running)
                        .count();
                    vec![
                        s.spec.name.clone(),
                        s.spec.node.clone(),
                        match (&s.spec.repo, &s.spec.file) {
                            (Some(repo), Some(file)) => format!("{repo} ({file})"),
                            (Some(repo), None) => format!("{repo} (services defined here)"),
                            (None, _) => "services defined here".into(),
                        },
                        s.commit.as_deref().map(short).unwrap_or_default(),
                        format!("{running}/{} running", s.services.len()),
                    ]
                }),
            );
        }
        StackCommand::Show { name } => {
            let s: StackView = api.get(&format!("/api/stacks/{name}")).await?;
            let deployments: Vec<StackDeploymentView> =
                api.get(&format!("/api/stacks/{name}/deployments")).await?;
            if json {
                return print_json(&serde_json::json!({ "stack": s, "deployments": deployments }));
            }
            println!("stack:     {}", s.spec.name);
            println!("node:      {}", s.spec.node);
            if let Some(repo) = &s.spec.repo {
                println!(
                    "repo:      {repo} ({})",
                    s.spec.branch.as_deref().unwrap_or("default branch")
                );
            }
            println!(
                "services:  {}",
                match &s.spec.file {
                    Some(file) => format!("from {file}, read at each deploy"),
                    None => "defined here".into(),
                }
            );
            if let Some(commit) = &s.commit {
                println!("commit:    {commit}");
            }
            println!();
            table(
                &["SERVICE", "APP", "STATE", "CPU", "MEMORY", "DOMAINS"],
                s.services.iter().map(|a| {
                    let st = a.stats.as_ref();
                    vec![
                        a.spec.service().unwrap_or_default().to_string(),
                        a.spec.name.clone(),
                        a.state.to_string(),
                        st.map(|s| format!("{:.1}%", s.cpu_percent))
                            .unwrap_or_default(),
                        st.filter(|s| s.mem_used > 0)
                            .map(|s| bytes(s.mem_used))
                            .unwrap_or_default(),
                        a.spec.domains.join(", "),
                    ]
                }),
            );
            if !deployments.is_empty() {
                println!();
                table(
                    &[
                        "DEPLOYMENT",
                        "STATUS",
                        "COMMIT",
                        "CREATED",
                        "SERVICES",
                        "ERROR",
                    ],
                    deployments.iter().take(10).map(|d| {
                        vec![
                            d.id.to_string(),
                            d.status.to_string(),
                            match (&d.commit, &d.commit_title) {
                                (Some(c), Some(t)) => format!("{} {}", short(c), truncate(t, 30)),
                                (Some(c), None) => short(c),
                                _ => String::new(),
                            },
                            output::ago(d.created_at),
                            d.services
                                .iter()
                                .map(|s| s.service.as_str())
                                .collect::<Vec<_>>()
                                .join(", "),
                            d.error.clone().unwrap_or_default(),
                        ]
                    }),
                );
            }
        }
        StackCommand::Create {
            name,
            git,
            branch,
            file,
            no_file,
            node,
            yes,
        } => {
            let node = match node {
                Some(n) => n,
                None => default_node(api).await?,
            };
            let file = match (&git, file, no_file) {
                (_, Some(file), _) => Some(file),
                (Some(repo), None, false) => {
                    let report = scan(api, repo, branch.as_deref()).await?;
                    match report.compose_files.first() {
                        Some(file) => {
                            eprintln!("using {file}");
                            Some(file.clone())
                        }
                        None => {
                            eprintln!(
                                "no compose file in the repository: define the services with `facade stack service add {name} …`"
                            );
                            None
                        }
                    }
                }
                _ => None,
            };
            let spec = StackSpec {
                name: name.clone(),
                node,
                repo: git,
                branch,
                file,
                services: BTreeMap::new(),
            };
            if spec.file.is_some() {
                let plan: StackPlan =
                    api.json(api.request(Method::POST, "/api/stacks/plan").json(
                        &StackPlanRequest {
                            spec: spec.clone(),
                            commit: None,
                        },
                    ))
                    .await?;
                if !json {
                    print_stack_plan(&plan);
                }
                if !yes && !confirm("Create this stack?")? {
                    bail!("aborted");
                }
            }
            let view: StackView = api
                .json(api.request(Method::POST, "/api/stacks").json(&spec))
                .await?;
            if json {
                return print_json(&view);
            }
            eprintln!(
                "created stack {name}. Set what is missing (facade env set {name}.<service> KEY), add domains (facade stack service update {name} <service> --domain …), then: facade deploy {name}"
            );
        }
        StackCommand::Update {
            name,
            git,
            branch,
            file,
            no_file,
            node,
        } => {
            let mut spec = api
                .get::<StackView>(&format!("/api/stacks/{name}"))
                .await?
                .spec;
            if git.is_some() {
                spec.repo = git;
            }
            if branch.is_some() {
                spec.branch = branch.filter(|b| !b.is_empty());
            }
            if file.is_some() {
                spec.file = file;
            }
            if no_file {
                spec.file = None;
            }
            if let Some(node) = node {
                spec.node = node;
            }
            save_stack(api, &spec, json).await?;
        }
        StackCommand::Plan { name, commit } => {
            let spec = api
                .get::<StackView>(&format!("/api/stacks/{name}"))
                .await?
                .spec;
            let plan: StackPlan = api
                .json(
                    api.request(Method::POST, "/api/stacks/plan")
                        .json(&StackPlanRequest { spec, commit }),
                )
                .await?;
            if json {
                return print_json(&plan);
            }
            print_stack_plan(&plan);
        }
        StackCommand::Remove { name, volumes, yes } => {
            let what = if volumes {
                "its services, their variables and its volumes (their data)"
            } else {
                "its services and their variables"
            };
            if !yes && !confirm(&format!("Delete stack {name}, {what}?"))? {
                bail!("aborted");
            }
            api.send(
                api.request(Method::DELETE, &format!("/api/stacks/{name}"))
                    .query(&[("volumes", volumes)]),
            )
            .await?;
            eprintln!("{name} deleted");
        }
        StackCommand::Service(cmd) => service(api, cmd, json).await?,
    }
    Ok(())
}

async fn save_stack(api: &Api, spec: &StackSpec, json: bool) -> anyhow::Result<()> {
    let view: StackView = api
        .json(
            api.request(Method::PUT, &format!("/api/stacks/{}", spec.name))
                .json(spec),
        )
        .await?;
    if json {
        return print_json(&view);
    }
    eprintln!(
        "saved; it applies from the next deploy: facade deploy {}",
        spec.name
    );
    Ok(())
}

async fn service(api: &Api, cmd: ServiceCommand, json: bool) -> anyhow::Result<()> {
    match cmd {
        ServiceCommand::Add {
            stack,
            service,
            args,
        }
        | ServiceCommand::Update {
            stack,
            service,
            args,
        } => {
            let mut spec = api
                .get::<StackView>(&format!("/api/stacks/{stack}"))
                .await?
                .spec;
            let mut settings = spec.services.get(&service).cloned().unwrap_or_default();
            // A database brings its image, volume and variables.
            let mut db_env = Vec::new();
            let mut url_var = "DATABASE_URL";
            if let Some(kind) = &args.database {
                let kind: DatabaseKind = kind.parse().map_err(|e: String| anyhow::anyhow!(e))?;
                let db = facade_core::templates::database_service(
                    kind,
                    args.version.as_deref(),
                    &stack,
                    &service,
                    || uuid::Uuid::new_v4().simple().to_string(),
                )?;
                settings = db.service.merged_over(&settings);
                db_env = db.env;
                url_var = db.url_var;
            }
            args.apply(&mut settings)?;
            spec.services.insert(service.clone(), settings);
            let view: StackView = api
                .json(
                    api.request(Method::PUT, &format!("/api/stacks/{stack}"))
                        .json(&spec),
                )
                .await?;
            if !db_env.is_empty() {
                let set = db_env
                    .into_iter()
                    .map(|(k, value, sensitive)| {
                        (
                            k,
                            SetEnv {
                                value,
                                sensitive,
                                build: None,
                            },
                        )
                    })
                    .collect();
                api.send(
                    api.request(Method::PATCH, &format!("/api/apps/{stack}.{service}/env"))
                        .json(&EnvPatch { set, unset: vec![] }),
                )
                .await?;
                if !json {
                    eprintln!(
                        "the other services connect with: facade env set {stack}.<service> {url_var}='{}' --public",
                        facade_core::env::reference(&format!("{stack}.{service}"), url_var)
                    );
                }
            }
            if json {
                return print_json(&view);
            }
            eprintln!(
                "saved {stack}.{service}; it applies from the next deploy: facade deploy {stack}"
            );
        }
        ServiceCommand::Remove { stack, service } => {
            let mut spec = api
                .get::<StackView>(&format!("/api/stacks/{stack}"))
                .await?
                .spec;
            if spec.services.remove(&service).is_none() {
                bail!("{stack} has no settings for {service}");
            }
            save_stack(api, &spec, json).await?;
        }
    }
    Ok(())
}

async fn git(api: &Api, cmd: GitCommand, json: bool) -> anyhow::Result<()> {
    match cmd {
        GitCommand::List => {
            let list: Vec<GitCredentialView> = api.get("/api/git/credentials").await?;
            if json {
                return print_json(&list);
            }
            if list.is_empty() {
                println!("no git credentials; public repositories need none");
            }
            table(
                &["NAME", "KIND", "SCOPE", "APPS"],
                list.iter().map(|c| {
                    vec![
                        c.name.clone(),
                        match c.kind {
                            GitCredentialKind::SshKey => "deploy key".into(),
                            GitCredentialKind::Token => "token".into(),
                        },
                        c.scope.clone(),
                        c.apps.join(", "),
                    ]
                }),
            );
        }
        GitCommand::Key {
            name,
            repo,
            private_key,
        } => {
            let private_key = match private_key {
                Some(path) => Some(
                    std::fs::read_to_string(&path)
                        .with_context(|| format!("reading {}", path.display()))?,
                ),
                None => None,
            };
            let created: GitCredentialView = api
                .json(api.request(Method::POST, "/api/git/credentials").json(
                    &CreateGitCredential {
                        name,
                        scope: repo,
                        secret: NewGitSecret::SshKey { private_key },
                    },
                ))
                .await?;
            if json {
                return print_json(&created);
            }
            println!("{}", created.public_key.as_deref().unwrap_or(""));
            eprintln!();
            match &created.add_key_url {
                Some(url) => eprintln!(
                    "Add this public key as a read-only deploy key at
  {url}"
                ),
                None => eprintln!(
                    "Add this public key as a read-only deploy key of {}",
                    created.scope
                ),
            }
            eprintln!("then `facade scan` or `facade deploy` the repository.");
        }
        GitCommand::Token {
            name,
            scope,
            username,
        } => {
            let token = if std::io::stdin().is_terminal() {
                rpassword::prompt_password("token: ")?
            } else {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s.trim().to_string()
            };
            let created: GitCredentialView = api
                .json(api.request(Method::POST, "/api/git/credentials").json(
                    &CreateGitCredential {
                        name,
                        scope,
                        secret: NewGitSecret::Token { username, token },
                    },
                ))
                .await?;
            if json {
                return print_json(&created);
            }
            eprintln!(
                "token {} stored for {}{}",
                created.name,
                created.scope,
                if created.apps.is_empty() {
                    String::new()
                } else {
                    format!(" (covers {})", created.apps.join(", "))
                }
            );
        }
        GitCommand::Remove { name } => {
            api.send(api.request(Method::DELETE, &format!("/api/git/credentials/{name}")))
                .await?;
            eprintln!("{name} removed");
        }
    }
    Ok(())
}

async fn node(api: &Api, cmd: NodeCommand, json: bool) -> anyhow::Result<()> {
    match cmd {
        NodeCommand::List => {
            let nodes: Vec<NodeView> = api.get("/api/nodes").await?;
            if json {
                return print_json(&nodes);
            }
            print_nodes(&nodes);
        }
        NodeCommand::Add { name } => {
            let created: CreatedNode = api
                .json(
                    api.request(Method::POST, "/api/nodes")
                        .json(&CreateNode { name }),
                )
                .await?;
            if json {
                return print_json(&created);
            }
            print_join(&created);
        }
        NodeCommand::Token { name } => {
            let created: CreatedNode = api
                .json(api.request(Method::POST, &format!("/api/nodes/{name}/token")))
                .await?;
            if json {
                return print_json(&created);
            }
            print_join(&created);
        }
        NodeCommand::Remove { name } => {
            api.send(api.request(Method::DELETE, &format!("/api/nodes/{name}")))
                .await?;
            eprintln!(
                "node {name} removed (stop its agent: systemctl disable --now facaded-agent)"
            );
        }
        NodeCommand::Prune { name, aggressive } => {
            api.send(
                api.request(Method::POST, &format!("/api/nodes/{name}/prune"))
                    .query(&[("aggressive", aggressive)]),
            )
            .await?;
            eprintln!("cleanup started on {name}; `facade node ls` shows the result in a minute");
        }
        NodeCommand::Ip { name, ip } => {
            let public_ip = (ip != "auto").then_some(ip);
            api.send(
                api.request(Method::PUT, &format!("/api/nodes/{name}/ip"))
                    .json(&SetNodeIp { public_ip }),
            )
            .await?;
            eprintln!("updated {name}; DNS records follow");
        }
        NodeCommand::Ports { name } => {
            let ports: Vec<Listener> = api.get(&format!("/api/nodes/{name}/ports")).await?;
            if json {
                return print_json(&ports);
            }
            table(
                &["PORT", "ADDRESS", "PROCESS", "APP"],
                ports.iter().map(|l| {
                    vec![
                        format!("{}/{}", l.port, l.protocol),
                        l.address.clone(),
                        match (&l.process, l.pid) {
                            (Some(p), Some(pid)) => format!("{p} ({pid})"),
                            (Some(p), None) => p.clone(),
                            _ => "?".into(),
                        },
                        l.app.clone().unwrap_or_default(),
                    ]
                }),
            );
        }
        NodeCommand::Disk { name } => {
            let nodes: Vec<NodeView> = api.get("/api/nodes").await?;
            let node = nodes
                .into_iter()
                .find(|n| n.name == name)
                .with_context(|| format!("no node {name}"))?;
            let mut apps: Vec<AppView> = api.get("/api/apps").await?;
            apps.retain(|a| a.spec.node == name);
            let mut volumes: Vec<VolumeView> = api.get("/api/volumes").await?;
            volumes.retain(|v| v.node == name);
            if json {
                return print_json(&serde_json::json!({
                    "storage": node.storage,
                    "apps": apps.iter().map(|a| (&a.spec.name, &a.disk)).collect::<BTreeMap<_, _>>(),
                }));
            }
            print_disk(&node, &apps, &volumes)?;
        }
        NodeCommand::Metrics { name } => {
            let metrics: Vec<NodeMetric> = api.get(&format!("/api/nodes/{name}/metrics")).await?;
            if json {
                return print_json(&metrics);
            }
            table(
                &["AT", "CPU", "LOAD", "MEMORY", "DISK"],
                metrics.iter().rev().take(40).map(|m| {
                    vec![
                        output::ago(m.at),
                        format!("{:.1}%", m.stats.cpu_percent),
                        format!("{:.2}", m.stats.load1),
                        format!("{} / {}", bytes(m.stats.mem_used), bytes(m.stats.mem_total)),
                        format!(
                            "{} / {}",
                            bytes(m.stats.disk_used),
                            bytes(m.stats.disk_total)
                        ),
                    ]
                }),
            );
        }
    }
    Ok(())
}

fn print_disk(node: &NodeView, apps: &[AppView], volumes: &[VolumeView]) -> anyhow::Result<()> {
    let Some(s) = &node.storage else {
        bail!(
            "no storage report from {} yet (agents send one when they start, then every 15 minutes)",
            node.name
        );
    };
    let percent = |n: u64| (n * 100).checked_div(s.disk_total).unwrap_or(0);
    println!(
        "{}: {} used of {} ({} %), measured {}",
        node.name,
        bytes(s.disk_used),
        bytes(s.disk_total),
        percent(s.disk_used),
        output::ago(s.at)
    );
    let orphaned: Vec<&VolumeView> = volumes.iter().filter(|v| !v.declared).collect();
    let orphaned_size: u64 = orphaned.iter().filter_map(|v| v.size).sum();
    // Measured, images and build cache are one row: Docker's figures for each
    // share layers.
    let docker_rows = match s.images_and_cache() {
        Some(both) => vec![(
            "images+cache",
            both,
            format!(
                "measured; Docker counts {} images and {} build cache ({} unused), sharing layers",
                bytes(s.images),
                bytes(s.build_cache),
                bytes(s.build_cache_reclaimable)
            ),
        )],
        None => vec![
            (
                "images",
                s.images,
                "apps' images, and those kept for rollbacks".to_string(),
            ),
            (
                "build cache",
                s.build_cache,
                format!("{} unused for now", bytes(s.build_cache_reclaimable)),
            ),
        ],
    };
    let rows = docker_rows.into_iter().chain([
        (
            "volumes",
            s.volumes_size,
            if orphaned.is_empty() {
                "apps' data".to_string()
            } else {
                format!(
                    "{} orphaned ({}): facade volume ls",
                    orphaned.len(),
                    bytes(orphaned_size)
                )
            },
        ),
        (
            "containers",
            s.containers,
            "written inside containers, lost on redeploy".to_string(),
        ),
        (
            "facade",
            s.facade_files,
            "static sites, certificates, builds; database, uploads, backups".to_string(),
        ),
        (
            "other",
            s.other(),
            "the system and other programs".to_string(),
        ),
        (
            "free",
            s.disk_total.saturating_sub(s.disk_used),
            String::new(),
        ),
    ]);
    table(
        &["", "SIZE", "%", ""],
        rows.map(|(what, size, note)| {
            vec![
                what.to_string(),
                bytes(size),
                format!("{} %", percent(size)),
                note,
            ]
        }),
    );
    if let Some(c) = &s.last_cleanup {
        println!(
            "last {}cleanup {}, freed {} (facade node prune {} [--aggressive])",
            if c.aggressive { "deep " } else { "" },
            output::ago(c.at),
            bytes(c.freed),
            node.name
        );
    }
    let mut apps: Vec<(&AppView, &AppDiskView)> = apps
        .iter()
        .filter_map(|a| Some((a, a.disk.as_ref()?)))
        .collect();
    if apps.is_empty() {
        return Ok(());
    }
    apps.sort_by_key(|(_, d)| std::cmp::Reverse(d.total()));
    println!();
    table(
        &["APP", "TOTAL", "IMAGES", "VOLUMES", "WRITTEN", "FILES"],
        apps.iter().map(|(a, d)| {
            vec![
                a.spec.name.clone(),
                bytes(d.total()),
                if d.rollback_images > 0 {
                    format!("{} (+{} rollback)", bytes(d.images), d.rollback_images)
                } else {
                    bytes(d.images)
                },
                bytes(d.volumes),
                if d.written >= WRITTEN_WARNING {
                    format!("{} (use a volume)", bytes(d.written))
                } else {
                    bytes(d.written)
                },
                bytes(d.files),
            ]
        }),
    );
    Ok(())
}

fn print_join(created: &CreatedNode) {
    println!("On the new machine (with Docker installed), run:\n");
    println!("  facaded agent --join {}\n", created.join_token);
    println!("or install it as a service with deploy/install.sh agent <token>.");
}

async fn app(api: &Api, cmd: AppCommand, json: bool) -> anyhow::Result<()> {
    match cmd {
        AppCommand::List => {
            let apps: Vec<AppView> = api.get("/api/apps").await?;
            if json {
                return print_json(&apps);
            }
            print_apps(&apps);
        }
        AppCommand::Show { name } => {
            let view: AppView = api.get(&format!("/api/apps/{name}")).await?;
            if json {
                return print_json(&view);
            }
            print_app_detail(&view);
        }
        AppCommand::Create { name, spec: args } => {
            let uploaded = match &args.executable {
                Some(path) => Some(upload(api, path).await?),
                None => None,
            };
            let mut source = args
                .source(None, uploaded.as_ref())?
                .context("give a source: --git <url>, --executable <file> or --image <ref>")?;
            let mut http_port = None;
            let mut suggested_command = vec![];
            if let Source::Git {
                repo,
                branch,
                build,
            } = &mut source
                && args.build_unspecified()
            {
                let report = scan(api, repo, branch.as_deref()).await?;
                let best = report.candidates.into_iter().next().context(
                    "nothing deployable found in the repository; pass --dockerfile, --rust-bin or --static",
                )?;
                if !json {
                    eprintln!("using {} ({})", best.title, best.detail);
                    if !best.env.is_empty() {
                        eprintln!("it reads: {}", best.env.join(", "));
                    }
                }
                for note in &best.notes {
                    eprintln!("note: {note}");
                }
                *build = best.build;
                http_port = best.http_port;
                suggested_command = best.command;
            }
            let mut spec = AppSpec {
                name,
                node: String::new(),
                source,
                http_port,
                domains: vec![],
                redirects: vec![],
                ports: vec![],
                tls_ports: vec![],
                host_network: false,
                limits: Limits::default(),
                delegate_cgroups: false,
                enabled: true,
                command: vec![],
                release_command: None,
                volumes: vec![],
                links: vec![],
                stack: None,
                healthcheck: None,
                stop_timeout_secs: None,
                stop_signal: None,
                backup: None,
            };
            args.apply(&mut spec, None)?;
            if spec.command.is_empty() && !suggested_command.is_empty() {
                if !json {
                    eprintln!(
                        "with command: {}",
                        facade_core::join_command(&suggested_command)
                    );
                }
                spec.command = suggested_command;
            }
            if spec.is_static() {
                spec.http_port = None;
            }
            if spec.node.is_empty() {
                spec.node = default_node(api).await?;
            }
            let view: AppView = api
                .json(api.request(Method::POST, "/api/apps").json(&spec))
                .await?;
            if json {
                return print_json(&view);
            }
            eprintln!(
                "created {}. Set its variables with `facade env set {0} KEY=VALUE`, then `facade deploy {0}`.",
                view.spec.name
            );
        }
        AppCommand::Update {
            name,
            spec: args,
            move_data,
        } => {
            let view: AppView = api.get(&format!("/api/apps/{name}")).await?;
            let mut spec = view.spec;
            let uploaded = match &args.executable {
                Some(path) => Some(upload(api, path).await?),
                None => None,
            };
            args.apply(&mut spec, uploaded.as_ref())?;
            let view: AppView = api
                .json(
                    api.request(Method::PUT, &format!("/api/apps/{name}"))
                        .query(&[("move_data", move_data)])
                        .json(&spec),
                )
                .await?;
            if json {
                return print_json(&view);
            }
            eprintln!("updated {name}");
        }
        AppCommand::Owner { app, user, admins } => {
            if user.is_none() && !admins {
                bail!("give a user, or --admins");
            }
            api.send(
                api.request(Method::PUT, &format!("/api/apps/{app}/owner"))
                    .json(&SetOwner {
                        owner: user.clone(),
                    }),
            )
            .await?;
            match user {
                Some(u) => eprintln!(
                    "{app} belongs to {u}: it counts against their cap and builds with their logins"
                ),
                None => eprintln!("{app} belongs to the admins"),
            }
        }
        AppCommand::Grant { app, user } => {
            api.send(api.request(Method::PUT, &format!("/api/apps/{app}/access/{user}")))
                .await?;
            eprintln!("{user} can manage {app}");
        }
        AppCommand::Revoke { app, user } => {
            api.send(api.request(Method::DELETE, &format!("/api/apps/{app}/access/{user}")))
                .await?;
            eprintln!("{user} can no longer manage {app}");
        }
        AppCommand::Remove { name, volumes, yes } => {
            let what = if volumes {
                "its container, its variables and its volumes (its data)"
            } else {
                "its container and its variables"
            };
            if !yes && !confirm(&format!("Delete {name}, {what}?"))? {
                bail!("aborted");
            }
            api.send(
                api.request(Method::DELETE, &format!("/api/apps/{name}"))
                    .query(&[("volumes", volumes)]),
            )
            .await?;
            eprintln!("{name} deleted");
            if !volumes {
                eprintln!("its volumes, if any, stay on the node (`facade volume ls`)");
            }
        }
    }
    Ok(())
}

async fn backup(api: &Api, cmd: BackupCommand, json: bool) -> anyhow::Result<()> {
    match cmd {
        BackupCommand::List { app } => {
            let mut req = api.request(Method::GET, "/api/backups");
            if let Some(app) = &app {
                req = req.query(&[("app", app)]);
            }
            let list: Vec<BackupView> = api.json(req).await?;
            if json {
                return print_json(&list);
            }
            if list.is_empty() {
                println!("no backups yet");
            }
            table(
                &["APP", "VOLUME", "ID", "TAKEN", "SIZE"],
                list.iter().map(|b| {
                    vec![
                        b.app.clone(),
                        b.volume.clone(),
                        b.id.to_string(),
                        output::date(b.id),
                        bytes(b.size),
                    ]
                }),
            );
        }
        BackupCommand::Run { app } => {
            let report: BackupReport = api
                .json(
                    api.request(Method::POST, "/api/backups")
                        .json(&BackupRequest { app }),
                )
                .await?;
            if json {
                return print_json(&report);
            }
            for b in &report.done {
                println!(
                    "saved {}/{} ({}), id {}",
                    b.app,
                    b.volume,
                    bytes(b.size),
                    b.id
                );
            }
            for f in &report.failed {
                println!("FAILED {f}");
            }
            if report.done.is_empty() && report.failed.is_empty() {
                println!("nothing to back up (no deployed app has volumes)");
            }
            if !report.failed.is_empty() {
                bail!("{} backup(s) failed", report.failed.len());
            }
        }
        BackupCommand::Get {
            app,
            volume,
            id,
            output,
        } => {
            let path =
                output.unwrap_or_else(|| PathBuf::from(format!("{app}-{volume}-{id}.tar.gz")));
            let resp = api
                .send(api.request(Method::GET, &format!("/api/backups/{app}/{volume}/{id}")))
                .await?;
            let size = download(resp, &path).await?;
            eprintln!("saved {} ({})", path.display(), bytes(size));
        }
        BackupCommand::Restore {
            app,
            volume,
            id,
            yes,
        } => {
            if !yes
                && !confirm(&format!(
                    "Replace everything in {app}/{volume} with backup {id} ({})?",
                    output::date(id)
                ))?
            {
                bail!("aborted");
            }
            api.send(api.request(
                Method::POST,
                &format!("/api/backups/{app}/{volume}/{id}/restore"),
            ))
            .await?;
            eprintln!("{app}/{volume} restored from backup {id}");
        }
        BackupCommand::Server { output } => {
            let path = output.unwrap_or_else(|| {
                PathBuf::from(format!("facade-server-{}.tar.gz", facade_core::now()))
            });
            let resp = api
                .send(api.request(Method::GET, "/api/server-backup"))
                .await?;
            let size = download(resp, &path).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            eprintln!(
                "saved {} ({}): the database and the key to its secrets, keep it private",
                path.display(),
                bytes(size)
            );
        }
    }
    Ok(())
}

async fn user(api: &Api, cmd: UserCommand, json: bool) -> anyhow::Result<()> {
    let print_password = |p: &OneTimePassword| {
        println!("{}'s one-time password: {}", p.username, p.password);
        println!("they log in with it (web UI or `facade login`) and must then choose their own");
    };
    match cmd {
        UserCommand::List => {
            let users: Vec<UserView> = api.get("/api/users").await?;
            if json {
                return print_json(&users);
            }
            table(
                &["USER", "ROLE", "MEMORY", "CPUS", "APPS"],
                users.iter().map(|u| {
                    let cap = |used: String, cap: Option<String>| match cap {
                        Some(cap) => format!("{used} / {cap}"),
                        None => "-".into(),
                    };
                    let mut apps = u.apps.clone();
                    apps.extend(u.shared_apps.iter().map(|a| format!("{a} (shared)")));
                    vec![
                        format!(
                            "{}{}",
                            u.username,
                            if u.must_change_password {
                                " (new password pending)"
                            } else {
                                ""
                            }
                        ),
                        u.role.clone(),
                        cap(
                            format!("{} MB", u.memory_allocated_mb),
                            u.memory_cap_mb.map(|c| format!("{c} MB")),
                        ),
                        cap(
                            u.cpus_allocated.to_string(),
                            u.cpu_cap.map(|c| c.to_string()),
                        ),
                        apps.join(", "),
                    ]
                }),
            );
        }
        UserCommand::Add {
            name,
            admin,
            memory,
            cpus,
        } => {
            let created: OneTimePassword = api
                .json(api.request(Method::POST, "/api/users").json(&CreateUser {
                    username: name,
                    role: if admin { "admin" } else { "member" }.into(),
                    memory_cap_mb: memory,
                    cpu_cap: cpus,
                }))
                .await?;
            if json {
                return print_json(&created);
            }
            print_password(&created);
        }
        UserCommand::Set {
            name,
            role,
            memory,
            cpus,
        } => {
            let users: Vec<UserView> = api.get("/api/users").await?;
            let current = users
                .into_iter()
                .find(|u| u.username == name)
                .with_context(|| format!("no user named {name}"))?;
            let update = UpdateUser {
                role: role.unwrap_or(current.role),
                memory_cap_mb: match memory {
                    Some(0) => None,
                    Some(m) => Some(m),
                    None => current.memory_cap_mb,
                },
                cpu_cap: match cpus {
                    Some(0.0) => None,
                    Some(c) => Some(c),
                    None => current.cpu_cap,
                },
            };
            let view: UserView = api
                .json(
                    api.request(Method::PUT, &format!("/api/users/{name}"))
                        .json(&update),
                )
                .await?;
            if json {
                return print_json(&view);
            }
            eprintln!("updated {name}");
        }
        UserCommand::ResetPassword { name } => {
            let reset: OneTimePassword = api
                .json(api.request(Method::POST, &format!("/api/users/{name}/password")))
                .await?;
            if json {
                return print_json(&reset);
            }
            print_password(&reset);
        }
        UserCommand::Remove { name, yes } => {
            if !yes && !confirm(&format!("Delete {name}? Their apps go back to the admins."))? {
                bail!("aborted");
            }
            api.send(api.request(Method::DELETE, &format!("/api/users/{name}")))
                .await?;
            eprintln!("{name} deleted");
        }
    }
    Ok(())
}

/// Streams a response body to a file, returning its size.
async fn download(resp: reqwest::Response, path: &std::path::Path) -> anyhow::Result<u64> {
    use tokio::io::AsyncWriteExt;
    let partial = path.with_extension("partial");
    let mut file = tokio::fs::File::create(&partial)
        .await
        .with_context(|| format!("creating {}", partial.display()))?;
    let mut body = resp.bytes_stream();
    let mut size = 0;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.context("download interrupted")?;
        size += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    if size == 0 {
        let _ = tokio::fs::remove_file(&partial).await;
        bail!("the server sent nothing (see the node's log)");
    }
    tokio::fs::rename(&partial, path).await?;
    Ok(size)
}

async fn registry(api: &Api, cmd: RegistryCommand, json: bool) -> anyhow::Result<()> {
    match cmd {
        RegistryCommand::List => {
            let list: Vec<RegistryLoginView> = api.get("/api/registries").await?;
            if json {
                return print_json(&list);
            }
            if list.is_empty() {
                println!("no logins; add one with `facade registry login ghcr.io --user <you>`");
            }
            table(
                &["HOST", "USER", "OWNER"],
                list.iter().map(|r| {
                    vec![
                        r.host.clone(),
                        r.username.clone(),
                        r.owner.clone().unwrap_or_else(|| "admins".into()),
                    ]
                }),
            );
        }
        RegistryCommand::Login {
            host,
            user,
            token_stdin,
        } => {
            let secret = if token_stdin {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s.trim().to_string()
            } else {
                rpassword::prompt_password(format!("token for {user}@{host}: "))?
            };
            api.send(
                api.request(Method::PUT, &format!("/api/registries/{host}"))
                    .json(&SetRegistryLogin {
                        username: user.clone(),
                        secret,
                    }),
            )
            .await?;
            eprintln!("saved the login for {host} (as {user})");
        }
        RegistryCommand::Remove { host } => {
            api.send(api.request(Method::DELETE, &format!("/api/registries/{host}")))
                .await?;
            eprintln!("removed the login for {host}");
        }
    }
    Ok(())
}

/// Uploads an executable, returning what the server made of it.
async fn upload(api: &Api, path: &std::path::Path) -> anyhow::Result<ArtifactView> {
    let file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    let size = file.metadata().await?.len();
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "app".into());
    eprintln!("uploading {filename} ({})", bytes(size));
    let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
    let artifact: ArtifactView = api
        .json(
            api.request(Method::POST, "/api/artifacts")
                .query(&[("filename", &filename)])
                .header("content-length", size)
                .body(body),
        )
        .await?;
    eprintln!("uploaded: {} executable", artifact.arch);
    Ok(artifact)
}

async fn scan(api: &Api, repo: &str, branch: Option<&str>) -> anyhow::Result<InspectReport> {
    eprintln!("scanning {repo}…");
    api.json(
        api.request(Method::POST, "/api/inspect")
            .json(&InspectRequest {
                repo: repo.to_string(),
                branch: branch.map(str::to_string),
            }),
    )
    .await
}

async fn default_node(api: &Api) -> anyhow::Result<String> {
    let nodes: Vec<NodeView> = api.get("/api/nodes").await?;
    nodes
        .into_iter()
        .find(|n| n.local)
        .map(|n| n.name)
        .context("no local node; pass --node")
}

/// Variables of an app (or a stack's service), or of a stack (what its compose
/// file reads, like `${DB_PASSWORD}`).
async fn env_path(api: &Api, target: &str) -> String {
    if stack_of(api, target).await.is_some() {
        format!("/api/stacks/{target}/env")
    } else {
        format!("/api/apps/{target}/env")
    }
}

async fn env(api: &Api, cmd: EnvCommand, json: bool) -> anyhow::Result<()> {
    match cmd {
        EnvCommand::List { app, reveal } => {
            let vars: Vec<EnvView> = api
                .json(
                    api.request(Method::GET, &env_path(api, &app).await)
                        .query(&[("reveal", reveal)]),
                )
                .await?;
            if json {
                return print_json(&vars);
            }
            table(
                &["KEY", "VALUE", "SENSITIVE", "BUILD"],
                vars.iter().map(|v| {
                    vec![
                        v.key.clone(),
                        v.value.clone().unwrap_or_else(|| "••••••".into()),
                        if v.sensitive { "yes" } else { "no" }.into(),
                        if v.build { "yes" } else { "no" }.into(),
                    ]
                }),
            );
        }
        EnvCommand::Set {
            app,
            vars,
            public,
            build,
            no_build,
        } => {
            let path = env_path(api, &app).await;
            let stack = path.starts_with("/api/stacks/");
            if stack && (build || no_build) {
                bail!(
                    "stack variables are not handed to builds; set --build on a service's own variable (facade env set {app}.<service> …)"
                );
            }
            let build = (build || no_build).then_some(build);
            let mut set = BTreeMap::new();
            for var in vars {
                let (key, value) = match var.split_once('=') {
                    Some((k, v)) => (k.to_string(), v.to_string()),
                    None => {
                        let value = if std::io::stdin().is_terminal() {
                            rpassword::prompt_password(format!("{var}: "))?
                        } else {
                            let mut s = String::new();
                            std::io::stdin().read_to_string(&mut s)?;
                            s.trim_end_matches(['\n', '\r']).to_string()
                        };
                        (var, value)
                    }
                };
                set.insert(
                    key,
                    SetEnv {
                        value,
                        sensitive: !public,
                        build,
                    },
                );
            }
            let keys: Vec<String> = set.keys().cloned().collect();
            let resp = api
                .send(
                    api.request(Method::PATCH, &path)
                        .json(&EnvPatch { set, unset: vec![] }),
                )
                .await?;
            eprintln!("set {} on {app}", keys.join(", "));
            if stack {
                eprintln!("stack variables apply from its next deploy: facade deploy {app}");
            } else {
                print_rebuilds(resp).await;
            }
        }
        EnvCommand::Unset { app, keys } => {
            let path = env_path(api, &app).await;
            let resp = api
                .send(api.request(Method::PATCH, &path).json(&EnvPatch {
                    set: BTreeMap::new(),
                    unset: keys.clone(),
                }))
                .await?;
            eprintln!("unset {} on {app}", keys.join(", "));
            if !path.starts_with("/api/stacks/") {
                print_rebuilds(resp).await;
            }
        }
    }
    Ok(())
}

/// The rebuilds an env change started, from the answer to its `PATCH`.
async fn print_rebuilds(resp: reqwest::Response) {
    let rebuilding: Vec<DeploymentView> = resp.json().await.unwrap_or_default();
    for d in rebuilding {
        eprintln!(
            "rebuilding {} for its build variables: facade deployments {} --id {}",
            d.app, d.app, d.id
        );
    }
}

fn print_nodes(nodes: &[NodeView]) {
    print_node_table(nodes);
    for n in nodes {
        for e in &n.ingress_errors {
            println!("warning: node {}: {e}", n.name);
        }
    }
}

fn print_node_table(nodes: &[NodeView]) {
    table(
        &[
            "NODE",
            "STATUS",
            "PUBLIC IP",
            "CPU",
            "MEMORY",
            "DISK",
            "IMAGES / BUILD CACHE",
            "SEEN",
        ],
        nodes.iter().map(|n| {
            let s = n.stats.as_ref();
            vec![
                format!("{}{}", n.name, if n.local { " (server)" } else { "" }),
                if n.connected { "online" } else { "OFFLINE" }.into(),
                format!(
                    "{}{}",
                    n.public_ip.clone().unwrap_or_else(|| "?".into()),
                    if n.public_ip_manual { " (set)" } else { "" }
                ),
                s.map(|s| format!("{:.0}% of {}", s.cpu_percent, s.cpus))
                    .unwrap_or_default(),
                s.map(|s| format!("{} / {}", bytes(s.mem_used), bytes(s.mem_total)))
                    .unwrap_or_default(),
                s.map(|s| format!("{} / {}", bytes(s.disk_used), bytes(s.disk_total)))
                    .unwrap_or_default(),
                n.storage
                    .as_ref()
                    .map(|st| format!("{} / {}", bytes(st.images), bytes(st.build_cache)))
                    .unwrap_or_default(),
                n.last_seen
                    .map(output::ago)
                    .unwrap_or_else(|| "never".into()),
            ]
        }),
    );
}

fn print_apps(apps: &[AppView]) {
    table(
        &["APP", "NODE", "STATE", "CPU", "MEMORY", "DOMAINS"],
        apps.iter().map(|a| {
            let s = a.stats.as_ref();
            vec![
                a.spec.name.clone(),
                a.spec.node.clone(),
                a.state.to_string(),
                s.map(|s| format!("{:.1}%", s.cpu_percent))
                    .unwrap_or_default(),
                s.filter(|s| s.mem_used > 0)
                    .map(|s| bytes(s.mem_used))
                    .unwrap_or_default(),
                a.spec.domains.join(", "),
            ]
        }),
    );
}

fn print_app_detail(view: &AppView) {
    let spec = &view.spec;
    if let Some(owner) = &view.owner {
        println!("owner:     {owner}");
    }
    if !view.access.is_empty() {
        println!("access:    {}", view.access.join(", "));
    }
    for domain in &spec.redirects {
        println!(
            "redirect:  {domain} → {}",
            spec.domains.first().map(String::as_str).unwrap_or("?")
        );
    }
    println!("name:      {}", spec.name);
    println!("node:      {}", spec.node);
    println!(
        "state:     {}{}",
        view.state,
        if spec.enabled { "" } else { " (disabled)" }
    );
    if let Some(message) = &view.message {
        println!("message:   {message}");
    }
    println!("revision:  {}", view.revision);
    if let Some(commit) = &view.commit {
        println!("commit:    {commit}");
    }
    match &spec.source {
        Source::Image { image } => println!("source:    image {image}"),
        Source::Executable {
            filename,
            arch,
            args,
            ..
        } => println!(
            "source:    executable {filename} ({arch}){}",
            if args.is_empty() {
                String::new()
            } else {
                format!(" {}", args.join(" "))
            }
        ),
        Source::Git {
            repo,
            branch,
            build,
        } => {
            println!(
                "source:    git {repo} ({})",
                branch.as_deref().unwrap_or("default branch")
            );
            match build {
                Build::Dockerfile {
                    dockerfile,
                    context,
                } => println!("build:     dockerfile {dockerfile} in {context}"),
                Build::Rust {
                    package,
                    bin,
                    features,
                    no_default_features,
                } => println!(
                    "build:     cargo --bin {bin}{}{}{}",
                    package
                        .as_ref()
                        .map(|p| format!(" -p {p}"))
                        .unwrap_or_default(),
                    if *no_default_features {
                        " --no-default-features"
                    } else {
                        ""
                    },
                    if features.is_empty() {
                        String::new()
                    } else {
                        format!(" --features {}", features.join(","))
                    }
                ),
                Build::Static {
                    dir,
                    dockerfile: None,
                    ..
                } => println!("build:     static files from {dir}"),
                Build::Static {
                    dir,
                    dockerfile: Some(dockerfile),
                    context,
                } => println!(
                    "build:     static files from {dir} of what {dockerfile} builds (context {})",
                    context.as_deref().unwrap_or(".")
                ),
                Build::Trunk { dir } => {
                    println!("build:     Rust web app in {dir}, built with trunk")
                }
                Build::Python { version, entry } => println!(
                    "build:     python {} {entry}",
                    version.as_deref().unwrap_or(facade_core::DEFAULT_PYTHON)
                ),
            }
        }
    }
    if let Some(port) = spec.http_port {
        println!("http port: {port}");
    }
    for domain in &spec.domains {
        println!("domain:    {domain}");
    }
    for port in &spec.ports {
        println!("publish:   {port}");
    }
    for port in &spec.tls_ports {
        println!("tls port:  {port}");
    }
    if spec.host_network {
        println!("network:   host");
    }
    if !spec.command.is_empty() {
        println!("command:   {}", facade_core::join_command(&spec.command));
    }
    if let Some(release) = &spec.release_command {
        println!("release:   {release}");
    }
    for volume in &spec.volumes {
        println!("volume:    {volume}");
    }
    for link in &spec.links {
        println!("link:      {} (reachable as {})", link.app, link.host());
    }
    if let Some(check) = &spec.healthcheck {
        println!("health:    {}", check.command());
    }
    if let Some(stack) = &spec.stack {
        println!(
            "stack:     service {} of {} (reached as {})",
            spec.service().unwrap_or_default(),
            stack.stack,
            stack.host
        );
    }
    if let Some(mb) = spec.limits.memory_mb {
        println!("memory:    {mb} MB max");
    }
    if let Some(cpus) = spec.limits.cpus {
        println!("cpus:      {cpus} max");
    }
    if let Some(pids) = spec.limits.pids {
        println!("pids:      {pids} max");
    }
    if spec.delegate_cgroups {
        println!("cgroups:   delegated (/sys/fs/cgroup is writable)");
    }
    if let Some(b) = &spec.backup {
        if let Some(before) = &b.before {
            println!("before backups: {before}");
        }
        if let Some(after) = &b.after {
            println!("after backups:  {after}");
        }
    }
    if spec.stop_timeout_secs.is_some() || spec.stop_signal.is_some() {
        println!(
            "stop:      {}, {}s to shut down",
            spec.stop_signal.as_deref().unwrap_or("image's signal"),
            spec.stop_wait_secs()
        );
    }
    if let Some(s) = &view.stats {
        println!(
            "usage:     {:.1}% CPU, {} memory, {} restarts",
            s.cpu_percent,
            bytes(s.mem_used),
            s.restarts
        );
    }
    if let Some(d) = view.disk.as_ref().filter(|d| d.total() > 0) {
        let mut parts = vec![format!("{} images", bytes(d.images))];
        if d.rollback_images > 0 {
            parts[0] += &format!(" (with {} for rollbacks)", d.rollback_images);
        }
        if d.volumes > 0 {
            parts.push(format!("{} volumes", bytes(d.volumes)));
        }
        if d.files > 0 {
            parts.push(format!("{} files", bytes(d.files)));
        }
        if d.written > 0 {
            parts.push(format!("{} written in the container", bytes(d.written)));
        }
        println!("disk:      {} ({})", bytes(d.total()), parts.join(", "));
        if d.written >= WRITTEN_WARNING {
            println!(
                "           what the container writes is lost on redeploy: keep it in a volume (--volume name:/path)"
            );
        }
    }
}
