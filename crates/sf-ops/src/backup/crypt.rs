//! Passphrase encryption for backup archives (pure Rust, streaming).
//!
//! File format (`*.zip.sfenc`), all integers little-endian:
//! ```text
//! offset size  field
//! 0      8     magic "SFBKENC1"
//! 8      1     scrypt log2(N)
//! 9      4     scrypt r
//! 13     4     scrypt p
//! 17     16    salt (random)
//! 33     7     nonce prefix (random)
//! 40     4     chunk size C (plaintext bytes per chunk)
//! 44     ...   chunks: AES-256-GCM(ciphertext || 16-byte tag), each holding C plaintext bytes except
//!              the final chunk (0..=C bytes). Nonce = prefix(7) || chunk index (u32 BE) || last flag (1 byte,
//!              1 on the final chunk). AAD = the 44-byte header. Key = scrypt(passphrase, salt, N, r, p, 32).
//! ```
//! The last-chunk flag and the counter in the nonce stop truncation, reordering and appending (the
//! STREAM construction). A wrong passphrase fails on the first chunk.

use std::io::{BufRead, BufReader, Read, Write};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;
use sf_core::{Error, Result};

pub const MAGIC: &[u8; 8] = b"SFBKENC1";
const HEADER_LEN: usize = 44;
const TAG: usize = 16;
/// Default plaintext chunk size (1 MiB).
pub const CHUNK: usize = 1 << 20;
/// scrypt cost used for new archives (N = 2^15, r = 8, p = 1: ~32 MiB RAM, well under a second).
const LOG_N: u8 = 15;
/// Upper bound on the scrypt memory a (possibly hostile) archive header may request.
const MAX_SCRYPT_MEM: u64 = 256 * 1024 * 1024;

fn derive(pass: &str, salt: &[u8], log_n: u8, r: u32, p: u32) -> Result<[u8; 32]> {
    if !(10..=20).contains(&log_n) || !(1..=32).contains(&r) || !(1..=16).contains(&p) {
        return Err(Error::bad(
            "backup header has unsupported scrypt parameters",
        ));
    }
    // L-10: scrypt needs about 128 * r * N bytes; refuse headers asking for more than 256 MiB.
    if 128u64 * u64::from(r) * (1u64 << log_n) > MAX_SCRYPT_MEM {
        return Err(Error::bad("backup header asks for too much scrypt memory"));
    }
    let params = scrypt::Params::new(log_n, r, p, 32)
        .map_err(|e| Error::internal(format!("scrypt params: {e}")))?;
    let mut key = [0u8; 32];
    scrypt::scrypt(pass.as_bytes(), salt, &params, &mut key)
        .map_err(|e| Error::internal(format!("scrypt: {e}")))?;
    Ok(key)
}

fn nonce(prefix: &[u8], idx: u32, last: bool) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..7].copy_from_slice(prefix.get(..7).unwrap_or(&[0u8; 7]));
    n[7..11].copy_from_slice(&idx.to_be_bytes());
    n[11] = u8::from(last);
    n
}

/// True when the file starts with the encrypted-archive magic.
pub fn is_encrypted(path: &std::path::Path) -> bool {
    let mut buf = [0u8; 8];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok()
        && &buf == MAGIC
}

/// Encrypts `input` to `output` with `chunk` plaintext bytes per frame.
pub fn encrypt_stream<R: Read, W: Write>(
    mut input: R,
    mut output: W,
    pass: &str,
    chunk: usize,
) -> Result<()> {
    if pass.is_empty() {
        return Err(Error::bad("passphrase must not be empty"));
    }
    let chunk = chunk.clamp(16, 64 << 20);
    let mut salt = [0u8; 16];
    let mut prefix = [0u8; 7];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut prefix);
    let (r, p) = (8u32, 1u32);
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(MAGIC);
    header.push(LOG_N);
    header.extend_from_slice(&r.to_le_bytes());
    header.extend_from_slice(&p.to_le_bytes());
    header.extend_from_slice(&salt);
    header.extend_from_slice(&prefix);
    header.extend_from_slice(&(chunk as u32).to_le_bytes());
    let key = derive(pass, &salt, LOG_N, r, p)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| Error::internal(e.to_string()))?;
    output.write_all(&header)?;

    // Hold one chunk back so the final frame (which may be full) can carry the last flag.
    let mut cur = vec![0u8; chunk];
    let mut cur_len = read_full(&mut input, &mut cur)?;
    let mut idx: u32 = 0;
    loop {
        let mut next = vec![0u8; chunk];
        let next_len = if cur_len == chunk {
            read_full(&mut input, &mut next)?
        } else {
            0
        };
        let last = next_len == 0;
        let ct = cipher
            .encrypt(
                Nonce::from_slice(&nonce(&prefix, idx, last)),
                Payload {
                    msg: cur.get(..cur_len).unwrap_or(&[]),
                    aad: &header,
                },
            )
            .map_err(|_| Error::internal("encrypt failed"))?;
        output.write_all(&ct)?;
        if last {
            break;
        }
        idx = idx
            .checked_add(1)
            .ok_or_else(|| Error::bad("archive too large"))?;
        cur = next;
        cur_len = next_len;
    }
    output.flush()?;
    Ok(())
}

