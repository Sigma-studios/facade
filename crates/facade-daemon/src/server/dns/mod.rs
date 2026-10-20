//! DNS providers: facade points each app's domain at the node running it, and
//! answers DNS-01 challenges for wildcard certificates.
//!
//! A provider is anything implementing [`Provider`]; adding one is a file here and
//! an arm in [`build`]. Without a provider, records are created by hand and facade
//! only checks them (see [`statuses`]).

#[cfg(feature = "pebble-test")]
mod challtestsrv;
pub mod porkbun;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, bail};
use facade_core::api::{DnsSettings, DnsUpdate, DomainStatus};
use futures_util::future::BoxFuture;
use parking_lot::RwLock;
use tracing::{info, warn};

use super::Server;

pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    /// Domains (zones) this account can manage.
    fn zones(&self) -> BoxFuture<'_, anyhow::Result<Vec<String>>>;
    /// Creates or updates the A/AAAA record `label.zone` (`label` empty for the apex).
    fn set_address<'a>(
        &'a self,
        zone: &'a str,
        label: &'a str,
        ip: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>>;
    fn delete_address<'a>(
        &'a self,
        zone: &'a str,
        label: &'a str,
        ip: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>>;
    /// Adds a TXT record, returning an id for [`Provider::delete_record`].
    fn add_txt<'a>(
        &'a self,
        zone: &'a str,
        label: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<String>>;
    fn delete_record<'a>(&'a self, zone: &'a str, id: &'a str)
    -> BoxFuture<'a, anyhow::Result<()>>;
}

const KV_PROVIDER: &str = "dns.provider";
const KV_CREDENTIALS: &str = "dns.credentials";

#[derive(Default)]
pub struct Dns {
    provider: RwLock<Option<Arc<dyn Provider>>>,
    zones: RwLock<Vec<String>>,
    error: RwLock<Option<String>>,
}

fn build(name: &str, credentials: &str) -> anyhow::Result<Arc<dyn Provider>> {
    match name {
        "porkbun" => {
            let keys: porkbun::Keys = serde_json::from_str(credentials)?;
            Ok(Arc::new(porkbun::Porkbun::new(keys, porkbun::API)?))
        }
        #[cfg(feature = "pebble-test")]
        "challtestsrv" => {
            let keys: porkbun::Keys = serde_json::from_str(credentials)?;
            Ok(Arc::new(challtestsrv::ChallTestSrv {
                url: keys.api_key,
                zones: keys.secret_key.split(',').map(str::to_string).collect(),
                client: reqwest::Client::new(),
            }))
        }
        other => bail!("unknown DNS provider `{other}`"),
    }
}

impl Dns {
    pub fn provider(&self) -> Option<Arc<dyn Provider>> {
        self.provider.read().clone()
    }

    /// The managed zone containing `domain`, and the label inside it.
    pub fn zone_for(&self, domain: &str) -> Option<(String, String)> {
        if self.provider.read().is_none() {
            return None;
        }
        zone_for(&self.zones.read(), domain)
    }

    pub fn settings(&self) -> DnsSettings {
        DnsSettings {
            provider: self.provider.read().as_ref().map(|p| p.name().to_string()),
            zones: self.zones.read().clone(),
            error: self.error.read().clone(),
        }
    }

    async fn install(&self, provider: Option<Arc<dyn Provider>>) {
        let zones = match &provider {
            Some(p) => p.zones().await,
            None => Ok(vec![]),
        };
        match zones {
            Ok(zones) => {
                *self.zones.write() = zones;
                *self.error.write() = None;
            }
            Err(e) => {
                warn!("DNS provider: {e:#}");
                *self.error.write() = Some(format!("{e:#}"));
            }
        }
        *self.provider.write() = provider;
    }
}

/// Longest zone that is `domain` or a parent of it.
pub fn zone_for(zones: &[String], domain: &str) -> Option<(String, String)> {
    zones
        .iter()
        .filter(|z| domain == z.as_str() || domain.ends_with(&format!(".{z}")))
        .max_by_key(|z| z.len())
        .map(|zone| {
            let label = domain
                .strip_suffix(zone.as_str())
                .unwrap_or_default()
                .trim_end_matches('.')
                .to_string();
            (zone.clone(), label)
        })
}

