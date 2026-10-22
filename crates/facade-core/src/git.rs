//! Git repository URLs: where a repository lives, which credential covers it, and
//! the URL to fetch it with once a credential is chosen.

/// A repository on a network host. Local paths and `file://` URLs have none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoLocation {
    /// Lowercase host name, without the port.
    pub host: String,
    pub port: Option<u16>,
    /// `owner/repo`, as written (some hosts are case-sensitive), without `.git`.
    pub path: String,
    pub ssh: bool,
}

impl RepoLocation {
    /// What scopes are matched against: `github.com/owner/repo`, lowercase.
    pub fn key(&self) -> String {
        format!("{}/{}", self.host, self.path.to_lowercase())
    }

    /// The repository's page in a browser (the hosts' usual layout).
    pub fn web_url(&self) -> String {
        format!("https://{}/{}", self.host, self.path)
    }

    pub fn https_url(&self) -> String {
        format!("https://{}/{}.git", self.host, self.path)
    }

    pub fn ssh_url(&self) -> String {
        match self.port.filter(|_| self.ssh) {
            Some(port) => format!("ssh://git@{}:{port}/{}.git", self.host, self.path),
            None => format!("git@{}:{}.git", self.host, self.path),
        }
    }

    /// The page where a deploy key for this repository is added, on hosts facade
    /// knows.
    pub fn deploy_key_page(&self) -> Option<String> {
        match self.host.as_str() {
            "github.com" => Some(format!(
                "https://github.com/{}/settings/keys/new",
                self.path
            )),
            "gitlab.com" => Some(format!(
                "https://gitlab.com/{}/-/settings/repository#js-deploy-keys-settings",
                self.path
            )),
            "bitbucket.org" => Some(format!(
                "https://bitbucket.org/{}/admin/access-keys/",
                self.path
            )),
            _ => None,
        }
    }
}

fn clean_path(path: &str) -> Option<String> {
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    (!path.is_empty() && !path.split('/').any(|p| p.is_empty() || p == ".."))
        .then(|| path.to_string())
}

fn split_port(host: &str) -> Option<(String, Option<u16>)> {
    let (host, port) = match host.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() => (h, Some(p.parse().ok()?)),
        Some((h, _)) => (h, None),
        None => (host, None),
    };
    let ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    ok.then(|| (host.to_ascii_lowercase(), port))
}

/// Where `url` points, for `https://`, `http://`, `ssh://`, `git://` and
/// `[user@]host:path` URLs.
pub fn parse(url: &str) -> Option<RepoLocation> {
    let url = url.trim();
    if let Some((scheme, rest)) = url.split_once("://") {
        let ssh = match scheme.to_ascii_lowercase().as_str() {
            "https" | "http" | "git" => false,
            "ssh" | "git+ssh" | "ssh+git" => true,
            _ => return None,
        };
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let (host, port) = split_port(host)?;
        return Some(RepoLocation {
            host,
            port,
            path: clean_path(path)?,
            ssh,
        });
    }
    // scp-like syntax: `git@github.com:owner/repo.git`.
    let (authority, path) = url.split_once(':')?;
    if authority.contains('/') || path.starts_with("//") {
        return None;
    }
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, _) = split_port(host)?;
    if !host.contains('.') && host != "localhost" {
        return None;
    }
    Some(RepoLocation {
        host,
        port: None,
        path: clean_path(path)?,
        ssh: true,
    })
}

/// Whether the URL carries a password or token (`https://user:token@…`,
/// `https://token@…`). A plain SSH user such as `git@` is fine.
pub fn has_embedded_credentials(url: &str) -> bool {
    let Some((scheme, rest)) = url.trim().split_once("://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    let Some((userinfo, _)) = authority.rsplit_once('@') else {
        return false;
    };
    match scheme.to_ascii_lowercase().as_str() {
        "ssh" | "git+ssh" | "ssh+git" => userinfo.contains(':'),
        _ => true,
    }
}