/// Decrypts a stream written by [`encrypt_stream`]. Fails on a wrong passphrase or any tampering.
pub fn decrypt_stream<R: Read, W: Write>(input: R, mut output: W, pass: &str) -> Result<()> {
    let mut input = BufReader::with_capacity(1 << 16, input);
    let mut header = [0u8; HEADER_LEN];
    input
        .read_exact(&mut header)
        .map_err(|_| Error::bad("not an encrypted SiloForge backup (short header)"))?;
    if header.get(..8) != Some(MAGIC.as_slice()) {
        return Err(Error::bad("not an encrypted SiloForge backup"));
    }
    let u32_at = |o: usize| -> u32 {
        let mut b = [0u8; 4];
        if let Some(s) = header.get(o..o + 4) {
            b.copy_from_slice(s);
        }
        u32::from_le_bytes(b)
    };
    let log_n = header[8];
    let (r, p) = (u32_at(9), u32_at(13));
    let salt = header.get(17..33).unwrap_or(&[]).to_vec();
    let prefix = header.get(33..40).unwrap_or(&[]).to_vec();
    let chunk = u32_at(40) as usize;
    if !(16..=(64 << 20)).contains(&chunk) {
        return Err(Error::bad("backup header has an invalid chunk size"));
    }
    let key = derive(pass, &salt, log_n, r, p)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| Error::internal(e.to_string()))?;
    let frame = chunk + TAG;
    let mut buf = vec![0u8; frame];
    let mut idx: u32 = 0;
    loop {
        let n = read_full(&mut input, &mut buf)?;
        if n < TAG {
            return Err(Error::bad("backup archive is truncated"));
        }
        let last = n < frame || input.fill_buf()?.is_empty();
        let pt = cipher
            .decrypt(
                Nonce::from_slice(&nonce(&prefix, idx, last)),
                Payload {
                    msg: buf.get(..n).unwrap_or(&[]),
                    aad: &header,
                },
            )
            .map_err(|_| {
                if idx == 0 {
                    Error::bad("wrong passphrase or corrupted backup")
                } else {
                    Error::bad("backup archive is corrupted or truncated")
                }
            })?;
        output.write_all(&pt)?;
        if last {
            break;
        }
        idx = idx
            .checked_add(1)
            .ok_or_else(|| Error::bad("archive too large"))?;
    }
    output.flush()?;
    Ok(())
}

fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let Some(rest) = buf.get_mut(n..) else { break };
        match r.read(rest) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(len: usize, chunk: usize) {
        let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        let mut enc = Vec::new();
        encrypt_stream(&data[..], &mut enc, "pw", chunk).unwrap();
        let mut dec = Vec::new();
        decrypt_stream(&enc[..], &mut dec, "pw").unwrap();
        assert_eq!(dec, data, "len {len} chunk {chunk}");
    }

    #[test]
    fn scrypt_memory_capped() {
        // L-10: 128 * 32 * 2^20 = 4 GiB is refused up front; the default cost still works.
        assert!(derive("pw", &[0u8; 16], 20, 32, 1).is_err());
        assert!(derive("pw", &[0u8; 16], 18, 16, 1).is_err());
        assert!(derive("pw", &[0u8; 16], 11, 8, 1).is_ok());
    }

    #[test]
    fn roundtrips_and_boundaries() {
        for len in [0, 1, 15, 16, 17, 32, 33, 100] {
            roundtrip(len, 16);
        }
    }

    #[test]
    fn rejects_wrong_pass_truncation_and_tamper() {
        let data = [7u8; 100];
        let mut enc = Vec::new();
        encrypt_stream(&data[..], &mut enc, "pw", 16).unwrap();
        assert!(decrypt_stream(&enc[..], &mut Vec::new(), "nope").is_err());
        // drop the final frame: the previous frame was not flagged last
        let frame = 16 + TAG;
        let cut = enc.len() - (enc.len() - HEADER_LEN) % frame;
        let cut = if cut == enc.len() { cut - frame } else { cut };
        assert!(decrypt_stream(&enc[..cut], &mut Vec::new(), "pw").is_err());
        let mut bad = enc.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(decrypt_stream(&bad[..], &mut Vec::new(), "pw").is_err());
        assert!(decrypt_stream(&b"garbage"[..], &mut Vec::new(), "pw").is_err());
        assert!(encrypt_stream(&data[..], &mut Vec::new(), "", 16).is_err());
    }
}
