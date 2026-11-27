//! Passwords, admin sessions and login rate limiting.

use std::net::IpAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;

use argon2::{Algorithm, Argon2, Params, PasswordHasher, PasswordVerifier, Version};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{KeyInit, Mac};
use sha2::Sha256;

use crate::crypto;

pub const ADMIN_COOKIE: &str = "m3usort_admin";
pub const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// Same policy as the Python version: 5 failures, then 30 minutes locked.
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(30 * 60);

// ---------------------------------------------------------------- passwords

/// Argon2id with the OWASP baseline (19 MiB, t=2, p=1). Lower than the key
/// derivation because the playlist password is checked on every playlist fetch.
fn hasher() -> Argon2<'static> {
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(19 * 1024, 2, 1, None).expect("valid argon2 params"),
    )
}

/// Salted Argon2id hash as a PHC string (`$argon2id$v=19$m=...$salt$hash`).
pub fn hash_password(password: &str) -> String {
    let salt = crypto::random_bytes::<16>();
    hasher()
        .hash_password_with_salt(password.as_bytes(), &salt)
        .expect("argon2 hashing with valid params cannot fail")
        .to_string()
}

pub fn verify_password(password: &str, phc: &str) -> bool {
    hasher().verify_password(password.as_bytes(), phc).is_ok()
}

/// Random human-friendly string (no 0/O/1/l/I), for setup codes and reset passwords.
pub fn random_code(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut out = String::with_capacity(len);
    while out.len() < len {
        for b in crypto::random_bytes::<32>() {
            // 256 is a multiple of 32: no modulo bias.
            out.push(ALPHABET[(b as usize) % ALPHABET.len()] as char);
            if out.len() == len {
                break;
            }
        }
    }
    out
}

// ---------------------------------------------------------------- sessions

/// Admin session cookies: `expiry.generation.nonce.mac`, HMAC-SHA256 signed with a
/// key derived from M3USORT_KEY. Stateless; invalidated by expiry, by bumping the
/// session generation (password change/reset) or by rotating M3USORT_KEY.
pub struct SessionSigner {
    key: [u8; 32],
}

type HmacSha256 = hmac::Hmac<Sha256>;

impl SessionSigner {
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }

    fn mac(&self, payload: &str) -> HmacSha256 {
        let mut m = <HmacSha256 as KeyInit>::new_from_slice(&self.key)
            .expect("HMAC accepts any key length");
        m.update(payload.as_bytes());
        m
    }

    pub fn issue(&self, generation: i64) -> String {
        let exp = now_secs() + SESSION_TTL.as_secs();
        let nonce = B64.encode(crypto::random_bytes::<16>());
        let payload = format!("{exp}.{generation}.{nonce}");
        let tag = B64.encode(self.mac(&payload).finalize().into_bytes());
        format!("{payload}.{tag}")
    }

    pub fn verify(&self, cookie: &str, generation: i64) -> bool {
        let Some((payload, tag)) = cookie.rsplit_once('.') else {
            return false;
        };
        let Ok(tag) = B64.decode(tag) else {
            return false;
        };
        // Constant-time comparison.
        if self.mac(payload).verify_slice(&tag).is_err() {
            return false;
        }
        let mut parts = payload.splitn(3, '.');
        let exp: u64 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let generation_in_cookie: i64 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(-1);
        exp > now_secs() && generation_in_cookie == generation
    }
}

pub fn set_cookie_header(name: &str, value: &str, max_age: Duration) -> String {
    // SameSite=Strict blocks cross-site form posts (CSRF). No `Secure`: the app is
    // served over plain HTTP on the LAN; a reverse proxy with TLS can add it.
    format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        max_age.as_secs()
    )
}

