//! Volume contents as archives: gzipped tars that keep owners and permissions, so a
//! database owned by the app's user stays writable after a move or a restore.
//!
//! These work on the volume's directory on the host (Docker's mountpoint), which
//! the agent can read and write because it runs as root.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read};
use std::path::Path;

use anyhow::Context;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

/// Writes a gzipped tar of everything in `dir` to `dest`.
pub fn archive(dir: &Path, dest: &Path) -> anyhow::Result<()> {
    let file = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let gz = GzEncoder::new(BufWriter::new(file), Compression::default());
    let mut tar = tar::Builder::new(gz);
    tar.follow_symlinks(false);
    tar.mode(tar::HeaderMode::Complete);
    tar.append_dir_all(".", dir)
        .with_context(|| format!("archiving {}", dir.display()))?;
    tar.into_inner()?.finish()?.into_inner()?.sync_all()?;
    Ok(())
}

/// Replaces the contents of `dir` with the tar (gzipped or not) in `src`.
pub fn restore(src: &Path, dir: &Path) -> anyhow::Result<()> {
    // Check the archive reads to the end before deleting anything.
    if entries(src).context("the archive is damaged")? == 0 {
        anyhow::bail!("the archive is empty");
    }
    clear(dir)?;
    let mut archive = tar::Archive::new(open(src)?);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    archive.set_unpack_xattrs(false);
    archive
        .unpack(dir)
        .with_context(|| format!("extracting into {}", dir.display()))?;
    Ok(())
}

/// Number of entries in the archive.
pub fn entries(src: &Path) -> anyhow::Result<usize> {
    let mut archive = tar::Archive::new(open(src)?);
    let mut n = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        std::io::copy(&mut entry, &mut std::io::sink())?;
        n += 1;
    }
    Ok(n)
}

/// The archive, decompressed if it starts with the gzip magic number.
fn open(src: &Path) -> anyhow::Result<Box<dyn Read>> {
    let mut file = BufReader::new(File::open(src)?);
    let mut magic = [0u8; 2];
    let gzipped = file.read_exact(&mut magic).is_ok() && magic == [0x1f, 0x8b];
    let file = BufReader::new(File::open(src)?);
    Ok(if gzipped {
        Box::new(GzDecoder::new(file))
    } else {
        Box::new(file)
    })
}

fn clear(dir: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() && !path.is_symlink() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Bytes used by the files under `dir` (symlinks not followed).
pub fn size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archives_and_restores() {
        let base = std::env::temp_dir().join(format!("facade-volumes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (src, dst, tar) = (base.join("src"), base.join("dst"), base.join("v.tar.gz"));
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("app.db"), b"data").unwrap();
        std::fs::write(src.join("sub/notes.txt"), b"hello").unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("stale"), b"old").unwrap();

        archive(&src, &tar).unwrap();
        assert_eq!(size(&src), 9);
        restore(&tar, &dst).unwrap();
        assert_eq!(std::fs::read(dst.join("app.db")).unwrap(), b"data");
        assert_eq!(std::fs::read(dst.join("sub/notes.txt")).unwrap(), b"hello");
        assert!(!dst.join("stale").exists(), "restore replaces the contents");

        std::fs::write(&tar, b"not an archive at all").unwrap();
        assert!(restore(&tar, &dst).is_err());
        assert!(dst.join("app.db").exists(), "a bad archive deletes nothing");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
