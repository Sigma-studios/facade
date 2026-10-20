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
use facade_core::{AppSpec, Build, Limits, PortMap, Source};
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
    },
    #[command(alias = "rm")]
    Remove {
        name: String,
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
    #[arg(long, conflicts_with = "git")]
    image: Option<String>,
    /// Build from a git repository.
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
    /// Port the app listens on for HTTP.
    #[arg(long)]
    port: Option<u16>,
    /// Route this domain to the app (repeatable).
    #[arg(long = "domain")]
    domains: Vec<String>,
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
    fn source(&self, current: Option<&Source>) -> anyhow::Result<Option<Source>> {
        if let Some(image) = &self.image {
            return Ok(Some(Source::Image {
                image: image.clone(),
            }));
        }
        let touches_git = self.git.is_some()
            || self.branch.is_some()
            || self.dockerfile.is_some()
            || self.context.is_some()
            || self.rust_bin.is_some()
            || self.static_dir.is_some();
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

    fn apply(&self, spec: &mut AppSpec) -> anyhow::Result<()> {
        if let Some(node) = &self.node {
            spec.node = node.clone();
        }
        if let Some(source) = self.source(Some(&spec.source))? {
            spec.source = source;
        }
        if let Some(port) = self.port {
            spec.http_port = (port != 0).then_some(port);
        }
        if !self.domains.is_empty() {
            spec.domains = self.domains.clone();
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
    Saved {
        server,
        fingerprint,
        token,
    }
    .save()?;
    eprintln!("logged in (saved to {})", client::config_path()?.display());
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
                    "{domain} → {name}. Point its DNS at the node running the server; HTTPS is set up automatically once it resolves."
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
            let source = args.source(None)?.context(
                "give a source: --image <ref>, or --git <url> [--rust-bin <bin> | --static <dir> | --dockerfile <path>]",
            )?;
            let mut spec = AppSpec {
                name,
                node: String::new(),
                source,
                http_port: None,
                domains: vec![],
                ports: vec![],
                host_network: false,
                limits: Limits::default(),
                enabled: true,
            };
            args.apply(&mut spec)?;
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
        AppCommand::Update { name, spec: args } => {
            let view: AppView = api.get(&format!("/api/apps/{name}")).await?;
            let mut spec = view.spec;
            args.apply(&mut spec)?;
            let view: AppView = api
                .json(
                    api.request(Method::PUT, &format!("/api/apps/{name}"))
                        .json(&spec),
                )
                .await?;
            if json {
                return print_json(&view);
            }
            eprintln!("updated {name}");
        }
        AppCommand::Remove { name, yes } => {
            if !yes && !confirm(&format!("Delete {name}, its container and its variables?"))? {
                bail!("aborted");
            }
            api.send(api.request(Method::DELETE, &format!("/api/apps/{name}")))
                .await?;
            eprintln!("{name} deleted");
        }
    }
    Ok(())
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
    table(
        &["NODE", "STATUS", "HOST", "CPU", "MEMORY", "DISK", "SEEN"],
        nodes.iter().map(|n| {
            let s = n.stats.as_ref();
            vec![
                format!("{}{}", n.name, if n.local { " (server)" } else { "" }),
                if n.connected { "online" } else { "OFFLINE" }.into(),
                n.hostname.clone().unwrap_or_default(),
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
    println!(
        "state:     {}{}",
        view.state,
        if spec.enabled { "" } else { " (disabled)" }
    );
    if let Some(message) = &view.message {
        println!("message:   {message}");
    }
    println!("revision:  {}", view.revision);
    match &spec.source {
        Source::Image { image } => println!("source:    image {image}"),
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
