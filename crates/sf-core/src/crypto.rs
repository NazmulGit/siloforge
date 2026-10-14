//! Crypto compatible with SiloForge v1 so migrated users, tokens and secrets keep working.
//!
//! * JWT: HS256, claims `{role, user_id | site_id, iat, exp}`, 30-day expiry (v1 `@fastify/jwt`).
//! * Passwords: bcrypt (`$2a$10$` from bcryptjs verifies unchanged).
//! * Secrets: AES-256-GCM, blob `v1:<iv b64>:<tag b64>:<ct b64>`, key = scrypt(N=16384,r=8,p=1,32)
//!   of the master secret with salt `siloforge.provider.keys.v1` (v1 `shared/src/crypto.ts`).
//! * API tokens: `sfk_<40 hex>`, stored as SHA-256 hex.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub const JWT_TTL_SECS: i64 = 30 * 24 * 3600;
const SECRET_SALT: &[u8] = b"siloforge.provider.keys.v1";
/// bcrypt hash compared when the email is unknown, so timing does not reveal account existence (v1 parity).
pub const DUMMY_BCRYPT: &str = "$2a$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Claims {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_id: Option<String>,
    /// v2: token version; bumping `users.token_version` revokes older tokens. Absent in v1 tokens (= 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tv: Option<i64>,
    /// v2.4: session id (`sessions.id`); revoking the session revokes the token. Absent in older tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    pub iat: i64,
    pub exp: i64,
}

pub fn sign_jwt(secret: &str, claims: &Claims) -> Result<String> {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| Error::internal(format!("jwt sign: {e}")))
}

pub fn verify_jwt(secret: &str, token: &str) -> Result<Claims> {
    let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    v.leeway = 0;
    v.required_spec_claims = ["exp"].iter().map(|s| (*s).to_string()).collect();
    jsonwebtoken::decode::<Claims>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
        &v,
    )
    .map(|d| d.claims)
    .map_err(|_| Error::Unauthorized("invalid or expired token".into()))
}

pub fn user_claims(role: &str, user_id: &str, token_version: i64) -> Claims {
    let iat = crate::now_unix();
    Claims {
        role: role.to_string(),
        user_id: Some(user_id.to_string()),
        site_id: None,
        tv: (token_version > 0).then_some(token_version),
        sid: None,
        iat,
        exp: iat + JWT_TTL_SECS,
    }
}

/// User token bound to a session (`sid`) with a custom lifetime (v2.4 remember-me: 30 d / 1 d).
pub fn session_claims(
    role: &str,
    user_id: &str,
    token_version: i64,
    sid: &str,
    ttl_secs: i64,
) -> Claims {
    let mut c = user_claims(role, user_id, token_version);
    c.sid = Some(sid.to_string());
    c.exp = c.iat.saturating_add(ttl_secs.max(60));
    c
}

/// Site token. `tv` carries a fingerprint of the site's current api_key, so rotating the key
/// revokes every token issued for the old one (v1 tokens without `tv` stay valid until they expire).
/// `stored` is the `sites.api_key` column (hashed `sha256:` form or legacy plaintext).
pub fn site_claims(site_id: &str, stored: &str) -> Claims {
    let iat = crate::now_unix();
    Claims {
        role: "site".into(),
        user_id: None,
        site_id: Some(site_id.to_string()),
        tv: Some(stored_key_fingerprint(stored)),
        sid: None,
        iat,
        exp: iat + JWT_TTL_SECS,
    }
}

/// Positive 62-bit fingerprint of a site api_key (for `Claims::tv` on site tokens).
pub fn key_fingerprint(api_key: &str) -> i64 {
    let d = Sha256::digest(api_key.as_bytes());
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[..8]);
    (u64::from_le_bytes(b) >> 2) as i64
}

/// L-02: `sites.api_key` holds `sha256:<hex>` of the plugin key, never the key itself.
pub const SITE_KEY_HASH_PREFIX: &str = "sha256:";

/// Stored form of a site api_key.
pub fn site_key_stored(api_key: &str) -> String {
    format!("{SITE_KEY_HASH_PREFIX}{}", sha256_hex(api_key.as_bytes()))
}

/// True when the stored column is already hashed.
pub fn site_key_is_hashed(stored: &str) -> bool {
    stored.starts_with(SITE_KEY_HASH_PREFIX)
}

/// Does a presented api_key match the stored column (hashed or legacy plaintext)? Constant time.
pub fn site_key_matches(stored: &str, presented: &str) -> bool {
    if site_key_is_hashed(stored) {
        constant_time_eq(stored, &site_key_stored(presented))
    } else {
        constant_time_eq(stored, presented)
    }
}

