//! Node.js `crypto` shim — Rust hostcalls for the QuickJS extension runtime.
//!
//! Registers native functions on the QuickJS global object that provide real
//! cryptographic operations (SHA-256, SHA-512, SHA-1, MD5, HMAC, random bytes,
//! UUID generation, constant-time comparison) to the `node:crypto` JS module.

use rquickjs::prelude::Func;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Register all crypto hostcalls on the QuickJS global object.
///
/// Call this during runtime initialization, after `ctx.globals()` is available.
pub fn register_crypto_hostcalls(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    register_hash_hostcall(global)?;
    register_hmac_hostcall(global)?;
    register_uuid_hostcall(global)?;
    register_random_int_hostcall(global)?;
    register_timing_safe_equal_hostcall(global)?;
    Ok(())
}

fn register_hash_hostcall(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    // __pi_crypto_hash_native(algorithm, data, encoding) -> digest string
    global.set(
        "__pi_crypto_hash_native",
        Func::from(
            |algorithm: String, data: String, encoding: String| -> rquickjs::Result<String> {
                let bytes = data.as_bytes();
                let hash_bytes: Vec<u8> = match algorithm.as_str() {
                    "sha256" => {
                        let mut h = Sha256::new();
                        h.update(bytes);
                        h.finalize().to_vec()
                    }
                    "sha512" => {
                        let mut h = sha2::Sha512::new();
                        h.update(bytes);
                        h.finalize().to_vec()
                    }
                    "sha1" => {
                        let mut h = sha1::Sha1::new();
                        h.update(bytes);
                        h.finalize().to_vec()
                    }
                    "md5" => {
                        let mut h = md5::Md5::new();
                        h.update(bytes);
                        h.finalize().to_vec()
                    }
                    _ => {
                        return Err(rquickjs::Error::new_from_js(
                            "string",
                            "unsupported hash algorithm",
                        ));
                    }
                };
                Ok(encode_output(&hash_bytes, &encoding))
            },
        ),
    )
}

fn register_hmac_hostcall(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    // __pi_crypto_hmac_native(algorithm, key, data, encoding) -> digest string
    global.set(
        "__pi_crypto_hmac_native",
        Func::from(
            |algorithm: String,
             key: String,
             data: String,
             encoding: String|
             -> rquickjs::Result<String> {
                use hmac::Mac;
                let hash_bytes = match algorithm.as_str() {
                    "sha256" => {
                        let mut mac = hmac::Hmac::<Sha256>::new_from_slice(key.as_bytes())
                            .map_err(|_| {
                                rquickjs::Error::new_from_js("key", "invalid HMAC key length")
                            })?;
                        mac.update(data.as_bytes());
                        mac.finalize().into_bytes().to_vec()
                    }
                    "sha512" => {
                        let mut mac = hmac::Hmac::<sha2::Sha512>::new_from_slice(key.as_bytes())
                            .map_err(|_| {
                                rquickjs::Error::new_from_js("key", "invalid HMAC key length")
                            })?;
                        mac.update(data.as_bytes());
                        mac.finalize().into_bytes().to_vec()
                    }
                    "sha1" => {
                        let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(key.as_bytes())
                            .map_err(|_| {
                                rquickjs::Error::new_from_js("key", "invalid HMAC key length")
                            })?;
                        mac.update(data.as_bytes());
                        mac.finalize().into_bytes().to_vec()
                    }
                    "md5" => {
                        let mut mac = hmac::Hmac::<md5::Md5>::new_from_slice(key.as_bytes())
                            .map_err(|_| {
                                rquickjs::Error::new_from_js("key", "invalid HMAC key length")
                            })?;
                        mac.update(data.as_bytes());
                        mac.finalize().into_bytes().to_vec()
                    }
                    _ => {
                        return Err(rquickjs::Error::new_from_js(
                            "string",
                            "unsupported HMAC algorithm",
                        ));
                    }
                };
                Ok(encode_output(&hash_bytes, &encoding))
            },
        ),
    )
}

fn register_uuid_hostcall(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    // __pi_crypto_random_uuid_native() -> v4 UUID string
    global.set(
        "__pi_crypto_random_uuid_native",
        Func::from(|| -> String { Uuid::new_v4().to_string() }),
    )
}

