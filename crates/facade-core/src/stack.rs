//! Stacks: services deployed together from one repository, at one commit, on one
//! node, where they reach each other by name over a network of their own.
//!
//! A stack's services come from a compose file in its repository, read again at
//! every deploy, or are defined in facade. Settings given in facade for a service
//! (domains, limits, …) apply over the file's.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::spec::{
    AppSpec, BackupSpec, Build, HealthCheck, Limits, PortMap, Source, SpecError, StackRef, Volume,
    validate_domain, validate_name,
};

fn err<T>(msg: impl Into<String>) -> Result<T, SpecError> {
    Err(SpecError(msg.into()))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StackSpec {
    pub name: String,
    /// Every service runs on this node.
    pub node: String,
    /// The repository services are built from (and the file read from).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Defaults to the repository's default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Compose file in the repository defining the services. Without one, the
    /// services are the ones defined in `services`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Services defined in facade, or settings applied over the file's services.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, ServiceSpec>,
}

/// A service's settings. Fields left out (`None`) come from the compose file, or
/// take their defaults for services defined in facade.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServiceSpec {
    /// A registry image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Or a build from the stack's repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<Build>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<PortMap>>,
    /// Stack volumes mounted into the service (shared by the services naming them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volumes: Option<Vec<Volume>>,
    /// Services started (and replaced, on deploy) before this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Limits::is_empty")]
    pub limits: Limits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthCheck>,
    /// Compose's `stop_grace_period`, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_timeout_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegate_cgroups: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupSpec>,
}

impl ServiceSpec {
    pub fn is_empty(&self) -> bool {
        *self == ServiceSpec::default()
    }

    /// The fields this one sets replace the ones of `base`.
    pub fn merged_over(&self, base: &ServiceSpec) -> ServiceSpec {
        let mut out = base.clone();
        if self.image.is_some() || self.build.is_some() {
            out.image = self.image.clone();
            out.build = self.build.clone();
        }
        macro_rules! take {
            ($($f:ident),*) => { $(if self.$f.is_some() { out.$f = self.$f.clone(); })* };
        }
        take!(
            command,
            release_command,
            http_port,
            domains,
            ports,
            volumes,
            depends_on,
            healthcheck,
            stop_timeout_secs,
            stop_signal,
            delegate_cgroups,
            backup
        );
        if self.limits.memory_mb.is_some() {
            out.limits.memory_mb = self.limits.memory_mb;
        }
        if self.limits.cpus.is_some() {
            out.limits.cpus = self.limits.cpus;
        }
        if self.limits.pids.is_some() {
            out.limits.pids = self.limits.pids;
        }
        out
    }

    fn validate(&self, service: &str) -> Result<(), SpecError> {
        let at = |e: SpecError| SpecError(format!("service {service}: {}", e.0));
        if self.image.is_some() && self.build.is_some() {
            return err(format!(
                "service {service}: give an image or a build, not both"
            ));
        }
        if let Some(image) = &self.image
            && (image.trim().is_empty() || image.starts_with('-'))
        {
            return err(format!("service {service}: invalid image"));
        }
        if self.build.as_ref().is_some_and(Build::is_static) {
            return err(format!(
                "service {service}: static sites are apps of their own, not stack services"
            ));
        }
        for domain in self.domains.iter().flatten() {
            validate_domain(domain).map_err(at)?;
        }
        for port in self.ports.iter().flatten() {
            port.validate().map_err(at)?;
        }
        for volume in self.volumes.iter().flatten() {
            volume.validate().map_err(at)?;
        }
        for dep in self.depends_on.iter().flatten() {
            validate_name("service name", dep).map_err(at)?;
        }
        Ok(())
    }

    /// The app running this service of `stack`, given its source.
    pub fn to_app(
        &self,
        stack: &StackSpec,
        service: &str,
        host: &str,
        enabled: bool,
    ) -> Result<AppSpec, SpecError> {
        let source = match (&self.image, &self.build) {
            (Some(image), None) => Source::Image {
                image: image.clone(),
            },
            (None, Some(build)) => Source::Git {
                repo: stack.repo.clone().ok_or_else(|| {
                    SpecError(format!(
                        "service {service} is built from the repository, but the stack has none"
                    ))
                })?,
                branch: stack.branch.clone(),
                build: build.clone(),
            },
            _ => {
                return err(format!("service {service} needs an image or a build"));
            }
        };
        let spec = AppSpec {
            name: stack.app_name(service),
            node: stack.node.clone(),
            source,
            http_port: self.http_port,
            domains: self.domains.clone().unwrap_or_default(),
            redirects: vec![],
            ports: self.ports.clone().unwrap_or_default(),
            tls_ports: vec![],
            host_network: false,
            limits: self.limits.clone(),
            delegate_cgroups: self.delegate_cgroups.unwrap_or(false),
            enabled,
            command: self.command.clone().unwrap_or_default(),
            release_command: self
                .release_command
                .clone()
                .filter(|r| !r.trim().is_empty()),
            volumes: self.volumes.clone().unwrap_or_default(),
            links: vec![],
            stack: Some(StackRef {
                stack: stack.name.clone(),
                host: host.to_string(),
            }),
            healthcheck: self.healthcheck.clone(),
            stop_timeout_secs: self.stop_timeout_secs,
            stop_signal: self.stop_signal.clone(),
            backup: self.backup.clone(),
        };
        spec.validate()
            .map_err(|e| SpecError(format!("service {service}: {}", e.0)))?;
        Ok(spec)
    }
}

