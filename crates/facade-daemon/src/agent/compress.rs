//! Static sites are compressed once when they are deployed: every text-like file
//! gets `.br` and `.gz` siblings, which the entry point serves to browsers that
//! accept them (`ServeDir::precompressed_*`). Nothing is compressed per request.

use std::io::Write;
use std::path::Path;

/// Extensions worth compressing. Images, video, fonts in woff/woff2 and archives
/// are compressed already.
const COMPRESSIBLE: &[&str] = &[
    "html",
    "htm",
    "css",
    "js",
    "mjs",
    "cjs",
    "json",
    "map",
    "wasm",
    "svg",
    "txt",
    "xml",
    "ico",
    "ttf",
    "otf",
    "eot",
    "webmanifest",
    "md",
    "csv",
    "tsv",
    "wgsl",
    "glsl",
];
/// Below this, the headers cost more than compression saves.
const MIN_SIZE: u64 = 1024;
/// Brotli's best setting is slow (about 1 MB/s); above this size a faster one
/// keeps deploys short for a few percent more bytes.
const BEST_UP_TO: u64 = 4 << 20;

#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub files: usize,
    pub bytes: u64,
    pub brotli_bytes: u64,
}

/// Writes `.br` and `.gz` next to every compressible file under `root` that has
/// none yet, keeping only those smaller than the original.
pub fn precompress(root: &Path) -> std::io::Result<Summary> {
    let mut summary = Summary::default();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() {
                dirs.push(path);
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let compressible = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| COMPRESSIBLE.contains(&e.to_ascii_lowercase().as_str()));
            let size = entry.metadata()?.len();
            if !compressible || size < MIN_SIZE {
                continue;
            }
            let br = sibling(&path, "br");
            let gz = sibling(&path, "gz");
            if br.exists() || gz.exists() {
                continue;
            }
            let data = std::fs::read(&path)?;
            let quality = if size <= BEST_UP_TO { 11 } else { 9 };
            let compressed = brotli(&data, quality)?;
            let brotli_size = compressed.len() as u64;
            if brotli_size < size {
                std::fs::write(&br, compressed)?;
            }
            let compressed = gzip(&data)?;
            if (compressed.len() as u64) < size {
                std::fs::write(&gz, compressed)?;
            }
            summary.files += 1;
            summary.bytes += size;
            summary.brotli_bytes += brotli_size.min(size);
        }
    }
    Ok(summary)
}

fn sibling(path: &Path, ext: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(ext);
    name.into()
}

fn brotli(data: &[u8], quality: u32) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 3);
    {
        let mut writer = brotli::CompressorWriter::new(&mut out, 1 << 16, quality, 22);
        writer.write_all(data)?;
    }
    Ok(out)
}

fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(
        Vec::with_capacity(data.len() / 3),
        flate2::Compression::best(),
    );
    encoder.write_all(data)?;
    encoder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compresses_text_once_and_leaves_the_rest() {
        let dir = std::env::temp_dir().join(format!("facade-precompress-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        let js = "console.log('hello');\n".repeat(200);
        std::fs::write(dir.join("assets/app.js"), &js).unwrap();
        std::fs::write(dir.join("tiny.css"), "body{}").unwrap();
        std::fs::write(dir.join("photo.png"), vec![7u8; 4096]).unwrap();
        // The site ships its own: left alone.
        std::fs::write(dir.join("index.html"), "<p>hi</p>".repeat(300)).unwrap();
        std::fs::write(dir.join("index.html.gz"), b"theirs").unwrap();

        let summary = precompress(&dir).unwrap();
        assert_eq!(summary.files, 1);
        assert_eq!(summary.bytes, js.len() as u64);
        let br = std::fs::read(dir.join("assets/app.js.br")).unwrap();
        let mut back = Vec::new();
        std::io::copy(&mut brotli::Decompressor::new(&br[..], 4096), &mut back).unwrap();
        assert_eq!(back, js.as_bytes());
        let gz = std::fs::read(dir.join("assets/app.js.gz")).unwrap();
        let mut back = Vec::new();
        std::io::copy(&mut flate2::read::GzDecoder::new(&gz[..]), &mut back).unwrap();
        assert_eq!(back, js.as_bytes());
        assert!(!dir.join("tiny.css.br").exists());
        assert!(!dir.join("photo.png.br").exists());
        assert!(!dir.join("index.html.br").exists());
        assert_eq!(std::fs::read(dir.join("index.html.gz")).unwrap(), b"theirs");

        // A second run finds nothing left to do.
        assert_eq!(precompress(&dir).unwrap(), Summary::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
