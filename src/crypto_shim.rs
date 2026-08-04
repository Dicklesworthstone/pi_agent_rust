//! Node.js `crypto` shim — Rust hostcalls for the QuickJS extension runtime.
//!
//! Registers native functions on the QuickJS global object that provide real
//! cryptographic operations (SHA-256, SHA-384, SHA-512, SHA-1, MD5, HMAC, random bytes,
//! UUID generation, Ed25519 signing/verification, constant-time comparison) to the `node:crypto`
//! JS module.

use pbkdf2::pbkdf2_hmac;
use ring::{
    aead::{AES_128_GCM, AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey},
    signature::{Ed25519KeyPair, UnparsedPublicKey},
};
use rquickjs::prelude::Func;
use scrypt::{Params as ScryptParams, scrypt};
use sha2::{Digest, Sha256, Sha384};
use uuid::Builder;

const KDF_MAX_OUTPUT_BYTES: usize = 1_048_576; // 1MB
const KDF_MAX_PBKDF2_ITERATIONS: u32 = 1_000_000;
const KDF_MAX_SCRYPT_LOG_N: u8 = 20; // N <= 2^20
const KDF_MAX_SCRYPT_R: u32 = 16;
const KDF_MAX_SCRYPT_P: u32 = 16;
const KDF_MAX_SCRYPT_MEM_BYTES: usize = 32 * 1024 * 1024; // 32MB

/// Register all crypto hostcalls on the QuickJS global object.
///
/// Call this during runtime initialization, after `ctx.globals()` is available.
pub fn register_crypto_hostcalls(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    register_hash_hostcall(global)?;
    register_hmac_hostcall(global)?;
    register_uuid_hostcall(global)?;
    register_random_int_hostcall(global)?;
    register_random_bytes_hostcall(global)?;
    register_timing_safe_equal_hostcall(global)?;
    register_pbkdf2_hostcall(global)?;
    register_scrypt_hostcall(global)?;
    register_aes_gcm_hostcalls(global)?;
    register_ed25519_hostcalls(global)?;
    Ok(())
}

