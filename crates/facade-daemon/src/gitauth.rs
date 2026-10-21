//! Running git against repositories that may be private, on the server (scans,
//! resolving the commit to deploy) and on agents (builds).
//!
//! Credentials never appear in URLs or arguments: an SSH key is written to a
//! private temporary file and named in `GIT_SSH_COMMAND`; a token goes in an
//! `Authorization` header set through `GIT_CONFIG_*` variables, scoped to its host.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use facade_core::proto::GitAuth;
use tokio::process::Command;

/// What `git` needs to reach one repository.
pub struct Access {
    /// The URL to fetch: rewritten to SSH for keys and to HTTPS for tokens.
    pub url: String,
    envs: Vec<(String, String)>,
    /// Holds the key file; removed on drop.
    scratch: Option<PathBuf>,
}

impl Drop for Access {
    fn drop(&mut self) {
        if let Some(dir) = &self.scratch {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

impl Access {
    /// Prepares access to `repo`. `state_dir` keeps host keys learned on first use
    /// (`known_hosts`) and the short-lived key files.
    pub fn new(repo: &str, auth: Option<&GitAuth>, state_dir: &Path) -> anyhow::Result<Self> {
        crate::tls::create_private_dir(state_dir)?;
        let pinned = state_dir.join("known_hosts.pinned");
        if std::fs::read_to_string(&pinned).ok().as_deref() != Some(facade_core::git::KNOWN_HOSTS) {
            std::fs::write(&pinned, facade_core::git::KNOWN_HOSTS)?;
        }
        let learned = state_dir.join("known_hosts");
        let location = facade_core::git::parse(repo);
        let mut envs = vec![
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
            // No credential helper of the machine's user gets involved.
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ];
        let mut config: Vec<(String, String)> = vec![];
        let mut scratch = None;
        let mut ssh = format!(
            "ssh -o BatchMode=yes -o ConnectTimeout=20 -o StrictHostKeyChecking=accept-new \
             -o UserKnownHostsFile=\"{} {}\"",
            learned.display(),
            pinned.display()
        );
        let url = match (auth, &location) {
            (Some(GitAuth::SshKey { private_key }), Some(location)) => {
                let dir = state_dir.join(format!("key-{}", uuid::Uuid::new_v4()));
                crate::tls::create_private_dir(&dir)?;
                let key = dir.join("id");
                let mut text = private_key.trim().to_string();
                text.push('\n');
                crate::tls::write_private(&key, text.as_bytes())?;
                scratch = Some(dir);
                ssh += &format!(
                    " -i {} -o IdentitiesOnly=yes -o IdentityAgent=none",
                    quote(&key)
                );
                if location.ssh {
                    repo.to_string()
                } else {
                    location.ssh_url()
                }
            }
            (
                Some(GitAuth::Token {
                    username, token, ..
                }),
                Some(location),
            ) => {
                let basic =
                    base64::engine::general_purpose::STANDARD.encode(format!("{username}:{token}"));
                // git matches the header's URL port too (self-hosted forges often
                // serve HTTPS on another port).
                let host = match location.port.filter(|_| !location.ssh) {
                    Some(port) => format!("{}:{port}", location.host),
                    None => location.host.clone(),
                };
                config.push((
                    format!("http.https://{host}/.extraHeader"),
                    format!("Authorization: Basic {basic}"),
                ));
                if location.ssh {
                    location.https_url()
                } else {
                    repo.to_string()
                }
            }
            _ => repo.to_string(),
        };
        envs.push(("GIT_SSH_COMMAND".into(), ssh));
        envs.push(("GIT_CONFIG_COUNT".into(), config.len().to_string()));
        for (i, (k, v)) in config.into_iter().enumerate() {
            envs.push((format!("GIT_CONFIG_KEY_{i}"), k));
            envs.push((format!("GIT_CONFIG_VALUE_{i}"), v));
        }
        Ok(Self { url, envs, scratch })
    }

    /// A `git` command that can reach the repository.
    pub fn git(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.envs(self.envs.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        cmd
    }

    /// The commit `reference` (a branch, a tag, or `HEAD` for the default branch)
    /// points to.
    pub async fn resolve(&self, reference: Option<&str>) -> anyhow::Result<String> {
        let reference = reference.unwrap_or("HEAD");
        let mut cmd = self.git();
        cmd.args(["ls-remote", "--", &self.url, reference]);
        let out = run(cmd, Duration::from_secs(60)).await?;
        let refs: Vec<(&str, &str)> = out.lines().filter_map(|l| l.split_once('\t')).collect();
        let pick = |name: &str| refs.iter().find(|(_, r)| *r == name).map(|(s, _)| *s);
        let found = if reference == "HEAD" {
            pick("HEAD")
        } else {
            pick(&format!("refs/heads/{reference}"))
                .or_else(|| pick(&format!("refs/tags/{reference}^{{}}")))
                .or_else(|| pick(&format!("refs/tags/{reference}")))
                .or_else(|| pick(reference))
        };
        match found {
            Some(sha) => Ok(sha.to_string()),
            None => bail!("no branch or tag `{reference}` in the repository"),
        }
    }
}

impl Access {
    /// A shallow checkout of exactly `commit` in `dest` (no submodules): what the
    /// server reads a stack's file and build contexts from. Returns its subject.
    pub async fn checkout(&self, commit: &str, dest: &Path) -> anyhow::Result<String> {
        tokio::fs::create_dir_all(dest).await?;
        let git = |args: &[&str]| {
            let mut cmd = self.git();
            cmd.arg("-C").arg(dest).args(args);
            cmd
        };
        let minute = Duration::from_secs(60);
        run(git(&["init", "-q"]), minute).await?;
        run(git(&["remote", "add", "--", "origin", &self.url]), minute).await?;
        let shallow = run(
            git(&["fetch", "-q", "--depth", "1", "origin", commit]),
            Duration::from_secs(180),
        )
        .await;
        if shallow.is_err() {
            run(git(&["fetch", "-q", "origin"]), Duration::from_secs(600)).await?;
            run(git(&["checkout", "-q", "--detach", commit]), minute)
                .await
                .with_context(|| format!("commit {commit} is not in the repository"))?;
        } else {
            run(git(&["checkout", "-q", "--detach", "FETCH_HEAD"]), minute).await?;
        }
        Ok(run(git(&["log", "-1", "--format=%s"]), minute)
            .await?
            .trim()
            .to_string())
    }

    /// Hash of a file or directory of the checkout in `dir` (`.` for the root): it
    /// changes exactly when its content does.
    pub async fn object_id(dir: &Path, path: &str) -> anyhow::Result<String> {
        let spec = match path.trim_matches('/') {
            "" | "." => "HEAD^{tree}".to_string(),
            p => format!("HEAD:{p}"),
        };
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(dir)
            .args(["rev-parse", "--verify", "-q", &spec]);
        Ok(run(cmd, Duration::from_secs(30))
            .await
            .map_err(|_| anyhow::anyhow!("`{path}` is not in the repository"))?
            .trim()
            .to_string())
    }
}

/// Runs a git command, returning stdout, or its last error line.
pub async fn run(mut cmd: Command, timeout: Duration) -> anyhow::Result<String> {
    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .with_context(|| format!("git took longer than {} seconds", timeout.as_secs()))?
        .context("running git (is it installed?)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{}", explain(&stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The useful line of git's error output, reworded when access was refused.
pub fn explain(stderr: &str) -> String {
    let lower = stderr.to_lowercase();
    let denied = [
        "could not read username",
        "authentication failed",
        "permission denied (publickey",
        "repository not found",
        "access denied",
        "403",
        "terminal prompts disabled",
        "could not read from remote repository",
    ]
    .iter()
    .any(|m| lower.contains(m))
        || (lower.contains("repository '") && lower.contains("' not found"));
    if denied {
        return format!(
            "{} this repository: it is private or does not exist. Add a deploy key or an \
             access token for it (Settings → Git access, or `facade git key`)",
            facade_core::api::NO_ACCESS
        );
    }
    if lower.contains("host key verification failed") {
        return "the git server's host key changed since facade first connected (see known_hosts in the data directory)".into();
    }
    stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("git failed")
        .trim_start_matches("fatal: ")
        .to_string()
}

/// A Docker CLI `config.json` with the given registry logins, for `docker build`
/// to pull private base images.
pub fn docker_config(registries: &[facade_core::proto::RegistryLogin]) -> Option<String> {
    let mut auths = serde_json::Map::new();
    for r in registries {
        let key = if r.host == "docker.io" {
            "https://index.docker.io/v1/".to_string()
        } else {
            r.host.clone()
        };
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", r.username, r.secret));
        auths.insert(key, serde_json::json!({ "auth": basic }));
    }
    (!auths.is_empty()).then(|| serde_json::json!({ "auths": auths }).to_string())
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn writes_docker_config() {
        let login = facade_core::proto::RegistryLogin {
            host: "ghcr.io".into(),
            username: "me".into(),
            secret: "tok".into(),
        };
        let config = docker_config(&[login]).unwrap();
        assert!(config.contains("\"ghcr.io\":{\"auth\":\"bWU6dG9r\"}"));
        assert!(docker_config(&[]).is_none());
    }
}
