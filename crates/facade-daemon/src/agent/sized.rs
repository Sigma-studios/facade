//! Volumes with a size: a filesystem of their own, in an image file on the node.
//!
//! Docker's volumes share the node's disk, and its local driver cannot mount an
//! image file. So the agent formats one (ext4, sparse: it takes the room its data
//! takes), mounts it through a systemd mount unit, which brings it back at boot
//! before Docker starts the containers, and makes the Docker volume a bind of a
//! directory inside it. That directory only exists on the mounted filesystem: a
//! container never starts writing to the node's disk instead.

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
                self.install_unit().await?;
            }
            Some(current) if bytes < current => bail!(
                "volume {} is {}: a volume cannot shrink",
                self.name(),
                facade_core::format_size_mb(current / (1024 * 1024))
            ),
            Some(_) => self.install_unit().await?,
        }
        if !self.is_mounted()? {
            run(
                "mount",
                &["-o", "loop", path(&image)?, path(&self.mountpoint())?],
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

    /// The mount unit, enabled: systemd mounts the volume at boot, before Docker.
    /// The agent mounts it itself otherwise: asking systemd to start a mount unit
    /// that is already active can wait forever on its loop device's unit.
    async fn install_unit(&self) -> anyhow::Result<()> {
        let unit = self.unit_name().await?;
        let content = format!(
            "# Written by facade for volume {name}.\n\
             [Unit]\n\
             Description=facade volume {name}\n\
             Before=docker.service\n\
             \n\
             [Mount]\n\
             What={what}\n\
             Where={where_}\n\
             Type=ext4\n\
             Options=loop,nofail\n\
             \n\
             [Install]\n\
             WantedBy=local-fs.target\n",
            name = self.name(),
            what = self.image().display(),
            where_ = self.mountpoint().display(),
        );
        let path = Path::new(UNIT_DIR).join(&unit);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(content.as_str()) {
            std::fs::write(&path, content)?;
            run("systemctl", &["daemon-reload"]).await?;
        }
        run("systemctl", &["enable", &unit]).await?;
        Ok(())
    }

    async fn unit_name(&self) -> anyhow::Result<String> {
        Ok(run(
            "systemd-escape",
            &["--path", "--suffix=mount", path(&self.mountpoint())?],
        )
        .await?
        .trim()
        .to_string())
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
        let unit = self.unit_name().await?;
        run("systemctl", &["disable", &unit]).await?;
        if self.is_mounted()? {
            run("umount", &[path(&self.mountpoint())?]).await?;
        }
        let path = Path::new(UNIT_DIR).join(&unit);
        if path.exists() {
            std::fs::remove_file(&path)?;
            run("systemctl", &["daemon-reload"]).await?;
        }
        if self.is_mounted()? {
            bail!("{} is still mounted", self.mountpoint().display());
        }
        std::fs::remove_dir_all(&self.dir)?;
        Ok(())
    }
}

const UNIT_DIR: &str = "/etc/systemd/system";

/// Why this node cannot have volumes with a size, if it cannot.
pub fn unsupported() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("a Linux node".into());
    }
    if !Path::new("/run/systemd/system").exists() {
        return Some("systemd on the node".into());
    }
    for tool in ["mkfs.ext4", "resize2fs", "losetup", "systemd-escape"] {
        let found = std::env::var_os("PATH")
            .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(tool).exists()))
            || Path::new("/sbin").join(tool).exists()
            || Path::new("/usr/sbin").join(tool).exists();
        if !found {
            return Some(format!("{tool} on the node (apt install e2fsprogs mount)"));
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
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
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