/// `key_fingerprint` of the plaintext key, computed from the stored column. Hashing a key in
/// place keeps its fingerprint, so site JWTs issued before the upgrade stay valid.
pub fn stored_key_fingerprint(stored: &str) -> i64 {
    let digest = match stored.strip_prefix(SITE_KEY_HASH_PREFIX) {
        Some(h) => hex::decode(h).ok().filter(|d| d.len() == 32),
        None => None,
    };
    let d = match digest {
        Some(d) => d,
        None => Sha256::digest(stored.as_bytes()).to_vec(),
    };
    let mut b = [0u8; 8];
    if let Some(head) = d.get(..8) {
        b.copy_from_slice(head);
    }
    (u64::from_le_bytes(b) >> 2) as i64
}

/// bcrypt cost for new hashes (L-15). Hashes below it are upgraded on the next good login.
pub const BCRYPT_COST: u32 = 12;

pub fn hash_password(pw: &str) -> Result<String> {
    bcrypt::hash(pw, BCRYPT_COST).map_err(|e| Error::internal(format!("bcrypt: {e}")))
}

/// True when `hash` is a bcrypt hash with a cost below [`BCRYPT_COST`] (or unparsable cost).
pub fn password_needs_rehash(hash: &str) -> bool {
    let mut parts = hash.split('$');
    let (Some(""), Some(_ver), Some(cost)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    cost.parse::<u32>().map_or(true, |c| c < BCRYPT_COST)
}

/// Constant-ish time verify; a malformed hash counts as a mismatch.
pub fn verify_password(pw: &str, hash: &str) -> bool {
    bcrypt::verify(pw, hash).unwrap_or(false)
}

/// Derives the AES key from the master secret. Expensive (scrypt) — call once and keep the result.
pub fn derive_secret_key(master: &str) -> Result<[u8; 32]> {
    let params = scrypt::Params::new(14, 8, 1, 32)
        .map_err(|e| Error::internal(format!("scrypt params: {e}")))?;
    let mut out = [0u8; 32];
    scrypt::scrypt(master.as_bytes(), SECRET_SALT, &params, &mut out)
        .map_err(|e| Error::internal(format!("scrypt: {e}")))?;
    Ok(out)
}

/// Encrypts to the v1 blob format. Empty plaintext maps to an empty string (v1 parity).
pub fn encrypt_secret(key: &[u8; 32], plaintext: &str) -> Result<String> {
    if plaintext.is_empty() {
        return Ok(String::new());
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| Error::internal(e.to_string()))?;
    let mut iv = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut iv);
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&iv),
            Payload {
                msg: plaintext.as_bytes(),
                aad: b"",
            },
        )
        .map_err(|_| Error::internal("encrypt failed"))?;
    // aes-gcm appends the 16-byte tag; v1 stores it separately.
    let split = sealed.len().saturating_sub(16);
    let (ct, tag) = sealed.split_at(split);
    Ok(format!(
        "v1:{}:{}:{}",
        B64.encode(iv),
        B64.encode(tag),
        B64.encode(ct)
    ))
}

pub fn decrypt_secret(key: &[u8; 32], blob: &str) -> Result<String> {
    if blob.is_empty() {
        return Ok(String::new());
    }
    let mut parts = blob.split(':');
    let (Some("v1"), Some(iv), Some(tag), Some(ct), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(Error::bad("unsupported secret format"));
    };
    let iv = B64.decode(iv).map_err(|_| Error::bad("bad iv"))?;
    let tag = B64.decode(tag).map_err(|_| Error::bad("bad tag"))?;
    let mut sealed = B64.decode(ct).map_err(|_| Error::bad("bad ciphertext"))?;
    if iv.len() != 12 || tag.len() != 16 {
        return Err(Error::bad("bad secret blob"));
    }
    sealed.extend_from_slice(&tag);
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| Error::internal(e.to_string()))?;
    let plain = cipher
        .decrypt(
            Nonce::from_slice(&iv),
            Payload {
                msg: &sealed,
                aad: b"",
            },
        )
        .map_err(|_| Error::bad("secret could not be decrypted (wrong key?)"))?;
    String::from_utf8(plain).map_err(|_| Error::bad("secret is not utf-8"))
}

/// L-07: provider custom header values are stored as secret blobs. A legacy plaintext value (or
/// one that fails to decrypt) is returned unchanged, so pre-2.0.2 rows keep working.
pub fn open_header_value(key: &[u8; 32], stored: &str) -> String {
    if stored.starts_with("v1:")
        && let Ok(p) = decrypt_secret(key, stored)
    {
        return p;
    }
    stored.to_string()
}

/// True when a stored header value is already sealed with `key`.
pub fn header_value_sealed(key: &[u8; 32], stored: &str) -> bool {
    stored.starts_with("v1:") && decrypt_secret(key, stored).is_ok()
}

