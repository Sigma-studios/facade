//! GitHub, for auto-deploys: the repository webhooks facade manages with an access
//! token (found by the URL they deliver to, so creating one is idempotent), the
//! signature on their deliveries, and what a delivery is about.

use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::{Method, StatusCode, header};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// What a token needs to manage a repository's webhooks.
pub const HOOK_PERMISSION: &str =
    "fine-grained token: Webhooks read and write; classic token: admin:repo_hook";

/// GitHub's REST API, with one token.
pub struct Api {
    client: reqwest::Client,
    base: String,
    token: String,
}

#[derive(Debug, Deserialize)]
pub struct Hook {
    pub id: i64,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub config: HookConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct HookConfig {
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: String,
}

impl Api {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("facade/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("HTTP client"),
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token))
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    /// The response if it succeeded, else GitHub's reason, worded for `repo`.
    async fn checked(
        &self,
        repo: &str,
        response: reqwest::Result<reqwest::Response>,
    ) -> anyhow::Result<reqwest::Response> {
        let response = response.context("cannot reach GitHub")?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let message = response
            .json::<ErrorBody>()
            .await
            .map(|b| b.message)
            .unwrap_or_default();
        match status {
            StatusCode::UNAUTHORIZED => bail!("GitHub refused the token (expired or revoked?)"),
            // 404 too: GitHub hides what a token may not manage.
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => bail!(
                "the token cannot manage webhooks of {repo} ({HOOK_PERMISSION}){}",
                if message.is_empty() {
                    String::new()
                } else {
                    format!(": {message}")
                }
            ),
            _ => bail!("GitHub answered {status} for {repo}: {message}"),
        }
    }

    /// `repo` is `owner/name`.
    pub async fn hooks(&self, repo: &str) -> anyhow::Result<Vec<Hook>> {
        let response = self
            .request(Method::GET, &format!("/repos/{repo}/hooks?per_page=100"))
            .send()
            .await;
        Ok(self.checked(repo, response).await?.json().await?)
    }

    /// The id of the repository's webhook delivering `events` to `url`, created,
    /// or updated if it differs (or its secret must be set again).
    pub async fn ensure_hook(
        &self,
        repo: &str,
        url: &str,
        secret: &str,
        events: &[String],
        reset_secret: bool,
    ) -> anyhow::Result<i64> {
        let config = serde_json::json!({
            "url": url,
            "content_type": "json",
            "secret": secret,
            "insecure_ssl": "0",
        });
        let existing = self
            .hooks(repo)
            .await?
            .into_iter()
            .find(|h| h.config.url.as_deref() == Some(url));
        match existing {
            Some(hook) => {
                let mut have = hook.events.clone();
                have.sort();
                let mut want = events.to_vec();
                want.sort();
                if hook.active && have == want && !reset_secret {
                    return Ok(hook.id);
                }
                let response = self
                    .request(Method::PATCH, &format!("/repos/{repo}/hooks/{}", hook.id))
                    .json(&serde_json::json!({
                        "active": true,
                        "events": events,
                        "config": config,
                    }))
                    .send()
                    .await;
                self.checked(repo, response).await?;
                Ok(hook.id)
            }
            None => {
                let response = self
                    .request(Method::POST, &format!("/repos/{repo}/hooks"))
                    .json(&serde_json::json!({
                        "name": "web",
                        "active": true,
                        "events": events,
                        "config": config,
                    }))
                    .send()
                    .await;
                let hook: Hook = self.checked(repo, response).await?.json().await?;
                Ok(hook.id)
            }
        }
    }

