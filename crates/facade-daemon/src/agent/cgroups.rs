//! Cgroup delegation: an app that asks for it gets its container's cgroup to divide
//! between its own processes, as a supervisor giving each child its own memory and
//! CPU share.
//!
//! Docker mounts `/sys/fs/cgroup` read-only in containers. With a private cgroup
//! namespace (Docker's default on cgroup v2) that mount shows the container's own
//! cgroup and nothing above it, so the agent remounts it read-write from the host,
//! inside the container's mount namespace, every time the container starts. The
//! container's limits stay out of its reach: with `nsdelegate` (systemd mounts cgroup2
//! with it), the kernel refuses writes from inside a cgroup namespace to the interface
//! files of its root, except `cgroup.procs`, `cgroup.threads` and
//! `cgroup.subtree_control`. Nodes without it cannot delegate.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, bail};
use bollard::query_parameters::{EventsOptions, InspectContainerOptions};
use futures_util::StreamExt;
use tracing::{debug, info, warn};

use super::runtime::{LABEL_CGROUPS, Runtime};

const CGROUP_PATH: &str = "/sys/fs/cgroup";

/// The `/sys/fs/cgroup` mount of a mount table (`/proc/<pid>/mountinfo`): its
/// mount options, filesystem type and superblock options. The last mount there
/// wins, as it hides the ones below.
fn cgroup_mount(mountinfo: &str) -> Option<(Vec<&str>, &str, Vec<&str>)> {
    mountinfo.lines().rev().find_map(|line| {
        let (mount, fs) = line.split_once(" - ")?;
        let mount: Vec<&str> = mount.split(' ').collect();
        if mount.get(4) != Some(&CGROUP_PATH) {
            return None;
        }
        let mut fs = fs.split(' ');
        let fstype = fs.next()?;
        let super_options = fs.nth(1).unwrap_or_default().split(',').collect();
        Some((mount.get(5)?.split(',').collect(), fstype, super_options))
    })
}

/// Why this node cannot delegate cgroups, if it cannot.
pub fn unsupported() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("cgroup delegation needs a Linux node".into());
    }
    let mountinfo = match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(m) => m,
        Err(e) => return Some(format!("cannot read the node's mounts: {e}")),
    };
    match cgroup_mount(&mountinfo) {
        None => Some(format!(
            "the node has no cgroup filesystem at {CGROUP_PATH}"
        )),
        Some((_, fstype, _)) if fstype != "cgroup2" => {
            Some("the node uses cgroup v1; delegation needs cgroup v2".into())
        }
        Some((_, _, options)) if !options.contains(&"nsdelegate") => Some(
            "the node's cgroup2 is mounted without nsdelegate, which would let a container \
             raise its own limits"
                .into(),
        ),
        Some(_) => None,
    }
}

/// Makes `/sys/fs/cgroup` writable in the running container `name`, unless it
/// already is. True if it had to.
pub async fn delegate(runtime: &Runtime, name: &str) -> anyhow::Result<bool> {
    let pid = running_pid(runtime, name).await?;
    let mountinfo = std::fs::read_to_string(format!("/proc/{pid}/mountinfo"))
        .with_context(|| format!("reading the mounts of {name}"))?;
    let Some((options, _, _)) = cgroup_mount(&mountinfo) else {
        bail!("{name} has no {CGROUP_PATH}");
    };
    if !options.contains(&"ro") {
        return Ok(false);
    }
    let options: Vec<String> = options.iter().map(|o| o.to_string()).collect();
    let namespace = std::fs::File::open(format!("/proc/{pid}/ns/mnt"))
        .with_context(|| format!("opening the mount namespace of {name}"))?;
    // The namespace held open is that container's only if its process is still the
    // one we looked at: a pid is not reused while its container runs.
    if running_pid(runtime, name).await? != pid {
        bail!("{name} restarted meanwhile");
    }
    tokio::task::spawn_blocking(move || {
        // Joining a mount namespace changes the thread for good: use a thread of
        // its own rather than one of the runtime's.
        std::thread::spawn(move || remount_writable(&namespace, &options))
            .join()
            .map_err(|_| anyhow::anyhow!("the remount thread panicked"))?
    })
    .await??;
    Ok(true)
}

async fn running_pid(runtime: &Runtime, name: &str) -> anyhow::Result<u32> {
    let info = runtime
        .docker()
        .inspect_container(name, None::<InspectContainerOptions>)
        .await?;
    let state = info.state.unwrap_or_default();
    match state.pid {
        Some(pid) if state.running == Some(true) && pid > 0 => Ok(pid as u32),
        _ => bail!("{name} is not running"),
    }
}