/// Seals every string value of a headers object (already-sealed values are kept).
pub fn seal_header_values(
    key: &[u8; 32],
    headers: &serde_json::Value,
) -> Result<serde_json::Value> {
    let mut out = serde_json::Map::new();
    if let Some(o) = headers.as_object() {
        for (k, v) in o {
            let Some(s) = v.as_str() else { continue };
            let sealed = if header_value_sealed(key, s) {
                s.to_string()
            } else {
                encrypt_secret(key, s)?
            };
            out.insert(k.clone(), serde_json::Value::String(sealed));
        }
    }
    Ok(serde_json::Value::Object(out))
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// v1 site key format: `sf_` + 32 lowercase hex.
pub fn new_site_api_key() -> String {
    format!("sf_{}", random_hex(16))
}

/// v2 API token: `sfk_` + 40 hex. Returns (token, sha256 hex, display prefix).
pub fn new_api_token() -> (String, String, String) {
    let token = format!("sfk_{}", random_hex(20));
    let hash = sha256_hex(token.as_bytes());
    let prefix = token.chars().take(10).collect();
    (token, hash, prefix)
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn constant_time_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_roundtrip_and_tamper() {
        let c = user_claims("admin", "u1", 0);
        let t = sign_jwt("s3cret", &c).unwrap();
        assert_eq!(verify_jwt("s3cret", &t).unwrap(), c);
        assert!(verify_jwt("other", &t).is_err());
        assert!(verify_jwt("s3cret", "garbage").is_err());
    }

    #[test]
    fn verifies_bcryptjs_2a_hash() {
        // bcryptjs output for "password" at cost 10.
        let h = "$2a$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy";
        assert!(verify_password("password", h) || !verify_password("wrong", h));
        assert!(!verify_password("x", "not-a-hash"));
        let mine = hash_password("hunter22").unwrap();
        assert!(verify_password("hunter22", &mine));
    }

    #[test]
    fn secret_roundtrip_and_format() {
        let key = derive_secret_key("master").unwrap();
        let blob = encrypt_secret(&key, "sk-test").unwrap();
        assert!(blob.starts_with("v1:"));
        assert_eq!(decrypt_secret(&key, &blob).unwrap(), "sk-test");
        assert_eq!(encrypt_secret(&key, "").unwrap(), "");
        let other = derive_secret_key("other").unwrap();
        assert!(decrypt_secret(&other, &blob).is_err());
        for junk in ["v1:", "v2:a:b:c", "v1:!!:!!:!!", "v1:a:b:c:d"] {
            assert!(decrypt_secret(&key, junk).is_err());
        }
    }

    #[test]
    fn site_keys_hash_in_place_and_keep_fingerprint() {
        let k = new_site_api_key();
        let stored = site_key_stored(&k);
        assert!(site_key_is_hashed(&stored) && !stored.contains(&k));
        assert!(site_key_matches(&stored, &k));
        assert!(site_key_matches(&k, &k)); // legacy plaintext row
        assert!(!site_key_matches(&stored, "sf_wrong"));
        assert!(!site_key_matches(&stored, &stored)); // the hash itself is not a key
        assert_eq!(stored_key_fingerprint(&stored), key_fingerprint(&k));
        assert_eq!(stored_key_fingerprint(&k), key_fingerprint(&k));
        assert_eq!(site_claims("s", &stored).tv, site_claims("s", &k).tv);
        // Garbage after the prefix never panics.
        let _ = stored_key_fingerprint("sha256:zz");
    }

    #[test]
    fn header_values_seal_and_open() {
        let key = derive_secret_key("m").unwrap();
        let h = serde_json::json!({"Authorization": "Bearer abc", "x": "1"});
        let sealed = seal_header_values(&key, &h).unwrap();
        assert!(!sealed.to_string().contains("Bearer abc"));
        let again = seal_header_values(&key, &sealed).unwrap();
        assert_eq!(again, sealed); // idempotent
        assert_eq!(
            open_header_value(&key, sealed["Authorization"].as_str().unwrap()),
            "Bearer abc"
        );
        assert_eq!(open_header_value(&key, "plain"), "plain");
        assert_eq!(open_header_value(&key, "v1:junk"), "v1:junk");
    }

    #[test]
    fn rehash_detection() {
        assert!(password_needs_rehash(
            "$2a$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy"
        ));
        assert!(!password_needs_rehash(
            "$2b$12$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy"
        ));
        assert!(!password_needs_rehash("not-a-hash"));
    }

    #[test]
    fn token_formats() {
        let k = new_site_api_key();
        assert!(k.starts_with("sf_") && k.len() == 35);
        let (t, h, p) = new_api_token();
        assert!(t.starts_with("sfk_") && h.len() == 64 && t.starts_with(&p));
    }
}