    /// Deletes a webhook; one already gone is fine.
    pub async fn delete_hook(&self, repo: &str, id: i64) -> anyhow::Result<()> {
        let response = self
            .request(Method::DELETE, &format!("/repos/{repo}/hooks/{id}"))
            .send()
            .await
            .context("cannot reach GitHub")?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        self.checked(repo, Ok(response)).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------- deliveries

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(data);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// Whether `signature` (the `X-Hub-Signature-256` header, `sha256=<hex>`) is the
/// body's, signed with `secret`. Compared in constant time.
pub fn verify(secret: &[u8], body: &[u8], signature: &str) -> bool {
    let Some(given) = signature
        .trim()
        .strip_prefix("sha256=")
        .and_then(|h| hex::decode(h).ok())
    else {
        return false;
    };
    let expected = hmac_sha256(secret, body);
    given.len() == expected.len()
        && given
            .iter()
            .zip(expected.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// `X-Hub-Signature-256` for a body.
#[cfg(test)]
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    format!("sha256={}", hex::encode(hmac_sha256(secret, body)))
}

/// What a delivery is about, as far as auto-deploy cares.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Sent when a webhook is created.
    Ping {
        repo: Option<String>,
    },
    /// A branch moved. `repo` is `github.com/owner/name`, lowercase.
    Push {
        repo: String,
        branch: String,
        default_branch: Option<String>,
    },
    /// An image was published to ghcr.io: `image` is `ghcr.io/owner/name`,
    /// lowercase; `tag` is `None` when the delivery names none.
    Package {
        repo: Option<String>,
        image: String,
        tag: Option<String>,
    },
    Other {
        repo: Option<String>,
    },
}

impl Event {
    /// The repository whose webhook sent it.
    pub fn repo(&self) -> Option<&str> {
        match self {
            Event::Ping { repo } | Event::Other { repo } | Event::Package { repo, .. } => {
                repo.as_deref()
            }
            Event::Push { repo, .. } => Some(repo),
        }
    }
}

fn repo_key(body: &serde_json::Value) -> Option<String> {
    let repository = body.get("repository")?;
    if let Some(location) = repository
        .get("html_url")
        .and_then(|u| u.as_str())
        .and_then(facade_core::git::parse)
    {
        return Some(location.key());
    }
    let name = repository.get("full_name")?.as_str()?;
    Some(format!("github.com/{}", name.to_lowercase()))
}

/// Where the webhook that sent a delivery sits: `github.com/owner` for an
/// organisation's, `github.com/owner/repo` for a repository's (lowercase), and
/// for a ping the events it sends. `target_type` is the
/// `X-GitHub-Hook-Installation-Target-Type` header; a ping also says it in its
/// body. `None` for anything else (a GitHub App, an unreadable body).
pub fn delivery_scope(target_type: &str, body: &[u8]) -> Option<(String, Option<Vec<String>>)> {
    let body: serde_json::Value = serde_json::from_slice(body).ok()?;
    let kind = match target_type {
        "" => body.pointer("/hook/type")?.as_str()?.to_lowercase(),
        t => t.to_lowercase(),
    };
    let scope = match kind.as_str() {
        "organization" => format!(
            "github.com/{}",
            body.pointer("/organization/login")?
                .as_str()?
                .to_lowercase()
        ),
        "repository" => repo_key(&body)?,
        _ => return None,
    };
    let events = body
        .pointer("/hook/events")
        .and_then(|e| e.as_array())
        .map(|e| {
            e.iter()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect()
        });
    Some((scope, events))
}

/// Reads a delivery of event `kind` (the `X-GitHub-Event` header).
pub fn parse_event(kind: &str, body: &[u8]) -> anyhow::Result<Event> {
    let body: serde_json::Value = serde_json::from_slice(body).context("the body is not JSON")?;
    let repo = repo_key(&body);
    let text = |v: Option<&serde_json::Value>| {
        v.and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Ok(match kind {
        "ping" => Event::Ping { repo },
        "push" => {
            let branch = text(body.get("ref"))
                .and_then(|r| r.strip_prefix("refs/heads/").map(str::to_string));
            let deleted = body.get("deleted").and_then(|d| d.as_bool()) == Some(true);
            match (repo, branch) {
                (Some(repo), Some(branch)) if !deleted => Event::Push {
                    repo,
                    branch,
                    default_branch: text(body.pointer("/repository/default_branch")),
                },
                (repo, _) => Event::Other { repo },
            }
        }
        "package" | "registry_package" => {
            let package = body.get("registry_package").or_else(|| body.get("package"));
            let action = text(body.get("action")).unwrap_or_default();
            let parsed = package.and_then(|p| {
                let name = text(p.get("name"))?;
                let owner = text(p.get("namespace")).or_else(|| text(p.pointer("/owner/login")))?;
                let version = p.get("package_version");
                let tag = version
                    .and_then(|v| text(v.pointer("/container_metadata/tag/name")))
                    .or_else(|| {
                        // `ghcr.io/owner/name:tag`
                        let url = text(version?.get("package_url"))?;
                        let last = url.rsplit('/').next()?;
                        last.split_once(':').map(|(_, t)| t.to_string())
                    })
                    .filter(|t| !t.starts_with("sha256"));
                Some((format!("ghcr.io/{owner}/{name}").to_lowercase(), tag))
            });
            match parsed {
                Some((image, tag)) if action == "published" || action == "updated" => {
                    Event::Package { repo, image, tag }
                }
                _ => Event::Other { repo },
            }
        }
        _ => Event::Other { repo },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_organisation_and_repository_webhooks_apart() {
        let ping = serde_json::json!({
            "hook": {"type": "Organization", "events": ["push", "package"]},
            "organization": {"login": "Sigma-studios"},
        });
        assert_eq!(
            delivery_scope("", ping.to_string().as_bytes()),
            Some((
                "github.com/sigma-studios".into(),
                Some(vec!["push".into(), "package".into()])
            ))
        );
        let push = serde_json::json!({
            "ref": "refs/heads/main",
            "repository": {"full_name": "Sigma-studios/Valet"},
            "organization": {"login": "Sigma-studios"},
        });
        let body = push.to_string();
        assert_eq!(
            delivery_scope("organization", body.as_bytes()),
            Some(("github.com/sigma-studios".into(), None))
        );
        assert_eq!(
            delivery_scope("repository", body.as_bytes()),
            Some(("github.com/sigma-studios/valet".into(), None))
        );
        assert_eq!(delivery_scope("integration", body.as_bytes()), None);
        assert_eq!(delivery_scope("", body.as_bytes()), None);
    }
    use axum::extract::{Path, State};
    use axum::http::HeaderMap;
    use axum::routing::{get, patch};
    use axum::{Json, Router};
    use parking_lot::Mutex;
    use std::sync::Arc;

    #[test]
    fn signs_like_github() {
        // RFC 4231, test case 2.
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // GitHub's documentation example.
        let signature = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(verify(
            b"It's a Secret to Everybody",
            b"Hello, World!",
            signature
        ));
        assert_eq!(
            sign(b"It's a Secret to Everybody", b"Hello, World!"),
            signature
        );
        assert!(!verify(b"another secret", b"Hello, World!", signature));
        assert!(!verify(
            b"It's a Secret to Everybody",
            b"Hello, World?",
            signature
        ));
        assert!(!verify(
            b"It's a Secret to Everybody",
            b"Hello, World!",
            "sha256=00"
        ));
        assert!(!verify(
            b"It's a Secret to Everybody",
            b"Hello, World!",
            "nope"
        ));
        let long = [7u8; 100];
        assert!(verify(&long, b"x", &sign(&long, b"x")));
    }

    #[test]
    fn reads_deliveries() {
        let push = serde_json::json!({
            "ref": "refs/heads/main",
            "after": "abc",
            "deleted": false,
            "repository": {
                "full_name": "Sigma-studios/Shop",
                "html_url": "https://github.com/Sigma-studios/Shop",
                "default_branch": "main"
            }
        });
        assert_eq!(
            parse_event("push", push.to_string().as_bytes()).unwrap(),
            Event::Push {
                repo: "github.com/sigma-studios/shop".into(),
                branch: "main".into(),
                default_branch: Some("main".into()),
            }
        );
        let tag = serde_json::json!({
            "ref": "refs/tags/v1",
            "repository": {"full_name": "me/shop"}
        });
        assert_eq!(
            parse_event("push", tag.to_string().as_bytes()).unwrap(),
            Event::Other {
                repo: Some("github.com/me/shop".into())
            }
        );
        let package = serde_json::json!({
            "action": "published",
            "registry_package": {
                "name": "notremodelesocial",
                "namespace": "sigma-dev",
                "package_type": "CONTAINER",
                "package_version": {
                    "container_metadata": {"tag": {"name": "latest", "digest": "sha256:1"}},
                    "package_url": "ghcr.io/sigma-dev/notremodelesocial:latest"
                }
            },
            "repository": {"full_name": "sigma-dev/notremodelesocial"}
        });
        assert_eq!(
            parse_event("registry_package", package.to_string().as_bytes()).unwrap(),
            Event::Package {
                repo: Some("github.com/sigma-dev/notremodelesocial".into()),
                image: "ghcr.io/sigma-dev/notremodelesocial".into(),
                tag: Some("latest".into()),
            }
        );
        let package = serde_json::json!({
            "action": "published",
            "package": {
                "name": "Blog",
                "owner": {"login": "Me"},
                "package_version": {"package_url": "ghcr.io/me/blog:edge"}
            }
        });
        assert_eq!(
            parse_event("package", package.to_string().as_bytes()).unwrap(),
            Event::Package {
                repo: None,
                image: "ghcr.io/me/blog".into(),
                tag: Some("edge".into()),
            }
        );
        assert!(matches!(
            parse_event("ping", br#"{"zen":"hi","repository":{"full_name":"me/x"}}"#).unwrap(),
            Event::Ping { repo: Some(_) }
        ));
        assert!(parse_event("push", b"not json").is_err());
    }

    #[derive(Default)]
    struct Mock {
        hooks: Vec<serde_json::Value>,
        calls: Vec<String>,
    }

    type Shared = Arc<Mutex<Mock>>;

    fn authorized(h: &HeaderMap) -> bool {
        h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer good")
            && h.get("x-github-api-version").is_some()
    }

    /// GitHub's hook endpoints for `me/shop`, which only token `good` may manage.
    async fn mock() -> (String, Shared) {
        let state = Shared::default();
        let denied = || {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"message": "Not Found"})),
            )
        };
        let list = move |State(s): State<Shared>,
                         Path((o, r)): Path<(String, String)>,
                         h: HeaderMap| async move {
            if !authorized(&h) || (o.as_str(), r.as_str()) != ("me", "shop") {
                return Err(denied());
            }
            let mut s = s.lock();
            s.calls.push("list".into());
            Ok(Json(serde_json::Value::Array(s.hooks.clone())))
        };
        let create = move |State(s): State<Shared>,
                           h: HeaderMap,
                           Json(body): Json<serde_json::Value>| async move {
            if !authorized(&h) {
                return Err(denied());
            }
            let mut s = s.lock();
            let id = s.hooks.len() as i64 + 1;
            let mut hook = body.clone();
            hook["id"] = id.into();
            assert_eq!(body["config"]["content_type"], "json");
            assert!(body["config"]["secret"].is_string());
            s.hooks.push(hook.clone());
            s.calls.push("create".into());
            Ok((StatusCode::CREATED, Json(hook)))
        };
        let update = move |State(s): State<Shared>,
                           Path((_, _, id)): Path<(String, String, i64)>,
                           h: HeaderMap,
                           Json(body): Json<serde_json::Value>| async move {
            if !authorized(&h) {
                return Err(denied());
            }
            let mut s = s.lock();
            s.calls.push(format!("update {id}"));
            let hook = s.hooks.iter_mut().find(|h| h["id"] == id).unwrap();
            hook["events"] = body["events"].clone();
            hook["config"] = body["config"].clone();
            Ok(Json(hook.clone()))
        };
        let delete = move |State(s): State<Shared>,
                           Path((_, _, id)): Path<(String, String, i64)>,
                           h: HeaderMap| async move {
            if !authorized(&h) {
                return Err(denied());
            }
            let mut s = s.lock();
            s.calls.push(format!("delete {id}"));
            let before = s.hooks.len();
            s.hooks.retain(|h| h["id"] != id);
            if s.hooks.len() == before {
                return Err(denied());
            }
            Ok(StatusCode::NO_CONTENT)
        };
        let app = Router::new()
            .route("/repos/{o}/{r}/hooks", get(list).post(create))
            .route("/repos/{o}/{r}/hooks/{id}", patch(update).delete(delete))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), state)
    }