/// Loads the provider saved in the database.
pub async fn load(server: &Server) -> anyhow::Result<()> {
    let Some(name) = server.store.kv_get(KV_PROVIDER)? else {
        return Ok(());
    };
    let sealed = server
        .store
        .kv_get(KV_CREDENTIALS)?
        .context("DNS provider without credentials")?;
    let credentials = server.secrets.open_string(&hex::decode(sealed)?)?;
    let provider = build(&name, &credentials)?;
    server.dns.install(Some(provider)).await;
    Ok(())
}

/// Checks and stores new provider settings. An empty provider removes them.
pub async fn update(server: &Server, update: DnsUpdate) -> Result<DnsSettings, String> {
    let name = update.provider.trim().to_lowercase();
    if name.is_empty() {
        server
            .store
            .kv_delete(KV_PROVIDER)
            .and_then(|_| server.store.kv_delete(KV_CREDENTIALS))
            .map_err(|e| format!("{e:#}"))?;
        server.dns.install(None).await;
        return Ok(server.dns.settings());
    }
    let credentials = match name.as_str() {
        #[cfg(feature = "pebble-test")]
        "challtestsrv" => serde_json::to_string(&porkbun::Keys {
            api_key: update.api_key.clone(),
            secret_key: update.secret_key.clone(),
        })
        .expect("serializable"),
        "porkbun" => {
            let keys = porkbun::Keys {
                api_key: update.api_key.trim().to_string(),
                secret_key: update.secret_key.trim().to_string(),
            };
            if keys.api_key.is_empty() || keys.secret_key.is_empty() {
                return Err("enter both the API key and the secret key".into());
            }
            serde_json::to_string(&keys).expect("serializable")
        }
        other => return Err(format!("unknown DNS provider `{other}`")),
    };
    let provider = build(&name, &credentials).map_err(|e| format!("{e:#}"))?;
    // Refuse keys that do not work rather than saving them.
    let zones = provider.zones().await.map_err(|e| format!("{e:#}"))?;
    let sealed = hex::encode(server.secrets.seal(credentials.as_bytes()));
    server
        .store
        .kv_put(KV_PROVIDER, &name)
        .and_then(|_| server.store.kv_put(KV_CREDENTIALS, &sealed))
        .map_err(|e| format!("{e:#}"))?;
    info!(provider = %name, zones = zones.len(), "DNS provider configured");
    server.dns.install(Some(provider)).await;
    server.domains_wake.notify_one();
    Ok(server.dns.settings())
}

/// A host name facade serves, and where.
#[derive(Debug, Clone)]
pub struct Desired {
    pub domain: String,
    pub app: Option<String>,
    pub node: String,
}

impl Server {
    pub fn desired_domains(&self) -> Vec<Desired> {
        let mut desired = Vec::new();
        if let Some(domain) = &self.settings.api_domain() {
            desired.push(Desired {
                domain: domain.clone(),
                app: None,
                node: self.cfg.local_node.clone(),
            });
        }
        let routes = self.routes.read();
        for (domain, app) in &routes.domains {
            if let Some(node) = routes.nodes.get(app) {
                desired.push(Desired {
                    domain: domain.clone(),
                    app: Some(app.clone()),
                    node: node.clone(),
                });
            }
        }
        desired.sort_by(|a, b| a.domain.cmp(&b.domain));
        desired
    }

    /// The public address of a node: set by hand, or where its agent connects
    /// from, or (for the server's own node) `public_host`.
    pub fn node_ip(&self, node: &str) -> Option<String> {
        let row = self
            .store
            .list_nodes()
            .ok()?
            .into_iter()
            .find(|n| n.name == node)?;
        if let Some(ip) = row.public_ip {
            return Some(ip);
        }
        if node == self.cfg.local_node {
            return self.public_host_ip.clone();
        }
        row.address
    }
}

