//! What an app is: where its code comes from, where it runs, and how it is reached.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SpecError(pub String);

fn err<T>(msg: impl Into<String>) -> Result<T, SpecError> {
    Err(SpecError(msg.into()))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSpec {
    pub name: String,
    /// The node (server) the app runs on.
    pub node: String,
    pub source: Source,
    /// Port inside the container that HTTP traffic for `domains` is sent to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_port: Option<u16>,
    /// Hostnames routed to this app by the ingress, with automatic TLS.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
    /// Raw ports published on the node's public interface (port remapping).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortMap>,
    /// Share the node's network stack. Needed by things like TURN relays that
    /// advertise their own ports; makes `ports` meaningless.
    #[serde(default, skip_serializing_if = "is_false")]
    pub host_network: bool,
    #[serde(default, skip_serializing_if = "Limits::is_empty")]
    pub limits: Limits,
    /// Stopped apps keep their config but have no running container.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}
fn is_false(b: &bool) -> bool {
    !*b
}

impl AppSpec {
    pub fn is_static(&self) -> bool {
        matches!(
            self.source,
            Source::Git {
                build: Build::Static { .. },
                ..
            }
        )
    }

    pub fn validate(&self) -> Result<(), SpecError> {
        validate_name("app name", &self.name)?;
        validate_name("node name", &self.node)?;
        for domain in &self.domains {
            validate_domain(domain)?;
        }
        if self.is_static() {
            if !self.ports.is_empty() || self.host_network || self.http_port.is_some() {
                return err("static sites cannot have ports, http_port or host_network");
            }
        } else if !self.domains.is_empty() && self.http_port.is_none() {
            return err("apps with domains need http_port (the port the app listens on)");
        }
        for port in &self.ports {
            port.validate()?;
        }
        if let Some(cpus) = self.limits.cpus
            && !(cpus > 0.0 && cpus <= 1024.0)
        {
            return err("limits.cpus must be > 0");
        }
        match &self.source {
            Source::Image { image } => {
                if image.trim().is_empty() || image.starts_with('-') {
                    return err("image must be a non-empty image reference");
                }
            }
            Source::Git {
                repo,
                branch,
                build,
            } => {
                if repo.trim().is_empty() || repo.starts_with('-') {
                    return err("repo must be a git URL");
                }
                if let Some(branch) = branch
                    && (branch.is_empty()
                        || branch.starts_with('-')
                        || branch.contains(char::is_whitespace))
                {
                    return err("branch is not a valid git ref");
                }
                match build {
                    Build::Dockerfile {
                        dockerfile,
                        context,
                    } => {
                        validate_repo_path(dockerfile)?;
                        validate_repo_path(context)?;
                    }
                    Build::Rust {
                        package,
                        bin,
                        features,
                        ..
                    } => {
                        let word = |s: &str| {
                            !s.is_empty()
                                && s.chars()
                                    .all(|c| c.is_ascii_alphanumeric() || "-_/".contains(c))
                        };
                        if !word(bin)
                            || !package.as_deref().is_none_or(word)
                            || !features.iter().all(|f| word(f))
                        {
                            return err("rust package, bin and features must be plain identifiers");
                        }
                    }
                    Build::Static { dir } => validate_repo_path(dir)?,
                }
            }
        }
        Ok(())
    }
}

/// A path inside the repository: relative, without `..`.
fn validate_repo_path(path: &str) -> Result<(), SpecError> {
    let p = std::path::Path::new(path);
    if path.is_empty()
        || p.is_absolute()
        || p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return err(format!(
            "`{path}` must be a relative path inside the repository"
        ));
    }
    Ok(())
}

pub fn validate_name(what: &str, name: &str) -> Result<(), SpecError> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if ok {
        Ok(())
    } else {
        err(format!(
            "{what} `{name}` must be 1-40 chars of a-z, 0-9 and inner dashes"
        ))
    }
}

