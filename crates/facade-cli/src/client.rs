//! Saved login and the HTTP client built from it.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use facade_core::api::ErrorBody;
use facade_core::pinned::PinnedVerifier;
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Saved {
    pub server: String,
    /// Pinned certificate fingerprint; absent when the server has a CA-signed certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    pub token: String,
}

pub fn config_path() -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var("FACADE_CLI_CONFIG") {
        return Ok(path.into());
    }
    let dir = dirs::config_dir().context("no config directory")?;
    Ok(dir.join("facade").join("cli.toml"))
}

impl Saved {
    /// `FACADE_SERVER` / `FACADE_TOKEN` / `FACADE_FINGERPRINT` override the saved file,
    /// which is how scripts and AI agents can use the CLI without `login`.
    pub fn load() -> anyhow::Result<Self> {
        let mut saved = match std::fs::read_to_string(config_path()?) {
            Ok(raw) => toml::from_str(&raw).context("reading saved login")?,
            Err(_) => Saved::default(),
        };
        if let Ok(server) = std::env::var("FACADE_SERVER") {
            saved.server = server;
        }
        if let Ok(token) = std::env::var("FACADE_TOKEN") {
            saved.token = token;
        }
        if let Ok(fp) = std::env::var("FACADE_FINGERPRINT") {
            saved.fingerprint = Some(fp);
        }
        if saved.server.is_empty() || saved.token.is_empty() {
            bail!("not logged in: run `facade login https://<server>:7701`");
        }
        Ok(saved)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = config_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let contents = toml::to_string(self)?;
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)?
                .write_all(contents.as_bytes())?;
        }
        #[cfg(not(unix))]
        std::fs::write(&path, contents)?;
        Ok(())
    }
}

pub fn http_client(fingerprint: Option<&str>) -> anyhow::Result<reqwest::Client> {
    let builder =
        reqwest::Client::builder().user_agent(concat!("facade-cli/", env!("CARGO_PKG_VERSION")));
    let builder = match fingerprint {
        Some(fp) => {
            let verifier = PinnedVerifier::new(Some(fp.to_string()));
            builder.use_preconfigured_tls(verifier.client_config())
        }
        None => builder,
    };
    Ok(builder.build()?)
}

/// Connects without trusting anything and reports the certificate presented.
pub async fn probe_fingerprint(server: &str) -> anyhow::Result<String> {
    let verifier: Arc<PinnedVerifier> = PinnedVerifier::new(None);
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(verifier.client_config())
        .build()?;
    client
        .get(format!("{server}/api/health"))
        .send()
        .await
        .with_context(|| format!("cannot reach {server}"))?;
    let seen = verifier
        .seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    seen.ok_or_else(|| anyhow!("server presented no certificate"))
}

pub struct Api {
    pub saved: Saved,
    client: reqwest::Client,
}

impl Api {
    pub fn new(saved: Saved) -> anyhow::Result<Self> {
        let client = http_client(saved.fingerprint.as_deref())?;
        Ok(Self { saved, client })
    }

    pub fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.client
            .request(
                method,
                format!("{}{path}", self.saved.server.trim_end_matches('/')),
            )
            .bearer_auth(&self.saved.token)
    }

    pub async fn send(&self, req: RequestBuilder) -> anyhow::Result<Response> {
        let resp = req.send().await.context("request failed")?;
        check(resp).await
    }

    pub async fn json<T: DeserializeOwned>(&self, req: RequestBuilder) -> anyhow::Result<T> {
        Ok(self.send(req).await?.json().await?)
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        self.json(self.request(Method::GET, path)).await
    }
}

pub async fn check(resp: Response) -> anyhow::Result<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let text = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<ErrorBody>(&text)
        .map(|e| e.error)
        .unwrap_or(text);
    if status == StatusCode::UNAUTHORIZED {
        bail!("{message} (run `facade login`)");
    }
    bail!("{message} ({status})")
}
