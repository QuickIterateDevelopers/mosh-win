// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! AES-128-OCB session encryption, wire-compatible with mosh's `Crypto::Session`.
//!
//! Wire format: 8-byte big-endian nonce value, then ciphertext with a 16-byte tag.
//! The 12-byte OCB nonce is four zero bytes followed by those 8 bytes.

use aes::Aes128;
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use ocb3::Ocb3;
use ocb3::aead::{Aead, KeyInit, generic_array::GenericArray};

pub struct Session {
    cipher: Ocb3<Aes128>,
}

pub fn parse_key(printable: &str) -> Result<[u8; 16]> {
    if printable.len() != 22 {
        bail!("Key must be 22 letters long.");
    }
    let padded = format!("{printable}==");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(padded)
        .map_err(|_| anyhow!("Key must be well-formed base64."))?;
    let key: [u8; 16] = bytes
        .try_into()
        .map_err(|_| anyhow!("Key must represent 16 octets."))?;
    Ok(key)
}

fn nonce_bytes(val: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&val.to_be_bytes());
    n
}

impl Session {
    pub fn new(key: &[u8; 16]) -> Self {
        Session { cipher: Ocb3::new(GenericArray::from_slice(key)) }
    }

    pub fn encrypt(&self, nonce_val: u64, plaintext: &[u8]) -> Vec<u8> {
        let nonce = nonce_bytes(nonce_val);
        let ct = self
            .cipher
            .encrypt(GenericArray::from_slice(&nonce), plaintext)
            .expect("OCB encryption failed");
        let mut out = Vec::with_capacity(8 + ct.len());
        out.extend_from_slice(&nonce_val.to_be_bytes());
        out.extend_from_slice(&ct);
        out
    }

    /// Returns the nonce value and plaintext, or None if the packet fails authentication.
    pub fn decrypt(&self, data: &[u8]) -> Option<(u64, Vec<u8>)> {
        if data.len() < 24 {
            return None;
        }
        let nonce_val = u64::from_be_bytes(data[..8].try_into().unwrap());
        let nonce = nonce_bytes(nonce_val);
        let pt = self.cipher.decrypt(GenericArray::from_slice(&nonce), &data[8..]).ok()?;
        Some((nonce_val, pt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = parse_key("AAAAAAAAAAAAAAAAAAAAAA").unwrap();
        let s = Session::new(&key);
        let ct = s.encrypt(0x8000_0000_0000_0005, b"hello");
        assert_eq!(ct.len(), 8 + 5 + 16);
        let (n, pt) = s.decrypt(&ct).unwrap();
        assert_eq!(n, 0x8000_0000_0000_0005);
        assert_eq!(pt, b"hello");
    }

    /// RFC 7253 Appendix A test vector (nonce BBAA99887766554433221100, empty AD, empty P).
    #[test]
    fn rfc7253_empty() {
        let key: [u8; 16] = (0u8..16).collect::<Vec<_>>().try_into().unwrap();
        let cipher = Ocb3::<Aes128>::new(GenericArray::from_slice(&key));
        let nonce = [0xBB, 0xAA, 0x99, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00];
        let ct = cipher.encrypt(GenericArray::from_slice(&nonce), &b""[..]).unwrap();
        let hex: String = ct.iter().map(|b| format!("{b:02X}")).collect();
        assert_eq!(hex, "785407BFFFC8AD9EDCC5520AC9111EE6");
    }
}