fn register_hash_hostcall(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    // __pi_crypto_hash_native(algorithm, data, encoding) -> digest string
    global.set(
        "__pi_crypto_hash_native",
        Func::from(
            |algorithm: String,
             data: rquickjs::TypedArray<'_, u8>,
             encoding: String|
             -> rquickjs::Result<String> {
                let bytes = data
                    .as_bytes()
                    .ok_or_else(|| rquickjs::Error::new_from_js("buffer", "Detached buffer"))?;
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
                    "sha384" => {
                        let mut h = Sha384::new();
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
             key: rquickjs::TypedArray<'_, u8>,
             data: rquickjs::TypedArray<'_, u8>,
             encoding: String|
             -> rquickjs::Result<String> {
                use hmac::Mac;
                let key_bytes = key
                    .as_bytes()
                    .ok_or_else(|| rquickjs::Error::new_from_js("buffer", "Detached key buffer"))?;
                let data_bytes = data.as_bytes().ok_or_else(|| {
                    rquickjs::Error::new_from_js("buffer", "Detached data buffer")
                })?;
                let hash_bytes = match algorithm.as_str() {
                    "sha256" => {
                        let mut mac =
                            hmac::Hmac::<Sha256>::new_from_slice(key_bytes).map_err(|_| {
                                rquickjs::Error::new_from_js("key", "invalid HMAC key length")
                            })?;
                        mac.update(data_bytes);
                        mac.finalize().into_bytes().to_vec()
                    }
                    "sha512" => {
                        let mut mac = hmac::Hmac::<sha2::Sha512>::new_from_slice(key_bytes)
                            .map_err(|_| {
                                rquickjs::Error::new_from_js("key", "invalid HMAC key length")
                            })?;
                        mac.update(data_bytes);
                        mac.finalize().into_bytes().to_vec()
                    }
                    "sha384" => {
                        let mut mac =
                            hmac::Hmac::<Sha384>::new_from_slice(key_bytes).map_err(|_| {
                                rquickjs::Error::new_from_js("key", "invalid HMAC key length")
                            })?;
                        mac.update(data_bytes);
                        mac.finalize().into_bytes().to_vec()
                    }
                    "sha1" => {
                        let mut mac =
                            hmac::Hmac::<sha1::Sha1>::new_from_slice(key_bytes).map_err(|_| {
                                rquickjs::Error::new_from_js("key", "invalid HMAC key length")
                            })?;
                        mac.update(data_bytes);
                        mac.finalize().into_bytes().to_vec()
                    }
                    "md5" => {
                        let mut mac =
                            hmac::Hmac::<md5::Md5>::new_from_slice(key_bytes).map_err(|_| {
                                rquickjs::Error::new_from_js("key", "invalid HMAC key length")
                            })?;
                        mac.update(data_bytes);
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
        Func::from(|| -> rquickjs::Result<String> {
            random_uuid().map_err(|err| map_entropy_error("randomUUID", err))
        }),
    )
}

fn register_random_int_hostcall(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    // __pi_crypto_random_int_native(min, max) -> integer in [min, max)
    global.set(
        "__pi_crypto_random_int_native",
        Func::from(|min: f64, max: f64| -> rquickjs::Result<f64> {
            // Guard against NaN/Inf inputs - critical security vulnerability
            if !min.is_finite() || !max.is_finite() {
                return Err(rquickjs::Error::new_from_js(
                    "number",
                    "min and max must be finite numbers",
                ));
            }
            if min >= max {
                return Err(rquickjs::Error::new_from_js(
                    "number",
                    "min must be less than max",
                ));
            }
            let range = max - min;
            let rand_bytes = random_bytes(8).map_err(|err| map_entropy_error("randomInt", err))?;
            let mut random_window = [0_u8; 8];
            random_window.copy_from_slice(&rand_bytes);
            // 53 bits of randomness (max safe integer precision in JS)
            let random = u64::from_le_bytes(random_window) >> 11;
            #[allow(clippy::cast_precision_loss)]
            let normalized = (random as f64) / ((1u64 << 53) as f64);
            Ok(min + (normalized * range).floor())
        }),
    )
}

fn register_random_bytes_hostcall(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    // __pi_crypto_random_bytes_native(size) -> hex string of random bytes
    global.set(
        "__pi_crypto_random_bytes_native",
        Func::from(|size: usize| -> rquickjs::Result<String> {
            if size > 10 * 1024 * 1024 {
                return Err(rquickjs::Error::new_from_js(
                    "number",
                    "randomBytes size limit exceeded (max 10MB)",
                ));
            }
            let bytes = random_bytes(size).map_err(|err| map_entropy_error("randomBytes", err))?;
            Ok(hex_lower(&bytes))
        }),
    )
}

fn register_timing_safe_equal_hostcall(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    // __pi_crypto_timing_safe_equal_native(a, b) -> bool
    global.set(
        "__pi_crypto_timing_safe_equal_native",
        Func::from(
            |a: rquickjs::TypedArray<'_, u8>,
             b: rquickjs::TypedArray<'_, u8>|
             -> rquickjs::Result<bool> {
                let a_bytes = a
                    .as_bytes()
                    .ok_or_else(|| rquickjs::Error::new_from_js("buffer", "Detached buffer"))?;
                let b_bytes = b
                    .as_bytes()
                    .ok_or_else(|| rquickjs::Error::new_from_js("buffer", "Detached buffer"))?;
                if a_bytes.len() != b_bytes.len() {
                    return Err(rquickjs::Error::new_from_js(
                        "buffer",
                        "Input buffers must have the same byte length",
                    ));
                }
                let mut result = 0u8;
                for (x, y) in a_bytes.iter().zip(b_bytes.iter()) {
                    result |= x ^ y;
                }
                Ok(result == 0)
            },
        ),
    )
}

fn register_pbkdf2_hostcall(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    // __pi_crypto_pbkdf2_native(password, salt, iterations, keylen, digest, encoding) -> digest string
    global.set(
        "__pi_crypto_pbkdf2_native",
        Func::from(
            |password: rquickjs::TypedArray<'_, u8>,
             salt: rquickjs::TypedArray<'_, u8>,
             iterations: u32,
             keylen: usize,
             digest: String,
             encoding: String|
             -> rquickjs::Result<String> {
                if iterations == 0 {
                    return Err(rquickjs::Error::new_from_js(
                        "number",
                        "pbkdf2 iterations must be positive",
                    ));
                }
                if keylen == 0 {
                    return Err(rquickjs::Error::new_from_js(
                        "number",
                        "pbkdf2 keylen must be positive",
                    ));
                }
                if keylen > KDF_MAX_OUTPUT_BYTES {
                    let msg =
                        format!("pbkdf2 keylen exceeds maximum ({KDF_MAX_OUTPUT_BYTES} bytes)");
                    return Err(rquickjs::Error::new_into_js_message(
                        "number", "pbkdf2", msg,
                    ));
                }
                if iterations > KDF_MAX_PBKDF2_ITERATIONS {
                    let msg =
                        format!("pbkdf2 iterations exceeds maximum ({KDF_MAX_PBKDF2_ITERATIONS})");
                    return Err(rquickjs::Error::new_into_js_message(
                        "number", "pbkdf2", msg,
                    ));
                }
                let password_bytes = password
                    .as_bytes()
                    .ok_or_else(|| rquickjs::Error::new_from_js("buffer", "Detached buffer"))?;
                let salt_bytes = salt
                    .as_bytes()
                    .ok_or_else(|| rquickjs::Error::new_from_js("buffer", "Detached buffer"))?;
                let mut out = vec![0u8; keylen];
                let () = match digest.as_str() {
                    "sha256" => {
                        pbkdf2_hmac::<Sha256>(password_bytes, salt_bytes, iterations, &mut out);
                    }
                    "sha512" => {
                        pbkdf2_hmac::<sha2::Sha512>(
                            password_bytes,
                            salt_bytes,
                            iterations,
                            &mut out,
                        );
                    }
                    "sha1" => {
                        pbkdf2_hmac::<sha1::Sha1>(password_bytes, salt_bytes, iterations, &mut out);
                    }
                    "md5" => {
                        pbkdf2_hmac::<md5::Md5>(password_bytes, salt_bytes, iterations, &mut out);
                    }
                    _ => {
                        return Err(rquickjs::Error::new_from_js(
                            "string",
                            "unsupported pbkdf2 digest",
                        ));
                    }
                };
                Ok(encode_output(&out, &encoding))
            },
        ),
    )
}

fn register_scrypt_hostcall(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    // __pi_crypto_scrypt_native(password, salt, keylen, log_n, r, p, encoding) -> digest string
    global.set(
        "__pi_crypto_scrypt_native",
        Func::from(
            |password: rquickjs::TypedArray<'_, u8>,
             salt: rquickjs::TypedArray<'_, u8>,
             keylen: usize,
             log_n: u8,
             r: u32,
             p: u32,
             encoding: String|
             -> rquickjs::Result<String> {
                if keylen == 0 {
                    return Err(rquickjs::Error::new_from_js(
                        "number",
                        "scrypt keylen must be positive",
                    ));
                }
                if keylen > KDF_MAX_OUTPUT_BYTES {
                    let msg = format!(
                        "scrypt keylen exceeds maximum ({KDF_MAX_OUTPUT_BYTES} bytes)"
                    );
                    return Err(rquickjs::Error::new_into_js_message(
                        "number",
                        "scrypt",
                        msg,
                    ));
                }
                if log_n > KDF_MAX_SCRYPT_LOG_N {
                    let msg = format!(
                        "scrypt N exceeds maximum (2^{KDF_MAX_SCRYPT_LOG_N})"
                    );
                    return Err(rquickjs::Error::new_into_js_message(
                        "number",
                        "scrypt",
                        msg,
                    ));
                }
                if r == 0 || p == 0 {
                    return Err(rquickjs::Error::new_from_js(
                        "number",
                        "scrypt r/p must be positive",
                    ));
                }
                if r > KDF_MAX_SCRYPT_R || p > KDF_MAX_SCRYPT_P {
                    let msg = format!(
                        "scrypt r/p exceeds maximum (r<= {KDF_MAX_SCRYPT_R}, p<= {KDF_MAX_SCRYPT_P})"
                    );
                    return Err(rquickjs::Error::new_into_js_message(
                        "number",
                        "scrypt",
                        msg,
                    ));
                }
                let n = 1usize
                    .checked_shl(u32::from(log_n))
                    .ok_or_else(|| rquickjs::Error::new_from_js("number", "invalid scrypt N"))?;
                let mem_bytes = 128usize
                    .checked_mul(r as usize)
                    .and_then(|value| value.checked_mul(n))
                    .and_then(|value| value.checked_mul(p as usize))
                    .ok_or_else(|| {
                        rquickjs::Error::new_from_js("number", "scrypt memory size overflow")
                    })?;
                if mem_bytes > KDF_MAX_SCRYPT_MEM_BYTES {
                    let msg = format!(
                        "scrypt parameters exceed memory limit ({KDF_MAX_SCRYPT_MEM_BYTES} bytes)"
                    );
                    return Err(rquickjs::Error::new_into_js_message(
                        "number",
                        "scrypt",
                        msg,
                    ));
                }
                let password_bytes = password
                    .as_bytes()
                    .ok_or_else(|| rquickjs::Error::new_from_js("buffer", "Detached buffer"))?;
                let salt_bytes = salt
                    .as_bytes()
                    .ok_or_else(|| rquickjs::Error::new_from_js("buffer", "Detached buffer"))?;
                let params = ScryptParams::new(log_n, r, p, keylen).map_err(|_| {
                    rquickjs::Error::new_from_js("number", "invalid scrypt params")
                })?;
                let mut out = vec![0u8; keylen];
                scrypt(password_bytes, salt_bytes, &params, &mut out).map_err(|_| {
                    rquickjs::Error::new_from_js("crypto", "scrypt derivation failed")
                })?;
                Ok(encode_output(&out, &encoding))
            },
        ),
    )
}

fn register_aes_gcm_hostcalls(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    global.set(
        "__pi_crypto_aes_gcm_encrypt_native",
        Func::from(
            |algorithm: String,
             key: rquickjs::TypedArray<'_, u8>,
             iv: rquickjs::TypedArray<'_, u8>,
             aad: rquickjs::TypedArray<'_, u8>,
             plaintext: rquickjs::TypedArray<'_, u8>,
             encoding: String|
             -> rquickjs::Result<String> {
                let key_bytes = key
                    .as_bytes()
                    .ok_or_else(|| rquickjs::Error::new_from_js("buffer", "Detached key buffer"))?;
                let iv_bytes = iv
                    .as_bytes()
                    .ok_or_else(|| rquickjs::Error::new_from_js("buffer", "Detached IV buffer"))?;
                let aad_bytes = aad
                    .as_bytes()
                    .ok_or_else(|| rquickjs::Error::new_from_js("buffer", "Detached AAD buffer"))?;
                let plaintext_bytes = plaintext.as_bytes().ok_or_else(|| {
                    rquickjs::Error::new_from_js("buffer", "Detached plaintext buffer")
                })?;
                let cipher = aes_gcm_key(&algorithm, key_bytes)?;
                let nonce = Nonce::try_assume_unique_for_key(iv_bytes).map_err(|_| {
                    rquickjs::Error::new_from_js("buffer", "AES-GCM IV must be exactly 12 bytes")
                })?;
                let mut out = plaintext_bytes.to_vec();
                let tag = cipher
                    .seal_in_place_separate_tag(nonce, Aad::from(aad_bytes), &mut out)
                    .map_err(|_| {
                        rquickjs::Error::new_from_js("crypto", "AES-GCM encryption failed")
                    })?;
                out.extend_from_slice(tag.as_ref());
                Ok(encode_output(&out, &encoding))
            },
        ),
    )?;

    global.set(
        "__pi_crypto_aes_gcm_decrypt_native",
        Func::from(
            |algorithm: String,
             key: rquickjs::TypedArray<'_, u8>,
             iv: rquickjs::TypedArray<'_, u8>,
             aad: rquickjs::TypedArray<'_, u8>,
             ciphertext: rquickjs::TypedArray<'_, u8>,
             auth_tag: rquickjs::TypedArray<'_, u8>,
             encoding: String|
             -> rquickjs::Result<String> {
                let key_bytes = key
                    .as_bytes()
                    .ok_or_else(|| rquickjs::Error::new_from_js("buffer", "Detached key buffer"))?;
                let iv_bytes = iv
                    .as_bytes()
                    .ok_or_else(|| rquickjs::Error::new_from_js("buffer", "Detached IV buffer"))?;
                let aad_bytes = aad
                    .as_bytes()
                    .ok_or_else(|| rquickjs::Error::new_from_js("buffer", "Detached AAD buffer"))?;
                let ciphertext_bytes = ciphertext.as_bytes().ok_or_else(|| {
                    rquickjs::Error::new_from_js("buffer", "Detached ciphertext buffer")
                })?;
                let tag_bytes = auth_tag.as_bytes().ok_or_else(|| {
                    rquickjs::Error::new_from_js("buffer", "Detached auth tag buffer")
                })?;
                if tag_bytes.len() != 16 {
                    return Err(rquickjs::Error::new_from_js(
                        "buffer",
                        "AES-GCM auth tag must be exactly 16 bytes",
                    ));
                }
                let cipher = aes_gcm_key(&algorithm, key_bytes)?;
                let nonce = Nonce::try_assume_unique_for_key(iv_bytes).map_err(|_| {
                    rquickjs::Error::new_from_js("buffer", "AES-GCM IV must be exactly 12 bytes")
                })?;
                let mut in_out = ciphertext_bytes.to_vec();
                in_out.extend_from_slice(tag_bytes);
                let plaintext = cipher
                    .open_in_place(nonce, Aad::from(aad_bytes), &mut in_out)
                    .map_err(|_| {
                        rquickjs::Error::new_from_js("crypto", "AES-GCM authentication failed")
                    })?;
                Ok(encode_output(plaintext, &encoding))
            },
        ),
    )
}

fn aes_gcm_key(algorithm: &str, key_bytes: &[u8]) -> rquickjs::Result<LessSafeKey> {
    let algorithm = match algorithm {
        "aes-128-gcm" => {
            if key_bytes.len() != 16 {
                return Err(rquickjs::Error::new_from_js(
                    "key",
                    "aes-128-gcm key must be exactly 16 bytes",
                ));
            }
            &AES_128_GCM
        }
        "aes-256-gcm" => {
            if key_bytes.len() != 32 {
                return Err(rquickjs::Error::new_from_js(
                    "key",
                    "aes-256-gcm key must be exactly 32 bytes",
                ));
            }
            &AES_256_GCM
        }
        _ => {
            return Err(rquickjs::Error::new_from_js(
                "string",
                "unsupported cipher algorithm",
            ));
        }
    };
    let unbound = UnboundKey::new(algorithm, key_bytes)
        .map_err(|_| rquickjs::Error::new_from_js("key", "invalid AES-GCM key"))?;
    Ok(LessSafeKey::new(unbound))
}

fn register_ed25519_hostcalls(global: &rquickjs::Object<'_>) -> rquickjs::Result<()> {
    // __pi_crypto_ed25519_sign_native(pkcs8_private_key, data, encoding) -> signature string
    global.set(
        "__pi_crypto_ed25519_sign_native",
        Func::from(
            |private_key: rquickjs::TypedArray<'_, u8>,
             data: rquickjs::TypedArray<'_, u8>,
             encoding: String|
             -> rquickjs::Result<String> {
                let private_key_bytes = private_key.as_bytes().ok_or_else(|| {
                    rquickjs::Error::new_from_js("buffer", "Detached private key buffer")
                })?;
                let data_bytes = data.as_bytes().ok_or_else(|| {
                    rquickjs::Error::new_from_js("buffer", "Detached data buffer")
                })?;
                let key_pair = Ed25519KeyPair::from_pkcs8(private_key_bytes)
                    .or_else(|_| Ed25519KeyPair::from_pkcs8_maybe_unchecked(private_key_bytes))
                    .map_err(|_| {
                        rquickjs::Error::new_from_js("key", "invalid Ed25519 PKCS#8 private key")
                    })?;
                let signature = key_pair.sign(data_bytes);
                Ok(encode_output(signature.as_ref(), &encoding))
            },
        ),
    )?;

    // __pi_crypto_ed25519_verify_native(spki_public_key, data, signature) -> bool
    global.set(
        "__pi_crypto_ed25519_verify_native",
        Func::from(
            |public_key: rquickjs::TypedArray<'_, u8>,
             data: rquickjs::TypedArray<'_, u8>,
             signature: rquickjs::TypedArray<'_, u8>|
             -> rquickjs::Result<bool> {
                let public_key_bytes = public_key.as_bytes().ok_or_else(|| {
                    rquickjs::Error::new_from_js("buffer", "Detached public key buffer")
                })?;
                let data_bytes = data.as_bytes().ok_or_else(|| {
                    rquickjs::Error::new_from_js("buffer", "Detached data buffer")
                })?;
                let signature_bytes = signature.as_bytes().ok_or_else(|| {
                    rquickjs::Error::new_from_js("buffer", "Detached signature buffer")
                })?;
                let raw_public_key = ed25519_public_key_from_spki(public_key_bytes)?;
                let verifier = UnparsedPublicKey::new(&ring::signature::ED25519, raw_public_key);
                Ok(verifier.verify(data_bytes, signature_bytes).is_ok())
            },
        ),
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
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(char::from(
            HEX.get(usize::from(byte >> 4)).copied().unwrap_or(b'?'),
        ));
        output.push(char::from(
            HEX.get(usize::from(byte & 0x0f)).copied().unwrap_or(b'?'),
        ));
    }
    output
}

/// Decode a hex string to bytes, ignoring invalid chars.
fn hex_decode(hex: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let mut chars = hex.chars();
    while let (Some(hi), Some(lo)) = (chars.next(), chars.next()) {
        if let (Some(h), Some(l)) = (hi.to_digit(16), lo.to_digit(16))
            && let Ok(byte) = u8::try_from(h * 16 + l)
        {
            bytes.push(byte);
        }
    }
    bytes
}

fn ed25519_public_key_from_spki(der: &[u8]) -> rquickjs::Result<&[u8]> {
    const ED25519_SPKI_PREFIX: &[u8] = &[
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    if der.len() == 32 {
        return Ok(der);
    }
    if der.len() == ED25519_SPKI_PREFIX.len() + 32
        && der.starts_with(ED25519_SPKI_PREFIX)
        && let Some(raw) = der.get(ED25519_SPKI_PREFIX.len()..)
    {
        return Ok(raw);
    }
    Err(rquickjs::Error::new_from_js(
        "key",
        "invalid Ed25519 SPKI public key",
    ))
}

fn map_entropy_error(api: &'static str, err: getrandom::Error) -> rquickjs::Error {
    tracing::error!(
        event = "pijs.crypto.entropy_failure",
        api,
        error = %err,
        "OS randomness unavailable"
    );
    rquickjs::Error::new_into_js_message("crypto", api, format!("OS randomness unavailable: {err}"))
}

fn fill_random_bytes_with<F, E>(len: usize, mut fill: F) -> Result<Vec<u8>, E>
where
    F: FnMut(&mut [u8]) -> Result<(), E>,
{
    let mut out = vec![0u8; len];
    if len > 0 {
        fill(&mut out)?;
    }
    Ok(out)
}

/// Generate random bytes from the operating system RNG.
fn random_bytes(len: usize) -> Result<Vec<u8>, getrandom::Error> {
    fill_random_bytes_with(len, getrandom::fill)
}

fn random_uuid_with<F, E>(mut fill: F) -> Result<String, E>
where
    F: FnMut(&mut [u8]) -> Result<(), E>,
{
    let mut bytes = [0_u8; 16];
    fill(&mut bytes)?;
    Ok(Builder::from_random_bytes(bytes).into_uuid().to_string())
}

/// Generate a random UUID v4 from OS RNG bytes so entropy failures surface as errors.
fn random_uuid() -> Result<String, getrandom::Error> {
    random_uuid_with(getrandom::fill)
}

/// The JS source for the `node:crypto` virtual module.
pub const NODE_CRYPTO_JS: &str = r"
// Helper: convert hex string to Uint8Array with Buffer-like toString
function hexToBuffer(hex) {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return bufferFromBytes(bytes);
}

function bufferFromBytes(input) {
  const bytes = new Uint8Array(input.length);
  bytes.set(input);
  bytes.toString = function(enc) {
    const normalized = normalizeBufferEncoding(enc);
    if (normalized === 'hex') return bufToHex(this);
    if (normalized === 'base64') {
      let binary = '';
      let chunk = [];
      for (let i = 0; i < this.length; i++) {
        chunk.push(this[i]);
        if (chunk.length >= 4096) {
          binary += String.fromCharCode.apply(null, chunk);
          chunk.length = 0;
        }
      }
      if (chunk.length > 0) {
        binary += String.fromCharCode.apply(null, chunk);
      }
      return globalThis.btoa(binary);
    }
    if (normalized === 'latin1' || normalized === 'binary') return bytesToOneByteString(this, false);
    if (normalized === 'ascii') return bytesToOneByteString(this, true);
    return new TextDecoder().decode(this);
  };
  return bytes;
}

// Helper: Uint8Array to hex string
function bufToHex(buf) {
  return Array.from(buf).map(b => b.toString(16).padStart(2, '0')).join('');
}

function stringToOneByteBytes(input) {
  const out = new Uint8Array(input.length);
  for (let i = 0; i < input.length; i++) {
    out[i] = input.charCodeAt(i) & 0xff;
  }
  return bufferFromBytes(out);
}

function bytesToOneByteString(input, stripHighBit) {
  let output = '';
  let chunk = [];
  for (let i = 0; i < input.length; i++) {
    chunk.push(stripHighBit ? (input[i] & 0x7f) : input[i]);
    if (chunk.length >= 4096) {
      output += String.fromCharCode.apply(null, chunk);
      chunk.length = 0;
    }
  }
  if (chunk.length > 0) {
    output += String.fromCharCode.apply(null, chunk);
  }
  return output;
}

function requireCryptoHostcall(hostcallName, apiName) {
  const hostcall = globalThis[hostcallName];
  if (typeof hostcall !== 'function') {
    throw new Error(`${apiName} not available: crypto hostcalls not registered`);
  }
  return hostcall;
}

function combineChunks(chunks) {
  const totalLen = chunks.reduce((acc, c) => acc + c.length, 0);
  const combined = new Uint8Array(totalLen);
  let offset = 0;
  for (const chunk of chunks) {
    combined.set(chunk, offset);
    offset += chunk.length;
  }
  return combined;
}

function toUint8Array(input, encoding) {
  if (input instanceof Uint8Array) return input;
  if (typeof input === 'string') {
    const enc = normalizeBufferEncoding(encoding);
    if (enc === 'hex') {
      if (input.length % 2 !== 0 || /[^0-9a-f]/i.test(input)) {
        throw new Error('invalid hex input');
      }
      return hexToBuffer(input);
    }
    if (enc === 'base64') return base64ToBytes(input);
    if (enc === 'latin1' || enc === 'binary' || enc === 'ascii') {
      return stringToOneByteBytes(input);
    }
    return new TextEncoder().encode(input);
  }
  return new TextEncoder().encode(String(input ?? ''));
}

function normalizeBufferEncoding(encoding) {
  if (encoding === undefined || encoding === null) return 'utf8';
  const enc = String(encoding).toLowerCase();
  if (enc === 'utf8' || enc === 'utf-8') return 'utf8';
  if (enc === 'latin1' || enc === 'binary' || enc === 'ascii') return enc;
  if (enc === 'hex' || enc === 'base64') return enc;
  throw new Error(`unsupported input encoding '${encoding}'`);
}

function encodeOutput(bytes, encoding) {
  const out = bufferFromBytes(bytes);
  if (encoding === undefined || encoding === null) return out;
  const enc = normalizeBufferEncoding(encoding);
  if (enc === 'hex') return out.toString('hex');
  if (enc === 'base64') return out.toString('base64');
  if (enc === 'utf8') return out.toString('utf8');
  if (enc === 'latin1' || enc === 'binary' || enc === 'ascii') return out.toString(enc);
  throw new Error(`unsupported output encoding '${encoding}'`);
}

function base64ToBytes(input) {
  const binary = globalThis.atob(String(input));
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    out[i] = binary.charCodeAt(i);
  }
  return out;
}

function decodePemKey(input, label, apiName) {
  const text = String(input ?? '');
  const begin = `-----BEGIN ${label}-----`;
  const end = `-----END ${label}-----`;
  const start = text.indexOf(begin);
  const finish = text.indexOf(end);
  if (start < 0 || finish < 0 || finish <= start) {
    throw new Error(`${apiName}: Ed25519 ${label} PEM is required`);
  }
  const body = text
    .slice(start + begin.length, finish)
    .replace(/\s+/g, '');
  if (body.length === 0) {
    throw new Error(`${apiName}: empty Ed25519 ${label} PEM`);
  }
  return base64ToBytes(body);
}

function keyMaterialToDer(key, label, apiName) {
  let material = key;
  if (material && typeof material === 'object' && !(material instanceof Uint8Array)) {
    if (!Object.prototype.hasOwnProperty.call(material, 'key')) {
      throw new Error(`${apiName}: unsupported Ed25519 key object`);
    }
    if (material.format && material.format !== 'pem' && material.format !== 'der') {
      throw new Error(`${apiName}: unsupported Ed25519 key format '${material.format}'`);
    }
    material = material.key;
  }
  if (material instanceof Uint8Array) {
    return material;
  }
  if (typeof material === 'string') {
    return decodePemKey(material, label, apiName);
  }
  throw new Error(`${apiName}: Ed25519 ${label} key must be PEM text or DER bytes`);
}

function normalizeSignVerifyAlgorithm(algorithm, apiName) {
  if (algorithm === null || algorithm === undefined) {
    return 'ed25519';
  }
  const name = normalizeDigestName(algorithm);
  throw new Error(`${apiName}: unsupported algorithm '${name}'; only Ed25519 with null/undefined algorithm is supported`);
}

function normalizeDigestName(input) {
  if (input === undefined || input === null) return 'sha1';
  return String(input)
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]/g, '');
}

function unsupportedCryptoApi(name) {
  throw new Error(`${name} is not implemented in the Pi node:crypto shim`);
}

function normalizeCryptoOptionsEncoding(options, apiName) {
  if (options === undefined || options === null) return undefined;
  if (typeof options !== 'object') {
    throw new Error(`${apiName}: options must be an object`);
  }
  if (!Object.prototype.hasOwnProperty.call(options, 'encoding')) return undefined;
  return normalizeBufferEncoding(options.encoding);
}

function normalizeCipherAlgorithm(algorithm, apiName) {
  if (algorithm === undefined || algorithm === null || String(algorithm).trim() === '') {
    throw new Error(`${apiName}: algorithm is required`);
  }
  return String(algorithm).trim().toLowerCase();
}

function validateAesGcmParams(algorithm, key, iv, apiName) {
  const algo = normalizeCipherAlgorithm(algorithm, apiName);
  if (algo !== 'aes-128-gcm' && algo !== 'aes-256-gcm') {
    throw new Error(`${apiName}: unsupported cipher algorithm '${algo}'`);
  }
  const keyBuf = toUint8Array(key);
  const expectedKeyLen = algo === 'aes-128-gcm' ? 16 : 32;
  if (keyBuf.length !== expectedKeyLen) {
    throw new Error(`${apiName}: ${algo} key must be exactly ${expectedKeyLen} bytes`);
  }
  const ivBuf = toUint8Array(iv);
  if (ivBuf.length !== 12) {
    throw new Error(`${apiName}: AES-GCM IV must be exactly 12 bytes`);
  }
  return { algo, keyBuf, ivBuf };
}

export function randomUUID() {
  const randomUuidNative = requireCryptoHostcall(
    '__pi_crypto_random_uuid_native',
    'randomUUID',
  );
  return randomUuidNative();
}

export function createHash(algorithm) {
  if (algorithm === undefined || algorithm === null || String(algorithm).trim() === '') {
    throw new Error('createHash: algorithm is required');
  }
  const algo = normalizeDigestName(algorithm);
  const chunks = [];
  let finalized = false;
  return {
    update(input, inputEncoding) {
      if (finalized) {
        throw new Error('Hash.digest() already called');
      }
      chunks.push(toUint8Array(input, inputEncoding));
      return this;
    },
    digest(encoding) {
      if (finalized) {
        throw new Error('Hash.digest() already called');
      }
      finalized = true;
      const hashNative = requireCryptoHostcall('__pi_crypto_hash_native', 'createHash');
      const data = combineChunks(chunks);
      const hex = hashNative(algo, data, 'hex');
      if (!encoding) return hexToBuffer(hex);
      if (encoding === 'hex') return hex;
      if (encoding === 'base64') {
        const buf = hexToBuffer(hex);
        return globalThis.btoa(String.fromCharCode(...buf));
      }
      throw new Error(`createHash.digest: unsupported encoding '${encoding}'`);
    },
  };
}

export function createHmac(algorithm, key, options) {
  if (algorithm === undefined || algorithm === null || String(algorithm).trim() === '') {
    throw new Error('createHmac: algorithm is required');
  }
  const algo = normalizeDigestName(algorithm);
  const chunks = [];
  const keyBuf = toUint8Array(key, normalizeCryptoOptionsEncoding(options, 'createHmac'));
  let finalized = false;
  return {
    update(input, inputEncoding) {
      if (finalized) {
        throw new Error('Hmac.digest() already called');
      }
      chunks.push(toUint8Array(input, inputEncoding));
      return this;
    },
    digest(encoding) {
      if (finalized) {
        throw new Error('Hmac.digest() already called');
      }
      finalized = true;
      const hmacNative = requireCryptoHostcall('__pi_crypto_hmac_native', 'createHmac');
      const data = combineChunks(chunks);
      const hex = hmacNative(algo, keyBuf, data, 'hex');
      if (!encoding) return hexToBuffer(hex);
      if (encoding === 'hex') return hex;
      if (encoding === 'base64') {
        const buf = hexToBuffer(hex);
        return globalThis.btoa(String.fromCharCode(...buf));
      }
      throw new Error(`createHmac.digest: unsupported encoding '${encoding}'`);
    },
  };
}

export function randomBytes(size) {
  if (!Number.isSafeInteger(size) || size < 0) {
    throw new Error('randomBytes: size must be a non-negative integer');
  }
  const randomBytesNative = requireCryptoHostcall(
    '__pi_crypto_random_bytes_native',
    'randomBytes',
  );
  return hexToBuffer(randomBytesNative(size));
}

export function randomInt(min, max) {
  if (max === undefined) { max = min; min = 0; }
  if (!Number.isSafeInteger(min) || !Number.isSafeInteger(max)) {
    throw new Error('randomInt: min/max must be safe integers');
  }
  if (min >= max) {
    throw new Error('randomInt: min must be less than max');
  }
  const randomIntNative = requireCryptoHostcall(
    '__pi_crypto_random_int_native',
    'randomInt',
  );
  return randomIntNative(min, max);
}

export function timingSafeEqual(a, b) {
  if (typeof globalThis.__pi_crypto_timing_safe_equal_native === 'function') {
    return globalThis.__pi_crypto_timing_safe_equal_native(a, b);
  }
  if (a.length !== b.length) throw new Error('Input buffers must have the same byte length');
  let result = 0;
  for (let i = 0; i < a.length; i++) result |= a[i] ^ b[i];
  return result === 0;
}


export function getHashes() {
  return ['md5', 'sha1', 'sha256', 'sha384', 'sha512'];
}

export function pbkdf2Sync(password, salt, iterations, keylen, digest) {
  const algo = normalizeDigestName(digest);
  if (!Number.isSafeInteger(iterations) || iterations <= 0) {
    throw new Error('pbkdf2Sync: iterations must be a positive integer');
  }
  if (!Number.isSafeInteger(keylen) || keylen <= 0) {
    throw new Error('pbkdf2Sync: keylen must be a positive integer');
  }
  if (iterations > 1000000) {
    throw new Error('pbkdf2Sync: iterations must be <= 1000000');
  }
  if (keylen > 1048576) {
    throw new Error('pbkdf2Sync: keylen must be <= 1048576');
  }
  const pbkdf2Native = requireCryptoHostcall(
    '__pi_crypto_pbkdf2_native',
    'pbkdf2Sync',
  );
  const hex = pbkdf2Native(
    toUint8Array(password),
    toUint8Array(salt),
    iterations,
    keylen,
    algo,
    'hex',
  );
  return hexToBuffer(hex);
}

export function pbkdf2(password, salt, iterations, keylen, digest, callback) {
  if (typeof digest === 'function') {
    callback = digest;
    digest = undefined;
  }
  if (typeof callback !== 'function') {
    throw new Error('pbkdf2: callback is required');
  }
  try {
    const value = pbkdf2Sync(password, salt, iterations, keylen, digest);
    callback(null, value);
  } catch (e) {
    callback(e);
  }
}

export function createCipheriv(algorithm, key, iv) {
  const { algo, keyBuf, ivBuf } = validateAesGcmParams(algorithm, key, iv, 'createCipheriv');
  const encryptNative = requireCryptoHostcall(
    '__pi_crypto_aes_gcm_encrypt_native',
    'createCipheriv',
  );
  const chunks = [];
  let aad = new Uint8Array(0);
  let finalized = false;
  let authTag = null;
  return {
    setAAD(input) {
      if (finalized) throw new Error('Cipher already finalized');
      if (chunks.length > 0) throw new Error('Cipher.setAAD() must be called before update()');
      aad = toUint8Array(input);
      return this;
    },
    update(input, inputEncoding, outputEncoding) {
      if (finalized) throw new Error('Cipher.final() already called');
      chunks.push(toUint8Array(input, inputEncoding));
      return encodeOutput(new Uint8Array(0), outputEncoding);
    },
    final(outputEncoding) {
      if (finalized) throw new Error('Cipher.final() already called');
      finalized = true;
      const combinedHex = encryptNative(algo, keyBuf, ivBuf, aad, combineChunks(chunks), 'hex');
      const combined = hexToBuffer(combinedHex);
      authTag = bufferFromBytes(combined.slice(combined.length - 16));
      const ciphertext = combined.slice(0, combined.length - 16);
      return encodeOutput(ciphertext, outputEncoding);
    },
    getAuthTag() {
      if (!finalized || authTag === null) {
        throw new Error('Cipher.getAuthTag() requires final() first');
      }
      return bufferFromBytes(authTag);
    },
  };
}

export function createDecipheriv(algorithm, key, iv) {
  const { algo, keyBuf, ivBuf } = validateAesGcmParams(algorithm, key, iv, 'createDecipheriv');
  const decryptNative = requireCryptoHostcall(
    '__pi_crypto_aes_gcm_decrypt_native',
    'createDecipheriv',
  );
  const chunks = [];
  let aad = new Uint8Array(0);
  let authTag = null;
  let finalized = false;
  return {
    setAAD(input) {
      if (finalized) throw new Error('Decipher already finalized');
      if (chunks.length > 0) throw new Error('Decipher.setAAD() must be called before update()');
      aad = toUint8Array(input);
      return this;
    },
    setAuthTag(tag) {
      if (finalized) throw new Error('Decipher already finalized');
      const tagBuf = toUint8Array(tag);
      if (tagBuf.length !== 16) {
        throw new Error('Decipher.setAuthTag() requires a 16-byte tag');
      }
      authTag = tagBuf;
      return this;
    },
    update(input, inputEncoding, outputEncoding) {
      if (finalized) throw new Error('Decipher.final() already called');
      chunks.push(toUint8Array(input, inputEncoding));
      return encodeOutput(new Uint8Array(0), outputEncoding);
    },
    final(outputEncoding) {
      if (finalized) throw new Error('Decipher.final() already called');
      if (authTag === null) throw new Error('Decipher.final() requires setAuthTag() first');
      finalized = true;
      const plaintextHex = decryptNative(
        algo,
        keyBuf,
        ivBuf,
        aad,
        combineChunks(chunks),
        authTag,
        'hex',
      );
      return encodeOutput(hexToBuffer(plaintextHex), outputEncoding);
    },
  };
}

export function scryptSync(password, salt, keylen, options) {
  if (!Number.isSafeInteger(keylen) || keylen <= 0) {
    throw new Error('scryptSync: keylen must be a positive integer');
  }
  if (keylen > 1048576) {
    throw new Error('scryptSync: keylen must be <= 1048576');
  }
  let encoding;
  let opts = {};
  if (typeof options === 'string') {
    encoding = options;
  } else if (options && typeof options === 'object') {
    opts = options;
    if (typeof options.encoding === 'string') {
      encoding = options.encoding;
    }
  }
  const nRaw = Number.isSafeInteger(opts.N)
    ? opts.N
    : (Number.isSafeInteger(opts.cost) ? opts.cost : 16384);
  const r = Number.isSafeInteger(opts.r) ? opts.r : 8;
  const p = Number.isSafeInteger(opts.p) ? opts.p : 1;
  if (r <= 0 || p <= 0) {
    throw new Error('scryptSync: r/p must be positive integers');
  }
  if (!Number.isSafeInteger(nRaw) || nRaw <= 1) {
    throw new Error('scryptSync: N must be an integer > 1');
  }
  const logN = Math.log2(nRaw);
  if (!Number.isFinite(logN) || Math.floor(logN) !== logN) {
    throw new Error('scryptSync: N must be a power of two');
  }
  if (logN > 20) {
    throw new Error('scryptSync: N must be <= 2^20');
  }
  if (r > 16 || p > 16) {
    throw new Error('scryptSync: r/p must be <= 16');
  }
  const maxMem = 32 * 1024 * 1024;
  const n = 1 << logN;
  const memBytes = 128 * r * n * p;
  if (memBytes > maxMem) {
    throw new Error(`scryptSync: parameters exceed memory limit (${maxMem} bytes)`);
  }
  const scryptNative = requireCryptoHostcall(
    '__pi_crypto_scrypt_native',
    'scryptSync',
  );
  const hex = scryptNative(
    toUint8Array(password),
    toUint8Array(salt),
    keylen,
    logN,
    r,
    p,
    'hex',
  );
  const buffer = hexToBuffer(hex);
  return encoding ? buffer.toString(encoding) : buffer;
}

export function scrypt(password, salt, keylen, options, callback) {
  if (typeof options === 'function') { callback = options; }
  if (typeof callback !== 'function') {
    throw new Error('scrypt: callback is required');
  }
  try {
    const value = scryptSync(password, salt, keylen, options);
    callback(null, value);
  } catch (e) {
    callback(e);
  }
}

export function generateKeyPairSync() { unsupportedCryptoApi('generateKeyPairSync'); }
export function publicEncrypt() { unsupportedCryptoApi('publicEncrypt'); }
export function privateDecrypt() { unsupportedCryptoApi('privateDecrypt'); }
export function sign(algorithm, data, key) {
  normalizeSignVerifyAlgorithm(algorithm, 'sign');
  const signNative = requireCryptoHostcall('__pi_crypto_ed25519_sign_native', 'sign');
  const keyDer = keyMaterialToDer(key, 'PRIVATE KEY', 'sign');
  const hex = signNative(keyDer, toUint8Array(data), 'hex');
  return hexToBuffer(hex);
}

export function verify(algorithm, data, key, signature) {
  normalizeSignVerifyAlgorithm(algorithm, 'verify');
  const verifyNative = requireCryptoHostcall('__pi_crypto_ed25519_verify_native', 'verify');
  const keyDer = keyMaterialToDer(key, 'PUBLIC KEY', 'verify');
  return verifyNative(keyDer, toUint8Array(data), toUint8Array(signature));
}

export default {
  randomUUID, createHash, createHmac, randomBytes,
  randomInt, timingSafeEqual, getHashes, pbkdf2Sync, pbkdf2,
  createCipheriv, createDecipheriv, scryptSync, scrypt,
  generateKeyPairSync, publicEncrypt, privateDecrypt, sign, verify,
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
            let bytes = random_bytes(len).expect("random bytes");
            assert_eq!(
                bytes.len(),
                len,
                "random_bytes({len}) should return {len} bytes"
            );
        }
    }

    #[test]
    fn random_bytes_two_calls_differ() {
        let a = random_bytes(32).expect("first random bytes");
        let b = random_bytes(32).expect("second random bytes");
        // Probability of collision is astronomically low.
        assert_ne!(a, b, "two random_bytes(32) calls should differ");
    }

    #[test]
    fn random_bytes_propagates_fill_errors() {
        let err = fill_random_bytes_with(8, |_| Err("entropy unavailable")).unwrap_err();
        assert_eq!(err, "entropy unavailable");
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
        let mut mac =
            hmac::Hmac::<Sha256>::new_from_slice(b"secret").expect("create HMAC with test key");
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
        let mut mac =
            hmac::Hmac::<sha1::Sha1>::new_from_slice(b"key").expect("create HMAC with test key");
        mac.update(b"data");
        let result = hex_lower(&mac.finalize().into_bytes());
        assert_eq!(result, "104152c5bfdca07bc633eebd46199f0255c9f49d");
    }

    // ─── UUID v4 format test ─────────────────────────────────────────────

    #[test]
    fn uuid_v4_format() {
        let id = random_uuid().expect("random uuid");
        let re = regex::Regex::new(
            r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$",
        )
        .expect("compile UUID v4 regex pattern");
        assert!(re.is_match(&id), "UUID should be v4 format: {id}");
    }

    #[test]
    fn uuid_v4_uniqueness() {
        let a = random_uuid().expect("first random uuid");
        let b = random_uuid().expect("second random uuid");
        assert_ne!(a, b);
    }

    #[test]
    fn random_uuid_propagates_fill_errors() {
        let err = random_uuid_with(|_| Err("entropy unavailable")).unwrap_err();
        assert_eq!(err, "entropy unavailable");
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

    // ─── random_bytes hostcall returns valid hex ──────────────────────────

    #[test]
    fn random_bytes_hostcall_roundtrip() {
        // Verify the hex encoding used by the hostcall decodes back correctly.
        for len in [0, 1, 8, 16, 32] {
            let hex = hex_lower(&random_bytes(len).expect("random bytes"));
            assert_eq!(hex.len(), len * 2, "hex should be 2x the byte length");
            let decoded = hex_decode(&hex);
            assert_eq!(decoded.len(), len, "decoded length should match original");
        }
    }

    // ─── random_int NaN/Inf protection tests ─────────────────────────────

    #[test]
    fn random_int_validates_finite_inputs() {
        // Test that NaN/Inf inputs are properly rejected (security vulnerability fix)
        use rquickjs::{Context, Runtime};

        let runtime = Runtime::new().expect("create runtime");
        let context = Context::full(&runtime).expect("create context");

        context
            .with(|ctx| -> rquickjs::Result<()> {
                let global = ctx.globals();
                register_random_int_hostcall(&global)?;
                let hostcall: rquickjs::Function = global.get("__pi_crypto_random_int_native")?;

                // All NaN/Inf inputs should be rejected with proper error messages
                assert!(hostcall.call::<_, f64>((f64::NAN, 10.0)).is_err());
                assert!(hostcall.call::<_, f64>((0.0, f64::NAN)).is_err());
                assert!(hostcall.call::<_, f64>((f64::INFINITY, 10.0)).is_err());
                assert!(hostcall.call::<_, f64>((0.0, f64::INFINITY)).is_err());
                assert!(hostcall.call::<_, f64>((f64::NEG_INFINITY, 10.0)).is_err());

                // Valid finite inputs should work
                let result = hostcall.call::<_, f64>((0.0, 100.0))?;
                assert!((0.0..100.0).contains(&result));

                Ok(())
            })
            .expect("test NaN/Inf validation");
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

    // ── Property tests ──────────────────────────────────────────────────

    mod proptest_crypto_shim {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn hex_lower_roundtrips_through_hex_decode(bytes in prop::collection::vec(any::<u8>(), 0..128)) {
                let encoded = hex_lower(&bytes);
                let decoded = hex_decode(&encoded);
                assert_eq!(
                    decoded, bytes,
                    "hex_lower → hex_decode should roundtrip"
                );
            }

            #[test]
            fn hex_lower_output_length_is_double_input(bytes in prop::collection::vec(any::<u8>(), 0..128)) {
                let encoded = hex_lower(&bytes);
                assert_eq!(
                    encoded.len(), bytes.len() * 2,
                    "hex output should be exactly 2x input length"
                );
            }

            #[test]
            fn hex_lower_output_is_lowercase_hex(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
                let encoded = hex_lower(&bytes);
                assert!(
                    encoded.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                    "hex_lower output should only contain lowercase hex chars: {encoded}"
                );
            }

            #[test]
            fn hex_decode_odd_length_drops_trailing(
                bytes in prop::collection::vec(any::<u8>(), 1..64),
                extra_char in prop::sample::select(vec!['0', '5', 'a', 'f']),
            ) {
                let mut hex = hex_lower(&bytes);
                hex.push(extra_char);
                let decoded = hex_decode(&hex);
                // With odd-length input, the last char is dropped
                assert_eq!(
                    decoded, bytes,
                    "odd hex should decode the even prefix correctly"
                );
            }

            #[test]
            fn encode_output_hex_matches_hex_lower(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
                let via_encode = encode_output(&bytes, "hex");
                let via_hex_lower = hex_lower(&bytes);
                assert_eq!(
                    via_encode, via_hex_lower,
                    "encode_output(hex) should match hex_lower"
                );
            }

            #[test]
            fn encode_output_unknown_encoding_falls_back_to_hex(
                bytes in prop::collection::vec(any::<u8>(), 0..32),
                encoding in "[a-z]{3,8}".prop_filter(
                    "must not be known encoding",
                    |e| e != "hex" && e != "base64",
                ),
            ) {
                let result = encode_output(&bytes, &encoding);
                let expected = hex_lower(&bytes);
                assert_eq!(
                    result, expected,
                    "unknown encoding '{encoding}' should fall back to hex"
                );
            }

            #[test]
            fn random_bytes_returns_correct_length(len in 0..256usize) {
                let bytes = random_bytes(len).expect("random bytes");
                assert_eq!(
                    bytes.len(), len,
                    "random_bytes({len}) should return {len} bytes"
                );
            }

            #[test]
            fn sha256_hash_is_always_32_bytes(data in prop::collection::vec(any::<u8>(), 0..200)) {
                let mut h = Sha256::new();
                h.update(&data);
                let result = h.finalize();
                assert_eq!(
                    result.len(), 32,
                    "SHA-256 should always produce 32 bytes"
                );
            }

            #[test]
            fn sha256_is_deterministic(data in prop::collection::vec(any::<u8>(), 0..200)) {
                let mut h1 = Sha256::new();
                h1.update(&data);
                let r1 = hex_lower(&h1.finalize());

                let mut h2 = Sha256::new();
                h2.update(&data);
                let r2 = hex_lower(&h2.finalize());

                assert_eq!(r1, r2, "SHA-256 must be deterministic");
            }

            #[test]
            fn timing_safe_equal_is_reflexive(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
                let mut result = 0u8;
                for (x, y) in bytes.iter().zip(bytes.iter()) {
                    result |= x ^ y;
                }
                assert_eq!(result, 0, "byte slice compared to itself should be equal");
            }

            #[test]
            fn timing_safe_unequal_detects_single_bit_flip(
                bytes in prop::collection::vec(any::<u8>(), 1..64),
                flip_idx in any::<prop::sample::Index>(),
                flip_bit in 0..8u8,
            ) {
                let idx = flip_idx.index(bytes.len());
                let mut other = bytes.clone();
                other[idx] ^= 1 << flip_bit;
                if other == bytes {
                    return Ok(());
                }
                let mut result = 0u8;
                for (x, y) in bytes.iter().zip(other.iter()) {
                    result |= x ^ y;
                }
                assert_ne!(result, 0, "flipped byte should be detected as unequal");
            }
        }
    }
}
