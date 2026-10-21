//! Docker side of the agent. Every managed container is named `facade-<app>` and
//! labelled with the app and a hash of the workload it was created from, so
//! convergence is "compare hash, recreate if different".

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use bollard::Docker;
use bollard::auth::DockerCredentials;
use bollard::models::{
    ContainerCreateBody, EndpointSettings, HealthConfig, HostConfig, HostConfigLogConfig, Mount,
    MountType, NetworkConnectRequest, NetworkCreateRequest, NetworkDisconnectRequest,
    NetworkingConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum, VolumeCreateRequest,
};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, ListContainersOptions,
    ListImagesOptions, ListVolumesOptions, LogsOptions, RemoveContainerOptions, RemoveImageOptions,
    RemoveVolumeOptions, RestartContainerOptions, StartContainerOptions, StatsOptions,
    StopContainerOptions, WaitContainerOptions,
};
use facade_core::proto::{AppState, AppStats, JoinToken, VolumeUsage, Workload};
use facade_core::{Build, Protocol, Source};
use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use super::DeployLog;
use super::build;
use super::volumes;
use crate::gitauth::Access;
use crate::lang;

const LABEL_APP: &str = "facade.app";
const LABEL_RUN: &str = "facade.run";
const LABEL_NODE: &str = "facade.node";
const LABEL_VOLUME: &str = "facade.volume";
const LABEL_STACK: &str = "facade.stack";

#[derive(Clone)]
pub struct Runtime(Arc<Inner>);

struct Inner {
    docker: Docker,
    dir: PathBuf,
    /// Containers are labelled with `token.node`, so several agents sharing one
    /// Docker daemon never touch each other's apps.
    token: JoinToken,
}

fn container(app: &str) -> String {
    format!("facade-{app}")
}

fn network(app: &str) -> String {
    format!("facade-{app}")
}

/// A stack's network. The dot keeps it apart from app networks.
fn stack_network(stack: &str) -> String {
    format!("facade.{stack}")
}

fn image_repo(app: &str) -> String {
    format!("facade-{app}")
}

fn short_hash(value: &impl Serialize) -> String {
    let json = serde_json::to_vec(value).expect("serializable");
    hex::encode(&Sha256::digest(json)[..8])
}

/// Identifies the container a workload needs. Credentials and aliases are left
/// out: they change without the container having to.
pub fn run_key(w: &Workload, image: &str) -> String {
    short_hash(&(&w.spec, &w.env, w.revision, &w.commit, image))
}

/// The image an app runs: its registry reference, or the tag its build gets
/// (named after its source and commit, or a stack service's build context
/// content), so a build is reused until either changes. `None` for static sites.
pub fn planned_image(w: &Workload) -> Option<String> {
    let key = match &w.spec.source {
        Source::Image { image } => return Some(facade_core::image_with_tag(image)),
        Source::Git {
            build: Build::Static { .. },
            ..
        } => return None,
        Source::Executable { .. } => short_hash(&w.spec.source),
        Source::Git { .. } => match (&w.build_key, &w.commit) {
            (Some(content), _) => short_hash(&(&w.spec.source, content)),
            (None, Some(commit)) => short_hash(&(&w.spec.source, commit)),
            (None, None) => short_hash(&(&w.spec.source, w.revision)),
        },
    };
    Some(format!("{}:{key}", image_repo(&w.spec.name)))
}

/// How long a deployment waits for another program to free its ports.
const PORT_WAIT: Duration = Duration::from_secs(30 * 60);

/// The names an app answers to on its own network.
fn aliases(w: &Workload) -> Vec<String> {
    let mut names = vec![w.spec.name.clone()];
    names.extend(w.aliases.iter().cloned());
    names.dedup();
    names
}

/// Built images kept besides the running one, for rollbacks.
const KEEP_IMAGES: usize = 2;

/// Longest a build may take before it is stopped.
const BUILD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 60);

async fn within_build_time<T>(
    build: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(BUILD_TIMEOUT, build)
        .await
        .map_err(|_| {
            anyhow!(
                "the build took longer than {} minutes and was stopped",
                BUILD_TIMEOUT.as_secs() / 60
            )
        })?
}

/// A working directory removed when dropped, even when the build is cancelled.
struct Scratch(PathBuf);

