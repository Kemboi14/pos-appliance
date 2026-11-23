//! Encrypted, authenticated frames over a byte stream.
//!
//! ```text
//! len:u32-le | version:u8 | till:u8 | nonce:24 | ciphertext ‖ tag:16
//! ```
//!
//! `len` counts everything after itself. The 26 header bytes after `len`
//! (and `len`) are authenticated as associated data. Keys come from the
//! store network key via BLAKE3 `derive_key`.

use alloc::vec::Vec;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

pub const VERSION: u8 = 1;
const HDR: usize = 4 + 1 + 1 + 24;
const TAG: usize = 16;
/// Largest payload accepted (catalog and pictures are sent in parts).
pub const MAX_PAYLOAD: usize = 4 * 1024 * 1024;

/// The store network key.
#[derive(Clone, PartialEq, Eq)]
pub struct StoreKey(pub [u8; 32]);

impl core::fmt::Debug for StoreKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("StoreKey(…)")
    }
}

impl StoreKey {
    fn derive(&self, purpose: &str) -> [u8; 32] {
        blake3::derive_key(purpose, &self.0)
    }
    /// A short public fingerprint (shown on screen to compare tills).
    pub fn fingerprint(&self) -> [u8; 4] {
        let k = self.derive("pos-appliance 2026 store network fingerprint");
        [k[0], k[1], k[2], k[3]]
    }

    /// Human-typable form: 52 base-32 characters in groups of four.
    pub fn to_code(&self) -> alloc::string::String {
        const A: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let mut bits: u32 = 0;
        let mut n = 0;
        let mut out = alloc::string::String::new();
        let mut emitted = 0;
        for &b in &self.0 {
            bits = bits << 8 | u32::from(b);
            n += 8;
            while n >= 5 {
                n -= 5;
                if emitted > 0 && emitted % 4 == 0 {
                    out.push('-');
                }
                out.push(char::from(A[((bits >> n) & 31) as usize]));
                emitted += 1;
            }
        }
        if n > 0 {
            if emitted % 4 == 0 {
                out.push('-');
            }
            out.push(char::from(A[((bits << (5 - n)) & 31) as usize]));
        }
        out
    }

    /// Parse [`StoreKey::to_code`] output (case and dashes ignored).
    pub fn from_code(s: &str) -> Option<StoreKey> {
        const A: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let mut bits: u32 = 0;
        let mut n = 0;
        let mut out = Vec::with_capacity(32);
        let mut chars = 0;
        for c in s.chars().filter(|c| !matches!(c, '-' | ' ')) {
            let c = c.to_ascii_uppercase() as u8;
            let v = A.iter().position(|&a| a == c)? as u32;
            chars += 1;
            bits = bits << 5 | v;
            n += 5;
            if n >= 8 {
                n -= 8;
                if out.len() < 32 {
                    out.push((bits >> n) as u8);
                }
            }
        }
        if chars != 52 {
            return None;
        }
        let k: [u8; 32] = out.try_into().ok()?;
        Some(StoreKey(k))
    }
}

/// Seals and opens frames for one store key.
pub struct Cipher {
    aead: XChaCha20Poly1305,
}

impl core::fmt::Debug for Cipher {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Cipher")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    TooLarge,
    Version,
    /// Wrong key, or the frame was altered.
    Auth,
}

impl Cipher {
    pub fn new(key: &StoreKey) -> Cipher {
        let k = key.derive("pos-appliance 2026 store network frames v1");
        Cipher {
            aead: XChaCha20Poly1305::new(&k.into()),
        }
    }

    /// Encrypt `payload` from `till` with a fresh random `nonce`.
    pub fn seal(&self, till: u8, nonce: [u8; 24], payload: &[u8]) -> Result<Vec<u8>, FrameError> {
        if payload.len() > MAX_PAYLOAD {
            return Err(FrameError::TooLarge);
        }
        let len = (HDR - 4 + payload.len() + TAG) as u32;
        let mut hdr = Vec::with_capacity(HDR);
        hdr.extend_from_slice(&len.to_le_bytes());
        hdr.push(VERSION);
        hdr.push(till);
        hdr.extend_from_slice(&nonce);
        let ct = self
            .aead
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: payload,
                    aad: &hdr,
                },
            )
            .map_err(|_| FrameError::Auth)?;
        hdr.extend_from_slice(&ct);
        Ok(hdr)
    }

    /// Take one complete frame off the front of `buf`, if there is one.
    /// Returns (sender till, payload). A bad frame is an error: the
    /// connection should be dropped.
    pub fn open(&self, buf: &mut Vec<u8>) -> Result<Option<(u8, Vec<u8>)>, FrameError> {
        if buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if !(HDR - 4 + TAG..=MAX_PAYLOAD + HDR + TAG).contains(&len) {
            return Err(FrameError::TooLarge);
        }
        if buf.len() < 4 + len {
            return Ok(None);
        }
        let frame: Vec<u8> = buf.drain(..4 + len).collect();
        if frame[4] != VERSION {
            return Err(FrameError::Version);
        }
        let till = frame[5];
        let (hdr, ct) = frame.split_at(HDR);
        let nonce: [u8; 24] = hdr[6..30].try_into().map_err(|_| FrameError::Auth)?;
        let pt = self
            .aead
            .decrypt(&XNonce::from(nonce), Payload { msg: ct, aad: hdr })
            .map_err(|_| FrameError::Auth)?;
        Ok(Some((till, pt)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_and_tamper() {
        let k = StoreKey([7; 32]);
        let c = Cipher::new(&k);
        let mut buf = c.seal(3, [1; 24], b"hello").unwrap();
        buf.extend(c.seal(4, [2; 24], b"world!").unwrap());
        // Partial frames wait for more bytes.
        let mut part = buf[..10].to_vec();
        assert_eq!(c.open(&mut part), Ok(None));
        assert_eq!(c.open(&mut buf), Ok(Some((3, b"hello".to_vec()))));
        assert_eq!(c.open(&mut buf), Ok(Some((4, b"world!".to_vec()))));
        assert!(buf.is_empty());
        // Any flipped bit, including the header's till byte, is refused.
        let good = c.seal(3, [9; 24], b"payload").unwrap();
        for i in 4..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 1;
            assert!(c.open(&mut bad).is_err(), "byte {i}");
        }
        // Another store's key cannot read it.
        let other = Cipher::new(&StoreKey([8; 32]));
        let mut g = good.clone();
        assert_eq!(other.open(&mut g), Err(FrameError::Auth));
    }

    #[test]
    fn key_code_round_trip() {
        let k = StoreKey(core::array::from_fn(|i| (i * 37 + 5) as u8));
        let s = k.to_code();
        assert_eq!(s.chars().filter(|c| *c != '-').count(), 52);
        assert_eq!(StoreKey::from_code(&s), Some(k.clone()));
        assert_eq!(
            StoreKey::from_code(&s.to_lowercase().replace('-', " ")),
            Some(k)
        );
        assert_eq!(StoreKey::from_code("ABCD"), None);
        let mut typo: alloc::vec::Vec<char> = s.chars().collect();
        typo[0] = '1';
        let typo: alloc::string::String = typo.into_iter().collect();
        assert_eq!(StoreKey::from_code(&typo), None);
    }
}
