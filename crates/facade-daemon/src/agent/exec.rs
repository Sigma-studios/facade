//! `facade exec` on the agent: a Docker exec in the app's container, its input
//! and output in frames on a data connection (see `facade_core::exec`).

use std::time::Duration;

use anyhow::{Context, bail};
use bollard::Docker;
use bollard::container::LogOutput;
use bollard::exec::{ResizeExecOptions, StartExecOptions, StartExecResults};
use bollard::query_parameters::InspectContainerOptions;
use facade_core::exec::{ExecRequest, Frame};
use futures_util::StreamExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, info};

/// Runs `request` in `container` and streams it over `stream`, which ends with
/// the exit code, or with why it could not run.
pub async fn session<S>(
    docker: Docker,
    app: String,
    container: String,
    request: ExecRequest,
    stream: S,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut from_caller, mut to_caller) = tokio::io::split(stream);
    let command = facade_core::join_command(&request.command);
    let result = run(
        &docker,
        &app,
        &container,
        request,
        &mut from_caller,
        &mut to_caller,
    )
    .await;
    let last = match result {
        Ok(Some(code)) => {
            info!(%app, "exec `{command}` exited with code {code}");
            Frame::Exit(code)
        }
        Ok(None) => {
            info!(%app, "exec `{command}`: the caller went away, stopped it");
            return;
        }
        Err(e) => {
            debug!(%app, "exec `{command}`: {e:#}");
            Frame::Error(format!("{e:#}"))
        }
    };
    let _ = to_caller.write_all(&last.encode()).await;
    let _ = to_caller.shutdown().await;
}

/// The exit code, or `None` when the caller hung up first (the command is then
/// stopped, as a terminal closing would).
async fn run<R, W>(
    docker: &Docker,
    app: &str,
    container: &str,
    request: ExecRequest,
    from_caller: &mut R,
    to_caller: &mut W,
) -> anyhow::Result<Option<i64>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if request.command.is_empty() {
        bail!("no command to run");
    }
    let running = docker
        .inspect_container(container, None::<InspectContainerOptions>)
        .await
        .ok()
        .and_then(|c| c.state?.running)
        .unwrap_or(false);
    if !running {
        bail!("{app} is not running on this node: commands run in its running container");
    }
    let env = request.tty.then(|| {
        vec![format!(
            "TERM={}",
            request.term.as_deref().unwrap_or("xterm-256color")
        )]
    });
    let exec = docker
        .create_exec(
            container,
            bollard::models::ExecConfig {
                cmd: Some(request.command.clone()),
                attach_stdin: Some(request.stdin),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                tty: Some(request.tty),
                env,
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("running a command in {app}"))?;
    let started = docker
        .start_exec(
            &exec.id,
            Some(StartExecOptions {
                detach: false,
                tty: request.tty,
                output_capacity: Some(64 * 1024),
            }),
        )
        .await?;
    let StartExecResults::Attached {
        mut output,
        mut input,
    } = started
    else {
        bail!("Docker did not attach to the command");
    };
    if request.tty
        && let Some((cols, rows)) = request.size
    {
        resize(docker, &exec.id, cols, rows).await;
    }
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        tokio::select! {
            read = from_caller.read(&mut chunk) => {
                let n = read.unwrap_or(0);
                if n == 0 {
                    stop(docker, &exec.id).await;
                    return Ok(None);
                }
                buf.extend_from_slice(&chunk[..n]);
                loop {
                    match Frame::decode(&mut buf) {
                        Ok(Some(Frame::Stdin(bytes))) => {
                            // A command that closed its input reads no more.
                            if input.write_all(&bytes).await.is_ok() {
                                let _ = input.flush().await;
                            }
                        }
                        Ok(Some(Frame::StdinEnd)) => {
                            let _ = input.shutdown().await;
                        }
                        Ok(Some(Frame::Resize(cols, rows))) => {
                            resize(docker, &exec.id, cols, rows).await;
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(e) => {
                            stop(docker, &exec.id).await;
                            bail!("unreadable input: {e}");
                        }
                    }
                }
            }
            out = output.next() => match out {
                Some(Ok(out)) => {
                    let frame = match out {
                        LogOutput::StdErr { message } => Frame::Stderr(message.to_vec()),
                        LogOutput::StdOut { message } | LogOutput::Console { message } => {
                            Frame::Stdout(message.to_vec())
                        }
                        LogOutput::StdIn { .. } => continue,
                    };
                    if to_caller.write_all(&frame.encode()).await.is_err() {
                        stop(docker, &exec.id).await;
                        return Ok(None);
                    }
                    let _ = to_caller.flush().await;
                }
                Some(Err(e)) => bail!("reading its output: {e}"),
                None => break,
            },
        }
    }
    // The output ends as the command exits; Docker records the code a moment
    // later.
    for _ in 0..50 {
        let info = docker.inspect_exec(&exec.id).await?;
        if info.running != Some(true)
            && let Some(code) = info.exit_code
        {
            return Ok(Some(code));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("the command's output ended but it did not exit")
}

async fn resize(docker: &Docker, exec: &str, cols: u16, rows: u16) {
    if cols == 0 || rows == 0 {
        return;
    }
    let size = ResizeExecOptions {
        height: rows,
        width: cols,
    };
    if let Err(e) = docker.resize_exec(exec, size).await {
        debug!("resizing an exec terminal: {e}");
    }
}

/// Stops a command whose caller is gone: SIGHUP, as when a terminal closes,
/// then SIGKILL if it is still there a few seconds later. Docker leaves an exec
/// running when nobody is attached any more.
async fn stop(docker: &Docker, exec: &str) {
    let pid = match docker.inspect_exec(exec).await {
        Ok(info) if info.running == Some(true) => info.pid.unwrap_or(0),
        _ => return,
    };
    // The pid is the host's: the agent runs next to Docker, outside containers.
    let Ok(pid) = i32::try_from(pid) else { return };
    if pid <= 1 {
        return;
    }
    signal(pid, libc::SIGHUP);
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match docker.inspect_exec(exec).await {
            Ok(info) if info.running == Some(true) => {}
            _ => return,
        }
    }
    signal(pid, libc::SIGKILL);
}

fn signal(pid: i32, signal: i32) {
    // SAFETY: kill(2) has no memory effects; a wrong pid only fails.
    unsafe {
        libc::kill(pid, signal);
    }
}
