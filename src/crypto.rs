//! Master key handling and encryption of stored credentials.
//!
//! One secret (`M3USORT_KEY`, 32 random bytes, base64) is the root of everything.
//! Purpose-specific keys are derived from it with HKDF-SHA256, so the master key
//! itself is never used directly for encryption or signing.
//!
//! Credentials are encrypted with XChaCha20-Poly1305. The 24-byte nonce is random
//! per encryption (safe to generate randomly at this size), and the secret's name
//! is bound in as associated data, so a ciphertext copied to another field fails
//! to decrypt instead of silently being accepted.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;

pub const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// HKDF "info" labels. Changing one of these invalidates everything derived from it.
const INFO_SECRETS: &[u8] = b"m3usort/secrets/v1";
const INFO_SESSION: &[u8] = b"m3usort/session/v1";

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("M3USORT_KEY is not valid base64")]
    NotBase64,
    #[error("M3USORT_KEY must decode to exactly 32 bytes, got {0}")]
    WrongLength(usize),
}

#[derive(Debug, thiserror::Error)]
#[error("decryption failed: wrong key or corrupted data")]
pub struct DecryptError;

/// The master key from `M3USORT_KEY`. Never logged, never stored.
pub struct MasterKey {
    secrets: [u8; KEY_LEN],
    session: [u8; KEY_LEN],
}

impl MasterKey {
    pub fn from_base64(value: &str) -> Result<Self, KeyError> {
        let raw = B64.decode(value.trim()).map_err(|_| KeyError::NotBase64)?;
        if raw.len() != KEY_LEN {
            return Err(KeyError::WrongLength(raw.len()));
        }
        let hk = Hkdf::<Sha256>::new(None, &raw);
        let mut secrets = [0u8; KEY_LEN];
        let mut session = [0u8; KEY_LEN];
        hk.expand(INFO_SECRETS, &mut secrets)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        hk.expand(INFO_SESSION, &mut session)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Ok(Self { secrets, session })
    }

    /// Key for signing session cookies (used by the web layer later).
    #[allow(dead_code)]
    pub fn session_key(&self) -> &[u8; KEY_LEN] {
        &self.session
    }

    /// Encrypt `plaintext` for the secret called `name`. Output: nonce || ciphertext.
    pub fn encrypt(&self, name: &str, plaintext: &[u8]) -> Vec<u8> {
        let cipher = XChaCha20Poly1305::new((&self.secrets).into());
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).expect("OS random number generator unavailable");
        let ct = cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plaintext,
                    aad: name.as_bytes(),
                },
            )
            .expect("XChaCha20-Poly1305 encryption cannot fail for in-memory input");
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    pub fn decrypt(&self, name: &str, blob: &[u8]) -> Result<Vec<u8>, DecryptError> {
        if blob.len() < NONCE_LEN {
            return Err(DecryptError);
        }
        let (nonce, ct) = blob.split_at(NONCE_LEN);
        let nonce = XNonce::try_from(nonce).map_err(|_| DecryptError)?;
        let cipher = XChaCha20Poly1305::new((&self.secrets).into());
        cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: ct,
                    aad: name.as_bytes(),
                },
            )
            .map_err(|_| DecryptError)
    }
}

/// Generate a fresh random master key, base64-encoded, ready for `.env`.
pub fn generate_key() -> String {
    let mut raw = [0u8; KEY_LEN];
    getrandom::fill(&mut raw).expect("OS random number generator unavailable");
    B64.encode(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = MasterKey::from_base64(&generate_key()).unwrap();
        let blob = key.encrypt("jellyfin_api_key", b"abc123");
        assert_eq!(key.decrypt("jellyfin_api_key", &blob).unwrap(), b"abc123");
    }

    #[test]
    fn wrong_key_fails() {
        let a = MasterKey::from_base64(&generate_key()).unwrap();
        let b = MasterKey::from_base64(&generate_key()).unwrap();
        let blob = a.encrypt("x", b"secret");
        assert!(b.decrypt("x", &blob).is_err());
    }

    #[test]
    fn swapped_field_fails() {
        let key = MasterKey::from_base64(&generate_key()).unwrap();
        let blob = key.encrypt("tmdb_api_key", b"secret");
        assert!(key.decrypt("jellyfin_api_key", &blob).is_err());
    }

    #[test]
    fn rejects_bad_keys() {
        assert!(matches!(
            MasterKey::from_base64("not base64!!"),
            Err(KeyError::NotBase64)
        ));
        assert!(matches!(
            MasterKey::from_base64("c2hvcnQ="),
            Err(KeyError::WrongLength(5))
        ));
    }
}
