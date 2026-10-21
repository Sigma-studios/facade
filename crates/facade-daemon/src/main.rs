//! `facaded`: the facade daemon.
//!
//! * `facaded server` — control plane: API, node hub, certificates and DNS,
//!   monitoring. Also runs an agent for its own machine.
//! * `facaded agent --join <token>` — runs and serves apps on a node, dialing the
//!   server.

mod agent;
mod config;
mod framing;
mod gitauth;
mod lang;
mod server;
mod tls;

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
        /// Where apps are served over HTTP (also used for certificate challenges).
        #[arg(long, env = "FACADE_HTTP_LISTEN", default_value = "0.0.0.0:80")]
        http_listen: std::net::SocketAddr,
        #[arg(long, env = "FACADE_HTTPS_LISTEN", default_value = "0.0.0.0:443")]
        https_listen: std::net::SocketAddr,
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
        Command::Server { config } => {
            let config = config::ServerConfig::load(&config)?;
            server::run(config).await
        }
        Command::Agent {
            join,
            data_dir,
            http_listen,
            https_listen,
        } => {
            let ingress = agent::IngressOptions {
                http: Some(http_listen),
                https: Some(https_listen),
            };
            agent::run_standalone(join, data_dir, ingress).await
        }
    }
}
