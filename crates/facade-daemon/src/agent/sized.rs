//! Volumes with a size: a filesystem of their own, in an image file on the node.
//!
//! Docker's volumes share the node's disk, and its local driver cannot mount an
//! image file. So the agent formats one (ext4, sparse: it takes the room its data
//! takes), mounts it, and makes the Docker volume a bind of a directory inside
//! it. That directory only exists on the mounted filesystem: a container never
//! starts writing to the node's disk instead.
//!
//! At boot, `facade-volumes.service` (`facaded mount-volumes`) mounts them all
//! before Docker starts the containers. The agent writes it where it installed
//! itself; on NixOS the module has it. Before it, each volume had a mount unit
//! of its own in `/etc/systemd/system`: [`boot_setup`] replaces those.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use tokio::process::Command;

/// The filesystem of a sized volume: `<data dir>/volumes/<docker volume>/`.
pub struct SizedVolume {
    dir: PathBuf,
}

impl SizedVolume {
    pub fn new(agent_dir: &Path, docker_name: &str) -> Self {
        Self {
            dir: agent_dir.join("volumes").join(docker_name),
        }
    }

    fn image(&self) -> PathBuf {
        self.dir.join("image")
    }

    fn mountpoint(&self) -> PathBuf {
        self.dir.join("mnt")
    }

    /// What the Docker volume binds.
    pub fn data(&self) -> PathBuf {
        self.mountpoint().join("data")
    }

    pub fn exists(&self) -> bool {
        self.image().exists()
    }

    /// Its size in bytes, if it has a filesystem.
    pub fn limit(&self) -> Option<u64> {
        std::fs::metadata(self.image()).ok().map(|m| m.len())
    }

    /// What its data takes: the space used in its filesystem while mounted (as
    /// `df` says, without walking the files), else what its image file takes.
    /// Docker cannot size these volumes: `docker system df` skips volumes with
    /// mount options.
    pub fn used(&self) -> Option<u64> {
        if self.is_mounted().unwrap_or(false)
            && let Some(used) = fs_used(&self.mountpoint())
        {
            return Some(used);
        }
        self.on_disk()
    }

    /// What its image file takes on the node's disk (it is sparse).
    pub fn on_disk(&self) -> Option<u64> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(self.image())
            .ok()
            .map(|m| m.blocks() * 512)
    }

    /// Creates the filesystem, or grows it to `size_mb`, and mounts it. Its data
    /// directory is the caller's to create (or fill).
    pub async fn ensure(&self, size_mb: u64) -> anyhow::Result<()> {
        if let Some(why) = unsupported() {
            bail!("volumes with a size need {why}");
        }
        let bytes = size_mb * 1024 * 1024;
        let image = self.image();
        match self.limit() {
            None => {
                std::fs::create_dir_all(self.mountpoint())?;
                let file = std::fs::File::create(&image)?;
                file.set_len(bytes)?;
                drop(file);
                // No blocks kept for root: the container runs as root and would get
                // them, so the size it sees is the size asked for.
                run("mkfs.ext4", &["-q", "-F", "-m", "0", path(&image)?]).await?;
            }
            Some(current) if bytes < current => bail!(
                "volume {} is {}: a volume cannot shrink",
                self.name(),
                facade_core::format_size_mb(current / (1024 * 1024))
            ),
            Some(_) => {}
        }
        self.mount().await?;
        if let Some(agent_dir) = self.dir.parent().and_then(Path::parent) {
            boot_setup(
                agent_dir,
                crate::platform::current().install_kind().is_managed(),
            )
            .await?;
        }
        if self.limit().is_some_and(|current| bytes > current) {
            self.grow(bytes).await?;
        }
        Ok(())
    }

    fn name(&self) -> String {
        self.dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    /// Mounts it, unless it is.
    async fn mount(&self) -> anyhow::Result<()> {
        if !self.is_mounted()? {
            run(
                "mount",
                &[
                    "-o",
                    "loop",
                    path(&self.image())?,
                    path(&self.mountpoint())?,
                ],
            )
            .await?;
        }
        Ok(())
    }

    /// The name of the mount unit a version before `facade-volumes.service`
    /// gave it.
    async fn legacy_unit(&self) -> anyhow::Result<String> {
        Ok(run(
            "systemd-escape",
            &["--path", "--suffix=mount", path(&self.mountpoint())?],
        )
        .await?
        .trim()
        .to_string())
    }

    /// Takes out its mount unit from before `facade-volumes.service`, if it
    /// has one, without unmounting it; whether it had one.
    async fn drop_legacy_unit(&self) -> anyhow::Result<bool> {
        let unit = self.legacy_unit().await?;
        let file = Path::new(LEGACY_UNIT_DIR).join(&unit);
        let ours = std::fs::read_to_string(&file)
            .is_ok_and(|c| c.starts_with("# Written by facade for volume"));
        if !ours {
            return Ok(false);
        }
        run("systemctl", &["disable", &unit]).await?;
        std::fs::remove_file(&file)?;
        Ok(true)
    }

    fn is_mounted(&self) -> anyhow::Result<bool> {
        let target = self.mountpoint();
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
        Ok(mountinfo
            .lines()
            .any(|l| l.split(' ').nth(4) == Some(&*target.to_string_lossy())))
    }

    /// Grows the image and its filesystem, mounted and in use.
    async fn grow(&self, bytes: u64) -> anyhow::Result<()> {
        let image = self.image();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&image)?
            .set_len(bytes)?;
        let found = run("losetup", &["-j", path(&image)?]).await?;
        let device = found
            .split(':')
            .next()
            .map(str::trim)
            .filter(|d| d.starts_with("/dev/"))
            .context("its loop device is gone")?
            .to_string();
        run("losetup", &["-c", &device]).await?;
        run("resize2fs", &[&device]).await?;
        Ok(())
    }

    /// Unmounts it and deletes its filesystem: its data is gone.
    pub async fn remove(&self) -> anyhow::Result<()> {
        if !self.exists() {
            return Ok(());
        }
        if self.drop_legacy_unit().await? {
            run("systemctl", &["daemon-reload"]).await?;
        }
        if self.is_mounted()? {
            run("umount", &[path(&self.mountpoint())?]).await?;
        }
        if self.is_mounted()? {
            bail!("{} is still mounted", self.mountpoint().display());
        }
        std::fs::remove_dir_all(&self.dir)?;
        Ok(())
    }
}

