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
    /// Public ingress.
    pub http_listen: SocketAddr,
    pub https_listen: SocketAddr,
    /// Name of the node the server itself runs apps on.
    pub local_node: String,
    /// Serve the API on this domain through the ingress too, with a real certificate.
    pub api_domain: Option<String>,
    pub acme: AcmeConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            data_dir: "/var/lib/facade".into(),
            public_host: None,
            node_listen: "0.0.0.0:7700".parse().unwrap(),
            api_listen: "0.0.0.0:7701".parse().unwrap(),
            http_listen: "0.0.0.0:80".parse().unwrap(),
            https_listen: "0.0.0.0:443".parse().unwrap(),
            local_node: "main".into(),
            api_domain: None,
            acme: AcmeConfig::default(),
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
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            email: None,
            staging: false,
        }
    }
}

impl ServerConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            tracing::warn!("{} not found, using defaults", path.display());
            return Ok(Self::default());
        }
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        facade_core::validate_name("local_node", &config.local_node)?;
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