    #[tokio::test]
    async fn manages_hooks_idempotently() {
        let (base, state) = mock().await;
        let api = Api::new(&base, "good");
        let url = "https://facade.example.com/api/hooks/github";
        let push = vec!["push".to_string()];
        state.lock().hooks.push(serde_json::json!({
            "id": 99, "active": true, "events": ["push"],
            "config": {"url": "https://ci.example.com/hook"}
        }));
        let id = api
            .ensure_hook("me/shop", url, "s", &push, false)
            .await
            .unwrap();
        assert_eq!(
            api.ensure_hook("me/shop", url, "s", &push, false)
                .await
                .unwrap(),
            id
        );
        let both = vec!["push".to_string(), "package".to_string()];
        assert_eq!(
            api.ensure_hook("me/shop", url, "s", &both, false)
                .await
                .unwrap(),
            id
        );
        assert_eq!(
            api.ensure_hook("me/shop", url, "s2", &both, true)
                .await
                .unwrap(),
            id
        );
        let update = format!("update {id}");
        assert_eq!(
            state.lock().calls,
            [
                "list",
                "create",
                "list",
                "list",
                update.as_str(),
                "list",
                update.as_str()
            ]
        );
        assert_eq!(
            state.lock().hooks.len(),
            2,
            "someone else's hook is left alone"
        );
        api.delete_hook("me/shop", id).await.unwrap();
        api.delete_hook("me/shop", id).await.unwrap();
        assert_eq!(state.lock().hooks.len(), 1);

        let denied = Api::new(&base, "read-only")
            .ensure_hook("me/shop", url, "s", &push, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            denied.contains("cannot manage webhooks of me/shop"),
            "{denied}"
        );
        assert!(denied.contains("Webhooks read and write"), "{denied}");
    }
}
