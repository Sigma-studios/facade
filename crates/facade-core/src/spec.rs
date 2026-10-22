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
    /// TCP ports the node's entry point takes TLS off before passing the
    /// connection to the app (IMAPS, SMTPS…), with the certificate of the app's
    /// domains.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tls_ports: Vec<TlsPort>,
    /// Share the node's network stack. Needed by things like TURN relays that
    /// advertise their own ports.
    #[serde(default, skip_serializing_if = "is_false")]
    pub host_network: bool,
    #[serde(default, skip_serializing_if = "Limits::is_empty")]
    pub limits: Limits,
    /// Stopped apps keep their config but have no running container.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Replaces the image's command (`CMD`), e.g. to run a worker from the same build.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Run with `sh -c` in a one-off container of the new build before it replaces the
    /// running one (database migrations). A failure stops the deploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_command: Option<String>,
    /// Docker volumes that outlive the container: `name:/path`. They stay on the node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<Volume>,
    /// Apps on the same node this one reaches by name (`db:5432`), over a private
    /// network.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    /// Set on the apps a stack runs, one per service (named `<stack>.<service>`).
    /// The stack decides their settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<StackRef>,
    /// How Docker tells the app is ready. A deploy waits for it (and, without one,
    /// for the container to stay up a few seconds) before counting as done, and
    /// a stack starts a service's dependents only then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthCheck>,
}

/// A Docker health check (`HEALTHCHECK`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthCheck {
    /// `["CMD", "pg_isready"]`, `["CMD-SHELL", "curl -f localhost:8080"]`, or
    /// `["NONE"]` to turn off the image's own.
    pub test: Vec<String>,
    #[serde(default = "two", skip_serializing_if = "is_two")]
    pub interval_secs: u32,
    #[serde(default = "five", skip_serializing_if = "is_five")]
    pub timeout_secs: u32,
    #[serde(default = "thirty", skip_serializing_if = "is_thirty")]
    pub retries: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub start_period_secs: u32,
}

fn two() -> u32 {
    2
}
fn five() -> u32 {
    5
}
fn thirty() -> u32 {
    30
}
fn is_two(n: &u32) -> bool {
    *n == 2
}
fn is_five(n: &u32) -> bool {
    *n == 5
}
fn is_thirty(n: &u32) -> bool {
    *n == 30
}
fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl HealthCheck {
    /// A command run with `sh -c` in the container.
    pub fn shell(command: &str) -> Self {
        HealthCheck {
            test: vec!["CMD-SHELL".into(), command.to_string()],
            interval_secs: 2,
            timeout_secs: 5,
            retries: 30,
            start_period_secs: 0,
        }
    }

    /// Turns off the image's own check.
    pub fn is_disabled(&self) -> bool {
        self.test.first().is_some_and(|t| t == "NONE")
    }

    /// The command, for display: `pg_isready -h 127.0.0.1`.
    pub fn command(&self) -> String {
        match self.test.split_first() {
            Some((kind, rest)) if kind == "CMD-SHELL" => rest.join(" "),
            Some((kind, rest)) if kind == "CMD" => join_command(rest),
            _ => "none".into(),
        }
    }

    pub fn validate(&self) -> Result<(), SpecError> {
        match self.test.first().map(String::as_str) {
            Some("NONE") => Ok(()),
            Some("CMD" | "CMD-SHELL") if self.test.len() > 1 => {
                if self.interval_secs == 0 || self.timeout_secs == 0 {
                    return err("health check interval and timeout must be above 0");
                }
                Ok(())
            }
            _ => err("a health check is [\"CMD\", …], [\"CMD-SHELL\", \"…\"] or [\"NONE\"]"),
        }
    }

    /// Longest it can take to become healthy before Docker gives up.
    pub fn max_wait_secs(&self) -> u64 {
        u64::from(self.start_period_secs)
            + u64::from(self.interval_secs + self.timeout_secs) * u64::from(self.retries + 1)
    }
}

/// Which stack an app is a service of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackRef {
    pub stack: String,
    /// The service's host name on the stack's network (its name in the compose
    /// file, which may contain `_`).
    pub host: String,
}

/// An app name: `name`, or `stack.service` for the services of a stack.
pub fn validate_app_name(name: &str) -> Result<(), SpecError> {
    match name.split_once('.') {
        Some((stack, service)) => {
            validate_name("stack name", stack)?;
            validate_name("service name", service)
        }
        None => validate_name("app name", name),
    }
}

