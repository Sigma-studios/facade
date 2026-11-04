//! Asking a container registry what a tag points to, without pulling: the digest
//! of its manifest (what `docker pull` reports as `Digest:`), and the labels of
//! its image (`org.opencontainers.image.source` names the repository it is built
//! from). Used by auto-deploy to notice a new image behind a tag.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use base64::Engine;
use facade_core::proto::RegistryLogin;
use reqwest::{Method, StatusCode, header};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Every kind of manifest, multi-platform indexes first: the digest is then the
/// one a pull of the tag reports.
const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json";

const SOURCE_LABEL: &str = "org.opencontainers.image.source";

/// An image reference, split the way the registry API wants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// As logins name it: `ghcr.io`, `docker.io`.
    pub registry: String,
    /// Where its API is: `https://ghcr.io`, `https://registry-1.docker.io`.
    pub base: String,
    /// `me/app`, `library/nginx`.
    pub repository: String,
    /// A tag, or a digest (`sha256:…`).
    pub reference: String,
}

impl ImageRef {
    pub fn parse(image: &str) -> Self {
        let full = facade_core::image_with_tag(image.trim());
        let registry = facade_core::image_registry(&full);
        let first_is_host = full
            .split_once('/')
            .is_some_and(|(first, _)| first.contains(['.', ':']) || first == "localhost");
        let rest = if first_is_host {
            full.split_once('/').map_or(full.as_str(), |(_, r)| r)
        } else {
            full.as_str()
        };
        let (name, reference) = match rest.split_once('@') {
            Some((name, digest)) => (name.to_string(), digest.to_string()),
            None => {
                let slash = rest.rfind('/').map_or(0, |i| i + 1);
                match rest[slash..].rfind(':') {
                    Some(i) => (
                        rest[..slash + i].to_string(),
                        rest[slash + i + 1..].to_string(),
                    ),
                    None => (rest.to_string(), "latest".to_string()),
                }
            }
        };
        let (base, repository) = if registry == "docker.io" {
            let name = if name.contains('/') {
                name
            } else {
                format!("library/{name}")
            };
            ("https://registry-1.docker.io".to_string(), name)
        } else {
            let host = registry.split(':').next().unwrap_or_default();
            // Like Docker, plain HTTP for registries on the machine itself.
            let scheme = if host == "localhost" || host.starts_with("127.") {
                "http"
            } else {
                "https"
            };
            (format!("{scheme}://{registry}"), name)
        };
        Self {
            registry,
            base,
            repository,
            reference,
        }
    }

    /// `ghcr.io/me/app:latest`: what webhooks and status lines name.
    pub fn display(&self) -> String {
        let sep = if self.reference.contains(':') {
            "@"
        } else {
            ":"
        };
        format!(
            "{}/{}{sep}{}",
            self.registry, self.repository, self.reference
        )
    }

    fn url(&self, kind: &str, reference: &str) -> String {
        format!("{}/v2/{}/{kind}/{reference}", self.base, self.repository)
    }
}

pub struct Registry {
    client: reqwest::Client,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("facade/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("HTTP client"),
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    manifests: Vec<IndexEntry>,
    #[serde(default)]
    config: Option<Descriptor>,
    #[serde(default)]
    annotations: HashMap<String, String>,
}

#[derive(Deserialize)]
struct IndexEntry {
    digest: String,
    #[serde(default)]
    platform: Option<Platform>,
}

#[derive(Deserialize)]
struct Platform {
    #[serde(default)]
    os: String,
    #[serde(default)]
    architecture: String,
}

#[derive(Deserialize)]
struct Descriptor {
    digest: String,
}

#[derive(Deserialize)]
struct ImageConfig {
    #[serde(default)]
    config: Option<ConfigLabels>,
}

#[derive(Deserialize)]
struct ConfigLabels {
    #[serde(default, rename = "Labels")]
    labels: Option<HashMap<String, String>>,
}

impl Registry {
    /// The digest the tag points to now.
    pub async fn digest(
        &self,
        image: &str,
        login: Option<&RegistryLogin>,
    ) -> anyhow::Result<String> {
        let image = ImageRef::parse(image);
        if image.reference.starts_with("sha256:") {
            return Ok(image.reference);
        }
        let url = image.url("manifests", &image.reference);
        let response = self.send(&image, login, Method::HEAD, &url).await?;
        let response = checked(&image, response)?;
        if let Some(digest) = response
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
        {
            return Ok(digest.to_string());
        }
        // Some registries only say it on GET: the digest is that of the body.
        let response = checked(&image, self.send(&image, login, Method::GET, &url).await?)?;
        let body = response.bytes().await?;
        Ok(format!("sha256:{}", hex::encode(Sha256::digest(&body))))
    }

