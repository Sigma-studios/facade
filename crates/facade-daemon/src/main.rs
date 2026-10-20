//! `facaded`: the facade daemon.
//!
//! * `facaded server` — control plane: API, node hub, certificates and DNS,
//!   monitoring. Also runs an agent for its own machine.
//! * `facaded agent --join <token>` — runs and serves apps on a node, dialing the
//!   server.
//! * `facaded ingress` — a node's HTTP(S) entry point as its own process (with
//!   `ingress = "external"`, or an agent's `--ingress-control`).

mod agent;
mod config;
mod framing;
mod gitauth;
mod ingress;
mod lang;
mod selfbin;
mod server;
mod tls;
mod upgrade;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "facaded", version, about = "facade daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the control plane (and an agent for this machine).
    Server {
        #[arg(
            long,
            short,
            env = "FACADE_CONFIG",
            default_value = "/etc/facade/server.toml"
        )]
        config: PathBuf,
    },
    /// Run an agent that executes apps on this machine.
    Agent {
        /// Join token from `facade node add`. Only needed the first time.
        #[arg(long, env = "FACADE_JOIN")]
        join: Option<String>,
        #[arg(
            long,
            env = "FACADE_AGENT_DIR",
            default_value = "/var/lib/facade-agent"
        )]
        data_dir: PathBuf,
        /// Where apps are served over HTTP (also used for certificate challenges),
        /// when the entry point runs inside the agent.
        #[arg(long, env = "FACADE_HTTP_LISTEN", default_value = "0.0.0.0:80")]
        http_listen: std::net::SocketAddr,
        #[arg(long, env = "FACADE_HTTPS_LISTEN", default_value = "0.0.0.0:443")]
        https_listen: std::net::SocketAddr,
        /// Use the entry point run by `facaded ingress` at this control socket
        /// instead of listening itself.
        #[arg(long, env = "FACADE_INGRESS_CONTROL")]
        ingress_control: Option<PathBuf>,
    },
    /// Run a node's HTTP(S) entry point. Addresses come from the server config
    /// when it exists (http_listen, https_listen, ingress_control), else from the
    /// flags. `systemctl reload` (SIGUSR2) hands over to the installed binary
    /// without dropping connections.
    Ingress {
        #[arg(long, short, default_value = "/etc/facade/server.toml")]
        config: PathBuf,
        #[arg(long)]
        http: Option<std::net::SocketAddr>,
        #[arg(long)]
        https: Option<std::net::SocketAddr>,
        #[arg(long)]
        control: Option<PathBuf>,
        #[arg(long, default_value = "/var/lib/facade-ingress")]
        data_dir: PathBuf,
        /// After a handover, how long the old process waits for its open
        /// connections to end.
        #[arg(long, default_value_t = 3600)]
        drain_secs: u64,
    },
    /// Before installing this binary on a server: which of its running apps it
    /// would restart or rebuild (see deploy/upgrade.sh).
    UpgradeCheck {
        #[arg(long, short, default_value = "/etc/facade/server.toml")]
        config: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Copy the server's database and master key to <data_dir>/backups/_server.
    BackupState {
        #[arg(long, short, default_value = "/etc/facade/server.toml")]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("no other crypto provider installed");

    match Cli::parse().command {
        Command::UpgradeCheck { config, json } => {
            upgrade::check(&config::ServerConfig::load(&config)?, json).await
        }
        Command::BackupState { config } => {
            upgrade::backup_state(&config::ServerConfig::load(&config)?)
        }
        Command::Server { config } => {
            let config = config::ServerConfig::load(&config)?;
            server::run(config).await
        }
        Command::Agent {
            join,
            data_dir,
            http_listen,
            https_listen,
            ingress_control,
        } => {
            let ingress = match ingress_control {
                Some(control) => agent::IngressMode::External { control },
                None => agent::IngressMode::Embedded {
                    http: Some(http_listen),
                    https: Some(https_listen),
                },
            };
            agent::run_standalone(join, data_dir, ingress).await
        }
        Command::Ingress {
            config,
            http,
            https,
            control,
            data_dir,
            drain_secs,
        } => {
            let cfg = if config.exists() {
                config::ServerConfig::load(&config)?
            } else {
                config::ServerConfig::default()
            };
            ingress::process::run(ingress::process::Options {
                http: Some(http.unwrap_or(cfg.http_listen)),
                https: Some(https.unwrap_or(cfg.https_listen)),
                control: control.unwrap_or(cfg.ingress_control),
                data_dir,
                drain: std::time::Duration::from_secs(drain_secs),
            })
            .await
        }
    }
}
