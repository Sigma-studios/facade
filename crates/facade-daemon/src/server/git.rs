//! Git credentials (deploy keys and access tokens), stored encrypted, and the git
//! operations the server runs itself: resolving the commit to deploy.

use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use facade_core::api::{CreateGitCredential, GitCredentialKind, GitCredentialView, NewGitSecret};
use facade_core::proto::GitAuth;
use facade_core::{Source, validate_name};
use rand::RngCore;

use super::Server;
use super::api::{ApiError, User};
use super::store::GitCredentialRow;
use crate::gitauth::Access;

/// The credential covering `repo`: the one with the most specific scope.
pub fn credential_for(server: &Server, repo: &str) -> anyhow::Result<Option<(String, GitAuth)>> {
    let Some(location) = facade_core::git::parse(repo) else {
        return Ok(None);
    };
    let best = server
        .store
        .list_git_credentials()?
        .into_iter()
        .filter(|c| facade_core::git::scope_covers(&c.scope, &location))
        .max_by_key(|c| c.scope.len());
    let Some(row) = best else {
        return Ok(None);
    };
    let secret = server.secrets.open_string(&row.sealed_secret)?;
    let auth = match row.kind {
        GitCredentialKind::SshKey => GitAuth::SshKey {
            private_key: secret,
        },
        GitCredentialKind::Token => GitAuth::Token {
            scope: row.scope.clone(),
            username: row
                .username
                .clone()
                .unwrap_or_else(|| "x-access-token".into()),
            token: secret,
        },
    };
    Ok(Some((row.name, auth)))
}

/// Git access to `repo` from the server, with its credential if one covers it.
pub fn access(server: &Server, repo: &str) -> anyhow::Result<(Access, Option<String>)> {
    let credential = credential_for(server, repo)?;
    let access = Access::new(
        repo,
        credential.as_ref().map(|(_, a)| a),
        &server.cfg.data_dir.join("git"),
    )?;
    Ok((access, credential.map(|(name, _)| name)))
}

/// The commit a deploy builds: `requested` if it is a full hash, else what the
/// branch or tag (by default the app's branch) points to now.
pub async fn resolve_commit(
    server: &Server,
    source: &Source,
    requested: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let Source::Git { repo, branch, .. } = source else {
        if requested.is_some() {
            bail!("only apps built from git can deploy a given commit");
        }
        return Ok(None);
    };
    let requested = requested.map(str::trim).filter(|r| !r.is_empty());
    if let Some(sha) = requested.filter(|r| facade_core::git::is_full_sha(r)) {
        return Ok(Some(sha.to_ascii_lowercase()));
    }
    if let Some(r) = requested
        && (r.starts_with('-') || r.contains(char::is_whitespace))
    {
        bail!("`{r}` is not a branch, tag or full commit hash");
    }
    let (access, _) = access(server, repo)?;
    let reference = requested.or(branch.as_deref());
    let sha = access.resolve(reference).await.map_err(|e| {
        let short =
            reference.is_some_and(|r| r.len() >= 7 && r.chars().all(|c| c.is_ascii_hexdigit()));
        if short {
            anyhow!("{e} (give the full commit hash, not an abbreviated one)")
        } else {
            e
        }
    })?;
    Ok(Some(sha))
}

fn view(server: &Server, row: GitCredentialRow) -> anyhow::Result<GitCredentialView> {
    let apps = server
        .store
        .list_apps()?
        .into_iter()
        .filter(|a| match &a.spec.source {
            Source::Git { repo, .. } => facade_core::git::parse(repo)
                .is_some_and(|l| facade_core::git::scope_covers(&row.scope, &l)),
            _ => false,
        })
        .map(|a| a.spec.name)
        .collect();
    // A deploy key is added on the page of the one repository it is scoped to.
    let add_key_url = match row.kind {
        GitCredentialKind::SshKey => scope_repository(&row.scope).and_then(|l| l.deploy_key_page()),
        GitCredentialKind::Token => None,
    };
    Ok(GitCredentialView {
        name: row.name,
        kind: row.kind,
        scope: row.scope,
        username: row.username,
        public_key: row.public_key,
        add_key_url,
        created_at: row.created_at,
        apps,
    })
}

/// `github.com/owner/repo` as a repository, when the scope names exactly one.
fn scope_repository(scope: &str) -> Option<facade_core::git::RepoLocation> {
    let (host, path) = scope.split_once('/')?;
    (path.split('/').count() >= 2).then(|| facade_core::git::RepoLocation {
        host: host.to_string(),
        port: None,
        path: path.to_string(),
        ssh: false,
    })
}