pub fn validate_domain(domain: &str) -> Result<(), SpecError> {
    let ok = !domain.is_empty()
        && domain.len() <= 253
        && domain.split('.').count() >= 2
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        });
    if ok {
        Ok(())
    } else {
        err(format!("`{domain}` is not a valid lowercase domain name"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Source {
    /// A prebuilt image from a registry.
    Image { image: String },
    /// A git repository, built on the node.
    Git {
        repo: String,
        /// Defaults to the repository's default branch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(default)]
        build: Build,
    },
}

/// `nginx` → `nginx:latest`; references with a tag or digest are kept. Without a
/// tag, Docker's pull API fetches every tag of the repository.
pub fn image_with_tag(image: &str) -> String {
    let last = image.rsplit('/').next().unwrap_or(image);
    if last.contains(':') || last.contains('@') {
        image.to_string()
    } else {
        format!("{image}:latest")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Build {
    /// `docker build` with a Dockerfile from the repository.
    Dockerfile {
        #[serde(default = "default_dockerfile")]
        dockerfile: String,
        #[serde(default = "default_context")]
        context: String,
    },
    /// A cargo binary, built in a generated multi-stage Dockerfile.
    Rust {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        package: Option<String>,
        bin: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        no_default_features: bool,
    },
    /// Files served as-is from a directory of the repository. No container.
    Static {
        #[serde(default = "default_context")]
        dir: String,
    },
}

fn default_dockerfile() -> String {
    "Dockerfile".into()
}
fn default_context() -> String {
    ".".into()
}

impl Default for Build {
    fn default() -> Self {
        Build::Dockerfile {
            dockerfile: default_dockerfile(),
            context: default_context(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

/// `host[-host_end]:container[/proto]`, e.g. `3478:3478/udp` or `49160-49260:49160-49260/udp`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortMap {
    pub host: u16,
    pub container: u16,
    /// Number of consecutive ports, for ranges.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub count: u16,
    #[serde(default)]
    pub protocol: Protocol,
}

fn one() -> u16 {
    1
}
fn is_one(n: &u16) -> bool {
    *n == 1
}

impl PortMap {
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.count == 0 || self.host == 0 || self.container == 0 {
            return err("ports must be non-zero");
        }
        if u32::from(self.host) + u32::from(self.count) - 1 > 65535
            || u32::from(self.container) + u32::from(self.count) - 1 > 65535
        {
            return err("port range overflows 65535");
        }
        Ok(())
    }
}

impl FromStr for PortMap {
    type Err = SpecError;

    fn from_str(s: &str) -> Result<Self, SpecError> {
        let (ports, protocol) = match s.split_once('/') {
            Some((p, "tcp")) => (p, Protocol::Tcp),
            Some((p, "udp")) => (p, Protocol::Udp),
            Some((_, other)) => return err(format!("unknown protocol `{other}`")),
            None => (s, Protocol::Tcp),
        };
        let (host, container) = ports.split_once(':').unwrap_or((ports, ports));
        let range = |r: &str| -> Result<(u16, u16), SpecError> {
            let parse = |p: &str| {
                p.trim()
                    .parse::<u16>()
                    .map_err(|_| SpecError(format!("`{p}` is not a port")))
            };
            match r.split_once('-') {
                Some((a, b)) => {
                    let (a, b) = (parse(a)?, parse(b)?);
                    if b < a {
                        return err(format!("range `{r}` is backwards"));
                    }
                    Ok((a, b - a + 1))
                }
                None => Ok((parse(r)?, 1)),
            }
        };
        let (host, host_count) = range(host)?;
        let (container, container_count) = range(container)?;
        if host_count != container_count {
            return err("host and container ranges differ in length");
        }
        let map = PortMap {
            host,
            container,
            count: host_count,
            protocol,
        };
        map.validate()?;
        Ok(map)
    }
}

impl fmt::Display for PortMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let proto = match self.protocol {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        };
        if self.count == 1 {
            write!(f, "{}:{}/{proto}", self.host, self.container)
        } else {
            let n = self.count - 1;
            write!(
                f,
                "{}-{}:{}-{}/{proto}",
                self.host,
                self.host + n,
                self.container,
                self.container + n
            )
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<f64>,
}

impl Limits {
    pub fn is_empty(&self) -> bool {
        self.memory_mb.is_none() && self.cpus.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_maps_round_trip() {
        for s in [
            "3478:3478/udp",
            "49160-49260:49160-49260/udp",
            "8080:80/tcp",
        ] {
            assert_eq!(s.parse::<PortMap>().unwrap().to_string(), s);
        }
        assert_eq!(
            "9090".parse::<PortMap>().unwrap().to_string(),
            "9090:9090/tcp"
        );
        assert!("1-3:1-2".parse::<PortMap>().is_err());
    }

    #[test]
    fn tags_images() {
        assert_eq!(image_with_tag("traefik/whoami"), "traefik/whoami:latest");
        assert_eq!(
            image_with_tag("localhost:5000/app"),
            "localhost:5000/app:latest"
        );
        assert_eq!(image_with_tag("nginx:1.27"), "nginx:1.27");
        assert_eq!(image_with_tag("nginx@sha256:abc"), "nginx@sha256:abc");
    }

    #[test]
    fn validates_domains() {
        assert!(validate_domain("app.example.com").is_ok());
        assert!(validate_domain("App.example.com").is_err());
        assert!(validate_domain("localhost").is_err());
    }
}
