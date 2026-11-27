//! Master key handling and encryption of stored credentials.
//!
//! One secret (`M3USORT_KEY`) is the root of everything. It can be either:
//! - a random key: exactly 32 bytes in base64 (what `gen-key` prints), used as is, or
//! - anything else: treated as a passphrase and stretched with Argon2id.
//!
//! Purpose-specific keys are then derived with HKDF-SHA256, so the master key
//! itself is never used directly for encryption or signing.
//!
//! Credentials are encrypted with XChaCha20-Poly1305. The 24-byte nonce is random
//! per encryption (safe to generate randomly at this size), and the secret's name
//! is bound in as associated data, so a ciphertext copied to another field fails
//! to decrypt instead of silently being accepted.

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;

pub const KEY_LEN: usize = 32;
pub const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;

/// Argon2id cost for passphrases. Runs once per start, so it can be well above the
/// OWASP minimum (19 MiB, t=2): 64 MiB, 3 passes, 1 lane.
const ARGON2_M_KIB: u32 = 64 * 1024;
const ARGON2_T: u32 = 3;
const ARGON2_P: u32 = 1;

/// Passphrases shorter than this get a warning in the log (not refused: user's choice).
pub const SHORT_PASSPHRASE: usize = 16;

/// HKDF "info" labels. Changing one of these invalidates everything derived from it.
const INFO_SECRETS: &[u8] = b"m3usort/secrets/v1";
const INFO_SESSION: &[u8] = b"m3usort/session/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// 32 random bytes in base64, used directly.
    Random,
    /// Anything else, stretched with Argon2id. Carries the length in characters.
    Passphrase(usize),
}

#[derive(Debug, thiserror::Error)]
#[error("decryption failed: wrong key or corrupted data")]
pub struct DecryptError;

/// The master key from `M3USORT_KEY`. Never logged, never stored.
pub struct MasterKey {
    secrets: [u8; KEY_LEN],
    session: [u8; KEY_LEN],
    kind: KeyKind,
}

impl MasterKey {
    /// Derive the key from `M3USORT_KEY`. `salt` is only used for passphrases;
    /// it is stored (not secret) in the database.
    pub fn derive(input: &str, salt: &[u8]) -> anyhow::Result<Self> {
        let input = input.trim();
        anyhow::ensure!(!input.is_empty(), "M3USORT_KEY is empty");

        let (ikm, kind) = match B64.decode(input) {
            Ok(raw) if raw.len() == KEY_LEN => (raw, KeyKind::Random),
            _ => {
                let params = Params::new(ARGON2_M_KIB, ARGON2_T, ARGON2_P, Some(KEY_LEN))
                    .map_err(|e| anyhow::anyhow!("argon2 params: {e}"))?;
                let mut out = vec![0u8; KEY_LEN];
                Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
                    .hash_password_into(input.as_bytes(), salt, &mut out)
                    .map_err(|e| anyhow::anyhow!("argon2: {e}"))?;
                (out, KeyKind::Passphrase(input.chars().count()))
            }
        };

        let hk = Hkdf::<Sha256>::new(None, &ikm);
        let mut secrets = [0u8; KEY_LEN];
        let mut session = [0u8; KEY_LEN];
        hk.expand(INFO_SECRETS, &mut secrets)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        hk.expand(INFO_SESSION, &mut session)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Ok(Self {
            secrets,
            session,
            kind,
        })
    }

    pub fn kind(&self) -> KeyKind {
        self.kind
    }

    /// Key for signing session cookies (used by the web layer later).
    #[allow(dead_code)]
    pub fn session_key(&self) -> &[u8; KEY_LEN] {
        &self.session
    }

    /// Encrypt `plaintext` for the secret called `name`. Output: nonce || ciphertext.
    pub fn encrypt(&self, name: &str, plaintext: &[u8]) -> Vec<u8> {
        let cipher = XChaCha20Poly1305::new((&self.secrets).into());
        let nonce: [u8; NONCE_LEN] = random_bytes();
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

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("OS random number generator unavailable");
    b
}

/// Generate a fresh random master key, base64-encoded, ready for `.env`.
pub fn generate_key() -> String {
    B64.encode(random_bytes::<KEY_LEN>())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SALT: &[u8] = b"0123456789abcdef";

    fn key(s: &str) -> MasterKey {
        MasterKey::derive(s, SALT).unwrap()
    }

    #[test]
    fn roundtrip() {
        let k = key(&generate_key());
        let blob = k.encrypt("jellyfin_api_key", b"abc123");
        assert_eq!(k.decrypt("jellyfin_api_key", &blob).unwrap(), b"abc123");
    }

    #[test]
    fn wrong_key_fails() {
        let blob = key(&generate_key()).encrypt("x", b"secret");
        assert!(key(&generate_key()).decrypt("x", &blob).is_err());
    }

    #[test]
    fn swapped_field_fails() {
        let k = key(&generate_key());
        let blob = k.encrypt("tmdb_api_key", b"secret");
        assert!(k.decrypt("jellyfin_api_key", &blob).is_err());
    }

    #[test]
    fn kinds() {
        assert_eq!(key(&generate_key()).kind(), KeyKind::Random);
        assert_eq!(key("MySecret123").kind(), KeyKind::Passphrase(11));
        assert!(MasterKey::derive("   ", SALT).is_err());
    }

    #[test]
    fn passphrase_depends_on_salt_and_is_stable() {
        let blob = key("correct horse").encrypt("x", b"v");
        assert_eq!(key("correct horse").decrypt("x", &blob).unwrap(), b"v");
        let other_salt = MasterKey::derive("correct horse", b"fedcba9876543210").unwrap();
        assert!(other_salt.decrypt("x", &blob).is_err());
        assert!(key("correct horsf").decrypt("x", &blob).is_err());
    }
}
