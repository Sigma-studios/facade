//! Logins for git hosts and registries, handed to `git` and `docker build`
//! without putting secrets on a command line (where `ps` would show them).

use base64::Engine;
use facade_core::proto::{Credential, CredentialKind};

fn basic(c: &Credential) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", c.username, c.secret))
}

/// Environment for `git` that sends the login for `repo`'s host with every HTTPS
/// request to it (git ≥ 2.31 reads config from `GIT_CONFIG_*`).
pub fn git_env<'a>(
    repo: &str,
    credentials: impl IntoIterator<Item = &'a Credential>,
) -> Vec<(String, String)> {
    let Some(host) = facade_core::git_host(repo) else {
        return vec![];
    };
    let Some(c) = credentials
        .into_iter()
        .find(|c| c.kind == CredentialKind::Git && c.host.eq_ignore_ascii_case(&host))
    else {
        return vec![];
    };
    vec![
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        (
            "GIT_CONFIG_KEY_0".into(),
            format!("http.https://{host}/.extraHeader"),
        ),
        (
            "GIT_CONFIG_VALUE_0".into(),
            format!("Authorization: Basic {}", basic(c)),
        ),
    ]
}

/// A Docker CLI `config.json` with every registry login, for `docker build` to
/// pull private base images.
pub fn docker_config<'a>(credentials: impl IntoIterator<Item = &'a Credential>) -> Option<String> {
    let mut auths = serde_json::Map::new();
    for c in credentials {
        if c.kind != CredentialKind::Registry {
            continue;
        }
        let key = if c.host == "docker.io" {
            "https://index.docker.io/v1/".to_string()
        } else {
            c.host.clone()
        };
        auths.insert(key, serde_json::json!({ "auth": basic(c) }));
    }
    (!auths.is_empty()).then(|| serde_json::json!({ "auths": auths }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(kind: CredentialKind, host: &str) -> Credential {
        Credential {
            kind,
            host: host.into(),
            username: "me".into(),
            secret: "tok".into(),
        }
    }

    #[test]
    fn picks_the_matching_login() {
        let creds = [
            cred(CredentialKind::Registry, "github.com"),
            cred(CredentialKind::Git, "github.com"),
        ];
        let env = git_env("https://github.com/me/private", &creds);
        assert_eq!(env[1].1, "http.https://github.com/.extraHeader");
        assert_eq!(env[2].1, "Authorization: Basic bWU6dG9r");
        assert!(git_env("https://gitlab.com/me/repo", &creds).is_empty());
        let config = docker_config(&[cred(CredentialKind::Registry, "ghcr.io")]).unwrap();
        assert!(config.contains("\"ghcr.io\":{\"auth\":\"bWU6dG9r\"}"));
        assert!(docker_config(&[cred(CredentialKind::Git, "github.com")]).is_none());
    }
}
