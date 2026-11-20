//! Length-prefixed frames (u32 little-endian length, then the message).

use quinn::{RecvStream, SendStream};
use serde::{Serialize, de::DeserializeOwned};

/// Largest accepted frame. Tick packets are tiny; this bounds hostile input.
pub const MAX_FRAME: usize = 4 << 20;

pub async fn write<T: Serialize>(s: &mut SendStream, m: &T) -> Result<(), String> {
    let body = crate::proto::encode(m);
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    s.write_all(&buf).await.map_err(|e| e.to_string())
}

pub async fn read<T: DeserializeOwned>(s: &mut RecvStream) -> Result<T, String> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await.map_err(|e| e.to_string())?;
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(format!("frame of {n} bytes is too large"));
    }
    let mut body = vec![0u8; n];
    s.read_exact(&mut body).await.map_err(|e| e.to_string())?;
    crate::proto::decode(&body)
}
