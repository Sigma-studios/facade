//! The containers on the node that facade does not manage: started by hand, by
//! docker compose, by another tool. Admins see them (they hold ports, disk and
//! memory facade cannot account for) and may remove one; facade's own containers
//! are never touched here, whatever the request names.

use std::collections::HashMap;

use anyhow::{Context, bail};
use bollard::Docker;
use bollard::models::ContainerInspectResponse;
use bollard::query_parameters::{
    InspectContainerOptions, ListContainersOptions, RemoveContainerOptions, RemoveImageOptions,
    StopContainerOptions,
};
use facade_core::Protocol;
use facade_core::proto::{ContainerPort, OtherContainer, RemovedContainer, container_warnings};

use super::runtime::{LABEL_APP, LABEL_NODE, is_not_found};

const COMPOSE_PROJECT: &str = "com.docker.compose.project";
const COMPOSE_SERVICE: &str = "com.docker.compose.service";

/// Whether the labels make a container (or an image) facade's: any node's, so
/// agents sharing a Docker daemon leave each other's alone too.
fn is_facades(labels: &HashMap<String, String>) -> bool {
    labels.contains_key(LABEL_APP) || labels.contains_key(LABEL_NODE)
}

/// Whether this agent runs inside container `id` (not how facade installs, but
/// removing it would cut the node off). In a container, `/etc/hostname` is
/// Docker's bind mount of `<docker>/containers/<id>/hostname`; under cgroup v1
/// its cgroup names it too. Not just any mention of the id: on the host, every
/// running container's `/dev/shm` shows up in our mounts.
fn is_own(id: &str) -> bool {
    let mounted = std::fs::read_to_string("/proc/self/mountinfo")
        .is_ok_and(|mounts| hostname_mounted_from(&mounts, id));
    mounted || std::fs::read_to_string("/proc/self/cgroup").is_ok_and(|c| c.contains(id))
}

/// Whether `/etc/hostname` in `mountinfo` is container `id`'s file.
fn hostname_mounted_from(mountinfo: &str, id: &str) -> bool {
    mountinfo.lines().any(|line| {
        // `36 35 98:0 /root /mount/point …`
        let fields: Vec<&str> = line.split_whitespace().collect();
        fields.get(4) == Some(&"/etc/hostname") && fields.get(3).is_some_and(|r| r.contains(id))
    })
}

fn short(id: &str) -> String {
    id.trim_start_matches("sha256:").chars().take(12).collect()
}

