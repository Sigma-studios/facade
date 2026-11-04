//! Where volume backups are kept: the tar of a volume cut into chunks, each chunk
//! stored once however many backups (of however many volumes) hold it, and one
//! manifest per backup listing its chunks in order.
//!
//! ```text
//! <backups>/_chunks/<2 hex>/<64 hex>          a chunk, zstd-compressed, named by
//!                                             the BLAKE3 hash of its contents
//! <backups>/<app>/<volume>/<time>.snapshot    the manifest of one backup
//! <backups>/<app>/<volume>/<time>.tar.gz      a full copy, from before chunks
//! ```
//!
//! Chunks are cut where the bytes say so (FastCDC), not at fixed offsets: what is
//! inserted or removed in the middle of the tar only moves the cuts around it, so
//! a day of small changes to a 2 GB volume adds the few chunks covering them.
//! The tar is chunked uncompressed (a gzipped one differs entirely after the first
//! change), and each chunk compressed on its own.
//!
//! Average chunks of 512 KiB (128 KiB to 2 MiB): every changed file, every page a
//! database rewrites, costs at least one new chunk, so smaller chunks store less
//! per change; but each is a file and a manifest line, and zstd compresses worse
//! below ~100 KiB. At 512 KiB a 2 GB volume is ~4000 chunks and a ~300 KB manifest.
//!
//! Crash safety: a chunk is written to a temporary file, synced and renamed, so a
//! file under a chunk's name is always whole. A manifest is written the same way,
//! after every chunk it lists (and their directories) is synced: a backup
//! interrupted at any point has no manifest, so it does not exist, and what it
//! left behind (unlisted chunks, temporary files) goes at the next
//! [`Store::collect_garbage`]. Backups running together are fine: they write the
//! same bytes under the same name. Garbage collection is not (it would delete
//! chunks a running backup counts on): callers keep it to itself (see
//! `backups::collect_garbage`).
//!
//! Chunks and manifests never change once written, so copying the store elsewhere
//! (another disk, object storage) is copying the files not there yet.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use facade_core::api::{BackupCheckReport, BackupStoreView};
use serde::{Deserialize, Serialize};

pub const CHUNKS_DIR: &str = "_chunks";
pub const SNAPSHOT_SUFFIX: &str = ".snapshot";
pub const FULL_SUFFIX: &str = ".tar.gz";

const MIN_CHUNK: usize = 128 * 1024;
const AVG_CHUNK: usize = 512 * 1024;
const MAX_CHUNK: usize = 2 * 1024 * 1024;
/// zstd's default: about gzip's ratio at several times its speed.
const ZSTD_LEVEL: i32 = 3;
const FORMAT: u32 = 1;
const TEMP_PREFIX: &str = ".tmp-";

pub type ChunkId = [u8; 32];

/// First line of a manifest; the chunks follow, one per line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub format: u32,
    pub app: String,
    pub volume: String,
    /// Unix time it was taken; also its id.
    pub id: u64,
    /// Bytes of the tar its chunks make up.
    pub tar_size: u64,
    /// What its chunks take in the store, shared ones included: about what a
    /// full copy of it would take.
    pub stored: u64,
    /// What it added to the store: the chunks no other backup had.
    pub added: u64,
    pub chunks: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRef {
    pub id: ChunkId,
    /// Its size, uncompressed.
    pub len: u32,
    /// What its file takes (compressed).
    pub stored: u32,
}

impl ChunkRef {
    fn line(&self) -> String {
        format!("{} {} {}", hex::encode(self.id), self.len, self.stored)
    }

    fn parse(line: &str) -> Option<Self> {
        let mut parts = line.split(' ');
        let id = parse_id(parts.next()?)?;
        let len = parts.next()?.parse().ok()?;
        let stored = parts.next()?.parse().ok()?;
        parts.next().is_none().then_some(Self { id, len, stored })
    }
}

