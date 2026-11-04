//! `facade exec`: a command in an app's container, over one upgraded HTTP
//! connection (see `facade_core::exec` for what goes over it).

use std::io::{IsTerminal, Write};

use anyhow::{Context, bail};
use facade_core::exec::{ExecRequest, Frame, UPGRADE};
use reqwest::{Method, StatusCode, header};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::client::{self, Api};

/// Runs the command and returns its exit code.
pub async fn run(
    api: &Api,
    app: &str,
    command: Vec<String>,
    interactive: bool,
    tty: bool,
) -> anyhow::Result<i32> {
    let request = ExecRequest {
        command,
        tty,
        stdin: interactive,
        size: if tty { terminal_size() } else { None },
        term: if tty {
            std::env::var("TERM").ok()
        } else {
            None
        },
    };
    let resp = api
        .request(Method::GET, &format!("/api/apps/{app}/exec"))
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, UPGRADE)
        .send()
        .await
        .context("request failed")?;
    if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        client::check(resp).await?;
        bail!(
            "the server does not run commands in containers yet: update it (facade server update)"
        );
    }
    let upgraded = resp.upgrade().await.context("switching to facade-exec")?;
    let (mut from_server, mut to_server) = tokio::io::split(upgraded);
    let mut line = serde_json::to_vec(&request)?;
    line.push(b'\n');
    to_server.write_all(&line).await?;
    to_server.flush().await?;

    // One writer for the input, the end of it and terminal sizes.
    let (frames, mut outgoing) = mpsc::channel::<Frame>(64);
    tokio::spawn(async move {
        while let Some(frame) = outgoing.recv().await {
            if to_server.write_all(&frame.encode()).await.is_err()
                || to_server.flush().await.is_err()
            {
                break;
            }
        }
        // Kept open until the process exits: closing it means "stop the command".
        std::future::pending::<()>().await;
    });
    if interactive {
        tokio::spawn(pump_stdin(frames.clone()));
    }
    #[cfg(unix)]
    if tty {
        tokio::spawn(watch_size(frames.clone()));
    }
    let _raw = (tty && interactive && std::io::stdin().is_terminal())
        .then(RawMode::enter)
        .flatten();

    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        while let Some(frame) = Frame::decode(&mut buf).map_err(|e| anyhow::anyhow!(e))? {
            match frame {
                Frame::Stdout(bytes) => {
                    let mut out = std::io::stdout().lock();
                    out.write_all(&bytes)?;
                    out.flush()?;
                }
                Frame::Stderr(bytes) => {
                    let mut err = std::io::stderr().lock();
                    err.write_all(&bytes)?;
                    err.flush()?;
                }
                Frame::Exit(code) => return Ok(i32::try_from(code).unwrap_or(1)),
                Frame::Error(message) => bail!("{message}"),
                _ => {}
            }
        }
        let n = from_server.read(&mut chunk).await?;
        if n == 0 {
            bail!("the connection closed before the command finished");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// What is typed or piped in, then its end.
async fn pump_stdin(frames: mpsc::Sender<Frame>) {
    let mut stdin = tokio::io::stdin();
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        match stdin.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if frames
                    .send(Frame::Stdin(chunk[..n].to_vec()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
    let _ = frames.send(Frame::StdinEnd).await;
}

#[cfg(unix)]
async fn watch_size(frames: mpsc::Sender<Frame>) {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut changes) = signal(SignalKind::window_change()) else {
        return;
    };
    while changes.recv().await.is_some() {
        if let Some((cols, rows)) = terminal_size()
            && frames.send(Frame::Resize(cols, rows)).await.is_err()
        {
            return;
        }
    }
}

/// Columns and rows of the terminal, if output goes to one.
#[cfg(unix)]
fn terminal_size() -> Option<(u16, u16)> {
    // SAFETY: TIOCGWINSZ fills the winsize it is given.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    for fd in [libc::STDOUT_FILENO, libc::STDIN_FILENO] {
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_col > 0 {
            return Some((size.ws_col, size.ws_row));
        }
    }
    None
}

#[cfg(not(unix))]
fn terminal_size() -> Option<(u16, u16)> {
    None
}

/// The local terminal passes every key on (Ctrl-C included, to the command)
/// and echoes nothing itself, while it lasts: the remote terminal does both.
struct RawMode {
    #[cfg(unix)]
    saved: libc::termios,
}

impl RawMode {
    #[cfg(unix)]
    fn enter() -> Option<Self> {
        // SAFETY: tcgetattr/tcsetattr read and write the termios they are given.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(Self { saved })
        }
    }

    #[cfg(not(unix))]
    fn enter() -> Option<Self> {
        None
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: puts back what tcgetattr read.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
    }
}
