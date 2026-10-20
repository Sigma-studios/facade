//! Let's Encrypt certificates for every routed domain, via HTTP-01 answered by the
//! ingress on port 80. Domains that do not resolve yet are skipped quietly, so
//! assigning a domain before its DNS exists costs nothing.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt,
    NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use tracing::{debug, info, warn};

use super::Server;
use super::store::CertRow;
use crate::tls;

impl Server {
    pub fn load_certs(&self) {
        let rows = match self.store.list_certs() {
            Ok(rows) => rows,
            Err(e) => return warn!("loading certificates: {e:#}"),
        };
        for row in rows {
            let loaded = self
                .secrets
                .open(&row.sealed_key)
                .and_then(|key| tls::certified_key(row.cert_pem.as_bytes(), &key));
            match loaded {
                Ok(key) => self.certs.insert(&row.domain, key),
                Err(e) => warn!(domain = %row.domain, "bad stored certificate: {e:#}"),
            }
        }
    }

    fn wanted_domains(&self) -> Vec<String> {
        let mut domains: Vec<String> = self.routes.read().domains.keys().cloned().collect();
        domains.extend(self.cfg.api_domain.clone());
        domains.sort();
        domains
    }
}

pub async fn run(server: Arc<Server>) -> anyhow::Result<()> {
    if !server.cfg.acme.enabled {
        info!("ACME disabled: HTTPS uses the self-signed certificate");
        return std::future::pending().await;
    }
    let mut account = None;
    // domain → (failures, next attempt)
    let mut backoff: HashMap<String, (u32, u64)> = HashMap::new();
    loop {
        let renew_at: HashMap<String, u64> = server
            .store
            .list_certs()?
            .into_iter()
            .map(|c| {
                // Renew after two thirds of the lifetime, whatever the CA's lifetime is.
                let lifetime = c.not_after.saturating_sub(c.not_before);
                (c.domain, c.not_before + lifetime * 2 / 3)
            })
            .collect();
        let now = facade_core::now();
        for domain in server.wanted_domains() {
            if renew_at.get(&domain).is_some_and(|at| *at > now) {
                continue;
            }
            if backoff.get(&domain).is_some_and(|(_, next)| *next > now) {
                continue;
            }
            if !resolves(&domain).await {
                debug!(%domain, "not resolvable yet, no certificate requested");
                continue;
            }
            let result = async {
                if account.is_none() {
                    account = Some(load_account(&server).await?);
                }
                issue(&server, account.as_ref().expect("set above"), &domain).await
            }
            .await;
            match result {
                Ok(()) => {
                    info!(%domain, "certificate issued");
                    backoff.remove(&domain);
                }
                Err(e) => {
                    let failures = backoff.get(&domain).map_or(0, |(n, _)| *n) + 1;
                    // 10 min, 20, 40 … capped at a day: Let's Encrypt limits failed validations.
                    let wait = (600u64 << failures.min(8)).min(86_400);
                    warn!(%domain, "certificate request failed (retry in {}m): {e:#}", wait / 60);
                    backoff.insert(domain, (failures, now + wait));
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
            _ = server.acme_wake.notified() => {
                // Let a burst of route changes settle.
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

async fn resolves(domain: &str) -> bool {
    tokio::net::lookup_host((domain, 80))
        .await
        .map(|mut addrs| addrs.next().is_some())
        .unwrap_or(false)
}

async fn load_account(server: &Server) -> anyhow::Result<Account> {
    let (key, url) = if server.cfg.acme.staging {
        ("acme_account_staging", LetsEncrypt::Staging.url())
    } else {
        ("acme_account", LetsEncrypt::Production.url())
    };
    if let Some(saved) = server.store.kv_get(key)? {
        let credentials: AccountCredentials = serde_json::from_str(&saved)?;
        return Ok(Account::builder()?.from_credentials(credentials).await?);
    }
    let contact = server
        .cfg
        .acme
        .email
        .as_ref()
        .map(|e| format!("mailto:{e}"));
    let contacts: Vec<&str> = contact.iter().map(String::as_str).collect();
    let (account, credentials) = Account::builder()?
        .create(
            &NewAccount {
                contact: &contacts,
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            url.to_owned(),
            None,
        )
        .await
        .context("creating ACME account")?;
    server
        .store
        .kv_put(key, &serde_json::to_string(&credentials)?)?;
    info!("created ACME account");
    Ok(account)
}

async fn issue(server: &Server, account: &Account, domain: &str) -> anyhow::Result<()> {
    let identifiers = [Identifier::Dns(domain.to_string())];
    let mut order = account.new_order(&NewOrder::new(&identifiers)).await?;
    let mut tokens = Vec::new();
    let result = async {
        let mut authorizations = order.authorizations();
        while let Some(authz) = authorizations.next().await {
            let mut authz = authz?;
            match authz.status {
                AuthorizationStatus::Valid => continue,
                AuthorizationStatus::Pending => {}
                other => bail!("authorization is {other:?}"),
            }
            let mut challenge = authz
                .challenge(ChallengeType::Http01)
                .ok_or_else(|| anyhow!("no http-01 challenge offered"))?;
            let token = challenge.token.clone();
            server.challenges.write().insert(
                token.clone(),
                challenge.key_authorization().as_str().to_string(),
            );
            tokens.push(token);
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
    {
        let mut challenges = server.challenges.write();
        for token in tokens {
            challenges.remove(&token);
        }
    }
    let (cert_pem, key_pem) = result?;

    let key = tls::certified_key(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let (not_before, not_after) = tls::validity(cert_pem.as_bytes())?;
    server.store.put_cert(&CertRow {
        domain: domain.to_string(),
        cert_pem,
        sealed_key: server.secrets.seal(key_pem.as_bytes()),
        not_before,
        not_after,
    })?;
    server.certs.insert(domain, key);
    Ok(())
}
