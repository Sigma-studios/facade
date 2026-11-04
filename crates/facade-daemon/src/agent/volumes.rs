//! Volume contents as archives: gzipped tars that keep owners and permissions, so a
//! database owned by the app's user stays writable after a move or a restore.
//!
//! These work on the volume's directory on the host (Docker's mountpoint), which
//! the agent can read and write because it runs as root.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read};
use std::path::{Path, PathBuf};

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
    append_sorted(&mut tar, dir).with_context(|| format!("archiving {}", dir.display()))?;
    tar.into_inner()?.finish()?.into_inner()?.sync_all()?;
    Ok(())
}

/// Appends `dir` and everything under it, as `append_dir_all(".", dir)` does,
/// but each directory's entries sorted by name. The order a filesystem lists
/// a directory in changes when it is rewritten (by a restore): backups, which
/// store again only what moved in the tar, would then store everything again.
fn append_sorted<W: std::io::Write>(tar: &mut tar::Builder<W>, dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    // Depth first, each directory before its entries; reversed on the stack.
    let mut stack = vec![(dir.to_path_buf(), PathBuf::from("."), true)];
    while let Some((path, name, is_dir)) = stack.pop() {
        if is_dir {
            tar.append_dir(&name, &path)?;
            let mut entries = std::fs::read_dir(&path)?.collect::<Result<Vec<_>, _>>()?;
            entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
            for entry in entries {
                let kind = entry.file_type()?;
                // A socket is its program's to create again, and cannot be archived.
                if kind.is_socket() {
                    continue;
                }
                stack.push((entry.path(), name.join(entry.file_name()), kind.is_dir()));
            }
            continue;
        }
        let meta = std::fs::symlink_metadata(&path)?;
        let kind = meta.file_type();
        if kind.is_file() || kind.is_symlink() {
            tar.append_path_with_name(&path, &name)?;
        } else {
            // A FIFO or a device node: a header alone (`append_path_with_name`
            // would name it by its path on the host).
            use std::os::unix::fs::MetadataExt;
            let entry_type = if kind.is_fifo() {
                tar::EntryType::Fifo
            } else if kind.is_char_device() {
                tar::EntryType::Char
            } else if kind.is_block_device() {
                tar::EntryType::Block
            } else {
                continue;
            };
            let mut header = tar::Header::new_gnu();
            header.set_metadata_in_mode(&meta, tar::HeaderMode::Complete);
            header.set_entry_type(entry_type);
            header.set_size(0);
            let dev = meta.rdev();
            header.set_device_major((((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0xfff)) as u32)?;
            header.set_device_minor((((dev >> 12) & 0xffff_ff00) | (dev & 0xff)) as u32)?;
            tar.append_data(&mut header, &name, std::io::empty())?;
        }
    }
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

    #[test]
    fn archives_in_name_order() {
        let base = std::env::temp_dir().join(format!("facade-sorted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (src, dst, tar) = (base.join("src"), base.join("dst"), base.join("v.tar.gz"));
        for name in ["b", "a/z", "a/m", "c"] {
            std::fs::create_dir_all(src.join(name)).unwrap();
        }
        for name in ["zz", "a/m/x", "b/f", "aa"] {
            std::fs::write(src.join(name), name).unwrap();
        }
        std::os::unix::fs::symlink("aa", src.join("link")).unwrap();
        let fifo = std::ffi::CString::new(src.join("pipe").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let _socket = std::os::unix::net::UnixListener::bind(src.join("sock")).unwrap();

        archive(&src, &tar).unwrap();
        let mut archive = tar::Archive::new(open(&tar).unwrap());
        let entries: Vec<(String, tar::EntryType)> = archive
            .entries()
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let name = e.path().unwrap().to_string_lossy().to_string();
                (name, e.header().entry_type())
            })
            .collect();
        let names: Vec<&str> = entries
            .iter()
            .map(|(n, _)| n.trim_end_matches('/'))
            .collect();
        assert_eq!(
            names,
            [
                ".", "a", "a/m", "a/m/x", "a/z", "aa", "b", "b/f", "c", "link", "pipe", "zz"
            ]
        );
        assert_eq!(entries[10].1, tar::EntryType::Fifo);

        std::fs::create_dir_all(&dst).unwrap();
        restore(&tar, &dst).unwrap();
        assert_eq!(
            std::fs::read_link(dst.join("link")).unwrap(),
            Path::new("aa")
        );
        assert_eq!(std::fs::read(dst.join("a/m/x")).unwrap(), b"a/m/x");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