/// A new ed25519 key pair: (private key in OpenSSH format, public key line).
fn generate_key(comment: &str) -> anyhow::Result<(String, String)> {
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let keypair = ssh_key::private::Ed25519Keypair::from_seed(&seed);
    let key = ssh_key::PrivateKey::new(ssh_key::private::KeypairData::Ed25519(keypair), comment)
        .map_err(|e| anyhow!("generating key: {e}"))?;
    let private = key
        .to_openssh(ssh_key::LineEnding::LF)
        .map_err(|e| anyhow!("encoding key: {e}"))?;
    let public = key
        .public_key()
        .to_openssh()
        .map_err(|e| anyhow!("encoding key: {e}"))?;
    Ok((private.to_string(), public))
}

/// Checks a pasted private key and returns its public half.
fn import_key(private: &str) -> anyhow::Result<String> {
    let key = ssh_key::PrivateKey::from_openssh(private.trim())
        .map_err(|e| anyhow!("not an OpenSSH private key: {e}"))?;
    if key.is_encrypted() {
        bail!("the key is protected by a passphrase; give an unencrypted key");
    }
    key.public_key()
        .to_openssh()
        .map_err(|e| anyhow!("encoding key: {e}"))
}

pub async fn list(
    State(server): State<Arc<Server>>,
    _: User,
) -> Result<Json<Vec<GitCredentialView>>, ApiError> {
    let rows = server.store.list_git_credentials()?;
    Ok(Json(
        rows.into_iter()
            .map(|r| view(&server, r))
            .collect::<anyhow::Result<_>>()?,
    ))
}

pub async fn create(
    State(server): State<Arc<Server>>,
    _: User,
    Json(req): Json<CreateGitCredential>,
) -> Result<(StatusCode, Json<GitCredentialView>), ApiError> {
    validate_name("credential name", &req.name).map_err(ApiError::bad_request)?;
    let scope = facade_core::git::normalize_scope(&req.scope).map_err(ApiError::bad_request)?;
    if server
        .store
        .list_git_credentials()?
        .iter()
        .any(|c| c.name == req.name)
    {
        return Err(ApiError::conflict(format!(
            "a credential named {} exists",
            req.name
        )));
    }
    let (kind, username, secret, public_key) = match req.secret {
        NewGitSecret::SshKey { private_key } => {
            let (private, public) = match private_key.filter(|k| !k.trim().is_empty()) {
                Some(private) => {
                    let public = import_key(&private).map_err(ApiError::bad_request)?;
                    (private.trim().to_string(), public)
                }
                None => generate_key(&format!("facade {}", req.name))?,
            };
            (GitCredentialKind::SshKey, None, private, Some(public))
        }
        NewGitSecret::Token { username, token } => {
            let token = token.trim().to_string();
            if token.is_empty() || token.contains(char::is_whitespace) {
                return Err(ApiError::bad_request("the token is empty or malformed"));
            }
            let username = username
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty());
            if username.as_deref().is_some_and(|u| u.contains(':')) {
                return Err(ApiError::bad_request("the username cannot contain `:`"));
            }
            (GitCredentialKind::Token, username, token, None)
        }
    };
    let row = GitCredentialRow {
        name: req.name,
        kind,
        scope,
        username,
        sealed_secret: server.secrets.seal(secret.as_bytes()),
        public_key,
        created_at: facade_core::now(),
    };
    server
        .store
        .put_git_credential(&row)
        .context("saving the credential")?;
    // Running apps whose repository it covers fetch with it from now on.
    let covered = view(&server, row)?;
    for app in &covered.apps {
        if let Err(e) = server.push(app).await {
            tracing::warn!(%app, "push failed: {e:#}");
        }
    }
    Ok((StatusCode::CREATED, Json(covered)))
}

pub async fn delete(
    State(server): State<Arc<Server>>,
    _: User,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    if !server.store.delete_git_credential(&name)? {
        return Err(ApiError::not_found(format!("no credential named {name}")));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_and_reads_keys() {
        let (private, public) = generate_key("facade test").unwrap();
        assert!(private.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(public.starts_with("ssh-ed25519 "));
        assert_eq!(import_key(&private).unwrap(), public);
        assert!(import_key("nope").is_err());
        let repo = scope_repository("github.com/me/repo").unwrap();
        assert_eq!(
            repo.deploy_key_page().unwrap(),
            "https://github.com/me/repo/settings/keys/new"
        );
        assert!(scope_repository("github.com/me").is_none());
    }
}