fn register_random_int_hostcall(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    // __pi_crypto_random_int_native(min, max) -> integer in [min, max)
    global.set(
        "__pi_crypto_random_int_native",
        Func::from(|min: f64, max: f64| -> rquickjs::Result<f64> {
            if min >= max {
                return Err(rquickjs::Error::new_from_js(
                    "number",
                    "min must be less than max",
                ));
            }
            let range = max - min;
            let rand_bytes = random_bytes(8);
            let mut random_window = [0_u8; 4];
            random_window.copy_from_slice(&rand_bytes[..4]);
            let random = f64::from(u32::from_le_bytes(random_window));
            let normalized = random / (f64::from(u32::MAX) + 1.0);
            Ok(min + (normalized * range).floor())
        }),
    )
}

fn register_timing_safe_equal_hostcall(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    // __pi_crypto_timing_safe_equal_native(a_hex, b_hex) -> bool
    global.set(
        "__pi_crypto_timing_safe_equal_native",
        Func::from(|a_hex: String, b_hex: String| -> rquickjs::Result<bool> {
            let a = hex_decode(&a_hex);
            let b = hex_decode(&b_hex);
            if a.len() != b.len() {
                return Err(rquickjs::Error::new_from_js(
                    "buffer",
                    "Input buffers must have the same byte length",
                ));
            }
            let mut result = 0u8;
            for (x, y) in a.iter().zip(b.iter()) {
                result |= x ^ y;
            }
            Ok(result == 0)
        }),
    )
}

/// Encode bytes as hex or base64 string.
fn encode_output(bytes: &[u8], encoding: &str) -> String {
    match encoding {
        "base64" => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        }
        _ => hex_lower(bytes),
    }
}

/// Convert bytes to lowercase hex string.
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
    ];
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[usize::from(byte >> 4)]);
        output.push(HEX[usize::from(byte & 0x0f)]);
    }
    output
}

/// Decode a hex string to bytes, ignoring invalid chars.
fn hex_decode(hex: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let mut chars = hex.chars();
    while let (Some(hi), Some(lo)) = (chars.next(), chars.next()) {
        if let (Some(h), Some(l)) = (hi.to_digit(16), lo.to_digit(16)) {
            if let Ok(byte) = u8::try_from(h * 16 + l) {
                bytes.push(byte);
            }
        }
    }
    bytes
}

/// Generate random bytes using UUID v4 as entropy source.
fn random_bytes(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let bytes = Uuid::new_v4().into_bytes();
        let remaining = len - out.len();
        out.extend_from_slice(&bytes[..remaining.min(bytes.len())]);
    }
    out
}

/// The JS source for the `node:crypto` virtual module.
pub const NODE_CRYPTO_JS: &str = r"
// Helper: convert hex string to Uint8Array with Buffer-like toString
function hexToBuffer(hex) {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  bytes.toString = function(enc) {
    if (enc === 'hex') return hex;
    if (enc === 'base64') return globalThis.btoa(String.fromCharCode(...this));
    return new TextDecoder().decode(this);
  };
  return bytes;
}

// Helper: Uint8Array to hex string
function bufToHex(buf) {
  return Array.from(buf).map(b => b.toString(16).padStart(2, '0')).join('');
}

export function randomUUID() {
  if (typeof globalThis.__pi_crypto_random_uuid_native === 'function') {
    return globalThis.__pi_crypto_random_uuid_native();
  }
  const r = Math.random().toString(16).slice(2);
  return `00000000-0000-4000-8000-${r.padEnd(12, '0').slice(0, 12)}`;
}

export function createHash(algorithm) {
  let data = '';
  return {
    update(input) {
      data += String(input ?? '');
      return this;
    },
    digest(encoding) {
      if (typeof globalThis.__pi_crypto_hash_native === 'function') {
        const hex = globalThis.__pi_crypto_hash_native(algorithm, data, 'hex');
        if (!encoding) return hexToBuffer(hex);
        if (encoding === 'hex') return hex;
        if (encoding === 'base64') {
          return globalThis.__pi_crypto_hash_native(algorithm, data, 'base64');
        }
        return hex;
      }
      // Fallback: djb2 (non-cryptographic)
      let hash = 5381;
      for (let i = 0; i < data.length; i++) {
        hash = ((hash << 5) + hash) + data.charCodeAt(i);
        hash = hash >>> 0;
      }
      return hash.toString(16).padStart(8, '0');
    },
  };
}

export function createHmac(algorithm, key) {
  let data = '';
  return {
    update(input) {
      data += String(input ?? '');
      return this;
    },
    digest(encoding) {
      if (typeof globalThis.__pi_crypto_hmac_native === 'function') {
        const hex = globalThis.__pi_crypto_hmac_native(algorithm, String(key), data, 'hex');
        if (!encoding) return hexToBuffer(hex);
        if (encoding === 'hex') return hex;
        if (encoding === 'base64') {
          return globalThis.__pi_crypto_hmac_native(algorithm, String(key), data, 'base64');
        }
        return hex;
      }
      throw new Error('HMAC not available: crypto hostcalls not registered');
    },
  };
}