#[cfg(target_os = "linux")]
fn remount_writable(namespace: &std::fs::File, options: &[String]) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;

    // Every per-mount flag but `ro` stays: a bare remount would drop nosuid & co.
    let mut flags = libc::MS_REMOUNT | libc::MS_BIND;
    for option in options {
        flags |= match option.as_str() {
            "nosuid" => libc::MS_NOSUID,
            "nodev" => libc::MS_NODEV,
            "noexec" => libc::MS_NOEXEC,
            "noatime" => libc::MS_NOATIME,
            "nodiratime" => libc::MS_NODIRATIME,
            "relatime" => libc::MS_RELATIME,
            "strictatime" => libc::MS_STRICTATIME,
            _ => 0,
        };
    }
    let check = |what: &str, result: libc::c_int| {
        if result == -1 {
            Err(std::io::Error::last_os_error()).context(what.to_string())
        } else {
            Ok(())
        }
    };
    let path = std::ffi::CString::new(CGROUP_PATH)?;
    // SAFETY: plain system calls on this thread, which shares nothing with the
    // rest of the process once `unshare(CLONE_FS)` returns and ends right after.
    unsafe {
        check("unshare", libc::unshare(libc::CLONE_FS))?;
        check(
            "entering the container's mount namespace",
            libc::setns(namespace.as_raw_fd(), libc::CLONE_NEWNS),
        )?;
        check(
            "remounting /sys/fs/cgroup",
            libc::mount(
                std::ptr::null(),
                path.as_ptr(),
                std::ptr::null(),
                flags,
                std::ptr::null(),
            ),
        )
    }
}

#[cfg(not(target_os = "linux"))]
fn remount_writable(_: &std::fs::File, _: &[String]) -> anyhow::Result<()> {
    bail!("cgroup delegation needs a Linux node")
}

/// Delegates the cgroups of the containers asking for it: those running now, then
/// each one starting, as Docker restarts containers on its own after a crash or a
/// reboot, and a restart mounts `/sys/fs/cgroup` read-only again.
pub async fn watch(runtime: Runtime) {
    if unsupported().is_some() {
        return;
    }
    loop {
        let filters = HashMap::from([
            ("type".to_string(), vec!["container".to_string()]),
            ("event".to_string(), vec!["start".to_string()]),
            (
                "label".to_string(),
                vec![runtime.node_filter(), LABEL_CGROUPS.to_string()],
            ),
        ]);
        let mut events = runtime.docker().events(Some(EventsOptions {
            filters: Some(filters),
            ..Default::default()
        }));
        // Subscribed first, so a container starting during this pass is not missed.
        for name in runtime.delegating_containers().await {
            done(&name, delegate(&runtime, &name).await);
        }
        while let Some(event) = events.next().await {
            let event = match event {
                Ok(e) => e,
                Err(e) => {
                    debug!("docker events: {e}");
                    break;
                }
            };
            let name = event
                .actor
                .and_then(|a| a.attributes)
                .and_then(|mut a| a.remove("name"));
            if let Some(name) = name {
                done(&name, delegate(&runtime, &name).await);
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn done(name: &str, result: anyhow::Result<bool>) {
    match result {
        Ok(true) => info!("delegated cgroups to {name}"),
        Ok(false) => {}
        Err(e) => warn!("delegating cgroups to {name}: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "\
24 1 252:1 / / rw,relatime shared:1 - ext4 /dev/vda1 rw
35 24 0:30 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime shared:9 - cgroup2 cgroup2 rw,nsdelegate,memory_recursiveprot
";
    const CONTAINER: &str = "\
612 611 0:30 / /sys/fs/cgroup ro,nosuid,nodev,noexec,relatime - cgroup2 cgroup rw,nsdelegate
";

    #[test]
    fn reads_the_cgroup_mount() {
        let (options, fstype, super_options) = cgroup_mount(HOST).unwrap();
        assert_eq!(options, ["rw", "nosuid", "nodev", "noexec", "relatime"]);
        assert_eq!(fstype, "cgroup2");
        assert!(super_options.contains(&"nsdelegate"));
        let (options, _, _) = cgroup_mount(CONTAINER).unwrap();
        assert!(options.contains(&"ro"));
        assert!(cgroup_mount("24 1 252:1 / / rw - ext4 /dev/vda1 rw").is_none());
    }
}
