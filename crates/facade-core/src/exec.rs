//! `facade exec`: a command run in an app's container, its input and output
//! carried over one connection from the CLI to the agent, through the server.
//!
//! The CLI opens `GET /api/apps/<app>/exec` with `Upgrade: facade-exec`; once
//! switched, it writes one JSON line, an [`ExecRequest`], then both sides write
//! [`Frame`]s: input and terminal sizes from the CLI, output and the exit code
//! from the agent. The server passes the bytes through untouched (to the agent's
//! data connection), so it never parses a frame.
//!
//! A frame is a one-byte kind, the payload's length (4 bytes, big endian), then
//! the payload. The CLI closing its side means it is gone: the agent stops the
//! command. The end of its input is a frame of its own.

use serde::{Deserialize, Serialize};

/// The `Upgrade` protocol name.
pub const UPGRADE: &str = "facade-exec";

/// Larger payloads are a broken stream, not data.
const MAX_FRAME: usize = 1 << 20;

/// What to run, and how.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecRequest {
    /// The program and its arguments (no shell unless it is the program).
    pub command: Vec<String>,
    /// Give it a terminal.
    #[serde(default)]
    pub tty: bool,
    /// Pass it the caller's input.
    #[serde(default)]
    pub stdin: bool,
    /// The terminal's size, columns and rows, with `tty`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<(u16, u16)>,
    /// The caller's `TERM`, with `tty`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// Caller → command.
    Stdin(Vec<u8>),
    /// The caller's input ended.
    StdinEnd,
    /// The caller's terminal changed size: columns, rows.
    Resize(u16, u16),
    /// Command → caller.
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The command exited with this code; the last frame.
    Exit(i64),
    /// It could not run, or the stream broke; the last frame.
    Error(String),
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let (kind, payload): (u8, std::borrow::Cow<[u8]>) = match self {
            Frame::Stdin(b) => (0, b.into()),
            Frame::Stdout(b) => (1, b.into()),
            Frame::Stderr(b) => (2, b.into()),
            Frame::StdinEnd => (3, (&[][..]).into()),
            Frame::Resize(cols, rows) => {
                let mut b = cols.to_be_bytes().to_vec();
                b.extend(rows.to_be_bytes());
                (4, b.into())
            }
            Frame::Exit(code) => (5, code.to_be_bytes().to_vec().into()),
            Frame::Error(message) => (6, message.as_bytes().into()),
        };
        let mut out = Vec::with_capacity(5 + payload.len());
        out.push(kind);
        out.extend((payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// Takes the first whole frame off `buf`; `None` until one is complete.
    pub fn decode(buf: &mut Vec<u8>) -> Result<Option<Frame>, String> {
        if buf.len() < 5 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
        if len > MAX_FRAME {
            return Err(format!("frame of {len} bytes"));
        }
        if buf.len() < 5 + len {
            return Ok(None);
        }
        let kind = buf[0];
        let payload: Vec<u8> = buf.drain(..5 + len).skip(5).collect();
        let frame = match kind {
            0 => Frame::Stdin(payload),
            1 => Frame::Stdout(payload),
            2 => Frame::Stderr(payload),
            3 => Frame::StdinEnd,
            4 if payload.len() == 4 => Frame::Resize(
                u16::from_be_bytes([payload[0], payload[1]]),
                u16::from_be_bytes([payload[2], payload[3]]),
            ),
            5 if payload.len() == 8 => {
                Frame::Exit(i64::from_be_bytes(payload.try_into().expect("8 bytes")))
            }
            6 => Frame::Error(String::from_utf8_lossy(&payload).into_owned()),
            _ => return Err(format!("unknown frame (kind {kind}, {len} bytes)")),
        };
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_in_pieces() {
        let frames = [
            Frame::Stdin(b"ls\n".to_vec()),
            Frame::StdinEnd,
            Frame::Resize(120, 40),
            Frame::Stdout(vec![0, 1, 2, 255]),
            Frame::Stderr(b"oops".to_vec()),
            Frame::Exit(-1),
            Frame::Error("no such app".into()),
        ];
        let bytes: Vec<u8> = frames.iter().flat_map(Frame::encode).collect();
        // Fed a byte at a time, as a network might.
        let mut buf = Vec::new();
        let mut decoded = Vec::new();
        for b in bytes {
            buf.push(b);
            while let Some(frame) = Frame::decode(&mut buf).unwrap() {
                decoded.push(frame);
            }
        }
        assert_eq!(decoded, frames);
        assert!(buf.is_empty());
    }

    #[test]
    fn refuses_garbage() {
        let mut buf = vec![9, 0, 0, 0, 0];
        assert!(Frame::decode(&mut buf).is_err());
        let mut buf = vec![1, 0xff, 0xff, 0xff, 0xff];
        assert!(Frame::decode(&mut buf).is_err());
    }
}