export function randomBytes(size) {
  if (typeof globalThis.__pi_crypto_random_bytes_native === 'function') {
    const arr = new Uint8Array(globalThis.__pi_crypto_random_bytes_native(size));
    const hex = bufToHex(arr);
    arr.toString = function(enc) {
      if (enc === 'hex') return hex;
      if (enc === 'base64') return globalThis.btoa(String.fromCharCode(...this));
      return new TextDecoder().decode(this);
    };
    return arr;
  }
  const arr = new Uint8Array(size);
  for (let i = 0; i < size; i++) arr[i] = Math.floor(Math.random() * 256);
  return arr;
}

export function randomInt(min, max) {
  if (max === undefined) { max = min; min = 0; }
  if (typeof globalThis.__pi_crypto_random_int_native === 'function') {
    return globalThis.__pi_crypto_random_int_native(min, max);
  }
  return min + Math.floor(Math.random() * (max - min));
}

export function timingSafeEqual(a, b) {
  if (typeof globalThis.__pi_crypto_timing_safe_equal_native === 'function') {
    return globalThis.__pi_crypto_timing_safe_equal_native(bufToHex(a), bufToHex(b));
  }
  if (a.length !== b.length) throw new Error('Input buffers must have the same byte length');
  let result = 0;
  for (let i = 0; i < a.length; i++) result |= a[i] ^ b[i];
  return result === 0;
}

export function getHashes() {
  return ['md5', 'sha1', 'sha256', 'sha512'];
}

