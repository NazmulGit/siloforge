//! TOTP (RFC 6238: HMAC-SHA1, 6 digits, 30 s steps) for v2.4 two-factor login, plus the base32
//! codec for the shared secret and the `otpauth://` enrolment URI.

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha1::Sha1;

pub const STEP_SECS: i64 = 30;
pub const DIGITS: u32 = 6;
/// Accepted clock drift in steps (either side).
pub const DRIFT_STEPS: i64 = 1;

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32 without padding.
pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let mut buf: u64 = 0;
    let mut bits = 0u32;
    for &b in data {
        buf = (buf << 8) | u64::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((buf >> bits) & 31) as usize;
            out.push(char::from(B32.get(idx).copied().unwrap_or(b'A')));
        }
    }
    if bits > 0 {
        let idx = ((buf << (5 - bits)) & 31) as usize;
        out.push(char::from(B32.get(idx).copied().unwrap_or(b'A')));
    }
    out
}

/// Decodes base32 (case-insensitive, spaces/`=` ignored). `None` on an invalid character.
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let mut buf: u64 = 0;
    let mut bits = 0u32;
    for c in s.chars() {
        if c == '=' || c == ' ' || c == '-' {
            continue;
        }
        let up = c.to_ascii_uppercase() as u8;
        let v = B32.iter().position(|&x| x == up)? as u64;
        buf = ((buf << 5) | v) & 0xFFFF_FFFF;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// A new random 160-bit secret, base32-encoded (32 chars).
pub fn new_secret() -> String {
    let mut b = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut b);
    base32_encode(&b)
}

/// HOTP value for one counter.
pub fn hotp(secret: &[u8], counter: u64) -> Option<u32> {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).ok()?;
    mac.update(&counter.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let off = usize::from(h.get(19).copied()? & 0x0f);
    let slice = h.get(off..off + 4)?;
    let bin = (u32::from(slice.first().copied()? & 0x7f) << 24)
        | (u32::from(slice.get(1).copied()?) << 16)
        | (u32::from(slice.get(2).copied()?) << 8)
        | u32::from(slice.get(3).copied()?);
    Some(bin % 10u32.pow(DIGITS))
}

/// Code for a UNIX time (zero-padded).
pub fn code_at(secret_b32: &str, unix: i64) -> Option<String> {
    let key = base32_decode(secret_b32)?;
    let step = u64::try_from(unix.div_euclid(STEP_SECS)).ok()?;
    hotp(&key, step).map(|v| format!("{v:06}"))
}

/// Verifies `code` at `unix` within ±[`DRIFT_STEPS`]; returns the matched step so callers can
/// refuse a replay (`step <= last_used_step`).
pub fn verify(secret_b32: &str, code: &str, unix: i64) -> Option<i64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != DIGITS as usize || !code.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let key = base32_decode(secret_b32)?;
    if key.is_empty() {
        return None;
    }
    let now_step = unix.div_euclid(STEP_SECS);
    for d in -DRIFT_STEPS..=DRIFT_STEPS {
        let step = now_step.checked_add(d)?;
        let Ok(c) = u64::try_from(step) else { continue };
        if let Some(v) = hotp(&key, c)
            && crate::crypto::constant_time_eq(&format!("{v:06}"), &code)
        {
            return Some(step);
        }
    }
    None
}

fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'@') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `otpauth://totp/<issuer>:<account>?secret=..&issuer=..&algorithm=SHA1&digits=6&period=30`.
pub fn otpauth_uri(issuer: &str, account: &str, secret_b32: &str) -> String {
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
        pct(issuer),
        pct(account),
        secret_b32,
        pct(issuer)
    )
}

/// Ten recovery codes `xxxxx-xxxxx` (lowercase base32 alphabet without look-alikes).
pub fn recovery_codes(n: usize) -> Vec<String> {
    const A: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..n)
        .map(|_| {
            let mut s = String::with_capacity(11);
            for i in 0..10 {
                if i == 5 {
                    s.push('-');
                }
                let idx = (rng.next_u32() as usize) % A.len();
                s.push(char::from(A.get(idx).copied().unwrap_or(b'a')));
            }
            s
        })
        .collect()
}

/// Canonical form of a typed recovery code (lowercase, no spaces; dash re-inserted).
pub fn normalize_recovery(code: &str) -> String {
    let c: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if c.len() == 10 {
        format!("{}-{}", &c[..5], &c[5..])
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 appendix B (SHA-1, secret "12345678901234567890"), truncated to 6 digits.
    #[test]
    fn rfc6238_vectors() {
        let secret = base32_encode(b"12345678901234567890");
        assert_eq!(secret, "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        for (t, want) in [
            (59, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
        ] {
            assert_eq!(code_at(&secret, t).unwrap(), want, "t={t}");
        }
    }

    #[test]
    fn verify_allows_one_step_drift_only() {
        let s = new_secret();
        let now = 1_700_000_000;
        let c = code_at(&s, now).unwrap();
        assert_eq!(verify(&s, &c, now), Some(now / 30));
        assert!(verify(&s, &c, now + 30).is_some());
        assert!(verify(&s, &c, now + 95).is_none());
        assert!(verify(&s, "12345", now).is_none());
        assert!(verify(&s, "abcdef", now).is_none());
        assert!(verify("!!!", "123456", now).is_none());
    }

    #[test]
    fn base32_round_trip_and_hostile() {
        for n in 0..40 {
            let d: Vec<u8> = (0..n).map(|i| (i * 37 % 256) as u8).collect();
            assert_eq!(base32_decode(&base32_encode(&d)).unwrap(), d);
        }
        assert!(base32_decode("ab1!").is_none());
        let _ = base32_decode(&"Z".repeat(10_000));
    }

    #[test]
    fn uri_and_recovery_codes() {
        let u = otpauth_uri("Silo Forge", "a@example.com", "ABC");
        assert!(u.starts_with(
            "otpauth://totp/Silo%20Forge:a@example.com?secret=ABC&issuer=Silo%20Forge"
        ));
        let codes = recovery_codes(10);
        assert_eq!(codes.len(), 10);
        assert!(
            codes
                .iter()
                .all(|c| c.len() == 11 && c.as_bytes()[5] == b'-')
        );
        assert_eq!(normalize_recovery(" ABCDE fghjk "), "abcde-fghjk");
    }
}