impl StackSpec {
    /// The app running one of the stack's services.
    pub fn app_name(&self, service: &str) -> String {
        format!("{}.{service}", self.name)
    }

    pub fn validate(&self) -> Result<(), SpecError> {
        validate_name("stack name", &self.name)?;
        validate_name("node name", &self.node)?;
        if let Some(repo) = &self.repo {
            if repo.trim().is_empty() || repo.starts_with('-') {
                return err("repo must be a git URL");
            }
            if crate::git::has_embedded_credentials(repo) {
                return err(
                    "do not put credentials in the repository URL: add a git credential instead",
                );
            }
        }
        if let Some(branch) = &self.branch
            && (branch.is_empty()
                || branch.starts_with('-')
                || branch.contains(char::is_whitespace))
        {
            return err("branch is not a valid git ref");
        }
        if let Some(file) = &self.file {
            if self.repo.is_none() {
                return err("a stack read from a file needs a repository");
            }
            let p = std::path::Path::new(file);
            if file.is_empty()
                || p.is_absolute()
                || p.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return err(format!("`{file}` must be a path inside the repository"));
            }
        }
        for (service, spec) in &self.services {
            validate_name("service name", service)?;
            spec.validate(service)?;
            if self.file.is_none() {
                if spec.image.is_none() && spec.build.is_none() {
                    return err(format!("service {service} needs an image or a build"));
                }
                if spec.build.is_some() && self.repo.is_none() {
                    return err(format!(
                        "service {service} is built from the repository, but the stack has none"
                    ));
                }
            }
        }
        Ok(())
    }
}

/// The order services are replaced in: each group after the ones it depends on.
/// Dependencies on unknown services are ignored; a cycle is an error.
pub fn deploy_order(deps: &[(String, Vec<String>)]) -> Result<Vec<Vec<String>>, SpecError> {
    let names: Vec<&String> = deps.iter().map(|(n, _)| n).collect();
    let mut done: Vec<String> = Vec::new();
    let mut levels = Vec::new();
    while done.len() < deps.len() {
        let level: Vec<String> = deps
            .iter()
            .filter(|(name, _)| !done.contains(name))
            .filter(|(_, on)| on.iter().all(|d| done.contains(d) || !names.contains(&d)))
            .map(|(name, _)| name.clone())
            .collect();
        if level.is_empty() {
            let stuck: Vec<&str> = deps
                .iter()
                .filter(|(n, _)| !done.contains(n))
                .map(|(n, _)| n.as_str())
                .collect();
            return err(format!(
                "services depend on each other in a loop: {}",
                stuck.join(", ")
            ));
        }
        done.extend(level.iter().cloned());
        levels.push(level);
    }
    Ok(levels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_by_dependencies() {
        let deps = vec![
            ("web".to_string(), vec!["api".to_string()]),
            (
                "api".to_string(),
                vec!["db".to_string(), "cache".to_string()],
            ),
            ("db".to_string(), vec![]),
            ("cache".to_string(), vec!["external".to_string()]),
        ];
        assert_eq!(
            deploy_order(&deps).unwrap(),
            vec![vec!["db", "cache"], vec!["api"], vec!["web"]]
        );
        let cycle = vec![
            ("a".to_string(), vec!["b".to_string()]),
            ("b".to_string(), vec!["a".to_string()]),
        ];
        assert!(deploy_order(&cycle).is_err());
    }

    #[test]
    fn builds_service_apps() {
        let mut stack = StackSpec {
            name: "shop".into(),
            node: "main".into(),
            repo: Some("https://github.com/me/shop".into()),
            branch: None,
            file: None,
            services: BTreeMap::new(),
        };
        let api = ServiceSpec {
            build: Some(Build::Dockerfile {
                dockerfile: "api/Dockerfile".into(),
                context: ".".into(),
            }),
            http_port: Some(8080),
            ..Default::default()
        };
        stack.services.insert("api".into(), api.clone());
        stack.validate().unwrap();
        let app = api.to_app(&stack, "api", "api", true).unwrap();
        assert_eq!(app.name, "shop.api");
        assert_eq!(app.service(), Some("api"));
        assert_eq!(
            app.volume_name(&"data:/data".parse().unwrap()),
            "facade.shop.data"
        );
        // Settings given in facade win over the file's.
        let file = ServiceSpec {
            image: Some("nginx".into()),
            http_port: Some(80),
            domains: Some(vec!["a.example.com".into()]),
            ..Default::default()
        };
        let over = ServiceSpec {
            domains: Some(vec!["b.example.com".into()]),
            ..Default::default()
        };
        let merged = over.merged_over(&file);
        assert_eq!(merged.http_port, Some(80));
        assert_eq!(merged.domains.unwrap(), ["b.example.com"]);
        // A service defined here needs a source.
        stack
            .services
            .insert("worker".into(), ServiceSpec::default());
        assert!(stack.validate().is_err());
    }
}
