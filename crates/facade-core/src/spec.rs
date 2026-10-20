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
    /// Hostnames routed to this app by the ingress, with automatic TLS. The first
    /// one is the app's main domain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
    /// Hostnames that permanently redirect to the main domain (`www.example.com` →
    /// `example.com`), path and query kept. They get certificates and DNS records
    /// like `domains`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redirects: Vec<String>,
    /// Raw ports published on the node's public interface (port remapping). With
    /// `host_network` nothing is remapped: they declare the ports the app binds
    /// itself, so conflicts are caught before it starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortMap>,
    /// Share the node's network stack. Needed by things like TURN relays that
    /// advertise their own ports.
    #[serde(default, skip_serializing_if = "is_false")]
    pub host_network: bool,
    /// Arguments for the program. With an executable, Rust or Python build they
    /// follow the program; with a Dockerfile or image they replace its `CMD`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Directories that keep their contents across deploys, restarts and
    /// configuration changes. They live on the app's node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<Volume>,
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
        for domain in self.domains.iter().chain(&self.redirects) {
            validate_domain(domain)?;
        }
        let mut hosts = std::collections::HashSet::new();
        for domain in self.domains.iter().chain(&self.redirects) {
            if !hosts.insert(domain) {
                return err(format!("{domain} is listed twice"));
            }
        }
        if !self.redirects.is_empty() && self.domains.is_empty() {
            return err("redirects need a domain to redirect to");
        }
        if self.is_static() {
            if !self.ports.is_empty()
                || self.host_network
                || self.http_port.is_some()
                || !self.args.is_empty()
                || !self.volumes.is_empty()
            {
                return err(
                    "static sites cannot have ports, http_port, host_network, args or volumes",
                );
            }
        } else if !self.domains.is_empty() && self.http_port.is_none() {
            return err("apps with domains need http_port (the port the app listens on)");
        }
        for port in &self.ports {
            port.validate()?;
            if self.host_network && port.host != port.container {
                return err(format!(
                    "{port}: with host_network the app binds its ports itself, so they cannot be remapped"
                ));
            }
        }
        for (i, a) in self.ports.iter().enumerate() {
            for b in &self.ports[i + 1..] {
                if a.protocol == b.protocol && a.overlaps(b) {
                    return err(format!("{a} and {b} overlap"));
                }
            }
        }
        let mut names = std::collections::HashSet::new();
        let mut paths = std::collections::HashSet::new();
        for volume in &self.volumes {
            volume.validate()?;
            if !names.insert(&volume.name) {
                return err(format!("two volumes are named {}", volume.name));
            }
            if !paths.insert(volume.path.trim_end_matches('/')) {
                return err(format!("two volumes are mounted at {}", volume.path));
            }
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
            Source::Executable {
                sha256,
                filename,
                arch,
            } => {
                if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
                    return err("upload an executable first");
                }
                if filename.is_empty() || filename.contains(['/', '\\']) {
                    return err("invalid executable file name");
                }
                if !matches!(arch.as_str(), "x86_64" | "aarch64") {
                    return err(format!("unsupported architecture `{arch}`"));
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
                    Build::Python { version, entry } => {
                        if let Some(version) = version
                            && !valid_python_version(version)
                        {
                            return err(format!("python version `{version}` must look like 3.12"));
                        }
                        PythonEntry::parse(entry)?;
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
    /// A Linux executable uploaded to the server (`POST /api/artifacts`), run in a
    /// minimal Debian image.
    Executable {
        /// SHA-256 of the uploaded file, which is how the server stores it.
        sha256: String,
        filename: String,
        /// CPU architecture the file was built for: `x86_64` or `aarch64`.
        arch: String,
    },
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
    /// A Python program, installed with pip in a generated Dockerfile from
    /// `requirements.txt` and/or `pyproject.toml`.
    Python {
        /// `3.12`; the current stable Python when unset.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        /// What to run: a script (`server.py`), a module (`-m mypkg`) or a command
        /// the project installs (`mybot`).
        entry: String,
    },
    /// Files served as-is from a directory of the repository. No container.
    Static {
        #[serde(default = "default_context")]
        dir: String,
    },
}

/// The Python used when a project does not say.
pub const DEFAULT_PYTHON: &str = "3.13";

fn valid_python_version(v: &str) -> bool {
    let mut parts = v.split('.');
    parts.next() == Some("3")
        && parts
            .next()
            .is_some_and(|m| !m.is_empty() && m.len() <= 2 && m.chars().all(|c| c.is_ascii_digit()))
        && parts.next().is_none()
}

/// How a [`Build::Python`] entry is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PythonEntry {
    /// `python <path>`
    Script(String),
    /// `python -m <module>`
    Module(String),
    /// A console script installed by the project.
    Command(String),
}

impl PythonEntry {
    pub fn parse(entry: &str) -> Result<Self, SpecError> {
        let entry = entry.trim();
        let ident = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
                && !s.starts_with(['-', '.'])
        };
        if let Some(module) = entry.strip_prefix("-m ") {
            let module = module.trim();
            if ident(module) {
                return Ok(PythonEntry::Module(module.to_string()));
            }
        } else if entry.ends_with(".py") {
            validate_repo_path(entry)?;
            return Ok(PythonEntry::Script(entry.to_string()));
        } else if ident(entry) {
            return Ok(PythonEntry::Command(entry.to_string()));
        }
        err(format!(
            "python entry `{entry}`: use a script (server.py), -m module, or a command name"
        ))
    }
}

/// A named directory of an app that outlives its containers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Volume {
    /// Short name, unique within the app (`data`).
    pub name: String,
    /// Where it is mounted in the container (`/data`).
    pub path: String,
}

