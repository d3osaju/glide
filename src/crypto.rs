//! Pre-shared-key encryption. Every byte on the wire (keystrokes included) is
//! sealed with ChaCha20-Poly1305. Each session derives fresh directional keys
//! from the PSK and two random nonces, and packets carry a 64-bit counter that
//! doubles as the AEAD nonce and as replay/reorder protection.

use base64::{engine::general_purpose::STANDARD, Engine};
use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit, Nonce};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;

pub type Key = [u8; 32];

pub fn gen_key() -> String {
    let mut k = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut k);
    STANDARD.encode(k)
}

pub fn parse_key(s: &str) -> Result<Key, String> {
    let bytes = STANDARD
        .decode(s.trim())
        .map_err(|e| format!("key is not valid base64: {e}"))?;
    bytes
        .try_into()
        .map_err(|_| "key must be 32 bytes (generate one with `glide genkey`)".to_string())
}

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

pub fn derive(psk: &Key, label: &str, salt: &[u8]) -> Key {
    let hk = Hkdf::<Sha256>::new(Some(salt), psk);
    let mut out = [0u8; 32];
    hk.expand(label.as_bytes(), &mut out).expect("32 bytes is a valid HKDF length");
    out
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    Nonce::from(n)
}

pub struct Sealer {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Sealer {
    pub fn new(key: &Key) -> Self {
        Sealer { cipher: ChaCha20Poly1305::new(key.into()), counter: 0 }
    }

    /// Returns `counter (8 bytes LE) || ciphertext+tag`.
    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let c = self.counter;
        self.counter += 1;
        let ct = self.cipher.encrypt(&nonce(c), plaintext).expect("encryption cannot fail");
        let mut out = Vec::with_capacity(8 + ct.len());
        out.extend_from_slice(&c.to_le_bytes());
        out.extend_from_slice(&ct);
        out
    }
}

pub struct Opener {
    cipher: ChaCha20Poly1305,
    next: u64,
    /// TCP: counters must be exactly sequential. UDP: strictly increasing,
    /// gaps allowed (lost packets), anything older is dropped.
    strict: bool,
}

impl Opener {
    pub fn new(key: &Key, strict: bool) -> Self {
        Opener { cipher: ChaCha20Poly1305::new(key.into()), next: 0, strict }
    }

    pub fn open(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        if packet.len() < 8 + 16 {
            return None;
        }
        let c = u64::from_le_bytes(packet[..8].try_into().ok()?);
        if c < self.next || (self.strict && c != self.next) {
            return None;
        }
        let pt = self.cipher.decrypt(&nonce(c), &packet[8..]).ok()?;
        self.next = c + 1;
        Some(pt)
    }
}

/// Directional keys for one KVM session.
pub struct SessionKeys {
    pub c2s_tcp: Key,
    pub s2c_tcp: Key,
    pub s2c_udp: Key,
}

impl SessionKeys {
    pub fn derive(psk: &Key, client_nonce: &[u8; 32], server_nonce: &[u8; 32]) -> Self {
        let mut salt = [0u8; 64];
        salt[..32].copy_from_slice(client_nonce);
        salt[32..].copy_from_slice(server_nonce);
        SessionKeys {
            c2s_tcp: derive(psk, "glide v1 c2s tcp", &salt),
            s2c_tcp: derive(psk, "glide v1 s2c tcp", &salt),
            s2c_udp: derive(psk, "glide v1 s2c udp", &salt),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_and_replay() {
        let k = derive(&[7; 32], "t", b"salt");
        let mut s = Sealer::new(&k);
        let mut o = Opener::new(&k, false);
        let p1 = s.seal(b"one");
        let p2 = s.seal(b"two");
        let p3 = s.seal(b"three");
        assert_eq!(o.open(&p2).as_deref(), Some(&b"two"[..]));
        assert_eq!(o.open(&p1), None, "older packet dropped");
        assert_eq!(o.open(&p2), None, "replay dropped");
        assert_eq!(o.open(&p3).as_deref(), Some(&b"three"[..]));
    }

    #[test]
    fn strict_and_wrong_key() {
        let k = derive(&[1; 32], "t", b"");
        let mut s = Sealer::new(&k);
        let p0 = s.seal(b"a");
        let p1 = s.seal(b"b");
        let mut o = Opener::new(&k, true);
        assert_eq!(o.open(&p1), None, "gap rejected in strict mode");
        assert!(o.open(&p0).is_some());
        let mut bad = Opener::new(&derive(&[2; 32], "t", b""), true);
        assert_eq!(bad.open(&p0), None);
    }

    #[test]
    fn key_roundtrip() {
        let s = gen_key();
        assert_eq!(parse_key(&s).unwrap().len(), 32);
        assert!(parse_key("short").is_err());
    }
}
