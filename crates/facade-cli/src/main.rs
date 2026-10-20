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
use facade_core::proto::{CredentialKind, Listener};
use facade_core::{AppSpec, Build, Limits, PortMap, Source, Volume};
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
    /// How this server is reached: its web UI domain, ACME contact, direct API port.
    Server {
        #[command(subcommand)]
        action: Option<ServerCommand>,
    },
    #[command(subcommand)]
    Node(NodeCommand),
    #[command(subcommand)]
    App(AppCommand),
    #[command(subcommand)]
    Env(EnvCommand),
    #[command(subcommand)]
    Domain(DomainCommand),
    /// App volumes: list, download, replace with an archive.
    #[command(subcommand)]
    Volume(VolumeCommand),
    /// Volume backups (taken on a schedule; see [backups] in server.toml).
    #[command(subcommand)]
    Backup(BackupCommand),
    /// Logins for private registries (image pulls, Dockerfile FROM) and git hosts.
    #[command(subcommand)]
    Auth(AuthCommand),
    /// Accounts: admins manage everything, members only their apps (admins only).
    #[command(subcommand)]
    User(UserCommand),
    /// Who you are logged in as, and your resource cap.
    Whoami,
    /// Every routed domain: where it must point, whether it does, and its certificate.
    Domains,
    /// Show or set the DNS provider facade manages records with.
    #[command(subcommand)]
    Dns(DnsCommand),
    /// Build (if needed) and (re)start an app, following the output.
    Deploy {
        app: String,
        /// Return immediately instead of following the build.
        #[arg(long)]
        detach: bool,
    },
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
    /// Fetch an app's branch and list the commits a deploy would bring.
    Updates {
        app: String,
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
    /// Set the public IP domains of this node's apps point to (`auto` to detect it).
    Ip { name: String, ip: String },
    /// Every listening port on the node, and which program or app holds it.
    Ports { name: String },
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
enum VolumeCommand {
    /// Every volume, with its node, size and last backup.
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
    /// Delete the volumes an app left on a node it no longer runs on.
    #[command(alias = "rm")]
    Remove {
        app: String,
        #[arg(long)]
        node: String,
        #[arg(long, short)]
        yes: bool,
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
enum AuthCommand {
    /// Saved logins (secrets are never shown).
    #[command(alias = "ls")]
    List,
    /// Log in to a container registry, e.g. `facade auth registry ghcr.io --user me`
    /// with a token that can read packages. Used for image pulls and Dockerfile FROM.
    Registry {
        host: String,
        #[arg(long, short)]
        user: String,
        /// Read the token from stdin instead of prompting.
        #[arg(long)]
        token_stdin: bool,
    },
    /// Log in to a git host for HTTPS clones, e.g. `facade auth git github.com --user me`
    /// with a token that can read the repositories.
    Git {
        host: String,
        #[arg(long, short)]
        user: String,
        #[arg(long)]
        token_stdin: bool,
    },
    #[command(alias = "rm")]
    Remove {
        /// `registry` or `git`
        kind: String,
        host: String,
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
    #[command(alias = "rm")]
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
    Remove {
        name: String,
        /// Also delete its volumes. Without this they stay on the node.
        #[arg(long)]
        delete_data: bool,
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
    /// Argument for the program (repeatable; replaces the current ones). With a
    /// Dockerfile or image they replace its CMD.
    #[arg(long = "arg", allow_hyphen_values = true)]
    args: Vec<String>,
    /// Remove every argument.
    #[arg(long, conflicts_with = "args")]
    no_args: bool,
    /// Build from a git repository. Without --dockerfile, --rust-bin or --static, the
    /// repository is scanned and the best match is used (see `facade scan`).
    #[arg(long)]
    git: Option<String>,
    #[arg(long)]
    branch: Option<String>,
    /// Dockerfile path in the repository (default build kind).
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
    /// Serve this repository directory as a static site.
    #[arg(long = "static", conflicts_with = "dockerfile")]
    static_dir: Option<String>,
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
    /// Keep a directory across deploys: `data:/data` (repeatable; replaces the list).
    #[arg(long = "volume", short = 'v')]
    volumes: Vec<Volume>,
    /// Remove every volume from the app (their data stays on the node).
    #[arg(long, conflicts_with = "volumes")]
    no_volumes: bool,
    /// Publish raw ports: `3478:3478/udp`, `49160-49260:49160-49260/udp` (repeatable).
    #[arg(long = "publish", short = 'p')]
    ports: Vec<PortMap>,
    /// Share the node's network (e.g. for TURN relays).
    #[arg(long)]
    host_network: Option<bool>,
    /// Memory limit in MB (0 removes it).
    #[arg(long)]
    memory: Option<u64>,
    /// CPU limit in cores (0 removes it).
    #[arg(long)]
    cpus: Option<f64>,
}

impl SpecArgs {
    /// The source these flags describe, given the current one and an executable
    /// uploaded for `--executable`.
    fn source(
        &self,
        current: Option<&Source>,
        uploaded: Option<&ArtifactView>,
    ) -> anyhow::Result<Option<Source>> {
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
            }));
        }
        let touches_git = self.git.is_some()
            || self.branch.is_some()
            || self.dockerfile.is_some()
            || self.context.is_some()
            || self.rust_bin.is_some()
            || self.static_dir.is_some()
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
        } else if let Some(dir) = &self.static_dir {
            Build::Static { dir: dir.clone() }
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
        if !self.args.is_empty() {
            spec.args = self.args.clone();
        }
        if self.no_args {
            spec.args.clear();
        }
        if !self.volumes.is_empty() {
            spec.volumes = self.volumes.clone();
        }
        if self.no_volumes {
            spec.volumes.clear();
        }
        if !self.ports.is_empty() {
            spec.ports = self.ports.clone();
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
    /// sensitive unless --public. Restarts the app once.
    Set {
        app: String,
        #[arg(required = true)]
        vars: Vec<String>,
        /// Mark as non-sensitive: shown in listings and included in exports.
        #[arg(long)]
        public: bool,
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
        Command::Server { action } => {
            let settings: ServerSettings = match action {
                None => api.get("/api/server-settings").await?,
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
        Command::Node(cmd) => node(api, cmd, json).await?,
        Command::App(cmd) => app(api, cmd, json).await?,
        Command::Env(cmd) => env(api, cmd, json).await?,
        Command::Volume(cmd) => volume(api, cmd, json).await?,
        Command::Backup(cmd) => backup(api, cmd, json).await?,
        Command::Auth(cmd) => auth(api, cmd, json).await?,
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
        Command::Deploy { app, detach } => {
            let deployment: DeploymentView = api
                .json(api.request(Method::POST, &format!("/api/apps/{app}/deploy")))
                .await?;
            if json && detach {
                return print_json(&deployment);
            }
            if detach {
                eprintln!("deployment {} started", deployment.id);
                return Ok(());
            }
            follow_deployment(api, deployment.id, json).await?;
        }
        Command::Restart { app } => {
            api.send(api.request(Method::POST, &format!("/api/apps/{app}/restart")))
                .await?;
            eprintln!("{app} restarted");
        }
        Command::Start { app } => {
            api.send(api.request(Method::POST, &format!("/api/apps/{app}/start")))
                .await?;
            eprintln!("{app} enabled");
        }
        Command::Stop { app } => {
            api.send(api.request(Method::POST, &format!("/api/apps/{app}/stop")))
                .await?;
            eprintln!("{app} stopped");
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
                    "-- {:?}{}",
                    d.status,
                    d.error.map(|e| format!(": {e}")).unwrap_or_default()
                );
                return Ok(());
            }
            let list: Vec<DeploymentView> =
                api.get(&format!("/api/apps/{app}/deployments")).await?;
            if json {
                return print_json(&list);
            }
            table(
                &["ID", "REV", "STATUS", "STARTED", "ERROR"],
                list.iter().map(|d| {
                    vec![
                        d.id.to_string(),
                        d.revision.to_string(),
                        format!("{:?}", d.status).to_lowercase(),
                        output::ago(d.started_at),
                        d.error
                            .clone()
                            .unwrap_or_default()
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_string(),
                    ]
                }),
            );
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
        Command::Scan { repo, branch } => {
            let report = scan(api, &repo, branch.as_deref()).await?;
            if json {
                return print_json(&report);
            }
            println!("commit {}", report.commit);
            if report.candidates.is_empty() {
                println!("nothing deployable found");
            }
            for (i, c) in report.candidates.iter().enumerate() {
                println!("{}. {}\n   {}", i + 1, c.title, c.detail);
                if !c.env.is_empty() {
                    println!("   reads {}", c.env.join(", "));
                }
                if !c.args.is_empty() {
                    println!("   suggested args: {}", c.args.join(" "));
                }
                for note in &c.notes {
                    println!("   note: {note}");
                }
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

async fn follow_deployment(api: &Api, id: uuid::Uuid, json: bool) -> anyhow::Result<()> {
    let mut printed = 0;
    loop {
        let d: DeploymentView = api.get(&format!("/api/deployments/{id}")).await?;
        if !json && d.log.len() > printed {
            print!("{}", &d.log[printed..]);
            std::io::stdout().flush()?;
            printed = d.log.len();
        }
        match d.status {
            DeploymentStatus::Running => tokio::time::sleep(Duration::from_millis(700)).await,
            DeploymentStatus::Succeeded => {
                if json {
                    return print_json(&d);
                }
                eprintln!("deployed {} (revision {})", d.app, d.revision);
                return Ok(());
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
            let mut suggested_args = vec![];
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
                suggested_args = best.args;
            }
            let mut spec = AppSpec {
                name,
                node: String::new(),
                source,
                http_port,
                domains: vec![],
                redirects: vec![],
                ports: vec![],
                host_network: false,
                args: vec![],
                volumes: vec![],
                limits: Limits::default(),
                enabled: true,
            };
            args.apply(&mut spec, None)?;
            if spec.args.is_empty() && !args.no_args && !suggested_args.is_empty() {
                if !json {
                    eprintln!("with args: {}", suggested_args.join(" "));
                }
                spec.args = suggested_args;
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
        AppCommand::Remove {
            name,
            delete_data,
            yes,
        } => {
            let question = if delete_data {
                format!("Delete {name}, its container, its variables AND ITS VOLUMES' DATA?")
            } else {
                format!("Delete {name}, its container and its variables (its volumes are kept)?")
            };
            if !yes && !confirm(&question)? {
                bail!("aborted");
            }
            let view: AppView = api.get(&format!("/api/apps/{name}")).await?;
            api.send(
                api.request(Method::DELETE, &format!("/api/apps/{name}"))
                    .query(&[("delete_data", delete_data)]),
            )
            .await?;
            eprintln!("{name} deleted");
            if !delete_data && !view.spec.volumes.is_empty() {
                eprintln!(
                    "its volumes stay on {}; delete them with `facade volume rm {name} --node {0}`",
                    view.spec.node
                );
            }
        }
    }
    Ok(())
}

async fn volume(api: &Api, cmd: VolumeCommand, json: bool) -> anyhow::Result<()> {
    match cmd {
        VolumeCommand::List => {
            let volumes: Vec<VolumeView> = api.get("/api/volumes").await?;
            if json {
                return print_json(&volumes);
            }
            if volumes.is_empty() {
                println!(
                    "no volumes; give an app one with `facade app update <app> --volume data:/data`"
                );
            }
            table(
                &["APP", "VOLUME", "PATH", "NODE", "SIZE", "LAST BACKUP"],
                volumes.iter().map(|v| {
                    vec![
                        v.app.clone(),
                        v.name.clone(),
                        v.path
                            .clone()
                            .unwrap_or_else(|| "(left over: no longer used)".into()),
                        v.node.clone(),
                        v.size.map(bytes).unwrap_or_else(|| "-".into()),
                        v.last_backup
                            .map(output::ago)
                            .unwrap_or_else(|| "never".into()),
                    ]
                }),
            );
        }
        VolumeCommand::Export {
            app,
            volume,
            output,
        } => {
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
        VolumeCommand::Import {
            app,
            volume,
            file,
            yes,
        } => {
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
        VolumeCommand::Remove { app, node, yes } => {
            if !yes && !confirm(&format!("Delete every volume of {app} on {node}?"))? {
                bail!("aborted");
            }
            api.send(api.request(Method::DELETE, &format!("/api/nodes/{node}/volumes/{app}")))
                .await?;
            eprintln!("deleted {app}'s volumes on {node}");
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

async fn auth(api: &Api, cmd: AuthCommand, json: bool) -> anyhow::Result<()> {
    let (kind, host, user, token_stdin) = match cmd {
        AuthCommand::List => {
            let list: Vec<CredentialView> = api.get("/api/credentials").await?;
            if json {
                return print_json(&list);
            }
            if list.is_empty() {
                println!("no logins; add one with `facade auth registry ghcr.io --user <you>`");
            }
            table(
                &["KIND", "HOST", "USER", "OWNER"],
                list.iter().map(|c| {
                    vec![
                        c.kind.to_string(),
                        c.host.clone(),
                        c.username.clone(),
                        c.owner.clone().unwrap_or_else(|| "admins".into()),
                    ]
                }),
            );
            return Ok(());
        }
        AuthCommand::Remove { kind, host } => {
            api.send(api.request(Method::DELETE, &format!("/api/credentials/{kind}/{host}")))
                .await?;
            eprintln!("removed the {kind} login for {host}");
            return Ok(());
        }
        AuthCommand::Registry {
            host,
            user,
            token_stdin,
        } => (CredentialKind::Registry, host, user, token_stdin),
        AuthCommand::Git {
            host,
            user,
            token_stdin,
        } => (CredentialKind::Git, host, user, token_stdin),
    };
    let secret = if token_stdin {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s.trim().to_string()
    } else {
        rpassword::prompt_password(format!("token for {user}@{host}: "))?
    };
    api.send(
        api.request(Method::PUT, &format!("/api/credentials/{kind}/{host}"))
            .json(&SetCredential {
                username: user.clone(),
                secret,
            }),
    )
    .await?;
    eprintln!("saved the {kind} login for {host} (as {user})");
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
                    Some(c) if c == 0.0 => None,
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

async fn env(api: &Api, cmd: EnvCommand, json: bool) -> anyhow::Result<()> {
    match cmd {
        EnvCommand::List { app, reveal } => {
            let vars: Vec<EnvView> = api
                .json(
                    api.request(Method::GET, &format!("/api/apps/{app}/env"))
                        .query(&[("reveal", reveal)]),
                )
                .await?;
            if json {
                return print_json(&vars);
            }
            table(
                &["KEY", "VALUE", "SENSITIVE"],
                vars.iter().map(|v| {
                    vec![
                        v.key.clone(),
                        v.value.clone().unwrap_or_else(|| "••••••".into()),
                        if v.sensitive { "yes" } else { "no" }.into(),
                    ]
                }),
            );
        }
        EnvCommand::Set { app, vars, public } => {
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
                    },
                );
            }
            let keys: Vec<String> = set.keys().cloned().collect();
            api.send(
                api.request(Method::PATCH, &format!("/api/apps/{app}/env"))
                    .json(&EnvPatch { set, unset: vec![] }),
            )
            .await?;
            eprintln!("set {} on {app}", keys.join(", "));
        }
        EnvCommand::Unset { app, keys } => {
            api.send(
                api.request(Method::PATCH, &format!("/api/apps/{app}/env"))
                    .json(&EnvPatch {
                        set: BTreeMap::new(),
                        unset: keys.clone(),
                    }),
            )
            .await?;
            eprintln!("unset {} on {app}", keys.join(", "));
        }
    }
    Ok(())
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
    println!("name:      {}", spec.name);
    println!("node:      {}", spec.node);
    if let Some(owner) = &view.owner {
        println!("owner:     {owner}");
    }
    if !view.access.is_empty() {
        println!("access:    {}", view.access.join(", "));
    }
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
        println!(
            "commit:    {} (check for newer ones with `facade updates {}`)",
            commit.get(..7).unwrap_or(commit),
            spec.name
        );
    }
    match &spec.source {
        Source::Image { image } => println!("source:    image {image}"),
        Source::Executable { filename, arch, .. } => {
            println!("source:    executable {filename} ({arch})")
        }
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
                Build::Static { dir } => println!("build:     static files from {dir}"),
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
    if !spec.args.is_empty() {
        println!("args:      {}", spec.args.join(" "));
    }
    for domain in &spec.domains {
        println!("domain:    {domain}");
    }
    for domain in &spec.redirects {
        println!(
            "redirect:  {domain} → {}",
            spec.domains.first().map(String::as_str).unwrap_or("?")
        );
    }
    for volume in &spec.volumes {
        println!("volume:    {} at {}", volume.name, volume.path);
    }
    for port in &spec.ports {
        println!("publish:   {port}");
    }
    if spec.host_network {
        println!("network:   host");
    }
    if let Some(mb) = spec.limits.memory_mb {
        println!("memory:    {mb} MB max");
    }
    if let Some(cpus) = spec.limits.cpus {
        println!("cpus:      {cpus} max");
    }
    if let Some(s) = &view.stats {
        println!(
            "usage:     {:.1}% CPU, {} memory, {} restarts",
            s.cpu_percent,
            bytes(s.mem_used),
            s.restarts
        );
    }
}