impl Volume {
    pub fn validate(&self) -> Result<(), SpecError> {
        validate_name("volume name", &self.name)?;
        let p = std::path::Path::new(&self.path);
        if !p.is_absolute()
            || self.path == "/"
            || p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            || self.path.contains([':', ','])
        {
            return err(format!(
                "volume path `{}` must be an absolute path in the container, like /data",
                self.path
            ));
        }
        Ok(())
    }
}

impl FromStr for Volume {
    type Err = SpecError;

    /// `name:/path`, e.g. `data:/data`.
    fn from_str(s: &str) -> Result<Self, SpecError> {
        let (name, path) = s
            .split_once(':')
            .ok_or_else(|| SpecError(format!("`{s}`: expected name:/path, e.g. data:/data")))?;
        let volume = Volume {
            name: name.trim().to_string(),
            path: path.trim().to_string(),
        };
        volume.validate()?;
        Ok(volume)
    }
}

impl fmt::Display for Volume {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.name, self.path)
    }
}

/// The registry an image reference is pulled from: `ghcr.io` for
/// `ghcr.io/me/app:1`, `docker.io` for `nginx`.
pub fn image_registry(image: &str) -> String {
    match image.split_once('/') {
        Some((first, _)) if first.contains(['.', ':']) || first == "localhost" => {
            first.to_ascii_lowercase()
        }
        _ => "docker.io".into(),
    }
}

/// The host of a git URL (`https://github.com/me/repo`, `git@github.com:me/repo`).
pub fn git_host(repo: &str) -> Option<String> {
    let rest = repo.split_once("://").map(|(_, r)| r).unwrap_or(repo);
    let rest = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest);
    let host = rest.split(['/', ':']).next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// Host port ranges intersect (whatever the protocols).
    pub fn overlaps(&self, other: &PortMap) -> bool {
        let end = |p: &PortMap| u32::from(p.host) + u32::from(p.count);
        u32::from(self.host) < end(other) && u32::from(other.host) < end(self)
    }

    /// Whether host port `port` is part of this mapping.
    pub fn contains(&self, port: u16) -> bool {
        port >= self.host && u32::from(port) < u32::from(self.host) + u32::from(self.count)
    }

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

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        })
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
    fn finds_registries_and_git_hosts() {
        assert_eq!(image_registry("nginx"), "docker.io");
        assert_eq!(image_registry("traefik/whoami"), "docker.io");
        assert_eq!(image_registry("ghcr.io/sigma-dev/site:latest"), "ghcr.io");
        assert_eq!(image_registry("localhost:5000/app"), "localhost:5000");
        assert_eq!(
            git_host("https://github.com/me/repo").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            git_host("https://user:tok@gitlab.com/me/repo.git").as_deref(),
            Some("gitlab.com")
        );
        assert_eq!(
            git_host("git@github.com:me/repo.git").as_deref(),
            Some("github.com")
        );
    }

    #[test]
    fn parses_volumes_and_python_entries() {
        let v: Volume = "data:/data".parse().unwrap();
        assert_eq!(v.to_string(), "data:/data");
        assert!("data:relative".parse::<Volume>().is_err());
        assert!("Data:/data".parse::<Volume>().is_err());
        assert!("data:/".parse::<Volume>().is_err());
        assert_eq!(
            PythonEntry::parse("server.py").unwrap(),
            PythonEntry::Script("server.py".into())
        );
        assert_eq!(
            PythonEntry::parse("-m poker").unwrap(),
            PythonEntry::Module("poker".into())
        );
        assert_eq!(
            PythonEntry::parse("mybot").unwrap(),
            PythonEntry::Command("mybot".into())
        );
        assert!(PythonEntry::parse("../x.py").is_err());
        assert!(PythonEntry::parse("rm -rf").is_err());
    }

    fn spec() -> AppSpec {
        AppSpec {
            name: "app".into(),
            node: "main".into(),
            source: Source::Image {
                image: "nginx".into(),
            },
            http_port: Some(80),
            domains: vec!["example.com".into()],
            redirects: vec![],
            ports: vec![],
            host_network: false,
            args: vec![],
            volumes: vec![],
            limits: Limits::default(),
            enabled: true,
        }
    }

    #[test]
    fn validates_specs() {
        assert!(spec().validate().is_ok());
        let mut s = spec();
        s.redirects = vec!["www.example.com".into()];
        assert!(s.validate().is_ok());
        s.redirects = vec!["example.com".into()];
        assert!(s.validate().is_err(), "a domain cannot redirect to itself");
        let mut s = spec();
        s.domains.clear();
        s.http_port = None;
        s.redirects = vec!["www.example.com".into()];
        assert!(s.validate().is_err(), "redirects need a main domain");
        let mut s = spec();
        s.volumes = vec!["a:/data".parse().unwrap(), "b:/data/".parse().unwrap()];
        assert!(s.validate().is_err(), "same mount point");
        let mut s = spec();
        s.ports = vec![
            "3478:3478/udp".parse().unwrap(),
            "3470-3480:3470-3480/udp".parse().unwrap(),
        ];
        assert!(s.validate().is_err(), "overlapping ports");
        let mut s = spec();
        s.host_network = true;
        s.ports = vec!["3478:3478/udp".parse().unwrap()];
        assert!(s.validate().is_ok());
        s.ports = vec!["3479:3478/udp".parse().unwrap()];
        assert!(s.validate().is_err(), "no remapping on the host network");
    }

    #[test]
    fn validates_domains() {
        assert!(validate_domain("app.example.com").is_ok());
        assert!(validate_domain("App.example.com").is_err());
        assert!(validate_domain("localhost").is_err());
    }
}