/// A host name on a private network: letters, digits, `-` and `_`.
pub fn validate_host(host: &str) -> Result<(), SpecError> {
    let ok = !host.is_empty()
        && host.len() <= 63
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !host.starts_with('-');
    if ok {
        Ok(())
    } else {
        err(format!("`{host}` is not a valid host name"))
    }
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

    /// The service name, for the apps of a stack.
    pub fn service(&self) -> Option<&str> {
        let stack = self.stack.as_ref()?;
        self.name
            .strip_prefix(stack.stack.as_str())
            .and_then(|s| s.strip_prefix('.'))
    }

    /// Ports the app takes on its node's network: its published ports, or on
    /// the host network the ports it declares and its HTTP port.
    pub fn host_ports(&self) -> Vec<PortMap> {
        let mut out = self.ports.clone();
        if self.host_network
            && let Some(port) = self.http_port
        {
            out.push(PortMap {
                host: port,
                container: port,
                count: 1,
                protocol: Protocol::Tcp,
            });
        }
        out
    }

    /// Ports the node's entry point listens on for the app (its TLS ports), as
    /// TCP mappings of the public port to itself.
    pub fn ingress_ports(&self) -> Vec<PortMap> {
        self.tls_ports
            .iter()
            .map(|t| PortMap {
                host: t.public,
                container: t.public,
                count: 1,
                protocol: Protocol::Tcp,
            })
            .collect()
    }

    /// Name of one of the app's volumes on its node. A stack's services share
    /// their stack's volumes, as Compose services share a project's.
    pub fn volume_name(&self, volume: &Volume) -> String {
        match &self.stack {
            Some(stack) => format!("facade.{}.{}", stack.stack, volume.name),
            None => volume.docker_name(&self.name),
        }
    }

    pub fn validate(&self) -> Result<(), SpecError> {
        match &self.stack {
            Some(stack) => {
                validate_name("stack name", &stack.stack)?;
                let Some(service) = self.service() else {
                    return err(format!(
                        "`{}` is not a service of stack {}",
                        self.name, stack.stack
                    ));
                };
                validate_name("service name", service)?;
                validate_host(&stack.host)?;
            }
            None => validate_name("app name", &self.name)?,
        }
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
        if let Some(check) = &self.healthcheck {
            check.validate()?;
        }
        if self.is_static() {
            if !self.ports.is_empty()
                || !self.tls_ports.is_empty()
                || self.host_network
                || self.http_port.is_some()
            {
                return err("static sites cannot have ports, http_port or host_network");
            }
            if !self.command.is_empty()
                || self.healthcheck.is_some()
                || self.release_command.is_some()
                || !self.volumes.is_empty()
                || !self.links.is_empty()
            {
                return err(
                    "static sites have no container: no command, release command, volumes or links",
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
        for (i, t) in self.tls_ports.iter().enumerate() {
            t.validate()?;
            if self.domains.is_empty() {
                return err("TLS ports need a domain (its certificate is used)");
            }
            if self.tls_ports[..i].iter().any(|o| o.public == t.public) {
                return err(format!("TLS port {} is listed twice", t.public));
            }
            if let Some(p) = self
                .host_ports()
                .iter()
                .find(|p| p.protocol == Protocol::Tcp && p.contains(t.public))
            {
                return err(format!("TLS port {} overlaps {p}", t.public));
            }
            if self.host_network && t.container == t.public {
                return err(format!(
                    "TLS port {t}: on the host network the app cannot listen on the public port itself"
                ));
            }
        }
        if let Some(cpus) = self.limits.cpus
            && !(cpus > 0.0 && cpus <= 1024.0)
        {
            return err("limits.cpus must be > 0");
        }
        if !self.command.is_empty() && matches!(self.source, Source::Executable { .. }) {
            return err("executables take arguments (args), not a command");
        }
        if self
            .release_command
            .as_deref()
            .is_some_and(|c| c.trim().is_empty())
        {
            return err("release_command is empty");
        }
        for (i, volume) in self.volumes.iter().enumerate() {
            volume.validate()?;
            if self.volumes[..i]
                .iter()
                .any(|v| v.name == volume.name || v.path == volume.path)
            {
                return err(format!(
                    "volume `{volume}` repeats another volume's name or path"
                ));
            }
        }
        if self.host_network && !self.links.is_empty() {
            return err("apps on the node's network cannot use links (use published ports)");
        }
        for (i, link) in self.links.iter().enumerate() {
            link.validate()?;
            if link.app == self.name {
                return err("an app cannot link to itself");
            }
            if self.links[..i]
                .iter()
                .any(|l| l.app == link.app || l.host() == link.host())
            {
                return err(format!("link `{link}` repeats another link"));
            }
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
                ..
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
                if crate::git::has_embedded_credentials(repo) {
                    return err(
                        "do not put credentials in the repository URL: add a git credential \
                         instead (`facade git token` or `facade git key`)",
                    );
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
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

    /// The command that starts it, as a `CMD`.
    pub fn command(&self) -> Vec<String> {
        match self {
            PythonEntry::Script(path) => vec!["python".into(), path.clone()],
            PythonEntry::Module(module) => vec!["python".into(), "-m".into(), module.clone()],
            PythonEntry::Command(command) => vec![command.clone()],
        }
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

/// `public[:container]`, e.g. `993:1143`: TLS-wrapped TCP on the node's public
/// `public` port, passed on in the clear to the app's `container` port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsPort {
    pub public: u16,
    pub container: u16,
}

impl TlsPort {
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.public == 0 || self.container == 0 {
            return err("ports must be non-zero");
        }
        if matches!(self.public, 80 | 443) {
            return err(format!(
                "TLS port {}: the entry point serves HTTP(S) there",
                self.public
            ));
        }
        Ok(())
    }
}

impl FromStr for TlsPort {
    type Err = SpecError;

    fn from_str(s: &str) -> Result<Self, SpecError> {
        let (public, container) = s.split_once(':').unwrap_or((s, s));
        let parse = |p: &str| {
            p.trim()
                .parse::<u16>()
                .map_err(|_| SpecError(format!("`{p}` is not a port")))
        };
        let port = TlsPort {
            public: parse(public)?,
            container: parse(container)?,
        };
        port.validate()?;
        Ok(port)
    }
}

impl fmt::Display for TlsPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.public, self.container)
    }
}

/// A named Docker volume mounted into the container: `data:/var/lib/postgresql`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Volume {
    pub name: String,
    pub path: String,
}

impl Volume {
    pub fn validate(&self) -> Result<(), SpecError> {
        let name_ok = !self.name.is_empty()
            && self.name.len() <= 40
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !self.name.starts_with('-')
            && !self.name.ends_with('-');
        if !name_ok {
            return err(format!(
                "volume name `{}` must be 1-40 chars of a-z, 0-9 and inner dashes",
                self.name
            ));
        }
        let path = std::path::Path::new(&self.path);
        if !self.path.starts_with('/')
            || self.path == "/"
            || self.path.contains(':')
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return err(format!(
                "volume path `{}` must be an absolute path other than /",
                self.path
            ));
        }
        Ok(())
    }

    /// Name of the Docker volume on the node. `_` appears in neither app nor
    /// volume names, so `a-b` + `c` and `a` + `b-c` never meet.
    pub fn docker_name(&self, app: &str) -> String {
        format!("facade-{app}_{}", self.name)
    }
}

impl FromStr for Volume {
    type Err = SpecError;

    fn from_str(s: &str) -> Result<Self, SpecError> {
        let Some((name, path)) = s.split_once(':') else {
            return err(format!("`{s}`: expected name:/path"));
        };
        let volume = Volume {
            name: name.trim().to_string(),
            path: path.trim().trim_end_matches('/').to_string(),
        };
        volume.validate()?;
        Ok(volume)
    }
}

impl TryFrom<String> for Volume {
    type Error = SpecError;
    fn try_from(s: String) -> Result<Self, SpecError> {
        s.parse()
    }
}

impl From<Volume> for String {
    fn from(v: Volume) -> String {
        v.to_string()
    }
}

impl fmt::Display for Volume {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.name, self.path)
    }
}

