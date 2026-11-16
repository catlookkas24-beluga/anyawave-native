//! Transport encryption, "rtpsize" modes: AAD = RTP header, 4-byte LE?? counter appended.
//! Modes: aead_aes256_gcm_rtpsize (preferred, AES-NI) / aead_xchacha20_poly1305_rtpsize.
//! Packet = header(12) | ciphertext | tag(16) | counter(4)
use aes_gcm::{aead::{AeadInPlace, KeyInit}, Aes256Gcm, Nonce};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

pub enum Cipher { Aes(Aes256Gcm), XChaCha(XChaCha20Poly1305) }

pub struct Transport { cipher: Cipher, counter: u32 }

impl Transport {
    pub fn new(mode: &str, key: &[u8; 32]) -> Option<Self> {
        let cipher = match mode {
            "aead_aes256_gcm_rtpsize" => Cipher::Aes(Aes256Gcm::new(key.into())),
            "aead_xchacha20_poly1305_rtpsize" => Cipher::XChaCha(XChaCha20Poly1305::new(key.into())),
            _ => return None,
        };
        Some(Self { cipher, counter: 0 })
    }
    /// out must be empty; writes full datagram. Zero allocations if `out` has capacity.
    pub fn seal(&mut self, header: &[u8; 12], payload: &[u8], out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(header);
        let start = out.len();
        out.extend_from_slice(payload);
        let ctr = self.counter.to_be_bytes(); // VERIFY endianness vs current Discord docs
        self.counter = self.counter.wrapping_add(1);
        let mut buf = out.split_off(start);
        let tag = match &self.cipher {
            Cipher::Aes(c) => { let mut n = [0u8; 12]; n[..4].copy_from_slice(&ctr);
                c.encrypt_in_place_detached(Nonce::from_slice(&n), header, &mut buf).unwrap().to_vec() }
            Cipher::XChaCha(c) => { let mut n = [0u8; 24]; n[..4].copy_from_slice(&ctr);
                c.encrypt_in_place_detached(XNonce::from_slice(&n), header, &mut buf).unwrap().to_vec() }
        };
        out.extend_from_slice(&buf);
        out.extend_from_slice(&tag);
        out.extend_from_slice(&ctr);
    }
}