/// Where mount units went before `facade-volumes.service`.
const LEGACY_UNIT_DIR: &str = "/etc/systemd/system";
const BOOT_UNIT: &str = "facade-volumes.service";

/// Every sized volume under `agent_dir`.
fn all(agent_dir: &Path) -> Vec<SizedVolume> {
    let Ok(entries) = std::fs::read_dir(agent_dir.join("volumes")) else {
        return vec![];
    };
    let mut out: Vec<SizedVolume> = entries
        .flatten()
        .map(|e| SizedVolume::new(agent_dir, &e.file_name().to_string_lossy()))
        .filter(SizedVolume::exists)
        .collect();
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out
}

/// `facaded mount-volumes`: mounts every sized volume under `agent_dir` that
/// is not. Run at boot by `facade-volumes.service`, before Docker. One that
/// cannot be mounted does not stop the others.
pub async fn mount_all(agent_dir: &Path) -> anyhow::Result<()> {
    let mut failed = vec![];
    for v in all(agent_dir) {
        if let Err(e) = v.mount().await {
            eprintln!("volume {}: {e:#}", v.name());
            failed.push(v.name());
        }
    }
    if !failed.is_empty() {
        bail!("could not mount {}", failed.join(", "));
    }
    Ok(())
}

/// Makes sure the sized volumes under `agent_dir` come back at boot: writes
/// and enables `facade-volumes.service` (unless the system's configuration
/// provides it: `managed`), mounts what is not, then takes out the mount units
/// of each volume from before it. Nothing is unmounted, and the old units go
/// only once the new one is in place. Nothing at all without sized volumes.
pub async fn boot_setup(agent_dir: &Path, managed: bool) -> anyhow::Result<()> {
    let volumes = all(agent_dir);
    if volumes.is_empty() || unsupported().is_some() {
        return Ok(());
    }
    if !managed {
        install_boot_unit(agent_dir).await?;
    }
    for v in &volumes {
        v.mount().await?;
    }
    let mut dropped = vec![];
    for v in &volumes {
        if v.drop_legacy_unit().await? {
            dropped.push(v.name());
        }
    }
    if !dropped.is_empty() {
        run("systemctl", &["daemon-reload"]).await?;
        tracing::info!(
            "volumes {} are mounted at boot by {BOOT_UNIT} now (their own mount units are gone)",
            dropped.join(", ")
        );
    }
    Ok(())
}