/// Every container without facade's labels, by name. Docker sizes what each
/// container wrote, which takes a moment on a busy node: this runs on demand.
pub async fn list(docker: &Docker) -> anyhow::Result<Vec<OtherContainer>> {
    let containers = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            size: true,
            ..Default::default()
        }))
        .await?;
    let mut out = Vec::new();
    for c in containers {
        let labels = c.labels.unwrap_or_default();
        let Some(id) = c.id else { continue };
        if is_facades(&labels) || is_own(&id) {
            continue;
        }
        // The list leaves restart policies out.
        let restart = docker
            .inspect_container(&id, None::<InspectContainerOptions>)
            .await
            .ok()
            .and_then(|i| restart_policy(&i));
        let mut ports: Vec<ContainerPort> = c
            .ports
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| {
                Some(ContainerPort {
                    protocol: match p.typ.map(|t| t.to_string()).as_deref() {
                        Some("udp") => Protocol::Udp,
                        _ => Protocol::Tcp,
                    },
                    address: p.ip.unwrap_or_else(|| "0.0.0.0".into()),
                    host: p.public_port?,
                    container: p.private_port,
                })
            })
            .collect();
        ports.sort_by(|a, b| {
            (a.host, a.protocol as u8, &a.address).cmp(&(b.host, b.protocol as u8, &b.address))
        });
        ports.dedup();
        out.push(OtherContainer {
            name: c
                .names
                .and_then(|n| n.into_iter().next())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_else(|| short(&id)),
            id: short(&id),
            image: c.image.unwrap_or_default(),
            state: c.state.map(|s| s.to_string()).unwrap_or_default(),
            status: c.status.unwrap_or_default(),
            created: c.created.unwrap_or(0).max(0) as u64,
            ports,
            compose_project: labels.get(COMPOSE_PROJECT).cloned(),
            compose_service: labels.get(COMPOSE_SERVICE).cloned(),
            restart,
            size: c.size_rw.map(|s| s.max(0) as u64),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// `always`, `unless-stopped`, `on-failure`; `None` for `no`.
fn restart_policy(info: &ContainerInspectResponse) -> Option<String> {
    let name = info
        .host_config
        .as_ref()?
        .restart_policy
        .as_ref()?
        .name?
        .to_string();
    (!name.is_empty() && name != "no").then_some(name)
}

/// Stops and removes `target` (a name or an id), with its anonymous volumes if
/// `volumes` (Docker never removes named ones with a container), and its image
/// if `image` and nothing else uses it. The container is inspected right
/// before, and only the id inspected is acted on, so a name or an id prefix
/// can never reach one of facade's.
pub async fn remove(
    docker: &Docker,
    target: &str,
    volumes: bool,
    image: bool,
) -> anyhow::Result<RemovedContainer> {
    let info = match docker
        .inspect_container(target, None::<InspectContainerOptions>)
        .await
    {
        Err(e) if is_not_found(&e) => bail!("no container {target} on this node"),
        other => other.with_context(|| format!("looking up container {target}"))?,
    };
    let id = info.id.clone().context("Docker gave the container no id")?;
    let name = info
        .name
        .as_deref()
        .map(|n| n.trim_start_matches('/').to_string())
        .unwrap_or_else(|| short(&id));
    let config = info.config.clone().unwrap_or_default();
    let labels = config.labels.clone().unwrap_or_default();
    if is_facades(&labels) {
        match labels.get(LABEL_APP) {
            Some(app) => {
                bail!("{name} is managed by facade (app {app}): stop or remove the app instead")
            }
            None => bail!("{name} is managed by facade"),
        }
    }
    if is_own(&id) {
        bail!("{name} is the container this agent runs in");
    }
    let warnings = container_warnings(
        labels.get(COMPOSE_PROJECT).map(String::as_str),
        restart_policy(&info).as_deref(),
    );

    let running = info.state.as_ref().and_then(|s| s.running).unwrap_or(false);
    if running {
        // Its own stop timeout (10 s unless it says otherwise), then SIGKILL.
        match docker
            .stop_container(&id, None::<StopContainerOptions>)
            .await
        {
            Ok(()) => {}
            // Stopped meanwhile.
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 304, ..
            }) => {}
            Err(e) => return Err(e).with_context(|| format!("stopping {name}")),
        }
    }
    docker
        .remove_container(
            &id,
            Some(RemoveContainerOptions {
                v: volumes,
                ..Default::default()
            }),
        )
        .await
        .with_context(|| format!("removing {name}"))?;
    tracing::info!(
        "removed container {name} ({}), which facade does not manage",
        short(&id)
    );

    let mut removed = Vec::new();
    if volumes {
        for mount in info.mounts.iter().flatten() {
            if mount.typ.as_deref() != Some("volume") {
                continue;
            }
            let Some(volume) = &mount.name else { continue };
            if let Err(e) = docker.inspect_volume(volume).await
                && is_not_found(&e)
            {
                removed.push(volume.clone());
            }
        }
    }

    let (mut removed_image, mut image_kept) = (None, None);
    if image && let Some(image_id) = &info.image {
        let shown = config.image.clone().unwrap_or_else(|| short(image_id));
        match remove_image(docker, image_id, &shown).await {
            Ok(()) => removed_image = Some(shown),
            Err(why) => image_kept = Some(why),
        }
    }
    Ok(RemovedContainer {
        name,
        id: short(&id),
        volumes: removed,
        image: removed_image,
        image_kept,
        warnings,
    })
}

/// Removes the image unless another container (any, stopped ones too) uses it
/// or facade built it. Says why it stays otherwise.
async fn remove_image(docker: &Docker, image_id: &str, shown: &str) -> Result<(), String> {
    let users: Vec<String> = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            ..Default::default()
        }))
        .await
        .map_err(|e| format!("image {shown} kept: {e}"))?
        .into_iter()
        .filter(|c| c.image_id.as_deref() == Some(image_id))
        .filter_map(|c| c.names?.into_iter().next())
        .map(|n| n.trim_start_matches('/').to_string())
        .collect();
    if !users.is_empty() {
        return Err(format!(
            "image {shown} kept: {} use{} it",
            users.join(", "),
            if users.len() == 1 { "s" } else { "" }
        ));
    }
    let inspected = docker
        .inspect_image(image_id)
        .await
        .map_err(|e| format!("image {shown} kept: {e}"))?;
    let tags = inspected.repo_tags.unwrap_or_default();
    let built_by_facade = inspected
        .config
        .and_then(|c| c.labels)
        .is_some_and(|l| is_facades(&l))
        || tags.iter().any(|t| t.starts_with("facade-"));
    if built_by_facade {
        return Err(format!(
            "image {shown} kept: facade built it (deploys and rollbacks use it)"
        ));
    }
    // Tag by tag, without forcing: the last one takes the image with it, and
    // Docker refuses if a container started using it meanwhile.
    let refs = if tags.is_empty() {
        vec![image_id.to_string()]
    } else {
        tags
    };
    for r in refs {
        docker
            .remove_image(&r, None::<RemoveImageOptions>, None)
            .await
            .map_err(|e| format!("image {shown} kept: {e}"))?;
    }
    tracing::info!("removed image {shown}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_its_own_container_only() {
        let (mine, other) = ("770da88943f4", "273e5a0ab837");
        // On the host: another container's /dev/shm among our mounts.
        let host = "120 30 0:52 / /var/lib/docker/containers/273e5a0ab837aa/mounts/shm rw - tmpfs shm rw\n";
        assert!(!hostname_mounted_from(host, other));
        // In a container: its hostname file.
        let inside = "779 750 0:37 /docker/containers/770da88943f4bb/hostname /etc/hostname rw,noatime - btrfs /dev/vdb1 rw\n";
        assert!(hostname_mounted_from(inside, mine));
        assert!(!hostname_mounted_from(inside, other));
    }
}