    /// The repository the image says it is built from (its
    /// `org.opencontainers.image.source` label or annotation), if it says.
    pub async fn source(
        &self,
        image: &str,
        login: Option<&RegistryLogin>,
    ) -> anyhow::Result<Option<String>> {
        let image = ImageRef::parse(image);
        let mut manifest = self.manifest(&image, login, &image.reference).await?;
        if let Some(source) = manifest.annotations.get(SOURCE_LABEL) {
            return Ok(Some(source.clone()));
        }
        if !manifest.manifests.is_empty() {
            // Any real platform will do (attestations are `unknown/unknown`).
            fn platform<'a>(e: &&'a IndexEntry) -> Option<(&'a str, &'a str)> {
                e.platform
                    .as_ref()
                    .map(|p| (p.os.as_str(), p.architecture.as_str()))
            }
            let pick = manifest
                .manifests
                .iter()
                .find(|e| platform(e) == Some(("linux", "amd64")))
                .or_else(|| {
                    manifest
                        .manifests
                        .iter()
                        .find(|e| platform(e).is_none_or(|(os, _)| os != "unknown"))
                })
                .context("the image index lists no image")?;
            let digest = pick.digest.clone();
            manifest = self.manifest(&image, login, &digest).await?;
            if let Some(source) = manifest.annotations.get(SOURCE_LABEL) {
                return Ok(Some(source.clone()));
            }
        }
        let Some(config) = manifest.config else {
            return Ok(None);
        };
        let url = image.url("blobs", &config.digest);
        let response = checked(&image, self.send(&image, login, Method::GET, &url).await?)?;
        let config: ImageConfig = response.json().await.context("reading the image config")?;
        Ok(config
            .config
            .and_then(|c| c.labels)
            .and_then(|mut l| l.remove(SOURCE_LABEL)))
    }

    async fn manifest(
        &self,
        image: &ImageRef,
        login: Option<&RegistryLogin>,
        reference: &str,
    ) -> anyhow::Result<Manifest> {
        let url = image.url("manifests", reference);
        let response = checked(image, self.send(image, login, Method::GET, &url).await?)?;
        response.json().await.context("reading the image manifest")
    }

    /// A request, anonymous first, then with what the registry's challenge asks
    /// for: a bearer token from its token service (with the login, if any), or
    /// the login itself.
    async fn send(
        &self,
        image: &ImageRef,
        login: Option<&RegistryLogin>,
        method: Method,
        url: &str,
    ) -> anyhow::Result<reqwest::Response> {
        let request = |auth: Option<&str>| {
            let mut r = self
                .client
                .request(method.clone(), url)
                .header(header::ACCEPT, MANIFEST_TYPES);
            if let Some(auth) = auth {
                r = r.header(header::AUTHORIZATION, auth);
            }
            r
        };
        let unreachable = || format!("cannot reach {}", image.registry);
        let response = request(None).send().await.with_context(unreachable)?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let challenge = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let auth = self.authorize(image, login, &challenge).await?;
        request(Some(&auth)).send().await.with_context(unreachable)
    }