impl Scratch {
    fn new(path: PathBuf) -> Self {
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn is_not_found(e: &bollard::errors::Error) -> bool {
    matches!(
        e,
        bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

fn is_not_modified(e: &bollard::errors::Error) -> bool {
    matches!(
        e,
        bollard::errors::Error::DockerResponseServerError {
            status_code: 304,
            ..
        }
    )
}

impl Runtime {
    pub async fn new(dir: PathBuf, token: JoinToken) -> anyhow::Result<Self> {
        let docker = Docker::connect_with_defaults().context("connecting to Docker")?;
        docker
            .ping()
            .await
            .context("Docker is not reachable (is it installed and running?)")?;
        for sub in ["static", "builds"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        // Left over by builds interrupted when the agent stopped.
        if let Ok(entries) = std::fs::read_dir(dir.join("builds")) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        Ok(Self(Arc::new(Inner { docker, dir, token })))
    }

    pub fn docker(&self) -> &Docker {
        &self.0.docker
    }

    fn static_dir(&self, app: &str) -> PathBuf {
        self.0.dir.join("static").join(app)
    }

    /// Bring one app to its desired state. `rebuild` builds again even when an
    /// image of the commit exists.
    /// The build phase of a stack deployment: build or pull the image only.
    pub async fn prepare(
        &self,
        w: &Workload,
        log: &DeployLog,
        rebuild: bool,
    ) -> anyhow::Result<()> {
        if w.spec.is_static() {
            bail!("static sites are not stack services");
        }
        let image = self.ensure_image(w, log, rebuild, true).await?;
        log.line(format!("built {image}"));
        Ok(())
    }

    pub async fn converge(
        &self,
        w: &Workload,
        log: &DeployLog,
        rebuild: bool,
        pull: bool,
    ) -> anyhow::Result<AppState> {
        let app = &w.spec.name;
        if w.spec.is_static() {
            if !w.spec.enabled {
                log.begin();
                return Ok(AppState::Stopped);
            }
            return self.converge_static(w, log).await;
        }
        if !w.spec.enabled {
            log.begin();
            self.stop(app).await?;
            log.line("stopped");
            return Ok(AppState::Stopped);
        }

        let image = self.ensure_image(w, log, rebuild, pull).await?;
        log.build_done();
        log.begin();
        let run_key = run_key(w, &image);
        let name = container(app);
        match self
            .docker()
            .inspect_container(&name, None::<InspectContainerOptions>)
            .await
        {
            Ok(existing) => {
                let same = existing
                    .config
                    .as_ref()
                    .and_then(|c| c.labels.as_ref())
                    .and_then(|l| l.get(LABEL_RUN))
                    .is_some_and(|k| *k == run_key);
                let running = existing
                    .state
                    .as_ref()
                    .and_then(|s| s.running)
                    .unwrap_or(false);
                if same {
                    self.ensure_aliases(w, &existing).await?;
                    if !running {
                        log.line("starting existing container");
                        self.start(&name).await?;
                    }
                    return Ok(AppState::Running);
                }
                if log.is_deployment() {
                    self.release(w, &image, log).await?;
                }
                log.line("configuration changed, replacing container");
                self.docker()
                    .remove_container(
                        &name,
                        Some(RemoveContainerOptions {
                            force: true,
                            ..Default::default()
                        }),
                    )
                    .await?;
            }
            Err(e) if is_not_found(&e) => {
                if log.is_deployment() {
                    self.release(w, &image, log).await?;
                }
            }
            Err(e) => return Err(e.into()),
        }

        if log.is_deployment() {
            self.wait_for_ports(w, log).await?;
        }
        self.create(w, &image, &run_key).await?;
        self.start(&name).await?;
        log.line(format!("started {name} from {image}"));
        if log.is_deployment() {
            self.wait_ready(&name, log).await?;
        }
        if !matches!(w.spec.source, Source::Image { .. }) {
            self.prune_images(app, &image, KEEP_IMAGES).await;
        }
        Ok(AppState::Running)
    }

    /// The image to run: pulled for `Source::Image`, built otherwise. Builds are
    /// tagged with a hash of the source and commit, so they are reused (on a
    /// redeploy, a restart, a rollback) until either changes.
    async fn ensure_image(
        &self,
        w: &Workload,
        log: &DeployLog,
        rebuild: bool,
        pull: bool,
    ) -> anyhow::Result<String> {
        match &w.spec.source {
            Source::Image { image } => {
                log.begin();
                let image = &facade_core::image_with_tag(image);
                let present = self.docker().inspect_image(image).await.is_ok();
                // An explicit deploy re-pulls, so moving tags like `latest` update.
                if !present || (log.is_deployment() && pull) {
                    log.state(AppState::Building);
                    let registry = facade_core::image_registry(image);
                    let credentials = w.registry(&registry).map(|r| DockerCredentials {
                        username: Some(r.username.clone()),
                        password: Some(r.secret.clone()),
                        serveraddress: Some(registry.clone()),
                        ..Default::default()
                    });
                    log.line(match &credentials {
                        Some(c) => format!(
                            "pulling {image} (logged in to {registry} as {})",
                            c.username.as_deref().unwrap_or_default()
                        ),
                        None => format!("pulling {image}"),
                    });
                    let mut pull = self.docker().create_image(
                        Some(CreateImageOptions {
                            from_image: Some(image.clone()),
                            ..Default::default()
                        }),
                        None,
                        credentials,
                    );
                    while let Some(progress) = pull.next().await {
                        let progress = progress.with_context(|| format!("pulling {image}"))?;
                        if let Some(status) = progress.status
                            && progress.progress_detail.is_none_or(|d| d.current.is_none())
                        {
                            log.line(status);
                        }
                    }
                }
                Ok(image.clone())
            }
            Source::Executable {
                sha256,
                filename,
                arch,
                args,
            } => {
                let tag = planned_image(w).expect("executables have an image");
                let key = tag.rsplit(':').next().unwrap_or_default().to_string();
                if !rebuild && self.docker().inspect_image(&tag).await.is_ok() {
                    return Ok(tag);
                }
                if arch != std::env::consts::ARCH {
                    bail!(
                        "{filename} is built for {arch} but this node is {}",
                        std::env::consts::ARCH
                    );
                }
                let _slot = log.build_slot(None).await;
                log.state(AppState::Building);
                let workdir = Scratch::new(
                    self.0
                        .dir
                        .join("builds")
                        .join(format!("{}-{key}", w.spec.name)),
                );
                tokio::fs::create_dir_all(&workdir.0).await?;
                within_build_time(async {
                    log.line(format!("downloading {filename}"));
                    super::fetch_artifact(&self.0.token, sha256, &workdir.0.join("app")).await?;
                    let dockerfile = workdir.0.join("Dockerfile");
                    tokio::fs::write(&dockerfile, lang::executable::dockerfile(filename, args))
                        .await?;
                    let options =
                        build::BuildOptions::new(None, &w.registries, &workdir.0, rebuild)?;
                    build::docker_build(&w.spec.name, &tag, &dockerfile, &workdir.0, &options, log)
                        .await
                })
                .await?;
                Ok(tag)
            }
            Source::Git {
                repo,
                branch,
                build,
            } => {
                // A stack service's image depends on its build context's content,
                // so it survives commits that do not touch it.
                let tag = planned_image(w).expect("built sources have an image");
                let key = tag.rsplit(':').next().unwrap_or_default().to_string();
                if !rebuild && self.docker().inspect_image(&tag).await.is_ok() {
                    if log.is_deployment() {
                        log.line("this code was built before: reusing its image");
                    }
                    return Ok(tag);
                }
                let _slot = log.build_slot(w.commit.clone()).await;
                log.state(AppState::Building);
                let builds = self.0.dir.join("builds");
                let workdir = Scratch::new(builds.join(format!("{}-{key}", w.spec.name)));
                let private = Scratch::new(builds.join(format!(".{}-{key}", w.spec.name)));
                within_build_time(async {
                    let _ = tokio::fs::remove_dir_all(&workdir.0).await;
                    let access = Access::new(repo, w.git_auth.as_ref(), &self.git_dir())?;
                    let (sha, title) = build::git_checkout(
                        &access,
                        branch.as_deref(),
                        w.commit.as_deref(),
                        &workdir.0,
                        log,
                    )
                    .await?;
                    log.commit(&sha, &title);
                    let (dockerfile, context) = match build {
                        Build::Dockerfile {
                            dockerfile,
                            context,
                        } => (workdir.0.join(dockerfile), workdir.0.join(context)),
                        Build::Static { .. } => unreachable!("static sites have no image"),
                        generated => {
                            let path = workdir.0.join(".facade.Dockerfile");
                            let contents = lang::generated_dockerfile(&w.spec.name, generated)
                                .expect("every other build kind generates a Dockerfile");
                            tokio::fs::write(&path, contents).await?;
                            (path, workdir.0.clone())
                        }
                    };
                    crate::tls::create_private_dir(&private.0)?;
                    let options = build::BuildOptions::new(
                        w.git_auth.as_ref(),
                        &w.registries,
                        &private.0,
                        rebuild,
                    )?;
                    build::docker_build(&w.spec.name, &tag, &dockerfile, &context, &options, log)
                        .await
                })
                .await?;
                Ok(tag)
            }
        }
    }

    /// Waits until a container just started is ready: healthy when it has a health
    /// check (its own or its image's), else still up after a few seconds.
    async fn wait_ready(&self, name: &str, log: &DeployLog) -> anyhow::Result<()> {
        let inspect = || async {
            self.docker()
                .inspect_container(name, None::<InspectContainerOptions>)
                .await
                .with_context(|| format!("inspecting {name}"))
        };
        let info = inspect().await?;
        let check = info
            .config
            .as_ref()
            .and_then(|c| c.healthcheck.clone())
            .filter(|h| {
                h.test
                    .as_ref()
                    .is_some_and(|t| t.first().is_some_and(|f| f != "NONE"))
            });
        let has_check = check.is_some()
            || info
                .state
                .as_ref()
                .and_then(|s| s.health.as_ref())
                .is_some();
        let secs = |ns: Option<i64>, default: u64| {
            ns.filter(|n| *n > 0)
                .map_or(default, |n| (n / 1_000_000_000) as u64)
        };
        let budget = match &check {
            Some(h) => {
                secs(h.start_period, 0)
                    + (secs(h.interval, 30) + secs(h.timeout, 30))
                        * (h.retries.unwrap_or(3).max(1) as u64 + 1)
            }
            // The image's own check, with Docker's defaults.
            None if has_check => 300,
            None => 3,
        };
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(budget.min(900) + 5);
        if has_check {
            log.line("waiting for the health check to pass");
        }
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let state = inspect().await?.state.unwrap_or_default();
            let restarts = state.restarting == Some(true);
            if state.running != Some(true) || restarts {
                bail!(
                    "the container stopped (exit code {}); its logs (facade logs) usually say why",
                    state.exit_code.unwrap_or(0)
                );
            }
            match state.health.as_ref().and_then(|h| h.status) {
                Some(bollard::models::HealthStatusEnum::HEALTHY) => {
                    log.line("healthy");
                    return Ok(());
                }
                Some(bollard::models::HealthStatusEnum::UNHEALTHY) => {
                    let last = state
                        .health
                        .and_then(|h| h.log)
                        .and_then(|l| l.last().cloned())
                        .and_then(|r| r.output)
                        .unwrap_or_default();
                    bail!("the health check failed: {}", last.trim());
                }
                _ if !has_check && tokio::time::Instant::now() >= deadline => return Ok(()),
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("the health check did not pass within {budget} seconds");
            }
        }
    }

    fn git_dir(&self) -> PathBuf {
        self.0.dir.join("git")
    }

    /// Runs the release command in a one-off container of the new image, with the
    /// app's environment, network and volumes. The deploy stops if it fails, and
    /// the running version stays.
    async fn release(&self, w: &Workload, image: &str, log: &DeployLog) -> anyhow::Result<()> {
        let Some(command) = &w.spec.release_command else {
            return Ok(());
        };
        let app = &w.spec.name;
        let name = format!("{}-release", container(app));
        log.line(format!("running release command: {command}"));
        let _ = self
            .docker()
            .remove_container(
                &name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;
        let mut body = self.container_body(w, image, "release").await?;
        body.cmd = Some(vec!["sh".into(), "-c".into(), command.clone()]);
        body.entrypoint = Some(vec![]);
        body.healthcheck = None;
        body.exposed_ports = None;
        if let Some(host) = body.host_config.as_mut() {
            host.port_bindings = None;
            host.restart_policy = None;
        }
        self.docker()
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(name.clone()),
                    ..Default::default()
                }),
                body,
            )
            .await
            .context("creating the release container")?;
        let result = async {
            self.connect_networks(w, &name).await?;
            self.start(&name).await?;
            let mut logs = self.docker().logs(
                &name,
                Some(LogsOptions {
                    follow: true,
                    stdout: true,
                    stderr: true,
                    ..Default::default()
                }),
            );
            let mut partial = String::new();
            while let Some(chunk) = logs.next().await {
                partial.push_str(&String::from_utf8_lossy(&chunk?.into_bytes()));
                while let Some(i) = partial.find('\n') {
                    let line: String = partial.drain(..=i).collect();
                    log.line(line.trim_end().to_string());
                }
            }
            if !partial.is_empty() {
                log.line(partial);
            }
            let mut wait = self
                .docker()
                .wait_container(&name, None::<WaitContainerOptions>);
            let code = match wait.next().await {
                Some(Ok(r)) => r.status_code,
                // A non-zero exit comes back as an error carrying the code.
                Some(Err(bollard::errors::Error::DockerContainerWaitError { code, .. })) => code,
                Some(Err(e)) => return Err(e.into()),
                None => 0,
            };
            if code != 0 {
                bail!(
                    "the release command failed (exit code {code}); the running version was kept"
                );
            }
            log.line("release command succeeded");
            Ok(())
        }
        .await;
        let _ = self
            .docker()
            .remove_container(
                &name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;
        result
    }

    /// The container for `w`: ports, limits, volumes, environment, and its private
    /// network with the names it answers to.
    async fn container_body(
        &self,
        w: &Workload,
        image: &str,
        run_key: &str,
    ) -> anyhow::Result<ContainerCreateBody> {
        let spec = &w.spec;
        let app = &spec.name;
        let mut exposed = Vec::new();
        let mut bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        if !spec.host_network {
            if let Some(port) = spec.http_port {
                // Loopback only, on a port Docker picks: the ingress reaches it through
                // the agent, never directly.
                let key = format!("{port}/tcp");
                exposed.push(key.clone());
                bindings.insert(
                    key,
                    Some(vec![PortBinding {
                        host_ip: Some("127.0.0.1".into()),
                        host_port: Some(String::new()),
                    }]),
                );
            }
            for map in &spec.ports {
                let proto = match map.protocol {
                    Protocol::Tcp => "tcp",
                    Protocol::Udp => "udp",
                };
                for i in 0..map.count {
                    let key = format!("{}/{proto}", map.container + i);
                    exposed.push(key.clone());
                    bindings.insert(
                        key,
                        Some(vec![PortBinding {
                            host_ip: Some("0.0.0.0".into()),
                            host_port: Some((map.host + i).to_string()),
                        }]),
                    );
                }
            }
        }

        let (network_mode, networking) = if spec.host_network {
            ("host".to_string(), None)
        } else {
            self.ensure_network(app).await?;
            let endpoint = EndpointSettings {
                aliases: Some(aliases(w)),
                ..Default::default()
            };
            (
                network(app),
                Some(NetworkingConfig {
                    endpoints_config: Some(HashMap::from([(network(app), endpoint)])),
                }),
            )
        };

        let mut mounts = Vec::new();
        for volume in &spec.volumes {
            let name = self.ensure_volume(spec, volume).await?;
            mounts.push(Mount {
                target: Some(volume.path.clone()),
                source: Some(name),
                typ: Some(MountType::VOLUME),
                ..Default::default()
            });
        }

        let host_config = HostConfig {
            port_bindings: Some(bindings),
            network_mode: Some(network_mode),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            memory: spec.limits.memory_mb.map(|mb| (mb * 1024 * 1024) as i64),
            nano_cpus: spec.limits.cpus.map(|c| (c * 1e9) as i64),
            pids_limit: Some(4096),
            security_opt: Some(vec!["no-new-privileges".into()]),
            init: Some(true),
            mounts: Some(mounts),
            log_config: Some(HostConfigLogConfig {
                typ: Some("json-file".into()),
                config: Some(HashMap::from([
                    ("max-size".to_string(), "10m".to_string()),
                    ("max-file".to_string(), "3".to_string()),
                ])),
            }),
            ..Default::default()
        };

        let second = 1_000_000_000i64;
        let healthcheck = spec.healthcheck.as_ref().map(|h| HealthConfig {
            test: Some(h.test.clone()),
            interval: Some(i64::from(h.interval_secs) * second),
            timeout: Some(i64::from(h.timeout_secs) * second),
            retries: Some(i64::from(h.retries)),
            start_period: Some(i64::from(h.start_period_secs) * second),
            ..Default::default()
        });
        Ok(ContainerCreateBody {
            image: Some(image.to_string()),
            healthcheck,
            env: Some(w.env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
            cmd: (!spec.command.is_empty()).then(|| spec.command.clone()),
            exposed_ports: Some(exposed),
            labels: Some(HashMap::from([
                (LABEL_APP.to_string(), app.clone()),
                (LABEL_RUN.to_string(), run_key.to_string()),
                (LABEL_NODE.to_string(), self.0.token.node.clone()),
            ])),
            host_config: Some(host_config),
            networking_config: networking,
            ..Default::default()
        })
    }

    async fn create(&self, w: &Workload, image: &str, run_key: &str) -> anyhow::Result<()> {
        let app = &w.spec.name;
        let body = self.container_body(w, image, run_key).await?;
        self.docker()
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(container(app)),
                    ..Default::default()
                }),
                body,
            )
            .await
            .with_context(|| format!("creating container for {app}"))?;
        self.connect_networks(w, &container(app)).await
    }

    /// Joins its stack's network, where it answers to its service name, and the
    /// private networks of the apps `w` links to, where they answer to their link
    /// names.
    async fn connect_networks(&self, w: &Workload, container_name: &str) -> anyhow::Result<()> {
        if let Some(stack) = &w.spec.stack
            && !w.spec.host_network
        {
            let net = stack_network(&stack.stack);
            self.ensure_named_network(&net, &stack.stack).await?;
            self.docker()
                .connect_network(
                    &net,
                    NetworkConnectRequest {
                        container: container_name.to_string(),
                        endpoint_config: Some(EndpointSettings {
                            aliases: Some(vec![stack.host.clone()]),
                            ..Default::default()
                        }),
                    },
                )
                .await
                .with_context(|| format!("joining stack {}'s network", stack.stack))?;
        }
        for link in &w.spec.links {
            self.ensure_network(&link.app).await?;
            self.docker()
                .connect_network(
                    &network(&link.app),
                    NetworkConnectRequest {
                        container: container_name.to_string(),
                        endpoint_config: Some(EndpointSettings::default()),
                    },
                )
                .await
                .with_context(|| format!("joining {}'s network", link.app))?;
        }
        Ok(())
    }

    /// Apps linking under a new name change the aliases of a running container
    /// without restarting it.
    async fn ensure_aliases(
        &self,
        w: &Workload,
        existing: &bollard::models::ContainerInspectResponse,
    ) -> anyhow::Result<()> {
        if w.spec.host_network {
            return Ok(());
        }
        let net = network(&w.spec.name);
        let current: Vec<String> = existing
            .network_settings
            .as_ref()
            .and_then(|n| n.networks.as_ref())
            .and_then(|n| n.get(&net))
            .and_then(|e| e.aliases.clone())
            .unwrap_or_default();
        let wanted = aliases(w);
        if wanted.iter().all(|a| current.contains(a)) {
            return Ok(());
        }
        let name = container(&w.spec.name);
        self.docker()
            .disconnect_network(
                &net,
                NetworkDisconnectRequest {
                    container: name.clone(),
                    force: Some(true),
                },
            )
            .await?;
        self.docker()
            .connect_network(
                &net,
                NetworkConnectRequest {
                    container: name,
                    endpoint_config: Some(EndpointSettings {
                        aliases: Some(wanted),
                        ..Default::default()
                    }),
                },
            )
            .await?;
        Ok(())
    }

    /// Waits until no other program holds the ports the app takes, so a service
    /// being moved into facade keeps running while its replacement builds, and
    /// the replacement starts as soon as the old one is stopped.
    async fn wait_for_ports(&self, w: &Workload, log: &DeployLog) -> anyhow::Result<()> {
        let wanted = w.spec.host_ports();
        if wanted.is_empty() {
            return Ok(());
        }
        let started = std::time::Instant::now();
        let mut last_said = None::<std::time::Instant>;
        loop {
            let (containers, published) = self.port_owners().await?;
            let listeners =
                tokio::task::spawn_blocking(move || super::ports::scan(&containers, &published))
                    .await?;
            let busy: Vec<String> = listeners
                .iter()
                .filter(|l| l.app.as_deref() != Some(w.spec.name.as_str()))
                .filter(|l| {
                    wanted
                        .iter()
                        .any(|p| p.protocol == l.protocol && p.contains(l.port))
                })
                .map(|l| {
                    let holder = match (&l.app, &l.process, l.pid) {
                        (Some(app), _, _) => format!("app {app}"),
                        (None, Some(p), Some(pid)) => format!("{p} (pid {pid})"),
                        _ => "another program".into(),
                    };
                    format!("{}/{} held by {holder}", l.port, l.protocol)
                })
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            if busy.is_empty() {
                if last_said.is_some() {
                    log.line("ports are free, starting");
                }
                return Ok(());
            }
            if started.elapsed() > PORT_WAIT {
                bail!(
                    "gave up after {} minutes waiting for {}",
                    PORT_WAIT.as_secs() / 60,
                    busy.join(", ")
                );
            }
            if last_said.is_none_or(|t| t.elapsed() >= Duration::from_secs(30)) {
                let message = format!(
                    "built; waiting for {} (stop it and {} starts within a second)",
                    busy.join(", "),
                    w.spec.name
                );
                log.line(&message);
                log.state_with(AppState::Pending, Some(message));
                last_said = Some(std::time::Instant::now());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// The directory holding a facade volume's files on this machine.
    async fn volume_dir(&self, name: &str) -> anyhow::Result<PathBuf> {
        let info = match self.docker().inspect_volume(name).await {
            Ok(info) => info,
            Err(e) if is_not_found(&e) => {
                bail!("there is no volume {name} on this node yet: deploy its app once first")
            }
            Err(e) => return Err(e.into()),
        };
        if info.labels.get(LABEL_NODE) != Some(&self.0.token.node) {
            bail!("{name} is not a facade volume of this node");
        }
        let dir = PathBuf::from(info.mountpoint);
        if !dir.is_dir() {
            bail!(
                "volume {name} is at {}, which this agent cannot reach (is it running as root, next to Docker?)",
                dir.display()
            );
        }
        Ok(dir)
    }

    /// Writes a gzipped tar of the volume to `out`. A running app is paused while
    /// the archive is made, so its files are captured at one instant (enough for
    /// SQLite and its WAL), then resumed before the archive is sent.
    pub async fn export_volume<W: AsyncWrite + Unpin>(
        &self,
        app: &str,
        volume: &str,
        mut out: W,
    ) -> anyhow::Result<()> {
        let dir = self.volume_dir(volume).await?;
        let tmp = self.scratch_file("export");
        let name = container(app);
        let running = self.is_running(&name).await;
        if running {
            self.docker().pause_container(&name).await?;
        }
        let result = {
            let (dir, tmp) = (dir.clone(), tmp.clone());
            tokio::task::spawn_blocking(move || volumes::archive(&dir, &tmp)).await?
        };
        if running && let Err(e) = self.docker().unpause_container(&name).await {
            tracing::error!("unpausing {name} after a volume export: {e}");
        }
        let sent = async {
            result?;
            let mut file = tokio::fs::File::open(&tmp).await?;
            tokio::io::copy(&mut file, &mut out).await?;
            out.shutdown().await?;
            anyhow::Ok(())
        }
        .await;
        let _ = tokio::fs::remove_file(&tmp).await;
        sent
    }

    /// Replaces the volume's contents with the tar read from `input`. The upload is
    /// received in full before the app is touched; the app is stopped while the
    /// files are replaced and started again if it was running.
    pub async fn import_volume<R: AsyncRead + Unpin>(
        &self,
        app: &str,
        volume: &str,
        create: Option<&facade_core::proto::NewVolume>,
        mut input: R,
        size: u64,
    ) -> anyhow::Result<()> {
        // An import may come before the app's first deploy: create the volume
        // with the labels its deploy would give it.
        if let Some(new) = create
            && self.docker().inspect_volume(volume).await.is_err()
        {
            let mut labels = HashMap::from([
                (LABEL_APP.to_string(), app.to_string()),
                (LABEL_NODE.to_string(), self.0.token.node.clone()),
                (LABEL_VOLUME.to_string(), new.name.clone()),
            ]);
            if let Some(stack) = &new.stack {
                labels.insert(LABEL_STACK.to_string(), stack.clone());
            }
            self.docker()
                .create_volume(VolumeCreateRequest {
                    name: Some(volume.to_string()),
                    labels: Some(labels),
                    ..Default::default()
                })
                .await
                .with_context(|| format!("creating volume {volume}"))?;
        }
        let dir = self.volume_dir(volume).await?;
        let tmp = self.scratch_file("import");
        let result = async {
            let mut file = tokio::fs::File::create(&tmp).await?;
            let received = tokio::io::copy(&mut input, &mut file).await?;
            if received != size {
                bail!("upload interrupted ({received} of {size} bytes)");
            }
            file.sync_all().await?;
            let name = container(app);
            let running = self.is_running(&name).await;
            if running {
                self.stop(app).await?;
            }
            let restored = {
                let (dir, tmp) = (dir.clone(), tmp.clone());
                tokio::task::spawn_blocking(move || volumes::restore(&tmp, &dir)).await?
            };
            if running {
                self.start(&name).await?;
            }
            restored
        }
        .await;
        let _ = tokio::fs::remove_file(&tmp).await;
        result
    }

    fn scratch_file(&self, what: &str) -> PathBuf {
        self.0
            .dir
            .join("builds")
            .join(format!(".{what}-{}.tar", uuid::Uuid::new_v4()))
    }

    async fn is_running(&self, name: &str) -> bool {
        self.docker()
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
            .ok()
            .and_then(|c| c.state)
            .and_then(|s| s.running)
            .unwrap_or(false)
    }

    /// Which app each managed container and each published host port belongs to,
    /// to name the owners of listening sockets.
    pub async fn port_owners(
        &self,
    ) -> anyhow::Result<(HashMap<String, String>, HashMap<(Protocol, u16), String>)> {
        let filters = HashMap::from([(
            "label".to_string(),
            vec![format!("{LABEL_NODE}={}", self.0.token.node)],
        )]);
        let containers = self
            .docker()
            .list_containers(Some(ListContainersOptions {
                all: false,
                filters: Some(filters),
                ..Default::default()
            }))
            .await?;
        let mut by_container = HashMap::new();
        let mut by_port = HashMap::new();
        for c in containers {
            let Some(app) = c.labels.as_ref().and_then(|l| l.get(LABEL_APP)).cloned() else {
                continue;
            };
            for port in c.ports.unwrap_or_default() {
                let Some(public) = port.public_port else {
                    continue;
                };
                let proto = match port.typ.map(|t| t.to_string()).as_deref() {
                    Some("udp") => Protocol::Udp,
                    _ => Protocol::Tcp,
                };
                by_port.insert((proto, public), app.clone());
            }
            if let Some(id) = c.id {
                by_container.insert(id, app);
            }
        }
        Ok((by_container, by_port))
    }

    /// The Docker volume for one of the app's volumes, created if needed.
    async fn ensure_volume(
        &self,
        spec: &facade_core::AppSpec,
        volume: &facade_core::Volume,
    ) -> anyhow::Result<String> {
        let name = spec.volume_name(volume);
        if self.docker().inspect_volume(&name).await.is_err() {
            let mut labels = HashMap::from([
                (LABEL_APP.to_string(), spec.name.clone()),
                (LABEL_NODE.to_string(), self.0.token.node.clone()),
                (LABEL_VOLUME.to_string(), volume.name.clone()),
            ]);
            if let Some(stack) = &spec.stack {
                labels.insert(LABEL_STACK.to_string(), stack.stack.clone());
            }
            self.docker()
                .create_volume(VolumeCreateRequest {
                    name: Some(name.clone()),
                    labels: Some(labels),
                    ..Default::default()
                })
                .await
                .with_context(|| format!("creating volume {name}"))?;
        }
        Ok(name)
    }

    /// The network a stack's services share.
    async fn ensure_named_network(&self, name: &str, stack: &str) -> anyhow::Result<()> {
        if self.docker().inspect_network(name, None).await.is_ok() {
            return Ok(());
        }
        let created = self
            .docker()
            .create_network(NetworkCreateRequest {
                name: name.to_string(),
                driver: Some("bridge".into()),
                labels: Some(HashMap::from([(
                    LABEL_STACK.to_string(),
                    stack.to_string(),
                )])),
                ..Default::default()
            })
            .await;
        match created {
            Ok(_) => Ok(()),
            Err(_) if self.docker().inspect_network(name, None).await.is_ok() => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// A bridge network per app, so apps cannot reach each other's containers
    /// unless they link to them.
    async fn ensure_network(&self, app: &str) -> anyhow::Result<()> {
        let name = network(app);
        if self.docker().inspect_network(&name, None).await.is_ok() {
            return Ok(());
        }
        let created = self
            .docker()
            .create_network(NetworkCreateRequest {
                name,
                driver: Some("bridge".into()),
                labels: Some(HashMap::from([(LABEL_APP.to_string(), app.to_string())])),
                ..Default::default()
            })
            .await;
        match created {
            Ok(_) => Ok(()),
            // Created meanwhile by a linked app's convergence.
            Err(_)
                if self
                    .docker()
                    .inspect_network(&network(app), None)
                    .await
                    .is_ok() =>
            {
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn start(&self, name: &str) -> anyhow::Result<()> {
        match self
            .docker()
            .start_container(name, None::<StartContainerOptions>)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if is_not_modified(&e) => Ok(()),
            Err(e) => Err(anyhow!(e).context(format!("starting {name}"))),
        }
    }

    async fn stop(&self, app: &str) -> anyhow::Result<()> {
        match self
            .docker()
            .stop_container(
                &container(app),
                Some(StopContainerOptions {
                    t: Some(10),
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) || is_not_modified(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn restart(&self, app: &str) -> anyhow::Result<()> {
        if self.static_dir(app).exists() {
            return Ok(());
        }
        self.docker()
            .restart_container(
                &container(app),
                Some(RestartContainerOptions {
                    t: Some(10),
                    ..Default::default()
                }),
            )
            .await
            .with_context(|| format!("restarting {app}"))
    }

    /// Removes the app's container, network and images; its volumes too when
    /// `volumes` is set (otherwise its data stays for a later app of that name).
    pub async fn remove(&self, app: &str, volumes: bool) -> anyhow::Result<()> {
        for name in [container(app), format!("{}-release", container(app))] {
            match self
                .docker()
                .remove_container(
                    &name,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
            {
                Ok(()) => {}
                Err(e) if is_not_found(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut networks = vec![network(app)];
        // A stack's network goes with its last service.
        if let Some((stack, _)) = app.split_once('.') {
            networks.push(stack_network(stack));
        }
        for net in networks {
            match self.docker().remove_network(&net).await {
                Ok(()) => {}
                Err(e) if is_not_found(&e) => {}
                // Containers are still attached; it goes with the last of them.
                Err(e) => tracing::debug!("removing network {net}: {e}"),
            }
        }
        self.prune_images(app, "", 0).await;
        let dir = self.static_dir(app);
        if dir.exists() {
            tokio::fs::remove_dir_all(dir).await?;
        }
        if volumes {
            for volume in self.volumes().await? {
                if volume.app == app {
                    self.remove_volume(&volume.name).await?;
                }
            }
        }
        Ok(())
    }

    /// facade volumes of this node, with whether a container uses them.
    pub async fn volumes(&self) -> anyhow::Result<Vec<VolumeUsage>> {
        let filters = HashMap::from([(
            "label".to_string(),
            vec![format!("{LABEL_NODE}={}", self.0.token.node)],
        )]);
        let list = self
            .docker()
            .list_volumes(Some(ListVolumesOptions {
                filters: Some(filters),
            }))
            .await?;
        let containers = self
            .docker()
            .list_containers(Some(ListContainersOptions {
                all: true,
                ..Default::default()
            }))
            .await?;
        let used: Vec<String> = containers
            .iter()
            .flat_map(|c| c.mounts.iter().flatten())
            .filter_map(|m| m.name.clone())
            .collect();
        let volumes: Vec<(VolumeUsage, PathBuf)> = list
            .volumes
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| {
                let dir = PathBuf::from(&v.mountpoint);
                Some(VolumeUsage {
                    app: v
                        .labels
                        .get(LABEL_STACK)
                        .or(v.labels.get(LABEL_APP))?
                        .clone(),
                    volume: v.labels.get(LABEL_VOLUME)?.clone(),
                    in_use: used.contains(&v.name),
                    name: v.name,
                    // Measured below, off the async threads.
                    size: dir.is_dir().then_some(0),
                })
                .map(|u| (u, dir))
            })
            .collect::<Vec<_>>();
        let mut volumes: Vec<VolumeUsage> = tokio::task::spawn_blocking(move || {
            volumes
                .into_iter()
                .map(|(mut u, dir)| {
                    if u.size.is_some() {
                        u.size = Some(volumes::size(&dir));
                    }
                    u
                })
                .collect()
        })
        .await?;
        volumes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(volumes)
    }

    /// Deletes a facade volume of this node.
    pub async fn remove_volume(&self, name: &str) -> anyhow::Result<()> {
        let volume = self
            .docker()
            .inspect_volume(name)
            .await
            .with_context(|| format!("no volume {name} on this node"))?;
        if volume.labels.get(LABEL_NODE) != Some(&self.0.token.node) {
            bail!("{name} is not a facade volume of this node");
        }
        self.docker()
            .remove_volume(name, None::<RemoveVolumeOptions>)
            .await
            .with_context(|| format!("deleting {name} (is a container still using it?)"))?;
        tracing::info!("deleted volume {name}");
        Ok(())
    }

    /// Removes built images of `app` other than `keep` and the `spare` most recent
    /// others, kept for quick rollbacks.
    pub async fn prune_images(&self, app: &str, keep: &str, spare: usize) {
        let filters = HashMap::from([("reference".to_string(), vec![image_repo(app)])]);
        let Ok(mut images) = self
            .docker()
            .list_images(Some(ListImagesOptions {
                filters: Some(filters),
                ..Default::default()
            }))
            .await
        else {
            return;
        };
        images.sort_by_key(|i| std::cmp::Reverse(i.created));
        let mut spared = 0;
        for image in images {
            if image.repo_tags.iter().any(|t| t == keep) {
                continue;
            }
            if spared < spare {
                spared += 1;
                continue;
            }
            for tag in image.repo_tags {
                let _ = self
                    .docker()
                    .remove_image(&tag, None::<RemoveImageOptions>, None)
                    .await;
            }
        }
    }

    /// The image each facade container of this node runs, by app.
    pub async fn running_images(&self) -> HashMap<String, String> {
        let filters = HashMap::from([(
            "label".to_string(),
            vec![format!("{LABEL_NODE}={}", self.0.token.node)],
        )]);
        let Ok(containers) = self
            .docker()
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            }))
            .await
        else {
            return HashMap::new();
        };
        containers
            .into_iter()
            .filter_map(|c| Some((c.labels?.get(LABEL_APP)?.clone(), c.image?)))
            .collect()
    }

    /// Apps with a managed container or static directory on this node.
    pub async fn managed_apps(&self) -> anyhow::Result<Vec<String>> {
        let mut apps = Vec::new();
        let filters = HashMap::from([(
            "label".to_string(),
            vec![
                LABEL_APP.to_string(),
                format!("{LABEL_NODE}={}", self.0.token.node),
            ],
        )]);
        let containers = self
            .docker()
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            }))
            .await?;
        for c in containers {
            if let Some(app) = c.labels.and_then(|mut l| l.remove(LABEL_APP)) {
                apps.push(app);
            }
        }
        let mut dirs = tokio::fs::read_dir(self.0.dir.join("static")).await?;
        while let Some(entry) = dirs.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with('.') {
                apps.push(name);
            }
        }
        apps.sort();
        apps.dedup();
        Ok(apps)
    }

    /// Where the app's HTTP port is reachable from this machine.
    pub async fn http_target(&self, w: &Workload) -> anyhow::Result<SocketAddr> {
        let port = w
            .spec
            .http_port
            .ok_or_else(|| anyhow!("{} has no http_port", w.spec.name))?;
        if w.spec.host_network {
            return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
        }
        let info = self
            .docker()
            .inspect_container(&container(&w.spec.name), None::<InspectContainerOptions>)
            .await
            .with_context(|| format!("{} has no container", w.spec.name))?;
        let host_port = info
            .network_settings
            .and_then(|n| n.ports)
            .and_then(|mut p| p.remove(&format!("{port}/tcp")))
            .flatten()
            .and_then(|b| b.into_iter().find_map(|b| b.host_port))
            .and_then(|p| p.parse::<u16>().ok())
            .ok_or_else(|| anyhow!("{} is not running", w.spec.name))?;
        Ok(SocketAddr::from(([127, 0, 0, 1], host_port)))
    }

    async fn converge_static(&self, w: &Workload, log: &DeployLog) -> anyhow::Result<AppState> {
        let Source::Git { repo, branch, .. } = &w.spec.source else {
            bail!("static sites come from git");
        };
        let dir = self.static_dir(&w.spec.name);
        let key = match &w.commit {
            Some(commit) => short_hash(&(&w.spec.source, commit)),
            None => short_hash(&(&w.spec.source, w.revision)),
        };
        let marker = dir.join(".facade-key");
        if tokio::fs::read_to_string(&marker).await.ok().as_deref() == Some(key.as_str()) {
            log.begin();
            return Ok(AppState::Running);
        }
        let _slot = log.build_slot(w.commit.clone()).await;
        log.state(AppState::Building);
        let staging = Scratch::new(
            self.0
                .dir
                .join("static")
                .join(format!(".{}-{key}", w.spec.name)),
        );
        within_build_time(async {
            let _ = tokio::fs::remove_dir_all(&staging.0).await;
            let access = Access::new(repo, w.git_auth.as_ref(), &self.git_dir())?;
            let (sha, title) = build::git_checkout(
                &access,
                branch.as_deref(),
                w.commit.as_deref(),
                &staging.0,
                log,
            )
            .await?;
            log.commit(&sha, &title);
            Ok(())
        })
        .await?;
        log.build_done();
        let _ = tokio::fs::remove_dir_all(staging.0.join(".git")).await;
        tokio::fs::write(staging.0.join(".facade-key"), &key).await?;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::rename(&staging.0, &dir).await?;
        log.line("static site updated");
        Ok(AppState::Running)
    }

    pub fn static_root(&self, w: &Workload) -> anyhow::Result<PathBuf> {
        let Source::Git {
            build: Build::Static { dir },
            ..
        } = &w.spec.source
        else {
            bail!("{} is not a static site", w.spec.name);
        };
        let root = self.static_dir(&w.spec.name).join(dir);
        if !root.is_dir() {
            bail!("{} has not been fetched yet", w.spec.name);
        }
        Ok(root)
    }

    pub async fn stream_logs<W: AsyncWrite + Unpin>(
        &self,
        app: &str,
        tail: u32,
        follow: bool,
        mut out: W,
    ) -> anyhow::Result<()> {
        if self.static_dir(app).exists() {
            out.write_all(b"static sites have no logs\n").await?;
            return Ok(());
        }
        let name = container(app);
        // The stream always ends cleanly, with the reason in it if it could not
        // go on: cut off, it reads as a network failure on the other side.
        let ended = match self
            .docker()
            .inspect_container(&name, None::<InspectContainerOptions>)
            .await
        {
            Err(e) if is_not_found(&e) => Some(format!(
                "{app} has no container on this node yet: its logs start with its next deploy"
            )),
            Err(e) => Some(format!("cannot read the logs of {app}: {e}")),
            Ok(_) => {
                let mut logs = self.docker().logs(
                    &name,
                    Some(LogsOptions {
                        follow,
                        stdout: true,
                        stderr: true,
                        timestamps: true,
                        tail: tail.to_string(),
                        ..Default::default()
                    }),
                );
                let mut ended = None;
                while let Some(chunk) = logs.next().await {
                    match chunk {
                        Ok(chunk) => {
                            out.write_all(&chunk.into_bytes()).await?;
                            out.flush().await?;
                        }
                        // Removed while followed: a redeploy replaced it.
                        Err(e) if is_not_found(&e) => {
                            ended = Some(format!("{app}'s container was replaced or removed"));
                            break;
                        }
                        Err(e) => {
                            ended = Some(format!("log stream ended: {e}"));
                            break;
                        }
                    }
                }
                if follow && ended.is_none() {
                    ended = Some(format!(
                        "{app}'s container stopped or was replaced (a deploy or a configuration change): reopen the logs to follow the new one"
                    ));
                }
                ended
            }
        };
        if let Some(reason) = ended {
            out.write_all(format!("[façade] {reason}\n").as_bytes())
                .await?;
        }
        out.shutdown().await?;
        Ok(())
    }

    pub async fn app_stats(&self, workloads: &[Workload]) -> Vec<AppStats> {
        let futures = workloads.iter().map(|w| self.one_app_stats(w));
        futures_util::future::join_all(futures).await
    }

    async fn one_app_stats(&self, w: &Workload) -> AppStats {
        let app = w.spec.name.clone();
        let mut stats = AppStats {
            app: app.clone(),
            state: AppState::Unknown,
            cpu_percent: 0.0,
            mem_used: 0,
            mem_limit: 0,
            net_rx: 0,
            net_tx: 0,
            restarts: 0,
        };
        if w.spec.is_static() {
            stats.state = match (w.spec.enabled, self.static_root(w).is_ok()) {
                (false, _) => AppState::Stopped,
                (true, true) => AppState::Running,
                (true, false) => AppState::Pending,
            };
            return stats;
        }
        let name = container(&app);
        let Ok(info) = self
            .docker()
            .inspect_container(&name, None::<InspectContainerOptions>)
            .await
        else {
            stats.state = if w.spec.enabled {
                AppState::Pending
            } else {
                AppState::Stopped
            };
            return stats;
        };
        stats.restarts = info.restart_count.unwrap_or(0).max(0) as u64;
        let state = info.state.unwrap_or_default();
        stats.state = if state.restarting == Some(true) {
            AppState::Crashed
        } else if state.running == Some(true) {
            AppState::Running
        } else if w.spec.enabled && state.exit_code.unwrap_or(0) != 0 {
            AppState::Crashed
        } else {
            AppState::Stopped
        };
        if stats.state != AppState::Running {
            return stats;
        }
        let mut stream = self.docker().stats(
            &name,
            Some(StatsOptions {
                stream: false,
                one_shot: false,
            }),
        );
        if let Some(Ok(s)) = stream.next().await {
            let cpu = s.cpu_stats.unwrap_or_default();
            let pre = s.precpu_stats.unwrap_or_default();
            let total = |c: &bollard::models::ContainerCpuStats| {
                c.cpu_usage
                    .as_ref()
                    .and_then(|u| u.total_usage)
                    .unwrap_or(0)
            };
            let cpu_delta = total(&cpu).saturating_sub(total(&pre)) as f64;
            let sys_delta =
                cpu.system_cpu_usage
                    .unwrap_or(0)
                    .saturating_sub(pre.system_cpu_usage.unwrap_or(0)) as f64;
            let cpus = cpu.online_cpus.unwrap_or(1).max(1) as f64;
            if sys_delta > 0.0 {
                stats.cpu_percent = (cpu_delta / sys_delta * cpus * 100.0) as f32;
            }
            if let Some(mem) = s.memory_stats {
                // Page cache is reclaimable; report what `docker stats` reports.
                let cache = mem
                    .stats
                    .as_ref()
                    .and_then(|m| m.get("inactive_file").or_else(|| m.get("cache")).copied())
                    .unwrap_or(0);
                stats.mem_used = mem.usage.unwrap_or(0).saturating_sub(cache);
                stats.mem_limit = mem.limit.unwrap_or(0);
            }
            for net in s.networks.unwrap_or_default().values() {
                stats.net_rx += net.rx_bytes.unwrap_or(0);
                stats.net_tx += net.tx_bytes.unwrap_or(0);
            }
        }
        stats
    }
}