export default {
  randomUUID, createHash, createHmac, randomBytes,
  randomInt, timingSafeEqual, getHashes,
};
";

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    // ─── hex_lower tests ─────────────────────────────────────────────────

    #[test]
    fn hex_lower_empty() {
        assert_eq!(hex_lower(&[]), "");
    }

    #[test]
    fn hex_lower_single_byte() {
        assert_eq!(hex_lower(&[0x00]), "00");
        assert_eq!(hex_lower(&[0xff]), "ff");
        assert_eq!(hex_lower(&[0xab]), "ab");
    }

    #[test]
    fn hex_lower_known_bytes() {
        assert_eq!(hex_lower(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[test]
    fn hex_lower_all_digits() {
        // Cover all hex chars: 0-9, a-f.
        assert_eq!(
            hex_lower(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]),
            "0123456789abcdef"
        );
    }

    // ─── hex_decode tests ────────────────────────────────────────────────

    #[test]
    fn hex_decode_empty() {
        assert_eq!(hex_decode(""), Vec::<u8>::new());
    }

    #[test]
    fn hex_decode_valid() {
        assert_eq!(hex_decode("deadbeef"), vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn hex_decode_uppercase() {
        assert_eq!(hex_decode("DEADBEEF"), vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn hex_decode_odd_length_drops_trailing() {
        // Odd length: last char has no pair, so it's ignored.
        assert_eq!(hex_decode("abc"), vec![0xab]);
    }

    #[test]
    fn hex_decode_invalid_chars_skipped() {
        // "gg" is not valid hex; the pair is skipped.
        assert_eq!(hex_decode("ffggaa"), vec![0xff, 0xaa]);
    }

    #[test]
    fn hex_decode_roundtrip() {
        let original = vec![0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let encoded = hex_lower(&original);
        assert_eq!(hex_decode(&encoded), original);
    }

    // ─── encode_output tests ─────────────────────────────────────────────

    #[test]
    fn encode_output_hex() {
        let bytes = [0xde, 0xad, 0xbe, 0xef];
        assert_eq!(encode_output(&bytes, "hex"), "deadbeef");
    }

    #[test]
    fn encode_output_base64() {
        let bytes = b"hello";
        assert_eq!(encode_output(bytes, "base64"), "aGVsbG8=");
    }

    #[test]
    fn encode_output_unknown_falls_back_to_hex() {
        let bytes = [0xff];
        assert_eq!(encode_output(&bytes, "unknown"), "ff");
    }

    // ─── random_bytes tests ──────────────────────────────────────────────

    #[test]
    fn random_bytes_correct_length() {
        for len in [0, 1, 4, 16, 32, 64, 100] {
            let bytes = random_bytes(len);
            assert_eq!(
                bytes.len(),
                len,
                "random_bytes({len}) should return {len} bytes"
            );
        }
    }

    #[test]
    fn random_bytes_two_calls_differ() {
        let a = random_bytes(32);
        let b = random_bytes(32);
        // Probability of collision is astronomically low.
        assert_ne!(a, b, "two random_bytes(32) calls should differ");
    }

    // ─── SHA-256 known-answer tests ──────────────────────────────────────

    #[test]
    fn sha256_hello() {
        let mut h = Sha256::new();
        h.update(b"hello");
        let result = hex_lower(&h.finalize());
        assert_eq!(
            result,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn sha256_empty() {
        let mut h = Sha256::new();
        h.update(b"");
        let result = hex_lower(&h.finalize());
        assert_eq!(
            result,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // ─── SHA-512 known-answer test ───────────────────────────────────────

    #[test]
    fn sha512_hello() {
        let mut h = sha2::Sha512::new();
        h.update(b"hello");
        let result = hex_lower(&h.finalize());
        assert_eq!(
            result,
            "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca72323c3d99ba5c11d7c7acc6e14b8c5da0c4663475c2e5c3adef46f73bcdec043"
        );
    }

    // ─── SHA-1 known-answer test ─────────────────────────────────────────

    #[test]
    fn sha1_hello() {
        let mut h = sha1::Sha1::new();
        h.update(b"hello");
        let result = hex_lower(&h.finalize());
        assert_eq!(result, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
    }

    // ─── MD5 known-answer test ───────────────────────────────────────────

    #[test]
    fn md5_hello() {
        let mut h = md5::Md5::new();
        h.update(b"hello");
        let result = hex_lower(&h.finalize());
        assert_eq!(result, "5d41402abc4b2a76b9719d911017c592");
    }

    // ─── HMAC-SHA256 known-answer test ───────────────────────────────────

    #[test]
    fn hmac_sha256_secret_hello() {
        use hmac::Mac;
        let mut mac = hmac::Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(b"hello");
        let result = hex_lower(&mac.finalize().into_bytes());
        assert_eq!(
            result,
            "88aab3ede8d3adf94d26ab90d3bafd4a2083070c3bcce9c014ee04a443847c0b"
        );
    }

    // ─── HMAC-SHA1 known-answer test ─────────────────────────────────────

    #[test]
    fn hmac_sha1_key_data() {
        use hmac::Mac;
        let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(b"key").unwrap();
        mac.update(b"data");
        let result = hex_lower(&mac.finalize().into_bytes());
        assert_eq!(result, "104152c5bfdca07bc633eebd46199f0255c9f49d");
    }

    // ─── UUID v4 format test ─────────────────────────────────────────────

    #[test]
    fn uuid_v4_format() {
        let id = Uuid::new_v4().to_string();
        let re = regex::Regex::new(
            r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$",
        )
        .unwrap();
        assert!(re.is_match(&id), "UUID should be v4 format: {id}");
    }

    #[test]
    fn uuid_v4_uniqueness() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_ne!(a, b);
    }

    // ─── Timing-safe comparison tests ────────────────────────────────────

    #[test]
    fn timing_safe_equal_same_bytes() {
        let a = hex_decode("01020304");
        let b = hex_decode("01020304");
        let mut result = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            result |= x ^ y;
        }
        assert_eq!(result, 0);
    }

    #[test]
    fn timing_safe_different_bytes() {
        let a = hex_decode("01020304");
        let b = hex_decode("01020305");
        let mut result = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            result |= x ^ y;
        }
        assert_ne!(result, 0);
    }

    // ─── encode_output base64 known-answer ───────────────────────────────

    #[test]
    fn encode_sha256_hello_base64() {
        let mut h = Sha256::new();
        h.update(b"hello");
        let result = encode_output(&h.finalize(), "base64");
        assert_eq!(result, "LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=");
    }

    // ─── NODE_CRYPTO_JS constant is non-empty ────────────────────────────

    #[test]
    fn node_crypto_js_has_content() {
        assert!(!NODE_CRYPTO_JS.is_empty());
        assert!(NODE_CRYPTO_JS.contains("createHash"));
        assert!(NODE_CRYPTO_JS.contains("createHmac"));
        assert!(NODE_CRYPTO_JS.contains("randomUUID"));
        assert!(NODE_CRYPTO_JS.contains("randomBytes"));
        assert!(NODE_CRYPTO_JS.contains("timingSafeEqual"));
        assert!(NODE_CRYPTO_JS.contains("getHashes"));
    }
}
