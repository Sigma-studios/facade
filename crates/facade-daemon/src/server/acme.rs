//! Certificates for every routed domain, issued here and pushed to the nodes that
//! serve them.
//!
//! * In a zone managed by the DNS provider, one wildcard certificate
//!   (`example.com` + `*.example.com`) covers the apex and every app one level
//!   below, validated with DNS-01.
//! * Anything else gets its own certificate, validated with HTTP-01 by the node
//!   the name points to. Names that do not resolve there yet are skipped quietly.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use facade_core::proto::{AgentRequest, CertBundle, ProxyRoute};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt,
    NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use tracing::{debug, info, warn};

use super::Server;
use super::dns;
use super::store::CertRow;
use crate::tls;

/// One certificate to have.
#[derive(Debug, Clone, PartialEq)]
struct Job {
    /// Key in the certificate table: `*.example.com` or `app.example.com`.
    name: String,
    names: Vec<String>,
    /// Validate through the DNS provider in this zone; otherwise HTTP-01.
    zone: Option<String>,
    /// For HTTP-01: the node that answers.
    node: Option<String>,
}

/// The certificate name that covers `domain`.
pub fn covering_name(server: &Server, domain: &str) -> String {
    match server.dns.zone_for(domain) {
        Some((zone, label)) if !label.contains('.') => format!("*.{zone}"),
        _ => domain.to_string(),
    }
}

/// Names a stored certificate covers.
pub fn names_of(cert_name: &str) -> Vec<String> {
    match cert_name.strip_prefix("*.") {
        Some(zone) => vec![zone.to_string(), cert_name.to_string()],
        None => vec![cert_name.to_string()],
    }
}

fn jobs(server: &Server) -> Vec<Job> {
    let mut jobs: Vec<Job> = Vec::new();
    for desired in server.desired_domains() {
        let name = covering_name(server, &desired.domain);
        if jobs.iter().any(|j| j.name == name) {
            continue;
        }
        let zone = server.dns.zone_for(&desired.domain).map(|(z, _)| z);
        jobs.push(Job {
            names: names_of(&name),
            node: zone.is_none().then(|| desired.node.clone()),
            zone,
            name,
        });
    }
    jobs
}

