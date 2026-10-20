//! Settings of the server itself that can change without a restart: the domain
//! of the web UI and API, the ACME contact, and whether the direct API port takes
//! connections from anywhere. Stored in the database once changed; until then
//! the config file's values apply.

use parking_lot::RwLock;
use tracing::info;

use super::store::Store;
use crate::config::ServerConfig;

const API_DOMAIN: &str = "settings.api_domain";
const ACME_EMAIL: &str = "settings.acme_email";
const DIRECT_LOCAL_ONLY: &str = "settings.direct_api_local_only";

#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub api_domain: Option<String>,
    pub acme_email: Option<String>,
    pub direct_api_local_only: bool,
}

#[derive(Default)]
pub struct SettingsCell(RwLock<Settings>);

/// A stored value wins over the config; an empty stored value means "none".
fn stored(store: &Store, key: &str, fallback: Option<String>) -> anyhow::Result<Option<String>> {
    Ok(match store.kv_get(key)? {
        Some(v) if v.is_empty() => None,
        Some(v) => Some(v),
        None => fallback,
    })
}

impl SettingsCell {
    pub fn load(store: &Store, cfg: &ServerConfig) -> anyhow::Result<Self> {
        Ok(Self(RwLock::new(Settings {
            api_domain: stored(store, API_DOMAIN, cfg.api_domain.clone())?,
            acme_email: stored(store, ACME_EMAIL, cfg.acme.email.clone())?,
            direct_api_local_only: store.kv_get(DIRECT_LOCAL_ONLY)?.as_deref() == Some("true"),
        })))
    }

    pub fn get(&self) -> Settings {
        self.0.read().clone()
    }

    pub fn api_domain(&self) -> Option<String> {
        self.0.read().api_domain.clone()
    }

    pub fn set_api_domain(&self, store: &Store, domain: Option<String>) -> anyhow::Result<()> {
        store.kv_put(API_DOMAIN, domain.as_deref().unwrap_or(""))?;
        info!(domain = ?domain, "API domain changed");
        self.0.write().api_domain = domain;
        Ok(())
    }

    pub fn set_acme_email(&self, store: &Store, email: Option<String>) -> anyhow::Result<()> {
        store.kv_put(ACME_EMAIL, email.as_deref().unwrap_or(""))?;
        self.0.write().acme_email = email;
        Ok(())
    }

    pub fn set_direct_api_local_only(&self, store: &Store, local_only: bool) -> anyhow::Result<()> {
        store.kv_put(DIRECT_LOCAL_ONLY, if local_only { "true" } else { "false" })?;
        info!(local_only, "direct API port access changed");
        self.0.write().direct_api_local_only = local_only;
        Ok(())
    }
}