/// `github.com/owner` from a URL or a written scope, lowercase and without `.git`.
pub fn normalize_scope(input: &str) -> Result<String, String> {
    let input = input.trim();
    if let Some(location) = parse(input) {
        return Ok(location.key());
    }
    let bare = input.trim_matches('/');
    let bare = bare.strip_suffix(".git").unwrap_or(bare);
    let (host, path) = bare.split_once('/').unwrap_or((bare, ""));
    let host_ok = (host.contains('.') || host == "localhost")
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    if !host_ok || path.split('/').any(|p| p == "..") || path.contains("//") {
        return Err(format!(
            "`{input}`: give a repository URL or a prefix like github.com/my-org"
        ));
    }
    Ok(if path.is_empty() {
        host.to_ascii_lowercase()
    } else {
        format!("{}/{}", host.to_ascii_lowercase(), path.to_lowercase())
    })
}

/// Whether a credential scoped to `scope` covers the repository.
pub fn scope_covers(scope: &str, location: &RepoLocation) -> bool {
    let key = location.key();
    key == scope || key.starts_with(&format!("{scope}/"))
}

/// Host keys of the big forges, so SSH clones work without trusting a first
/// connection. Other hosts are remembered on first use.
pub const KNOWN_HOSTS: &str = "\
github.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl
github.com ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBEmKSENjQEezOmxkZMy7opKgwFB9nkt5YRrYMjNuG5N87uRgg6CLrbo5wAdT/y6v0mKV0U2w0WZ2YB/++Tpockg=
gitlab.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAfuCHKVTjquxvt6CM6tdG4SLp1Btn/nOeHHE5UOzRdf
bitbucket.org ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIazEu89wgQZ4bqs3d63QSMzYVa0MuJ2e2gKTKqu+UUO
";

/// A full commit hash.
pub fn is_full_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        for url in [
            "https://github.com/Sigma-studios/facade",
            "https://github.com/Sigma-studios/facade.git",
            "https://GitHub.com/Sigma-studios/facade/",
            "git@github.com:Sigma-studios/facade.git",
            "ssh://git@github.com/Sigma-studios/facade",
        ] {
            let l = parse(url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!(l.key(), "github.com/sigma-studios/facade", "{url}");
            assert_eq!(l.path, "Sigma-studios/facade");
            assert_eq!(
                l.web_url(),
                "https://github.com/Sigma-studios/facade",
                "{url}"
            );
        }
        let l = parse("ssh://git@git.example.com:2222/team/app.git").unwrap();
        assert_eq!(l.ssh_url(), "ssh://git@git.example.com:2222/team/app.git");
        assert_eq!(l.https_url(), "https://git.example.com/team/app.git");
        assert_eq!(
            parse("https://github.com/me/repo").unwrap().ssh_url(),
            "git@github.com:me/repo.git"
        );
        assert!(parse("/srv/repo").is_none());
        assert!(parse("file:///srv/repo").is_none());
        assert!(parse("./repo").is_none());
    }

    #[test]
    fn spots_credentials() {
        assert!(has_embedded_credentials(
            "https://x:ghp_abc@github.com/me/r"
        ));
        assert!(has_embedded_credentials("https://ghp_abc@github.com/me/r"));
        assert!(!has_embedded_credentials("https://github.com/me/r"));
        assert!(!has_embedded_credentials("ssh://git@github.com/me/r"));
        assert!(!has_embedded_credentials("git@github.com:me/r.git"));
        assert!(has_embedded_credentials("ssh://git:pw@github.com/me/r"));
    }

    #[test]
    fn scopes() {
        assert_eq!(
            normalize_scope("https://github.com/Me/Repo.git").unwrap(),
            "github.com/me/repo"
        );
        assert_eq!(normalize_scope("github.com/Me/").unwrap(), "github.com/me");
        assert_eq!(normalize_scope("gitlab.com").unwrap(), "gitlab.com");
        assert!(normalize_scope("not a scope").is_err());
        let repo = parse("git@github.com:me/repo.git").unwrap();
        assert!(scope_covers("github.com/me", &repo));
        assert!(scope_covers("github.com/me/repo", &repo));
        assert!(scope_covers("github.com", &repo));
        assert!(!scope_covers("github.com/m", &repo));
        assert!(!scope_covers("github.com/me/repo2", &repo));
    }
}
