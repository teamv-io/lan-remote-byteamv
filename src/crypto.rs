//! Password-based encryption for the control channel and video stream.
//!
//! A per-connection random salt is exchanged in the clear; both sides derive the
//! same 32-byte key from the shared password via Argon2id. All subsequent traffic
//! is sealed with XChaCha20-Poly1305 (a random 24-byte nonce per message, which is
//! collision-safe to generate randomly). A wrong password yields a different key,
//! so authentication falls out for free: decryption simply fails and the peer is
//! rejected.

use anyhow::{anyhow, Result};
use argon2::Argon2;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};

/// Length of the handshake salt, in bytes.
pub const SALT_LEN: usize = 16;
/// XChaCha20-Poly1305 nonce length.
const NONCE_LEN: usize = 24;

/// Fill an array with cryptographically secure random bytes from the OS.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("OS RNG unavailable");
    b
}

/// Derive a 32-byte key from a password and salt using Argon2id.
pub fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow!("argon2 key derivation: {e}"))?;
    Ok(key)
}

/// An AEAD cipher keyed from the derived password key. Cheap to clone (just the key).
#[derive(Clone)]
pub struct Cipher {
    aead: XChaCha20Poly1305,
}

impl Cipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            aead: XChaCha20Poly1305::new(Key::from_slice(key)),
        }
    }

    /// Encrypt `plaintext`, returning `nonce(24) || ciphertext+tag`.
    pub fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let nonce_bytes = random_bytes::<NONCE_LEN>();
        let nonce = XNonce::from_slice(&nonce_bytes);
        let ct = self
            .aead
            .encrypt(nonce, plaintext)
            .expect("AEAD encryption never fails for valid input");
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        out
    }

    /// Decrypt a `nonce(24) || ciphertext+tag` buffer. Returns `None` if the data is
    /// malformed or authentication fails (wrong key / tampering).
    pub fn open(&self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < NONCE_LEN {
            return None;
        }
        let (nonce_bytes, ct) = data.split_at(NONCE_LEN);
        let nonce = XNonce::from_slice(nonce_bytes);
        self.aead.decrypt(nonce, ct).ok()
    }
}