/// The unit mounting them at boot, for the facaded installed here.
fn boot_unit(exe: &Path, agent_dir: &Path) -> String {
    format!(
        "# Written by facade: mounts the volumes with a size before Docker starts\n\
         # the containers using them.\n\
         [Unit]\n\
         Description=facade volumes with a size\n\
         After=local-fs.target\n\
         Before=docker.service\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart={exe} mount-volumes {dir}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        exe = exe.display(),
        dir = agent_dir.display(),
    )
}

async fn install_boot_unit(agent_dir: &Path) -> anyhow::Result<()> {
    let exe = crate::selfbin::installed_path()?;
    let content = boot_unit(&exe, agent_dir);
    let file = Path::new(LEGACY_UNIT_DIR).join(BOOT_UNIT);
    if std::fs::read_to_string(&file).ok().as_deref() != Some(content.as_str()) {
        let partial = file.with_extension("partial");
        std::fs::write(&partial, &content)
            .with_context(|| format!("writing {}", file.display()))?;
        std::fs::rename(&partial, &file)?;
        run("systemctl", &["daemon-reload"]).await?;
    }
    run("systemctl", &["enable", BOOT_UNIT]).await?;
    Ok(())
}

/// What the image files of every sized volume under `agent_dir` take on the
/// node's disk, the volumes no app declares any more included.
pub fn on_disk_total(agent_dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(agent_dir.join("volumes")) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|e| SizedVolume::new(agent_dir, &e.file_name().to_string_lossy()).on_disk())
        .sum()
}

/// Space used in the filesystem mounted at `dir`, from `statvfs`.
fn fs_used(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stat` is a valid out pointer.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let blocks = (stat.f_blocks as u64).saturating_sub(stat.f_bfree as u64);
    Some(blocks * stat.f_frsize as u64)
}

/// Why this node cannot have volumes with a size, if it cannot.
pub fn unsupported() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("a Linux node".into());
    }
    if !Path::new("/run/systemd/system").exists() {
        return Some("systemd on the node".into());
    }
    let platform = crate::platform::current();
    for tool in ["mkfs.ext4", "resize2fs", "losetup", "systemd-escape"] {
        if let Err(missing) = platform.tool(tool) {
            return Some(missing.to_string());
        }
    }
    None
}

fn path(p: &Path) -> anyhow::Result<&str> {
    p.to_str()
        .context("the agent's data directory is not UTF-8")
}

/// Runs a command; its stdout, or its stderr as the error. A command taking
/// longer than two minutes is stopped rather than holding its app up.
async fn run(program: &str, args: &[&str]) -> anyhow::Result<String> {
    run_within(program, args, std::time::Duration::from_secs(120)).await
}

async fn run_within(
    program: &str,
    args: &[&str],
    within: std::time::Duration,
) -> anyhow::Result<String> {
    let output = Command::new(program)
        .args(args)
        .env("PATH", crate::platform::tool_path())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(within, output)
        .await
        .map_err(|_| anyhow::anyhow!("{program} {} did not finish", args.join(" ")))?
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Copies what `from` holds into `to`, which must not exist yet. Through a
/// temporary directory, so `to` only appears once complete.
pub async fn copy_into(from: &Path, to: &Path) -> anyhow::Result<()> {
    let partial = to.with_extension("partial");
    if partial.exists() {
        std::fs::remove_dir_all(&partial)?;
    }
    std::fs::create_dir_all(&partial)?;
    let source = format!("{}/.", path(from)?);
    // Moving a large volume takes as long as it takes.
    let copied = run_within(
        "cp",
        &["-a", &source, path(&partial)?],
        std::time::Duration::from_secs(24 * 3600),
    )
    .await;
    if let Err(e) = copied {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(e.context("copying the volume's data (does it fit in the size asked for?)"));
    }
    std::fs::rename(&partial, to)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_unit_runs_this_facaded_before_docker() {
        let unit = boot_unit(
            Path::new("/usr/local/bin/facaded"),
            Path::new("/var/lib/facade/agent"),
        );
        assert!(
            unit.contains("ExecStart=/usr/local/bin/facaded mount-volumes /var/lib/facade/agent\n")
        );
        assert!(unit.contains("Before=docker.service\n"));
        assert!(unit.contains("Type=oneshot\n"));
        assert!(unit.lines().all(|l| !l.starts_with(' ')), "{unit}");
    }

    #[test]
    fn measures_a_filesystem_without_walking_it() {
        let used = fs_used(&std::env::temp_dir()).expect("statvfs");
        assert!(used > 0);
        assert_eq!(fs_used(Path::new("/no/such/dir")), None);
    }
}