    async fn authorize(
        &self,
        image: &ImageRef,
        login: Option<&RegistryLogin>,
        challenge: &str,
    ) -> anyhow::Result<String> {
        let (scheme, params) = parse_challenge(challenge);
        let basic = |l: &RegistryLogin| {
            let encoded = base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", l.username, l.secret));
            format!("Basic {encoded}")
        };
        match scheme.to_ascii_lowercase().as_str() {
            "basic" => login.map(basic).ok_or_else(|| {
                anyhow!(
                    "{} asks for a login: facade registry login {}",
                    image.registry,
                    image.registry
                )
            }),
            "bearer" => {
                let realm = params
                    .get("realm")
                    .context("the registry's challenge names no token service")?;
                let scope = params
                    .get("scope")
                    .cloned()
                    .unwrap_or_else(|| format!("repository:{}:pull", image.repository));
                let mut query = vec![("scope", scope)];
                if let Some(service) = params.get("service") {
                    query.push(("service", service.clone()));
                }
                let mut request = self.client.get(realm).query(&query);
                if let Some(l) = login {
                    request = request.header(header::AUTHORIZATION, basic(l));
                }
                let response = request
                    .send()
                    .await
                    .with_context(|| format!("cannot reach {realm}"))?;
                if !response.status().is_success() {
                    bail!(
                        "{} refused {} ({}){}",
                        image.registry,
                        if login.is_some() {
                            "the login"
                        } else {
                            "anonymous access"
                        },
                        response.status(),
                        if login.is_some() {
                            ""
                        } else {
                            ": facade registry login"
                        }
                    );
                }
                let token: TokenResponse = response
                    .json()
                    .await
                    .context("reading the registry token")?;
                let token = token
                    .token
                    .or(token.access_token)
                    .context("the registry gave no token")?;
                Ok(format!("Bearer {token}"))
            }
            _ => bail!("{} wants an unknown kind of login", image.registry),
        }
    }
}

/// The response if it succeeded, else why not, in words.
fn checked(image: &ImageRef, response: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    match response.status() {
        s if s.is_success() => Ok(response),
        StatusCode::NOT_FOUND => bail!("{} not found in the registry", image.display()),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => bail!(
            "{} refused access to {}: facade registry login {}",
            image.registry,
            image.display(),
            image.registry
        ),
        s => bail!("{} answered {s} for {}", image.registry, image.display()),
    }
}

/// `Bearer realm="…",service="…",scope="…"` → (`Bearer`, its parameters).
fn parse_challenge(header: &str) -> (String, HashMap<String, String>) {
    let header = header.trim();
    let (scheme, rest) = header.split_once(' ').unwrap_or((header, ""));
    let mut params = HashMap::new();
    let mut chars = rest.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| *c == ',' || c.is_whitespace()) {
            chars.next();
        }
        let key: String = chars.by_ref().take_while(|c| *c != '=').collect();
        if key.is_empty() {
            break;
        }
        let mut value = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => value.extend(chars.next()),
                    '"' => break,
                    c => value.push(c),
                }
            }
        } else {
            value = chars.by_ref().take_while(|c| *c != ',').collect();
        }
        params.insert(key.trim().to_ascii_lowercase(), value);
    }
    (scheme.to_string(), params)
}