/// Another app on the same node, reachable from this one as `alias` (default: its
/// name): `db` or `shop-db:db`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Link {
    pub app: String,
    pub alias: Option<String>,
}

impl Link {
    /// The host name the linked app answers to.
    pub fn host(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.app)
    }

    pub fn validate(&self) -> Result<(), SpecError> {
        validate_app_name(&self.app)?;
        if let Some(alias) = &self.alias {
            validate_name("link alias", alias)?;
        }
        Ok(())
    }
}

impl FromStr for Link {
    type Err = SpecError;

    fn from_str(s: &str) -> Result<Self, SpecError> {
        let (app, alias) = match s.split_once(':') {
            Some((app, alias)) => (app.trim(), Some(alias.trim())),
            None => (s.trim(), None),
        };
        let link = Link {
            app: app.to_string(),
            alias: alias.filter(|a| *a != app).map(str::to_string),
        };
        link.validate()?;
        Ok(link)
    }
}

impl TryFrom<String> for Link {
    type Error = SpecError;
    fn try_from(s: String) -> Result<Self, SpecError> {
        s.parse()
    }
}

impl From<Link> for String {
    fn from(l: Link) -> String {
        l.to_string()
    }
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.alias {
            Some(alias) => write!(f, "{}:{alias}", self.app),
            None => f.write_str(&self.app),
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

/// Splits a command line on spaces, keeping "quoted parts" together.
pub fn split_command(line: &str) -> Result<Vec<String>, SpecError> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => current.push(c),
        }
    }
    if quote.is_some() {
        return err("unclosed quote in the command");
    }
    if started || !current.is_empty() {
        words.push(current);
    }
    Ok(words)
}

