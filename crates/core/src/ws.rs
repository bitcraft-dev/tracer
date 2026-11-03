//! WebSocket framing (RFC 6455), for both ends Tracer plays: the server of
//! the local API ([`crate::api`]) and the client of a `WEBSOCKET` request
//! ([`crate::realtime`]). Hand-written, as the rest of the core's HTTP
//! plumbing is, so no crate comes in for it.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

pub const CONTINUATION: u8 = 0x0;
pub const TEXT: u8 = 0x1;
pub const BINARY: u8 = 0x2;
pub const CLOSE: u8 = 0x8;
pub const PING: u8 = 0x9;
pub const PONG: u8 = 0xA;

/// One frame, ready to write. A client's frames are masked (`mask`), a
/// server's are not.
pub fn frame(opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let bit = if mask.is_some() { 0x80 } else { 0 };
    let mut frame = vec![0x80 | opcode];
    match payload.len() {
        n if n < 126 => frame.push(bit | n as u8),
        n if n <= u16::MAX as usize => {
            frame.push(bit | 126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            frame.push(bit | 127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    match mask {
        Some(mask) => {
            frame.extend_from_slice(&mask);
            frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        }
        None => frame.extend_from_slice(payload),
    }
    frame
}

/// A frame as read: whether it ends its message, its opcode, its payload
/// (unmasked).
#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// Take the first whole frame off the front of `buf`, if it holds one yet.
/// Masked or not, as either end may send. A frame over `max` bytes is an
/// error, before its payload has arrived.
pub fn take_frame(buf: &mut Vec<u8>, max: usize) -> Result<Option<Frame>, String> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let (fin, opcode, masked) = (buf[0] & 0x80 != 0, buf[0] & 0x0F, buf[1] & 0x80 != 0);
    let (len, mut at) = match buf[1] & 0x7F {
        126 if buf.len() < 4 => return Ok(None),
        126 => (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4),
        127 if buf.len() < 10 => return Ok(None),
        127 => (u64::from_be_bytes(buf[2..10].try_into().unwrap()) as usize, 10),
        n => (n as usize, 2),
    };
    if len > max {
        return Err(format!("a frame of {len} bytes, over the {max} allowed"));
    }
    let mask = if masked {
        if buf.len() < at + 4 {
            return Ok(None);
        }
        at += 4;
        Some([buf[at - 4], buf[at - 3], buf[at - 2], buf[at - 1]])
    } else {
        None
    };
    if buf.len() < at + len {
        return Ok(None);
    }
    let mut payload: Vec<u8> = buf.drain(..at + len).skip(at).collect();
    if let Some(mask) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    Ok(Some(Frame { fin, opcode, payload }))
}

/// What a server answers a handshake's `Sec-WebSocket-Key` with.
pub fn accept_key(key: &str) -> String {
    base64(&sha1(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes()))
}

/// A handshake's `Sec-WebSocket-Key`: 16 random bytes, in base64.
pub fn new_key() -> String {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&random_u64().to_le_bytes());
    bytes[8..].copy_from_slice(&random_u64().to_le_bytes());
    base64(&bytes)
}

/// A client frame's mask.
pub fn new_mask() -> [u8; 4] {
    (random_u64() as u32).to_le_bytes()
}

/// From the OS's randomness, through `RandomState`'s per-instance seed: no
/// more than masks and handshake keys need, which only have to be
/// unpredictable to the page or proxy in between.
fn random_u64() -> u64 {
    let mut h = RandomState::new().build_hasher();
    h.write_u64(std::process::id() as u64);
    h.finish()
}

pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, t);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

pub fn base64(data: &[u8]) -> String {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_answers_as_rfc_6455_says() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(new_key().len(), 24);
    }

    #[test]
    fn a_frame_reads_back_masked_or_not_and_only_once_whole() {
        for mask in [None, Some([1, 2, 3, 250])] {
            for len in [0, 5, 125, 126, 70_000] {
                let payload: Vec<u8> = (0..len).map(|i| i as u8).collect();
                let bytes = frame(TEXT, &payload, mask);
                let mut buf = bytes[..bytes.len() - 1].to_vec();
                if len > 0 {
                    assert_eq!(take_frame(&mut buf, 1 << 20), Ok(None), "{len}");
                }
                let mut buf = bytes.clone();
                buf.extend_from_slice(&[0x89, 0]);
                assert_eq!(take_frame(&mut buf, 1 << 20), Ok(Some(Frame { fin: true, opcode: TEXT, payload })));
                assert_eq!(take_frame(&mut buf, 1 << 20), Ok(Some(Frame { fin: true, opcode: PING, payload: vec![] })));
                assert!(buf.is_empty());
            }
        }
        let mut big = frame(BINARY, &[0; 300], None);
        assert!(take_frame(&mut big, 100).is_err());
    }
}
