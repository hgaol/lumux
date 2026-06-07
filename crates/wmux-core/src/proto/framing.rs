//! Length-delimited framing for protocol messages.
//!
//! Each frame is `u32` big-endian length prefix + that many bincode bytes. The
//! [`FrameCodec`] buffers partial reads so a frame split across multiple
//! transport reads is reassembled correctly — the same robustness the grid
//! parser has for VT bytes, applied to IPC.

use serde::{de::DeserializeOwned, Serialize};

/// Maximum frame size (16 MiB) — guards against a corrupt/hostile length
/// prefix causing an unbounded allocation.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum FrameError {
    Serialize(String),
    Deserialize(String),
    TooLarge(usize),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Serialize(e) => write!(f, "frame serialize: {e}"),
            FrameError::Deserialize(e) => write!(f, "frame deserialize: {e}"),
            FrameError::TooLarge(n) => write!(f, "frame too large: {n} bytes"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Serialize a message into a length-prefixed frame.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let body = bincode::serialize(msg).map_err(|e| FrameError::Serialize(e.to_string()))?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Deserialize a message from a frame body (no length prefix).
pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    bincode::deserialize(body).map_err(|e| FrameError::Deserialize(e.to_string()))
}

/// Accumulates bytes and yields complete frame bodies as they become available.
/// Transport read loops push whatever they read; `next_frame` pops one body at
/// a time.
#[derive(Debug, Default)]
pub struct FrameCodec {
    buf: Vec<u8>,
}

impl FrameCodec {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append freshly-read bytes.
    pub fn extend(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pop the next complete frame body, or None if more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_FRAME {
            return Err(FrameError::TooLarge(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None); // body not fully arrived yet
        }
        let body = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Ok(Some(body))
    }

    /// Decode the next complete message of type `T`, if a full frame is buffered.
    pub fn next_message<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError> {
        match self.next_frame()? {
            Some(body) => Ok(Some(decode(&body)?)),
            None => Ok(None),
        }
    }
}
