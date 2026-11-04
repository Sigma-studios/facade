//! Bytes to and from clients, counted on the socket: what crosses the wire,
//! TLS and headers included, as a hosting provider's traffic quota counts it.
//!
//! A client connection counts for the app it is open for (the one its first
//! request went to, or a TLS port's). Counting happens in the connection's
//! own fields, and reaches the app's shared counters every few kilobytes and
//! when the connection closes: an addition and a comparison per read or write.

use std::io;
use std::pin::Pin;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::task::{Context, Poll};

use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::traffic::AppTraffic;

/// Bytes kept on the connection before they are added to its app's counters.
const FLUSH_EVERY: u64 = 16 * 1024;

/// What a client connection is to the entry point: the app its bytes count
/// for once known, and whether it is facade's own outside check.
#[derive(Default)]
pub struct Tally {
    pub app: OnceLock<Arc<AppTraffic>>,
    pub checker: AtomicBool,
}

impl Tally {
    /// A connection known to be for `app` from the start (a TLS port).
    pub fn for_app(app: Option<Arc<AppTraffic>>) -> Arc<Self> {
        let tally = Arc::new(Self::default());
        if let Some(app) = app {
            let _ = tally.app.set(app);
        }
        tally
    }
}

/// A client socket counting what goes through it.
pub struct Metered<S> {
    inner: S,
    tally: Arc<Tally>,
    received: u64,
    sent: u64,
}

impl<S> Metered<S> {
    pub fn new(inner: S, tally: Arc<Tally>) -> Self {
        Self {
            inner,
            tally,
            received: 0,
            sent: 0,
        }
    }

    fn count(&mut self, received: u64, sent: u64) {
        self.received += received;
        self.sent += sent;
        if self.received + self.sent >= FLUSH_EVERY {
            self.flush_counts();
        }
    }

    /// Hands what was counted to the app, once there is one; until then (the
    /// TLS handshake, the first request's headers) it waits here.
    fn flush_counts(&mut self) {
        if let Some(app) = self.tally.app.get() {
            app.add_bytes(self.received, self.sent);
            self.received = 0;
            self.sent = 0;
        }
    }
}

impl<S> Drop for Metered<S> {
    fn drop(&mut self) {
        self.flush_counts();
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Metered<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = result {
            let read = (buf.filled().len() - before) as u64;
            self.count(read, 0);
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Metered<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = result {
            self.count(0, n as u64);
        }
        result
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(n)) = result {
            self.count(0, n as u64);
        }
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn counts_for_the_app_once_known() {
        let (a, mut b) = tokio::io::duplex(64 * 1024);
        let tally = Arc::new(Tally::default());
        let mut metered = Metered::new(a, tally.clone());
        // Before the app is known (a handshake): kept on the connection.
        b.write_all(&[0; 100]).await.unwrap();
        let mut buf = [0u8; 100];
        metered.read_exact(&mut buf).await.unwrap();
        let app = AppTraffic::new();
        let _ = tally.app.set(app.clone());
        metered.write_all(&[1; 300]).await.unwrap();
        assert_eq!(app.snapshot().bytes_out, 0, "small amounts wait");
        metered.write_all(&vec![2; 20_000]).await.unwrap();
        let t = app.snapshot();
        assert_eq!((t.bytes_in, t.bytes_out), (100, 20_300));
        metered.write_all(&[3; 7]).await.unwrap();
        drop(metered);
        assert_eq!(app.snapshot().bytes_out, 20_307, "the rest when it closes");
    }
}