/// The digest a pull reported in a deployment's log (`Digest: sha256:…`), the
/// last one if it pulled more than once.
pub fn pulled_digest(log: &str) -> Option<String> {
    log.lines().rev().find_map(|line| {
        let at = line.find("Digest: sha256:")?;
        let digest: String = line[at + "Digest: ".len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == ':')
            .collect();
        (digest.len() == "sha256:".len() + 64).then_some(digest)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, Query};
    use axum::http::HeaderMap;
    use axum::response::IntoResponse;
    use axum::routing::get;

    #[test]
    fn parses_references() {
        let r = ImageRef::parse("nginx");
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.base, "https://registry-1.docker.io");
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.reference, "latest");
        let r = ImageRef::parse("ghcr.io/sigma-dev/notremodelesocial:latest");
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.base, "https://ghcr.io");
        assert_eq!(r.repository, "sigma-dev/notremodelesocial");
        assert_eq!(r.display(), "ghcr.io/sigma-dev/notremodelesocial:latest");
        let r = ImageRef::parse("traefik/whoami:v1.10");
        assert_eq!(
            (r.repository.as_str(), r.reference.as_str()),
            ("traefik/whoami", "v1.10")
        );
        let r = ImageRef::parse("localhost:5000/me/app");
        assert_eq!(r.base, "http://localhost:5000");
        assert_eq!(
            (r.repository.as_str(), r.reference.as_str()),
            ("me/app", "latest")
        );
        let r = ImageRef::parse("registry.example.com:8443/a/b/c@sha256:abc");
        assert_eq!(r.base, "https://registry.example.com:8443");
        assert_eq!(
            (r.repository.as_str(), r.reference.as_str()),
            ("a/b/c", "sha256:abc")
        );
    }

    #[test]
    fn parses_challenges() {
        let (scheme, p) = parse_challenge(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:me/app:pull,push""#,
        );
        assert_eq!(scheme, "Bearer");
        assert_eq!(p["realm"], "https://ghcr.io/token");
        assert_eq!(p["service"], "ghcr.io");
        assert_eq!(p["scope"], "repository:me/app:pull,push");
        let (scheme, p) = parse_challenge(r#"Basic realm="Registry Realm""#);
        assert_eq!(scheme, "Basic");
        assert_eq!(p["realm"], "Registry Realm");
    }

    #[test]
    fn finds_the_pulled_digest() {
        let d = "a".repeat(64);
        let log = format!(
            "pulling ghcr.io/me/app:latest\nlatest: Pulling from me/app\nDigest: sha256:{d}\nStatus: Downloaded newer image for ghcr.io/me/app:latest\nstarted facade-app"
        );
        assert_eq!(pulled_digest(&log), Some(format!("sha256:{d}")));
        assert_eq!(pulled_digest("nothing here"), None);
    }

    const INDEX_DIGEST: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    /// A registry that wants a bearer token from its own token service, which
    /// wants the login `me:secret`.
    async fn mock() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let realm = format!("http://{addr}/token");
        let authorized = |h: &HeaderMap| {
            h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer t0k")
        };
        let challenge = move || {
            (
                StatusCode::UNAUTHORIZED,
                [(
                    "www-authenticate",
                    format!(r#"Bearer realm="{realm}",service="mock""#),
                )],
            )
                .into_response()
        };
        let manifests = {
            let challenge = challenge.clone();
            move |Path((repo, name, reference)): Path<(String, String, String)>, h: HeaderMap| {
                let challenge = challenge.clone();
                async move {
                    if !authorized(&h) {
                        return challenge();
                    }
                    if format!("{repo}/{name}") != "me/app" {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    let body = match reference.as_str() {
                        "latest" => serde_json::json!({
                            "mediaType": "application/vnd.oci.image.index.v1+json",
                            "manifests": [
                                {"digest": "sha256:att", "platform": {"os": "unknown", "architecture": "unknown"}},
                                {"digest": "sha256:arm", "platform": {"os": "linux", "architecture": "arm64"}},
                            ]
                        }),
                        "sha256:arm" => serde_json::json!({"config": {"digest": "sha256:cfg"}}),
                        _ => return StatusCode::NOT_FOUND.into_response(),
                    };
                    ([("docker-content-digest", INDEX_DIGEST)], axum::Json(body)).into_response()
                }
            }
        };
        let blobs = move |h: HeaderMap| async move {
            if !authorized(&h) {
                return challenge();
            }
            axum::Json(serde_json::json!({"config": {"Labels": {
                "org.opencontainers.image.source": "https://github.com/me/app"
            }}}))
            .into_response()
        };
        let token = |Query(q): Query<HashMap<String, String>>, h: HeaderMap| async move {
            let basic = base64::engine::general_purpose::STANDARD.encode("me:secret");
            let ok = h.get("authorization").and_then(|v| v.to_str().ok())
                == Some(format!("Basic {basic}").as_str())
                && q.get("scope").map(String::as_str) == Some("repository:me/app:pull")
                && q.get("service").map(String::as_str) == Some("mock");
            if ok {
                axum::Json(serde_json::json!({"token": "t0k"})).into_response()
            } else {
                StatusCode::UNAUTHORIZED.into_response()
            }
        };
        let app = axum::Router::new()
            .route("/v2/{repo}/{name}/manifests/{reference}", get(manifests))
            .route("/v2/me/app/blobs/sha256:cfg", get(blobs))
            .route("/token", get(token));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("127.0.0.1:{}", addr.port())
    }

    #[tokio::test]
    async fn asks_a_registry() {
        let host = mock().await;
        let registry = Registry::default();
        let login = RegistryLogin {
            host: host.clone(),
            username: "me".into(),
            secret: "secret".into(),
        };
        let image = format!("{host}/me/app:latest");
        assert_eq!(
            registry.digest(&image, Some(&login)).await.unwrap(),
            INDEX_DIGEST
        );
        assert_eq!(
            registry
                .source(&image, Some(&login))
                .await
                .unwrap()
                .as_deref(),
            Some("https://github.com/me/app")
        );
        let anonymous = registry.digest(&image, None).await.unwrap_err().to_string();
        assert!(
            anonymous.contains("refused anonymous access"),
            "{anonymous}"
        );
        let missing = registry
            .digest(&format!("{host}/me/other:latest"), Some(&login))
            .await
            .unwrap_err()
            .to_string();
        assert!(missing.contains("refused"), "{missing}");
    }
}
