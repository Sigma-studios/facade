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
    ContainerCreateBody, HostConfig, HostConfigLogConfig, Mount, MountType, NetworkCreateRequest,
    PortBinding, RestartPolicy, RestartPolicyNameEnum, VolumeCreateRequest,
};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, ListContainersOptions,
    ListImagesOptions, ListVolumesOptions, LogsOptions, RemoveContainerOptions, RemoveImageOptions,
    RemoveVolumeOptions, RestartContainerOptions, StartContainerOptions, StatsOptions,
    StopContainerOptions,
};
use facade_core::proto::{AppState, AppStats, CredentialKind, JoinToken, VolumeInfo, Workload};
use facade_core::{Build, Protocol, Source};
use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use super::DeployLog;
use super::build;
use super::volumes;
use crate::lang;

const LABEL_APP: &str = "facade.app";
const LABEL_RUN: &str = "facade.run";
const LABEL_NODE: &str = "facade.node";
const LABEL_VOLUME: &str = "facade.volume";

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

fn image_repo(app: &str) -> String {
    format!("facade-{app}")
}

/// `_` cannot appear in app or volume names, so this never collides.
fn volume_name(app: &str, volume: &str) -> String {
    format!("facade-{app}_{volume}")
}

fn short_hash(value: &impl Serialize) -> String {
    let json = serde_json::to_vec(value).expect("serializable");
    hex::encode(&Sha256::digest(json)[..8])
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
        let docker = Docker::connect_with_local_defaults().context("connecting to Docker")?;
        docker
            .ping()
            .await
            .context("Docker is not reachable (is it installed and running?)")?;
        for sub in ["static", "builds"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        Ok(Self(Arc::new(Inner { docker, dir, token })))
    }

    fn docker(&self) -> &Docker {
        &self.0.docker
    }

    fn static_dir(&self, app: &str) -> PathBuf {
        self.0.dir.join("static").join(app)
    }

    /// Bring one app to its desired state.
    pub async fn converge(&self, w: &Workload, log: &DeployLog) -> anyhow::Result<AppState> {
        let app = &w.spec.name;
        if w.spec.is_static() {
            if !w.spec.enabled {
                return Ok(AppState::Stopped);
            }
            return self.converge_static(w, log).await;
        }
        if !w.spec.enabled {
            self.stop(app).await?;
            log.line("stopped");
            return Ok(AppState::Stopped);
        }

        let image = self.ensure_image(w, log).await?;
        let run_key = short_hash(&(w.run_identity(), &image));
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
                    if !running {
                        log.line("starting existing container");
                        self.start(&name).await?;
                    }
                    return Ok(AppState::Running);
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
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(e.into()),
        }

        self.create(w, &image, &run_key).await?;
        self.start(&name).await?;
        log.line(format!("started {name} from {image}"));
        if !matches!(w.spec.source, Source::Image { .. }) {
            self.prune_images(app, &image).await;
        }
        Ok(AppState::Running)
    }

    /// The image to run: pulled for `Source::Image`, built otherwise. Builds are
    /// tagged with a hash of the source and revision, so they are reused until either
    /// changes.
    async fn ensure_image(&self, w: &Workload, log: &DeployLog) -> anyhow::Result<String> {
        match &w.spec.source {
            Source::Image { image } => {
                let image = &facade_core::image_with_tag(image);
                let present = self.docker().inspect_image(image).await.is_ok();
                // An explicit deploy re-pulls, so moving tags like `latest` update.
                if !present || log.is_deployment() {
                    log.state(AppState::Building);
                    let registry = facade_core::image_registry(image);
                    let credentials = w.credential(CredentialKind::Registry, &registry).map(|c| {
                        DockerCredentials {
                            username: Some(c.username.clone()),
                            password: Some(c.secret.clone()),
                            serveraddress: Some(registry.clone()),
                            ..Default::default()
                        }
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
            } => {
                let key = short_hash(&(&w.spec.source, w.revision));
                let tag = format!("{}:{key}", image_repo(&w.spec.name));
                if self.docker().inspect_image(&tag).await.is_ok() {
                    return Ok(tag);
                }
                if arch != std::env::consts::ARCH {
                    bail!(
                        "{filename} is built for {arch} but this node is {}",
                        std::env::consts::ARCH
                    );
                }
                log.state(AppState::Building);
                let workdir = self
                    .0
                    .dir
                    .join("builds")
                    .join(format!("{}-{key}", w.spec.name));
                let _ = tokio::fs::remove_dir_all(&workdir).await;
                tokio::fs::create_dir_all(&workdir).await?;
                let result = async {
                    log.line(format!("downloading {filename}"));
                    super::fetch_artifact(&self.0.token, sha256, &workdir.join("app")).await?;
                    let dockerfile = workdir.join("Dockerfile");
                    tokio::fs::write(&dockerfile, lang::executable::dockerfile(filename)).await?;
                    build::docker_build(&w.spec.name, &tag, &dockerfile, &workdir, w, log).await
                }
                .await;
                let _ = tokio::fs::remove_dir_all(&workdir).await;
                result?;
                Ok(tag)
            }
            Source::Git {
                repo,
                branch,
                build,
            } => {
                let key = short_hash(&(&w.spec.source, w.revision));
                let tag = format!("{}:{key}", image_repo(&w.spec.name));
                if self.docker().inspect_image(&tag).await.is_ok() {
                    return Ok(tag);
                }
                log.state(AppState::Building);
                let workdir = self
                    .0
                    .dir
                    .join("builds")
                    .join(format!("{}-{key}", w.spec.name));
                let _ = tokio::fs::remove_dir_all(&workdir).await;
                let result = async {
                    build::git_checkout(repo, branch.as_deref(), &workdir, w, log).await?;
                    let (dockerfile, context) = match build {
                        Build::Dockerfile {
                            dockerfile,
                            context,
                        } => (workdir.join(dockerfile), workdir.join(context)),
                        Build::Static { .. } => unreachable!("static sites have no image"),
                        generated => {
                            let path = workdir.join(".facade.Dockerfile");
                            let contents = lang::generated_dockerfile(&w.spec.name, generated)
                                .expect("every other build kind generates a Dockerfile");
                            tokio::fs::write(&path, contents).await?;
                            (path, workdir.clone())
                        }
                    };
                    build::docker_build(&w.spec.name, &tag, &dockerfile, &context, w, log).await
                }
                .await;
                let _ = tokio::fs::remove_dir_all(&workdir).await;
                result?;
                Ok(tag)
            }
        }
    }

    /// Creates the app's volumes that do not exist yet.
    async fn ensure_volumes(&self, w: &Workload) -> anyhow::Result<Vec<Mount>> {
        let mut mounts = Vec::new();
        for volume in &w.spec.volumes {
            let name = self.ensure_volume(&w.spec.name, &volume.name).await?;
            mounts.push(Mount {
                target: Some(volume.path.clone()),
                source: Some(name),
                typ: Some(MountType::VOLUME),
                ..Default::default()
            });
        }
        Ok(mounts)
    }

    async fn ensure_volume(&self, app: &str, volume: &str) -> anyhow::Result<String> {
        let name = volume_name(app, volume);
        if self.docker().inspect_volume(&name).await.is_ok() {
            return Ok(name);
        }
        self.docker()
            .create_volume(VolumeCreateRequest {
                name: Some(name.clone()),
                labels: Some(HashMap::from([
                    (LABEL_APP.to_string(), app.to_string()),
                    (LABEL_VOLUME.to_string(), volume.to_string()),
                    (LABEL_NODE.to_string(), self.0.token.node.clone()),
                ])),
                ..Default::default()
            })
            .await
            .with_context(|| format!("creating volume {name}"))?;
        Ok(name)
    }

    /// The directory holding a volume's files on this machine.
    async fn volume_dir(&self, app: &str, volume: &str) -> anyhow::Result<PathBuf> {
        let name = self.ensure_volume(app, volume).await?;
        let info = self.docker().inspect_volume(&name).await?;
        let dir = PathBuf::from(info.mountpoint);
        if !dir.is_dir() {
            bail!(
                "volume {name} is at {}, which this agent cannot reach (is it running as root, next to Docker?)",
                dir.display()
            );
        }
        Ok(dir)
    }

    /// Every facade volume of this node, with its size.
    pub async fn list_volumes(&self) -> anyhow::Result<Vec<VolumeInfo>> {
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
        let mut out = Vec::new();
        for v in list.volumes.unwrap_or_default() {
            let (Some(app), Some(name)) = (v.labels.get(LABEL_APP), v.labels.get(LABEL_VOLUME))
            else {
                continue;
            };
            let dir = PathBuf::from(&v.mountpoint);
            let size =
                tokio::task::spawn_blocking(move || dir.is_dir().then(|| volumes::size(&dir)))
                    .await
                    .ok()
                    .flatten();
            out.push(VolumeInfo {
                app: app.clone(),
                name: name.clone(),
                size,
            });
        }
        out.sort_by(|a, b| (&a.app, &a.name).cmp(&(&b.app, &b.name)));
        Ok(out)
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
        let dir = self.volume_dir(app, volume).await?;
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
        mut input: R,
        size: u64,
    ) -> anyhow::Result<()> {
        let dir = self.volume_dir(app, volume).await?;
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

    async fn create(&self, w: &Workload, image: &str, run_key: &str) -> anyhow::Result<()> {
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

        let network_mode = if spec.host_network {
            "host".to_string()
        } else {
            self.ensure_network(app).await?;
            network(app)
        };
        let mounts = self.ensure_volumes(w).await?;

        let host_config = HostConfig {
            port_bindings: Some(bindings),
            network_mode: Some(network_mode),
            mounts: (!mounts.is_empty()).then_some(mounts),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            memory: spec.limits.memory_mb.map(|mb| (mb * 1024 * 1024) as i64),
            nano_cpus: spec.limits.cpus.map(|c| (c * 1e9) as i64),
            pids_limit: Some(4096),
            security_opt: Some(vec!["no-new-privileges".into()]),
            init: Some(true),
            log_config: Some(HostConfigLogConfig {
                typ: Some("json-file".into()),
                config: Some(HashMap::from([
                    ("max-size".to_string(), "10m".to_string()),
                    ("max-file".to_string(), "3".to_string()),
                ])),
            }),
            ..Default::default()
        };

        let body = ContainerCreateBody {
            image: Some(image.to_string()),
            cmd: (!spec.args.is_empty()).then(|| spec.args.clone()),
            env: Some(w.env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
            exposed_ports: Some(exposed),
            labels: Some(HashMap::from([
                (LABEL_APP.to_string(), app.clone()),
                (LABEL_RUN.to_string(), run_key.to_string()),
                (LABEL_NODE.to_string(), self.0.token.node.clone()),
            ])),
            host_config: Some(host_config),
            ..Default::default()
        };
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
        Ok(())
    }

    /// A bridge network per app, so apps cannot reach each other's containers.
    async fn ensure_network(&self, app: &str) -> anyhow::Result<()> {
        let name = network(app);
        if self.docker().inspect_network(&name, None).await.is_ok() {
            return Ok(());
        }
        self.docker()
            .create_network(NetworkCreateRequest {
                name,
                driver: Some("bridge".into()),
                labels: Some(HashMap::from([(LABEL_APP.to_string(), app.to_string())])),
                ..Default::default()
            })
            .await?;
        Ok(())
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

    /// Removes the app's container, network, images and static files. Its volumes
    /// stay unless `delete_volumes`: data is only ever deleted on purpose.
    pub async fn remove(&self, app: &str, delete_volumes: bool) -> anyhow::Result<()> {
        match self
            .docker()
            .remove_container(
                &container(app),
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
        match self.docker().remove_network(&network(app)).await {
            Ok(()) => {}
            Err(e) if is_not_found(&e) => {}
            Err(e) => tracing::warn!("removing network for {app}: {e}"),
        }
        self.prune_images(app, "").await;
        let dir = self.static_dir(app);
        if dir.exists() {
            tokio::fs::remove_dir_all(dir).await?;
        }
        if delete_volumes {
            for volume in self.list_volumes().await? {
                if volume.app == app {
                    let name = volume_name(app, &volume.name);
                    self.docker()
                        .remove_volume(&name, Some(RemoveVolumeOptions { force: true }))
                        .await
                        .with_context(|| format!("deleting volume {name}"))?;
                    tracing::info!("deleted volume {name}");
                }
            }
        }
        Ok(())
    }

    /// Removes built images of `app` other than `keep`.
    async fn prune_images(&self, app: &str, keep: &str) {
        let filters = HashMap::from([("reference".to_string(), vec![image_repo(app)])]);
        let Ok(images) = self
            .docker()
            .list_images(Some(ListImagesOptions {
                filters: Some(filters),
                ..Default::default()
            }))
            .await
        else {
            return;
        };
        for image in images {
            for tag in image.repo_tags {
                if tag != keep {
                    let _ = self
                        .docker()
                        .remove_image(&tag, None::<RemoveImageOptions>, None)
                        .await;
                }
            }
        }
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
        let key = short_hash(&(&w.spec.source, w.revision));
        let marker = dir.join(".facade-key");
        if tokio::fs::read_to_string(&marker).await.ok().as_deref() == Some(key.as_str()) {
            return Ok(AppState::Running);
        }
        log.state(AppState::Building);
        let staging = self
            .0
            .dir
            .join("static")
            .join(format!(".{}-{key}", w.spec.name));
        let _ = tokio::fs::remove_dir_all(&staging).await;
        build::git_checkout(repo, branch.as_deref(), &staging, w, log).await?;
        let _ = tokio::fs::remove_dir_all(staging.join(".git")).await;
        tokio::fs::write(staging.join(".facade-key"), &key).await?;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::rename(&staging, &dir).await?;
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
        let mut logs = self.docker().logs(
            &container(app),
            Some(LogsOptions {
                follow,
                stdout: true,
                stderr: true,
                timestamps: true,
                tail: tail.to_string(),
                ..Default::default()
            }),
        );
        while let Some(chunk) = logs.next().await {
            let chunk = chunk?;
            out.write_all(&chunk.into_bytes()).await?;
            out.flush().await?;
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