fn parse_id(hex_id: &str) -> Option<ChunkId> {
    let mut id = [0u8; 32];
    hex::decode_to_slice(hex_id, &mut id).ok()?;
    Some(id)
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// The store in `root` (the backups directory).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn chunks_dir(&self) -> PathBuf {
        self.root.join(CHUNKS_DIR)
    }

    fn chunk_path(&self, id: &ChunkId) -> PathBuf {
        let name = hex::encode(id);
        self.chunks_dir().join(&name[..2]).join(name)
    }

    pub fn snapshot_path(&self, app: &str, volume: &str, id: u64) -> PathBuf {
        self.root
            .join(app)
            .join(volume)
            .join(format!("{id}{SNAPSHOT_SUFFIX}"))
    }

    /// Stores `data` unless a chunk with its contents is there already. Directories
    /// a new chunk went into are added to `to_sync`.
    fn put_chunk(
        &self,
        data: &[u8],
        to_sync: &mut BTreeSet<PathBuf>,
    ) -> io::Result<(ChunkRef, bool)> {
        let id: ChunkId = blake3::hash(data).into();
        let path = self.chunk_path(&id);
        let len = data.len() as u32;
        if let Ok(meta) = std::fs::metadata(&path) {
            let stored = meta.len() as u32;
            return Ok((ChunkRef { id, len, stored }, false));
        }
        let compressed = zstd::bulk::compress(data, ZSTD_LEVEL)?;
        let dir = path.parent().expect("chunks are in a directory");
        if !dir.is_dir() {
            create_private_dirs(dir)?;
            to_sync.insert(self.chunks_dir());
            to_sync.insert(self.root.clone());
        }
        let temp = dir.join(format!("{TEMP_PREFIX}{}", uuid::Uuid::new_v4()));
        let written = (|| {
            let mut file = private_file(&temp)?;
            file.write_all(&compressed)?;
            file.sync_all()?;
            std::fs::rename(&temp, &path)
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&temp);
            return Err(e);
        }
        to_sync.insert(dir.to_path_buf());
        let stored = compressed.len() as u32;
        Ok((ChunkRef { id, len, stored }, true))
    }

    /// The contents of a chunk, checked against its hash.
    pub fn read_chunk(&self, chunk: &ChunkRef) -> anyhow::Result<Vec<u8>> {
        let name = hex::encode(chunk.id);
        let compressed = match std::fs::read(self.chunk_path(&chunk.id)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => bail!("chunk {name} is missing"),
            Err(e) => return Err(e).with_context(|| format!("reading chunk {name}")),
        };
        let data = zstd::bulk::decompress(&compressed, chunk.len as usize)
            .map_err(|e| anyhow!("chunk {name} is damaged ({e})"))?;
        if data.len() != chunk.len as usize || <ChunkId>::from(blake3::hash(&data)) != chunk.id {
            bail!("chunk {name} is damaged (its contents do not match its hash)");
        }
        Ok(data)
    }

    /// Cuts the tar read from `tar` into chunks, stores the new ones, then writes
    /// the manifest of backup `id` of `app`/`volume`.
    pub fn write_snapshot(
        &self,
        app: &str,
        volume: &str,
        id: u64,
        tar: impl Read,
    ) -> anyhow::Result<Header> {
        let mut header = Header {
            format: FORMAT,
            app: app.into(),
            volume: volume.into(),
            id,
            tar_size: 0,
            stored: 0,
            added: 0,
            chunks: 0,
        };
        let mut chunks = Vec::new();
        let mut to_sync = BTreeSet::new();
        for chunk in fastcdc::v2020::StreamCDC::new(tar, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK) {
            let chunk = chunk.map_err(|e| match e {
                fastcdc::v2020::Error::IoError(e) => anyhow!(e).context("reading the archive"),
                e => anyhow!("chunking the archive: {e}"),
            })?;
            let (chunk, new) = self.put_chunk(&chunk.data, &mut to_sync)?;
            header.tar_size += chunk.len as u64;
            header.stored += chunk.stored as u64;
            if new {
                header.added += chunk.stored as u64;
            }
            chunks.push(chunk);
        }
        if chunks.is_empty() {
            bail!("the archive is empty");
        }
        header.chunks = chunks.len();
        // The chunks' names must be on disk before a manifest lists them.
        for dir in &to_sync {
            sync_dir(dir)?;
        }
        let path = self.snapshot_path(app, volume, id);
        let dir = path.parent().expect("manifests are in a directory");
        create_private_dirs(dir)?;
        let temp = dir.join(format!("{TEMP_PREFIX}{}", uuid::Uuid::new_v4()));
        let written = (|| {
            let mut out = BufWriter::new(private_file(&temp)?);
            serde_json::to_writer(&mut out, &header)?;
            out.write_all(b"\n")?;
            for chunk in &chunks {
                writeln!(out, "{}", chunk.line())?;
            }
            out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            std::fs::rename(&temp, &path)?;
            sync_dir(dir)
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&temp);
            return Err(anyhow!(e).context("writing the manifest"));
        }
        Ok(header)
    }

    /// A reader of the tar of a backup, from its chunks.
    pub fn reader(&self, chunks: Vec<ChunkRef>) -> SnapshotReader {
        SnapshotReader {
            store: self.clone(),
            chunks: chunks.into_iter(),
            current: Vec::new(),
            pos: 0,
        }
    }

    /// Fails naming the first chunk of `chunks` that is not there.
    pub fn check_present(&self, chunks: &[ChunkRef]) -> anyhow::Result<()> {
        for chunk in chunks {
            if !self.chunk_path(&chunk.id).is_file() {
                bail!("chunk {} is missing", hex::encode(chunk.id));
            }
        }
        Ok(())
    }

    /// Every manifest, as `(app, volume, path)`.
    fn snapshots(&self) -> Vec<(String, String, PathBuf)> {
        let mut out = Vec::new();
        for (app, volume, file) in volume_files(&self.root) {
            if file.to_string_lossy().ends_with(SNAPSHOT_SUFFIX) {
                out.push((app, volume, file));
            }
        }
        out
    }

    /// Every chunk file, with what it takes.
    fn chunk_files(&self) -> Vec<(PathBuf, u64)> {
        let mut out = Vec::new();
        let Ok(shards) = std::fs::read_dir(self.chunks_dir()) else {
            return out;
        };
        for shard in shards.flatten() {
            let Ok(files) = std::fs::read_dir(shard.path()) else {
                continue;
            };
            for file in files.flatten() {
                let size = file.metadata().map(|m| m.len()).unwrap_or(0);
                out.push((file.path(), size));
            }
        }
        out
    }

    /// Deletes the chunks no manifest lists, and what interrupted backups left.
    /// Nothing may be writing to the store meanwhile. A manifest that cannot be
    /// read stops it: the chunks it lists would look unused.
    pub fn collect_garbage(&self) -> anyhow::Result<Collected> {
        let mut used = HashSet::new();
        for (_, _, path) in self.snapshots() {
            let (_, chunks) = read_manifest(&path).with_context(|| {
                format!(
                    "{} cannot be read, so unused chunks are kept (delete it if it is damaged)",
                    path.display()
                )
            })?;
            used.extend(chunks.into_iter().map(|c| c.id));
        }
        let mut collected = Collected::default();
        for (path, size) in self.chunk_files() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let unused = if name.starts_with(TEMP_PREFIX) {
                true
            } else if let Some(id) = parse_id(&name) {
                !used.contains(&id)
            } else {
                false
            };
            if unused && std::fs::remove_file(&path).is_ok() {
                collected.files += 1;
                collected.bytes += size;
            }
        }
        // Manifests being written, and full copies being received (before chunks).
        for (_, _, path) in volume_files(&self.root) {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if (name.starts_with(TEMP_PREFIX) || name.ends_with(".partial"))
                && std::fs::remove_file(&path).is_ok()
            {
                collected.files += 1;
            }
        }
        Ok(collected)
    }

    /// What the store holds and takes.
    pub fn view(&self) -> BackupStoreView {
        let mut view = BackupStoreView::default();
        for (path, size) in self.chunk_files() {
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(parse_id)
                .is_some()
            {
                view.chunks += 1;
                view.chunk_bytes += size;
            }
        }
        for (_, _, path) in volume_files(&self.root) {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.ends_with(SNAPSHOT_SUFFIX) {
                if let Ok(header) = read_header(&path) {
                    view.snapshots += 1;
                    view.snapshot_bytes += header.stored;
                }
            } else if name.ends_with(FULL_SUFFIX) && !name.starts_with('.') {
                view.full_copies += 1;
                view.full_copy_bytes += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            }
        }
        view
    }

    /// Reads every chunk the manifests list and checks it against its hash.
    /// Damaged chunks are deleted, so that the next backup holding the same data
    /// stores it again rather than pointing at the damaged copy.
    pub fn check(&self) -> BackupCheckReport {
        let mut problems = Vec::new();
        // Each chunk once, with the backups holding it.
        let mut holders: BTreeMap<ChunkId, (ChunkRef, Vec<String>)> = BTreeMap::new();
        for (app, volume, path) in self.snapshots() {
            match read_manifest(&path) {
                Ok((header, chunks)) => {
                    for chunk in chunks {
                        let entry = holders.entry(chunk.id).or_insert((chunk, Vec::new()));
                        let name = format!("{app}/{volume} {}", header.id);
                        if entry.1.last() != Some(&name) {
                            entry.1.push(name);
                        }
                    }
                }
                Err(e) => problems.push(format!("{}: {e:#}", path.display())),
            }
        }
        let mut verified = 0;
        for (chunk, held_by) in holders.values() {
            match self.read_chunk(chunk) {
                Ok(_) => verified += 1,
                Err(e) => {
                    let mut problem = format!("{e:#}, in {}", held_by.join(", "));
                    if self.chunk_path(&chunk.id).exists()
                        && std::fs::remove_file(self.chunk_path(&chunk.id)).is_ok()
                    {
                        problem += " (deleted: the next backup stores it again if the data is still there)";
                    }
                    problems.push(problem);
                }
            }
        }
        BackupCheckReport {
            store: self.view(),
            verified,
            problems,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Collected {
    pub files: u64,
    pub bytes: u64,
}

/// The manifest's first line.
pub fn read_header(path: &Path) -> anyhow::Result<Header> {
    let mut line = String::new();
    BufReader::new(File::open(path)?).read_line(&mut line)?;
    let header: Header = serde_json::from_str(&line).context("bad manifest header")?;
    if header.format != FORMAT {
        bail!("manifest format {} is not supported", header.format);
    }
    Ok(header)
}

pub fn read_manifest(path: &Path) -> anyhow::Result<(Header, Vec<ChunkRef>)> {
    let mut lines = BufReader::new(File::open(path)?).lines();
    let first = lines.next().context("empty manifest")??;
    let header: Header = serde_json::from_str(&first).context("bad manifest header")?;
    if header.format != FORMAT {
        bail!("manifest format {} is not supported", header.format);
    }
    let mut chunks = Vec::with_capacity(header.chunks);
    for line in lines {
        let line = line?;
        chunks.push(ChunkRef::parse(&line).with_context(|| format!("bad manifest line {line:?}"))?);
    }
    if chunks.len() != header.chunks
        || chunks.iter().map(|c| c.len as u64).sum::<u64>() != header.tar_size
    {
        bail!("the manifest is incomplete");
    }
    Ok((header, chunks))
}

/// What each app's backups take: its full copies, and the chunks its manifests
/// list, each counted once, a chunk listed by several apps split between them.
pub fn usage_by_app(root: &Path) -> BTreeMap<String, u64> {
    let mut by_app: BTreeMap<String, u64> = BTreeMap::new();
    let mut chunks: HashMap<ChunkId, (u32, BTreeSet<String>)> = HashMap::new();
    for (app, _, path) in volume_files(root) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if name.ends_with(FULL_SUFFIX) {
            *by_app.entry(app).or_default() +=
                std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        } else if name.ends_with(SNAPSHOT_SUFFIX)
            && let Ok((_, list)) = read_manifest(&path)
        {
            for chunk in list {
                chunks
                    .entry(chunk.id)
                    .or_insert_with(|| (chunk.stored, BTreeSet::new()))
                    .1
                    .insert(app.clone());
            }
        }
    }
    for (stored, apps) in chunks.into_values() {
        let share = stored as u64 / apps.len() as u64;
        for app in apps {
            *by_app.entry(app).or_default() += share;
        }
    }
    by_app.retain(|_, size| *size > 0);
    by_app
}

/// Files in the volume directories, as `(app, volume, path)`. Names starting
/// with `_` (the chunks, the server's copies) cannot be apps'.
fn volume_files(root: &Path) -> Vec<(String, String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(apps) = std::fs::read_dir(root) else {
        return out;
    };
    for app in apps.flatten() {
        let name = app.file_name().to_string_lossy().to_string();
        if name.starts_with(['_', '.']) {
            continue;
        }
        let Ok(volumes) = std::fs::read_dir(app.path()) else {
            continue;
        };
        for volume in volumes.flatten() {
            let volume_name = volume.file_name().to_string_lossy().to_string();
            let Ok(files) = std::fs::read_dir(volume.path()) else {
                continue;
            };
            for file in files.flatten() {
                out.push((name.clone(), volume_name.clone(), file.path()));
            }
        }
    }
    out
}

/// The tar of a backup, read chunk by chunk (one in memory at a time).
pub struct SnapshotReader {
    store: Store,
    chunks: std::vec::IntoIter<ChunkRef>,
    current: Vec<u8>,
    pos: usize,
}

impl Read for SnapshotReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.current.len() {
            let Some(next) = self.chunks.next() else {
                return Ok(0);
            };
            self.current = self
                .store
                .read_chunk(&next)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:#}")))?;
            self.pos = 0;
        }
        let n = buf.len().min(self.current.len() - self.pos);
        buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn private_file(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

fn create_private_dirs(dir: &Path) -> io::Result<()> {
    let mut path = PathBuf::new();
    for part in dir.components() {
        path.push(part);
        if !path.exists() {
            crate::tls::create_private_dir(&path)?;
        }
    }
    Ok(())
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> (PathBuf, Store) {
        let dir = std::env::temp_dir().join(format!("facade-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), Store::new(dir))
    }

    /// Bytes that do not compress or repeat, like the files of a real volume.
    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 32);
        let mut counter = 0u64;
        while out.len() < len {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&seed.to_le_bytes());
            hasher.update(&counter.to_le_bytes());
            out.extend_from_slice(hasher.finalize().as_bytes());
            counter += 1;
        }
        out.truncate(len);
        out
    }

    fn restore(store: &Store, path: &Path) -> Vec<u8> {
        let (_, chunks) = read_manifest(path).unwrap();
        let mut out = Vec::new();
        store.reader(chunks).read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn stores_only_what_changed() {
        let (dir, store) = temp_store("dedup");
        let mut data = noise(20 << 20, 1);
        let first = store
            .write_snapshot("blog", "data", 100, &data[..])
            .unwrap();
        assert_eq!(first.tar_size, data.len() as u64);
        assert_eq!(
            first.added, first.stored,
            "the first backup stores everything"
        );
        assert!(
            first.chunks >= 10 && first.chunks <= 160,
            "{} chunks",
            first.chunks
        );

        // A few bytes changed and 100 KB inserted in the middle.
        data[5 << 20] ^= 0xff;
        let tail = data.split_off(12 << 20);
        data.extend_from_slice(&noise(100_000, 2));
        data.extend_from_slice(&tail);
        let second = store
            .write_snapshot("blog", "data", 200, &data[..])
            .unwrap();
        assert!(
            second.added < 6 * MAX_CHUNK as u64,
            "added {} of {}",
            second.added,
            second.stored
        );
        let same = store
            .write_snapshot("blog", "data", 300, &data[..])
            .unwrap();
        assert_eq!(same.added, 0);

        let path = store.snapshot_path("blog", "data", 200);
        assert_eq!(restore(&store, &path), data);
        assert_eq!(read_header(&path).unwrap(), second);
        let view = store.view();
        assert_eq!(view.snapshots, 3);
        assert_eq!(view.chunk_bytes, first.added + second.added);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn collects_unused_chunks_only() {
        let (dir, store) = temp_store("gc");
        let a = noise(3 << 20, 3);
        let mut b = a.clone();
        b.extend_from_slice(&noise(3 << 20, 4));
        store.write_snapshot("blog", "data", 100, &a[..]).unwrap();
        store.write_snapshot("blog", "data", 200, &b[..]).unwrap();
        // What a killed backup leaves: a temporary file, a chunk without manifest.
        let shard = store.chunks_dir().join("00");
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(shard.join(".tmp-x"), b"partial").unwrap();
        let orphan = blake3::hash(b"orphan");
        std::fs::write(shard.join(orphan.to_hex().as_str()), b"x").unwrap();
        std::fs::write(dir.join("blog/data/.tmp-y"), b"partial manifest").unwrap();
        assert_eq!(store.collect_garbage().unwrap().files, 3);
        assert_eq!(
            restore(&store, &store.snapshot_path("blog", "data", 100)),
            a
        );

        let before = store.view().chunks;
        std::fs::remove_file(store.snapshot_path("blog", "data", 200)).unwrap();
        let collected = store.collect_garbage().unwrap();
        assert!(collected.files > 0 && collected.files < before);
        assert_eq!(store.view().chunks, before - collected.files);
        assert_eq!(
            restore(&store, &store.snapshot_path("blog", "data", 100)),
            a
        );
        assert!(store.check().problems.is_empty());

        // An unreadable manifest stops collection rather than losing its chunks.
        std::fs::write(dir.join("blog/data/300.snapshot"), b"{\"format\":1").unwrap();
        std::fs::remove_file(store.snapshot_path("blog", "data", 100)).unwrap();
        assert!(store.collect_garbage().is_err());
        assert_eq!(store.view().chunks, before - collected.files);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn detects_damage() {
        let (dir, store) = temp_store("check");
        let data = noise(2 << 20, 5);
        store
            .write_snapshot("blog", "data", 100, &data[..])
            .unwrap();
        let path = store.snapshot_path("blog", "data", 100);
        let (_, chunks) = read_manifest(&path).unwrap();
        assert_eq!(store.check().verified, chunks.len() as u64);

        // A chunk replaced by another valid zstd frame of the same length.
        let victim = store.chunk_path(&chunks[0].id);
        let other = zstd::bulk::compress(&noise(chunks[0].len as usize, 6), ZSTD_LEVEL).unwrap();
        std::fs::write(&victim, other).unwrap();
        let mut out = Vec::new();
        let err = store
            .reader(chunks.clone())
            .read_to_end(&mut out)
            .unwrap_err();
        assert!(err.to_string().contains("damaged"), "{err}");
        let report = store.check();
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(!victim.exists(), "a damaged chunk is deleted");
        assert!(store.check_present(&chunks).is_err());
        // The next backup of the same data stores it again.
        let header = store
            .write_snapshot("blog", "data", 200, &data[..])
            .unwrap();
        assert_eq!(header.added, chunks[0].stored as u64);
        assert_eq!(restore(&store, &path), data);

        // A truncated manifest is not a backup.
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, &text[..text.len() - 20]).unwrap();
        assert!(read_manifest(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn splits_shared_chunks_between_apps() {
        let (dir, store) = temp_store("usage");
        // Longer than the largest chunk, so that its first chunk is b's too.
        let shared = noise(4 << 20, 7);
        let mut own = shared.clone();
        own.extend_from_slice(&noise(4 << 20, 8));
        let a = store.write_snapshot("a", "data", 100, &shared[..]).unwrap();
        store.write_snapshot("a", "data", 200, &shared[..]).unwrap();
        let b = store.write_snapshot("b", "data", 100, &own[..]).unwrap();
        std::fs::write(dir.join("b/data/50.tar.gz"), vec![0u8; 1000]).unwrap();
        let usage = usage_by_app(&dir);
        let total: u64 = usage.values().sum();
        assert!(usage["a"] < a.stored, "a shares its chunks with b");
        assert!(usage["b"] > b.stored / 2);
        // Integer division may lose a byte per shared chunk.
        let on_disk = store.view().chunk_bytes + 1000;
        assert!(total <= on_disk && total + a.chunks as u64 >= on_disk);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_empty_and_truncated_archives() {
        let (dir, store) = temp_store("empty");
        assert!(store.write_snapshot("blog", "data", 100, &[][..]).is_err());
        // A gzip cut short fails while reading, before any manifest is written.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&noise(1 << 20, 9)).unwrap();
        let gz = gz.finish().unwrap();
        let cut = flate2::bufread::GzDecoder::new(&gz[..gz.len() / 2]);
        assert!(store.write_snapshot("blog", "data", 200, cut).is_err());
        assert!(!store.snapshot_path("blog", "data", 200).exists());
        assert!(store.view().snapshots == 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