/// The reverse of [`split_command`], for display and editing.
pub fn join_command(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            if w.is_empty() || w.contains(char::is_whitespace) {
                if w.contains('"') {
                    format!("'{w}'")
                } else {
                    format!("\"{w}\"")
                }
            } else {
                w.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_commands() {
        let words = split_command(r#"sh -c "echo hi" '' x"#).unwrap();
        assert_eq!(words, ["sh", "-c", "echo hi", "", "x"]);
        assert_eq!(join_command(&words), r#"sh -c "echo hi" "" x"#);
        assert!(split_command("a \"b").is_err());
    }

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
    fn parses_volumes_and_links() {
        let v: Volume = "data:/var/lib/postgresql/".parse().unwrap();
        assert_eq!(v.to_string(), "data:/var/lib/postgresql");
        assert_eq!(v.docker_name("db"), "facade-db_data");
        assert!("data:relative".parse::<Volume>().is_err());
        assert!("Data:/x".parse::<Volume>().is_err());
        assert!("data:/".parse::<Volume>().is_err());
        let l: Link = "shop-db:db".parse().unwrap();
        assert_eq!((l.app.as_str(), l.host()), ("shop-db", "db"));
        assert_eq!("db:db".parse::<Link>().unwrap().to_string(), "db");
        assert!("Db".parse::<Link>().is_err());
    }

    #[test]
    fn finds_registries() {
        assert_eq!(image_registry("nginx"), "docker.io");
        assert_eq!(image_registry("traefik/whoami"), "docker.io");
        assert_eq!(image_registry("ghcr.io/sigma-dev/site:latest"), "ghcr.io");
        assert_eq!(image_registry("localhost:5000/app"), "localhost:5000");
    }

    #[test]
    fn parses_python_entries() {
        assert_eq!(
            PythonEntry::parse("server.py").unwrap(),
            PythonEntry::Script("server.py".into())
        );
        assert_eq!(
            PythonEntry::parse("-m poker").unwrap().command(),
            ["python", "-m", "poker"]
        );
        assert_eq!(
            PythonEntry::parse("mybot").unwrap(),
            PythonEntry::Command("mybot".into())
        );
        assert!(PythonEntry::parse("../x.py").is_err());
        assert!(PythonEntry::parse("rm -rf").is_err());
    }

    fn spec() -> AppSpec {
        serde_json::from_value(serde_json::json!({
            "name": "app",
            "node": "main",
            "source": { "type": "image", "image": "nginx" },
            "http_port": 80,
            "domains": ["example.com"],
        }))
        .unwrap()
    }

    #[test]
    fn validates_redirects_and_ports() {
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
        s.ports = vec![
            "3478:3478/udp".parse().unwrap(),
            "3470-3480:3470-3480/udp".parse().unwrap(),
        ];
        assert!(s.validate().is_err(), "overlapping ports");
        let mut s = spec();
        s.host_network = true;
        s.ports = vec!["3478:3478/udp".parse().unwrap()];
        assert!(s.validate().is_ok());
        assert_eq!(s.host_ports().len(), 2, "declared ports and the HTTP port");
        s.ports = vec!["3479:3478/udp".parse().unwrap()];
        assert!(s.validate().is_err(), "no remapping on the host network");
        let mut s = spec();
        s.tls_ports = vec!["993:1143".parse().unwrap(), "465:1587".parse().unwrap()];
        assert!(s.validate().is_ok());
        assert_eq!(s.ingress_ports()[0].host, 993);
        assert_eq!(s.tls_ports[1].to_string(), "465:1587");
        s.tls_ports.push("993:2000".parse().unwrap());
        assert!(s.validate().is_err(), "a public port twice");
        let mut s = spec();
        s.tls_ports = vec!["993:1143".parse().unwrap()];
        s.ports = vec!["990-999:990-999".parse().unwrap()];
        assert!(s.validate().is_err(), "overlaps a published port");
        s.ports.clear();
        s.domains.clear();
        s.http_port = None;
        assert!(s.validate().is_err(), "needs a domain for its certificate");
        assert!("443:8443".parse::<TlsPort>().is_err());
        assert_eq!("993".parse::<TlsPort>().unwrap().container, 993);
    }

    #[test]
    fn validates_domains() {
        assert!(validate_domain("app.example.com").is_ok());
        assert!(validate_domain("App.example.com").is_err());
        assert!(validate_domain("localhost").is_err());
    }
}