/// Makes the provider's records match the desired domains. Only records facade
/// created itself are ever changed or deleted.
pub async fn reconcile(server: &Server) {
    let Some(provider) = server.dns.provider() else {
        return;
    };
    let current: HashMap<String, (String, String)> = match server.store.dns_records() {
        Ok(rows) => rows.into_iter().map(|(d, z, ip)| (d, (z, ip))).collect(),
        Err(e) => return warn!("DNS: {e:#}"),
    };
    let mut wanted = Vec::new();
    for desired in server.desired_domains() {
        let Some((zone, label)) = server.dns.zone_for(&desired.domain) else {
            continue;
        };
        let Some(ip) = server.node_ip(&desired.node) else {
            server.set_dns_error(
                &desired.domain,
                format!("node {} has no known public address", desired.node),
            );
            continue;
        };
        wanted.push(desired.domain.clone());
        if current
            .get(&desired.domain)
            .is_some_and(|(_, old)| *old == ip)
        {
            server.clear_dns_error(&desired.domain);
            continue;
        }
        match provider.set_address(&zone, &label, &ip).await {
            Ok(()) => {
                info!(domain = %desired.domain, %ip, "DNS record set");
                let _ = server.store.put_dns_record(&desired.domain, &zone, &ip);
                server.clear_dns_error(&desired.domain);
            }
            Err(e) => {
                warn!(domain = %desired.domain, "DNS: {e:#}");
                server.set_dns_error(&desired.domain, format!("{e:#}"));
            }
        }
    }
    for (domain, (zone, ip)) in current {
        if wanted.contains(&domain) {
            continue;
        }
        let label = zone_for(std::slice::from_ref(&zone), &domain)
            .map(|(_, l)| l)
            .unwrap_or_default();
        match provider.delete_address(&zone, &label, &ip).await {
            Ok(()) => info!(%domain, "DNS record removed"),
            Err(e) => warn!(%domain, "removing DNS record: {e:#}"),
        }
        let _ = server.store.delete_dns_record(&domain);
    }
}

impl Server {
    fn set_dns_error(&self, domain: &str, error: String) {
        self.domain_errors
            .write()
            .insert(format!("dns:{domain}"), error);
    }

    fn clear_dns_error(&self, domain: &str) {
        self.domain_errors.write().remove(&format!("dns:{domain}"));
    }
}

/// What every routed name looks like right now.
pub async fn statuses(server: &Server) -> Vec<DomainStatus> {
    let certs = server.store.list_certs().unwrap_or_default();
    let mut out = Vec::new();
    for desired in server.desired_domains() {
        let expected_ip = server.node_ip(&desired.node);
        let resolved: Vec<String> =
            match tokio::net::lookup_host((desired.domain.as_str(), 443)).await {
                Ok(addrs) => {
                    let mut ips: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
                    ips.sort();
                    ips.dedup();
                    ips
                }
                Err(_) => vec![],
            };
        let dns_ok = match &expected_ip {
            Some(ip) => resolved.contains(ip),
            None => !resolved.is_empty(),
        };
        let cert_name = super::acme::covering_name(server, &desired.domain);
        let cert = certs.iter().find(|c| c.domain == cert_name);
        let errors = server.domain_errors.read();
        out.push(DomainStatus {
            managed: server.dns.zone_for(&desired.domain).is_some(),
            dns_error: errors.get(&format!("dns:{}", desired.domain)).cloned(),
            cert_error: errors.get(&format!("cert:{cert_name}")).cloned(),
            certificate: cert.map(|c| c.domain.clone()),
            cert_expires: cert.map(|c| c.not_after),
            domain: desired.domain,
            app: desired.app,
            node: desired.node,
            expected_ip,
            resolved,
            dns_ok,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_zones() {
        let zones = vec!["example.com".to_string(), "dev.example.com".to_string()];
        assert_eq!(
            zone_for(&zones, "example.com"),
            Some(("example.com".into(), "".into()))
        );
        assert_eq!(
            zone_for(&zones, "app.example.com"),
            Some(("example.com".into(), "app".into()))
        );
        assert_eq!(
            zone_for(&zones, "api.dev.example.com"),
            Some(("dev.example.com".into(), "api".into()))
        );
        assert_eq!(zone_for(&zones, "notexample.com"), None);
    }
}