pub async fn run(server: Arc<Server>) -> anyhow::Result<()> {
    let mut account = None;
    // certificate name → (failures, next attempt)
    let mut backoff: HashMap<String, (u32, u64)> = HashMap::new();
    loop {
        dns::reconcile(&server).await;
        if server.cfg.acme.enabled {
            issue_due(&server, &mut account, &mut backoff).await;
        }
        push_ingress_all(&server).await;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
            _ = server.domains_wake.notified() => {
                // Let a burst of changes settle.
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

async fn issue_due(
    server: &Arc<Server>,
    account: &mut Option<Account>,
    backoff: &mut HashMap<String, (u32, u64)>,
) {
    let renew_at: HashMap<String, u64> = server
        .store
        .list_certs()
        .unwrap_or_default()
        .into_iter()
        .map(|c| {
            // Renew after two thirds of the lifetime, whatever the CA's lifetime is.
            let lifetime = c.not_after.saturating_sub(c.not_before);
            (c.domain, c.not_before + lifetime * 2 / 3)
        })
        .collect();
    let now = facade_core::now();
    for job in jobs(server) {
        if renew_at.get(&job.name).is_some_and(|at| *at > now) {
            continue;
        }
        if backoff.get(&job.name).is_some_and(|(_, next)| *next > now) {
            continue;
        }
        if let Some(node) = &job.node
            && !points_to(server, &job.name, node).await
        {
            debug!(name = %job.name, "does not point at its node yet, no certificate requested");
            continue;
        }
        let result = async {
            if account.is_none() {
                *account = Some(load_account(server).await?);
            }
            issue(server, account.as_ref().expect("set above"), &job).await
        }
        .await;
        let key = format!("cert:{}", job.name);
        match result {
            Ok(()) => {
                info!(name = %job.name, "certificate issued");
                backoff.remove(&job.name);
                server.domain_errors.write().remove(&key);
            }
            Err(e) => {
                let failures = backoff.get(&job.name).map_or(0, |(n, _)| *n) + 1;
                // 10 min, 20, 40 … capped at a day: CAs limit failed validations.
                let wait = (600u64 << failures.min(8)).min(86_400);
                warn!(name = %job.name, "certificate request failed (retry in {}m): {e:#}", wait / 60);
                backoff.insert(job.name.clone(), (failures, now + wait));
                server.domain_errors.write().insert(key, format!("{e:#}"));
            }
        }
    }
}

/// Whether `domain` resolves to the node's public address (or at all, when the
/// address is unknown).
async fn points_to(server: &Server, domain: &str, node: &str) -> bool {
    let Ok(addrs) = tokio::net::lookup_host((domain, 80)).await else {
        return false;
    };
    let ips: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
    match server.node_ip(node) {
        Some(ip) => ips.contains(&ip),
        None => !ips.is_empty(),
    }
}

/// Gives the CA the current contact address, if an account exists already
/// (otherwise it gets the address when it is created).
pub async fn update_contact(server: &Server) -> anyhow::Result<()> {
    if !server.cfg.acme.enabled || !account_exists(server)? {
        return Ok(());
    }
    let account = load_account(server).await?;
    let contact = server
        .settings
        .get()
        .acme_email
        .map(|e| format!("mailto:{e}"));
    let contacts: Vec<&str> = contact.iter().map(String::as_str).collect();
    account
        .update_contacts(&contacts)
        .await
        .context("updating the ACME account's contact")?;
    Ok(())
}

fn directory_url(server: &Server) -> String {
    let acme = &server.cfg.acme;
    match (&acme.directory, acme.staging) {
        (Some(url), _) => url.clone(),
        (None, true) => LetsEncrypt::Staging.url().to_string(),
        (None, false) => LetsEncrypt::Production.url().to_string(),
    }
}

fn account_exists(server: &Server) -> anyhow::Result<bool> {
    Ok(server
        .store
        .kv_get(&format!("acme_account:{}", directory_url(server)))?
        .is_some())
}

async fn load_account(server: &Server) -> anyhow::Result<Account> {
    let acme = &server.cfg.acme;
    let url = directory_url(server);
    let key = format!("acme_account:{url}");
    let builder = || match &acme.root_ca {
        Some(path) => Account::builder_with_root(path),
        None => Account::builder(),
    };
    if let Some(saved) = server.store.kv_get(&key)? {
        let credentials: AccountCredentials = serde_json::from_str(&saved)?;
        return Ok(builder()?.from_credentials(credentials).await?);
    }
    let contact = server
        .settings
        .get()
        .acme_email
        .map(|e| format!("mailto:{e}"));
    let contacts: Vec<&str> = contact.iter().map(String::as_str).collect();
    let (account, credentials) = builder()?
        .create(
            &NewAccount {
                contact: &contacts,
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            url,
            None,
        )
        .await
        .context("creating ACME account")?;
    server
        .store
        .kv_put(&key, &serde_json::to_string(&credentials)?)?;
    info!("created ACME account");
    Ok(account)
}

/// DNS records or HTTP answers created for a challenge, removed afterwards.
enum Cleanup {
    Txt { zone: String, id: String },
    Http { node: String, token: String },
}

async fn issue(server: &Arc<Server>, account: &Account, job: &Job) -> anyhow::Result<()> {
    let identifiers: Vec<Identifier> = job
        .names
        .iter()
        .map(|n| Identifier::Dns(n.clone()))
        .collect();
    let mut order = account.new_order(&NewOrder::new(&identifiers)).await?;
    let mut cleanup = Vec::new();
    let result = async {
        // First publish every answer, then (after DNS has had time to spread) tell
        // the CA they are ready.
        let mut authorizations = order.authorizations();
        while let Some(authz) = authorizations.next().await {
            let mut authz = authz?;
            match authz.status {
                AuthorizationStatus::Valid => continue,
                AuthorizationStatus::Pending => {}
                other => bail!("authorization is {other:?}"),
            }
            if let Some(zone) = &job.zone {
                let provider = server
                    .dns
                    .provider()
                    .ok_or_else(|| anyhow!("the DNS provider was removed"))?;
                let challenge = authz
                    .challenge(ChallengeType::Dns01)
                    .ok_or_else(|| anyhow!("no dns-01 challenge offered"))?;
                let domain = challenge.identifier().to_string();
                let domain = domain.trim_start_matches("*.");
                let (_, label) = dns::zone_for(std::slice::from_ref(zone), domain)
                    .ok_or_else(|| anyhow!("{domain} is not in {zone}"))?;
                let record = if label.is_empty() {
                    "_acme-challenge".to_string()
                } else {
                    format!("_acme-challenge.{label}")
                };
                let value = challenge.key_authorization().dns_value();
                let id = provider.add_txt(zone, &record, &value).await?;
                cleanup.push(Cleanup::Txt {
                    zone: zone.clone(),
                    id,
                });
            } else {
                let node = job.node.clone().expect("HTTP-01 jobs have a node");
                let challenge = authz
                    .challenge(ChallengeType::Http01)
                    .ok_or_else(|| anyhow!("no http-01 challenge offered"))?;
                let token = challenge.token.clone();
                server
                    .hub
                    .request(
                        &node,
                        AgentRequest::Challenge {
                            token: token.clone(),
                            key_authorization: Some(
                                challenge.key_authorization().as_str().to_string(),
                            ),
                        },
                    )
                    .await
                    .with_context(|| format!("node {node} cannot answer the challenge"))?;
                cleanup.push(Cleanup::Http { node, token });
            }
        }
        if job.zone.is_some() {
            tokio::time::sleep(Duration::from_secs(server.cfg.acme.dns_propagation_secs)).await;
        }
        let mut authorizations = order.authorizations();
        while let Some(authz) = authorizations.next().await {
            let mut authz = authz?;
            if authz.status != AuthorizationStatus::Pending {
                continue;
            }
            let kind = if job.zone.is_some() {
                ChallengeType::Dns01
            } else {
                ChallengeType::Http01
            };
            let mut challenge = authz
                .challenge(kind)
                .ok_or_else(|| anyhow!("challenge disappeared"))?;
            challenge.set_ready().await?;
        }
        let status = order.poll_ready(&RetryPolicy::default()).await?;
        if status != OrderStatus::Ready {
            bail!("order ended as {status:?}");
        }
        let key_pem = order.finalize().await?;
        let cert_pem = order.poll_certificate(&RetryPolicy::default()).await?;
        Ok((cert_pem, key_pem))
    }
    .await;

    for item in cleanup {
        let outcome = match &item {
            Cleanup::Txt { zone, id } => match server.dns.provider() {
                Some(provider) => provider.delete_record(zone, id).await,
                None => Ok(()),
            },
            Cleanup::Http { node, token } => {
                server
                    .hub
                    .request(
                        node,
                        AgentRequest::Challenge {
                            token: token.clone(),
                            key_authorization: None,
                        },
                    )
                    .await
            }
        };
        if let Err(e) = outcome {
            warn!(name = %job.name, "cleaning up challenge: {e:#}");
        }
    }
    let (cert_pem, key_pem) = result?;

    let (not_before, not_after) = tls::validity(cert_pem.as_bytes())?;
    server.store.put_cert(&CertRow {
        domain: job.name.clone(),
        cert_pem,
        sealed_key: server.secrets.seal(key_pem.as_bytes()),
        not_before,
        not_after,
    })?;
    Ok(())
}

/// Sends every node the certificates for the names it serves.
pub async fn push_ingress_all(server: &Arc<Server>) {
    let nodes: Vec<String> = match server.store.list_nodes() {
        Ok(nodes) => nodes.into_iter().map(|n| n.name).collect(),
        Err(_) => return,
    };
    for node in nodes {
        if server.hub.is_connected(&node)
            && let Err(e) = push_ingress(server, &node).await
        {
            warn!(%node, "sending certificates: {e:#}");
        }
    }
}

pub async fn push_ingress(server: &Server, node: &str) -> anyhow::Result<()> {
    let served: Vec<String> = server
        .desired_domains()
        .into_iter()
        .filter(|d| d.node == node)
        .map(|d| covering_name(server, &d.domain))
        .collect();
    let mut certs = Vec::new();
    for row in server.store.list_certs()? {
        if !served.contains(&row.domain) {
            continue;
        }
        let key_pem = String::from_utf8(server.secrets.open(&row.sealed_key)?)?;
        certs.push(CertBundle {
            names: names_of(&row.domain),
            cert_pem: row.cert_pem,
            key_pem,
        });
    }
    let mut proxies = Vec::new();
    if node == server.cfg.local_node
        && let Some(domain) = &server.settings.api_domain()
    {
        proxies.push(ProxyRoute {
            domain: domain.clone(),
            port: server.cfg.api_loopback.port(),
        });
    }
    server
        .hub
        .request(node, AgentRequest::Ingress { certs, proxies })
        .await
}