pub fn clear_cookie_header(name: &str) -> String {
    format!("{name}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

pub fn read_cookie<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- rate limiting

/// Per-IP failure counter, stored in the database. Per IP (not global like the
/// Python version) so a guest typing wrong passwords can't lock the admin out
/// from another device. Clear with `m3usort reset-ip <ip>`.
#[derive(Clone)]
pub struct LoginLimiter {
    db: SqlitePool,
}

impl LoginLimiter {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    /// `Err(minutes_left)` while locked. A database error never locks anyone out.
    pub async fn check(&self, ip: IpAddr) -> Result<(), u64> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT locked_until FROM login_failures WHERE ip = ?")
                .bind(ip.to_string())
                .fetch_optional(&self.db)
                .await
                .unwrap_or(None);
        let now = now_secs() as i64;
        match row {
            Some((until,)) if until > now => Err(((until - now) as u64).div_ceil(60)),
            _ => Ok(()),
        }
    }

    pub async fn fail(&self, ip: IpAddr) {
        let now = now_secs() as i64;
        let window = LOCKOUT.as_secs() as i64;
        // Start a new window if the previous one is older than the lockout period
        // (or a previous lockout has expired); otherwise count up.
        let res: Result<(i64,), _> = sqlx::query_as(
            "INSERT INTO login_failures (ip, failures, first_failure, locked_until) VALUES (?1, 1, ?2, 0)
             ON CONFLICT(ip) DO UPDATE SET
               failures      = CASE WHEN first_failure < ?2 - ?3 OR (locked_until > 0 AND locked_until <= ?2)
                                    THEN 1 ELSE failures + 1 END,
               first_failure = CASE WHEN first_failure < ?2 - ?3 OR (locked_until > 0 AND locked_until <= ?2)
                                    THEN ?2 ELSE first_failure END,
               locked_until  = CASE WHEN locked_until > 0 AND locked_until <= ?2 THEN 0 ELSE locked_until END
             RETURNING failures",
        )
        .bind(ip.to_string())
        .bind(now)
        .bind(window)
        .fetch_one(&self.db)
        .await;
        if let Ok((n,)) = res
            && n >= i64::from(MAX_FAILURES)
        {
            let _ = sqlx::query(
                "UPDATE login_failures SET locked_until = ? WHERE ip = ? AND locked_until <= ?",
            )
            .bind(now + window)
            .bind(ip.to_string())
            .bind(now)
            .execute(&self.db)
            .await;
            if n == i64::from(MAX_FAILURES) {
                tracing::warn!(
                    "too many failed logins from {ip}; locked for {} minutes (clear with: m3usort reset-ip {ip})",
                    LOCKOUT.as_secs() / 60
                );
            }
        }
    }

    pub async fn reset(&self, ip: IpAddr) {
        let _ = sqlx::query("DELETE FROM login_failures WHERE ip = ?")
            .bind(ip.to_string())
            .execute(&self.db)
            .await;
    }
}

/// IPs that are locked right now, with minutes left. For `m3usort locked-ips`.
pub async fn locked_ips(db: &SqlitePool) -> anyhow::Result<Vec<(String, u64)>> {
    let now = now_secs() as i64;
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT ip, locked_until FROM login_failures WHERE locked_until > ? ORDER BY ip",
    )
    .bind(now)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(ip, u)| (ip, ((u - now) as u64).div_ceil(60)))
        .collect())
}

/// Remove one IP's failures (or all with `None`). Returns rows removed.
pub async fn reset_ip(db: &SqlitePool, ip: Option<IpAddr>) -> anyhow::Result<u64> {
    let res = match ip {
        Some(ip) => {
            sqlx::query("DELETE FROM login_failures WHERE ip = ?")
                .bind(ip.to_string())
                .execute(db)
                .await?
        }
        None => {
            sqlx::query("DELETE FROM login_failures")
                .execute(db)
                .await?
        }
    };
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip_and_salted() {
        let a = hash_password("IPTV");
        let b = hash_password("IPTV");
        assert!(a.starts_with("$argon2id$"));
        assert_ne!(a, b, "same password must give different hashes (salt)");
        assert!(verify_password("IPTV", &a));
        assert!(!verify_password("iptv", &a));
    }

    #[test]
    fn session_cookie() {
        let s = SessionSigner::new([7u8; 32]);
        let c = s.issue(3);
        assert!(s.verify(&c, 3));
        assert!(!s.verify(&c, 4), "old generation must be rejected");
        let mut tampered = c.clone();
        tampered.replace_range(0..1, "9");
        assert!(!s.verify(&tampered, 3));
        assert!(
            !SessionSigner::new([8u8; 32]).verify(&c, 3),
            "other key must be rejected"
        );
        assert!(!s.verify("garbage", 3));
    }

    #[tokio::test]
    async fn limiter_locks_after_five_and_resets() {
        let dir = std::env::temp_dir().join(format!("m3usort-lim-{}", random_code(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::open(&dir.join("t.db"), true).await.unwrap();
        let l = LoginLimiter::new(db.clone());
        let ip: IpAddr = "192.168.1.5".parse().unwrap();
        let other: IpAddr = "192.168.1.6".parse().unwrap();
        for _ in 0..4 {
            l.fail(ip).await;
        }
        assert!(l.check(ip).await.is_ok());
        l.fail(ip).await;
        assert!(l.check(ip).await.is_err());
        assert!(l.check(other).await.is_ok(), "other IPs unaffected");
        assert_eq!(locked_ips(&db).await.unwrap().len(), 1);
        assert_eq!(reset_ip(&db, Some(ip)).await.unwrap(), 1);
        assert!(l.check(ip).await.is_ok());
        l.fail(other).await;
        assert_eq!(reset_ip(&db, None).await.unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codes() {
        let c = random_code(8);
        assert_eq!(c.len(), 8);
        assert!(
            c.chars()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
        );
    }
}
