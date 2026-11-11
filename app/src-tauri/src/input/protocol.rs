//! Messages on the input stream (`clipet/input/1`) and their framing: a
//! big-endian `u16` length and a small JSON object. One stream per session.
//!
//! Ordering and replay: QUIC delivers a stream in order, encrypted and
//! replay-protected; on top, every message after `Enter` carries a strictly
//! increasing sequence number (`s`), and anything not newer is dropped.

use super::geometry::Side;
use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_MESSAGE: usize = 4096;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Windows,
    MacOs,
    Other,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Controller → controlled.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToControlled {
    /// Starts a session. `side` is where the controlled device sits as seen
    /// from the controller; `fraction` is the position along that edge.
    Enter { v: u32, session: u64, side: Side, fraction: f64, os: Os },
    /// Pointer movement in device-independent units (1/96 inch).
    Move { s: u64, dx: f32, dy: f32 },
    Button { s: u64, b: Button, down: bool },
    /// Wheel units: 120 = one notch.
    Scroll { s: u64, dx: f32, dy: f32 },
    /// A physical key (USB HID usage).
    Key { s: u64, k: u16, down: bool, rep: bool },
    Heartbeat { s: u64 },
    /// The controller takes its input back.
    Leave { s: u64 },
}

impl ToControlled {
    pub fn seq(&self) -> Option<u64> {
        match self {
            ToControlled::Enter { .. } => None,
            ToControlled::Move { s, .. }
            | ToControlled::Button { s, .. }
            | ToControlled::Scroll { s, .. }
            | ToControlled::Key { s, .. }
            | ToControlled::Heartbeat { s }
            | ToControlled::Leave { s } => Some(*s),
        }
    }
}

/// Controlled → controller.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToController {
    Accepted { os: Os },
    Denied { reason: String },
    /// The pointer crossed back over the shared edge at `fraction`.
    Return { fraction: f64 },
    /// The controlled side ended the session (Stop Sharing, timeout…).
    Ended { reason: String },
}

pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if body.len() > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut frame = Vec::with_capacity(body.len() + 2);
    frame.extend_from_slice(&(body.len() as u16).to_be_bytes());
    frame.extend_from_slice(&body);
    // One write per message: low latency, no partial frames on the wire.
    w.write_all(&frame).await?;
    w.flush().await
}

/// `Ok(None)` at a clean end of stream.
pub async fn read_msg<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<Option<T>> {
    let mut len = [0u8; 2];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u16::from_be_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "malformed message"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn messages_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let msgs = vec![
            ToControlled::Enter { v: 1, session: 7, side: Side::Right, fraction: 0.25, os: Os::Windows },
            ToControlled::Move { s: 1, dx: 3.5, dy: -1.0 },
            ToControlled::Key { s: 2, k: 0x04, down: true, rep: false },
            ToControlled::Leave { s: 3 },
        ];
        for m in &msgs {
            write_msg(&mut a, m).await.unwrap();
        }
        drop(a);
        for m in &msgs {
            assert_eq!(read_msg::<_, ToControlled>(&mut b).await.unwrap().as_ref(), Some(m));
        }
        assert_eq!(read_msg::<_, ToControlled>(&mut b).await.unwrap(), None);
    }

    #[tokio::test]
    async fn oversized_and_garbage_messages_are_errors() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        a.write_all(&u16::MAX.to_be_bytes()).await.unwrap();
        assert!(read_msg::<_, ToControlled>(&mut b).await.is_err());
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&3u16.to_be_bytes()).await.unwrap();
        a.write_all(b"abc").await.unwrap();
        assert!(read_msg::<_, ToControlled>(&mut b).await.is_err());
    }
}
