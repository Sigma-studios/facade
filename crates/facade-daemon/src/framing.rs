//! Newline-delimited JSON over byte streams.

use anyhow::{Context, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_LINE: usize = 16 * 1024 * 1024;

/// Reads one line without buffering past it, so the stream can carry raw bytes
/// afterwards (data connections). Only used for the short hello line; TLS streams
/// buffer internally so byte-wise reads are cheap.
pub async fn read_line_unbuffered<R: AsyncRead + Unpin>(r: &mut R) -> anyhow::Result<String> {
    let mut buf = Vec::with_capacity(256);
    loop {
        let byte = r.read_u8().await.context("connection closed")?;
        if byte == b'\n' {
            break;
        }
        buf.push(byte);
        if buf.len() > 64 * 1024 {
            bail!("hello line too long");
        }
    }
    Ok(String::from_utf8(buf)?)
}

pub async fn write_json<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    value: &T,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

/// Buffered reader of JSON lines, for control connections.
pub struct JsonLines<R> {
    inner: tokio::io::BufReader<R>,
    line: String,
}

impl<R: AsyncRead + Unpin> JsonLines<R> {
    pub fn new(r: R) -> Self {
        Self {
            inner: tokio::io::BufReader::new(r),
            line: String::new(),
        }
    }

    /// `Ok(None)` on a clean end of stream.
    pub async fn next<T: DeserializeOwned>(&mut self) -> anyhow::Result<Option<T>> {
        use tokio::io::AsyncBufReadExt;
        self.line.clear();
        let n = (&mut self.inner)
            .take(MAX_LINE as u64)
            .read_line(&mut self.line)
            .await?;
        if n == 0 {
            return Ok(None);
        }
        if !self.line.ends_with('\n') && n >= MAX_LINE {
            bail!("message too large");
        }
        Ok(Some(serde_json::from_str(self.line.trim_end())?))
    }
}
