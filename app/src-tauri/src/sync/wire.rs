//! Framing for sync and pairing streams: a big-endian `u32` length, a JSON
//! header, and for images the raw PNG bytes right after it. Works over any
//! tokio byte stream (iroh QUIC streams today, anything else later).

use super::engine::{ItemState, Prefs};
use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u32 = 1;
/// Largest JSON header (a text item is at most 2 MB; escaping can grow it).
pub const MAX_HEADER_BYTES: u32 = 8 * 1024 * 1024;
/// Largest image payload accepted from the network.
pub const MAX_BLOB_BYTES: u64 = super::store::MAX_IMAGE_MB as u64 * 1024 * 1024;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Frame {
    /// Opens a sync stream: "send me your changes after `cursor`".
    Request {
        version: u32,
        cursor: i64,
        prefs: Prefs,
        addr: Option<String>,
        /// False when the asking device doesn't sync clipboard history with
        /// us (only presence is wanted).
        #[serde(default = "yes")]
        items: bool,
    },
    /// One logged change. For images, `blob_len` bytes of PNG follow.
    Event { seq: i64, op: String, item_id: String, at: i64, item: Option<ItemState>, blob_len: u64 },
    /// Everything up to `seq` has been sent (or skipped by filters).
    CaughtUp { seq: i64 },
    Heartbeat,
    PairHello {
        version: u32,
        /// The invitation secret (hex); empty when pairing with a code.
        secret: String,
        name: String,
        addr: Option<String>,
        /// SPAKE2 message (hex) when pairing with a nearby device's code.
        #[serde(default)]
        spake: Option<String>,
    },
    /// The host's SPAKE2 message.
    PairSpake { msg: String },
    /// Key confirmation: proof that this side derived the same key.
    PairProof { mac: String },
    /// The joining device's user confirmed the verification code.
    PairConfirm,
    PairPending { code: String },
    PairAccepted { name: String },
    PairRejected { reason: String },
    PairAck,
}

fn yes() -> bool {
    true
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame, blob: Option<&[u8]>) -> io::Result<()> {
    let header = serde_json::to_vec(frame).map_err(|e| invalid(&e.to_string()))?;
    if header.len() as u64 > u64::from(MAX_HEADER_BYTES) {
        return Err(invalid("frame too large"));
    }
    let expected = match frame {
        Frame::Event { blob_len, .. } => *blob_len,
        _ => 0,
    };
    if blob.map_or(0, |b| b.len() as u64) != expected {
        return Err(invalid("blob length does not match the frame"));
    }
    w.write_all(&(header.len() as u32).to_be_bytes()).await?;
    w.write_all(&header).await?;
    if let Some(blob) = blob {
        w.write_all(blob).await?;
    }
    w.flush().await
}

/// Reads one frame. `Ok(None)` is a clean end of stream between frames; a
/// stream that ends inside a frame is an error, so a half-received image is
/// never applied.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<(Frame, Option<Vec<u8>>)>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len);
    if len > MAX_HEADER_BYTES {
        return Err(invalid("frame too large"));
    }
    let mut header = vec![0u8; len as usize];
    r.read_exact(&mut header).await?;
    let frame: Frame = serde_json::from_slice(&header).map_err(|_| invalid("malformed frame"))?;
    let blob = match &frame {
        Frame::Event { blob_len, .. } if *blob_len > 0 => {
            if *blob_len > MAX_BLOB_BYTES {
                return Err(invalid("image too large"));
            }
            let mut blob = vec![0u8; *blob_len as usize];
            r.read_exact(&mut blob).await?;
            Some(blob)
        }
        _ => None,
    };
    Ok(Some((frame, blob)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(blob_len: u64) -> Frame {
        Frame::Event { seq: 7, op: "put".into(), item_id: "x".into(), at: 1, item: None, blob_len }
    }

    #[tokio::test]
    async fn frames_round_trip_with_and_without_blobs() {
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        write_frame(&mut a, &Frame::Heartbeat, None).await.unwrap();
        write_frame(&mut a, &event(5), Some(b"12345")).await.unwrap();
        write_frame(&mut a, &Frame::CaughtUp { seq: 9 }, None).await.unwrap();
        drop(a);
        assert_eq!(read_frame(&mut b).await.unwrap(), Some((Frame::Heartbeat, None)));
        assert_eq!(read_frame(&mut b).await.unwrap(), Some((event(5), Some(b"12345".to_vec()))));
        assert_eq!(read_frame(&mut b).await.unwrap(), Some((Frame::CaughtUp { seq: 9 }, None)));
        assert_eq!(read_frame(&mut b).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_interrupted_image_is_an_error_not_a_frame() {
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        let header = serde_json::to_vec(&event(100)).unwrap();
        a.write_all(&(header.len() as u32).to_be_bytes()).await.unwrap();
        a.write_all(&header).await.unwrap();
        a.write_all(&[0u8; 40]).await.unwrap();
        drop(a);
        assert!(read_frame(&mut b).await.is_err());
    }

    #[tokio::test]
    async fn oversized_and_malformed_frames_are_rejected_before_allocating() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        a.write_all(&(MAX_HEADER_BYTES + 1).to_be_bytes()).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());

        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let header = serde_json::to_vec(&event(MAX_BLOB_BYTES + 1)).unwrap();
        a.write_all(&(header.len() as u32).to_be_bytes()).await.unwrap();
        a.write_all(&header).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());

        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        a.write_all(&4u32.to_be_bytes()).await.unwrap();
        a.write_all(b"nope").await.unwrap();
        assert!(read_frame(&mut b).await.is_err());
    }

    #[tokio::test]
    async fn blob_length_must_match() {
        let (mut a, _b) = tokio::io::duplex(1 << 16);
        assert!(write_frame(&mut a, &event(3), Some(b"1234")).await.is_err());
        assert!(write_frame(&mut a, &event(3), None).await.is_err());
    }
}
