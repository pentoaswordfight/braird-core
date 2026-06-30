//! Crypto primitives mirroring the WebCrypto calls the JS makes. Key material is
//! held in `Zeroizing` and wiped on drop.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::CryptoError;

const AES_KEY_LEN: usize = 32;
const GCM_IV_LEN: usize = 12;

/// HKDF-SHA256 → 32-byte key (AES-256 paths; WebCrypto `deriveKey(...,length:256)`).
pub fn hkdf32(salt: &[u8], ikm: &[u8], info: &[u8]) -> Zeroizing<[u8; 32]> {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut okm = Zeroizing::new([0u8; 32]);
    hk.expand(info, okm.as_mut_slice()).expect("hkdf expand 32");
    okm
}

/// HKDF-SHA256 → `len`-byte key. The content-tag HMAC key MUST be 64 bytes:
/// WebCrypto `deriveKey({name:'HMAC',hash:'SHA-256'})` with NO `length` defaults the
/// key to the hash BLOCK size (64), not the 32-byte output size — a 32-byte port is
/// wrong-but-plausible (SUR-716 known footgun).
pub fn hkdf_expand(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Zeroizing<Vec<u8>> {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut okm = Zeroizing::new(vec![0u8; len]);
    hk.expand(info, okm.as_mut_slice()).expect("hkdf expand n");
    okm
}

/// PBKDF2-HMAC-SHA256 → 32-byte key. `iterations` is a frozen wire constant.
pub fn pbkdf2_sha256(pin: &[u8], salt: &[u8], iterations: u32) -> Zeroizing<[u8; 32]> {
    let mut dk = Zeroizing::new([0u8; 32]);
    pbkdf2::pbkdf2_hmac::<Sha256>(pin, salt, iterations, dk.as_mut_slice());
    dk
}

/// AES-256-GCM seal. `key` is always our 32-byte MK/wrapping key and `iv` our fresh
/// 12-byte nonce, so a length mismatch here is a caller bug — hence `expect`.
pub fn aes_encrypt(key: &[u8], iv: &[u8], pt: &[u8], aad: Option<&[u8]>) -> Vec<u8> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(iv);
    match aad {
        Some(a) => cipher.encrypt(nonce, Payload { msg: pt, aad: a }),
        None => cipher.encrypt(nonce, pt),
    }
    .expect("aes-gcm encrypt")
}

/// AES-256-GCM open. Guards key/iv length because the ciphertext + iv can originate
/// from untrusted/foreign input (a PWA-written row), so a bad length must be a clean
/// `BadInput`, never a panic.
pub fn aes_decrypt(
    key: &[u8],
    iv: &[u8],
    ct: &[u8],
    aad: Option<&[u8]>,
) -> Result<Vec<u8>, CryptoError> {
    if key.len() != AES_KEY_LEN || iv.len() != GCM_IV_LEN {
        return Err(CryptoError::BadInput("bad key/iv length".into()));
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(iv);
    match aad {
        Some(a) => cipher.decrypt(nonce, Payload { msg: ct, aad: a }),
        None => cipher.decrypt(nonce, ct),
    }
    .map_err(|_| CryptoError::DecryptFailed)
}

/// Standard (NOT url-safe) base64 — matches WebCrypto `btoa`.
pub fn b64encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub fn b64decode(s: &str) -> Result<Vec<u8>, CryptoError> {
    STANDARD
        .decode(s)
        .map_err(|e| CryptoError::BadInput(format!("base64: {e}")))
}

/// Fill `buf` with CSPRNG bytes. On wasm32 this routes to the browser crypto via
/// getrandom's `js` feature (SUR-716 WASM CSPRNG condition).
pub fn fill_random(buf: &mut [u8]) {
    getrandom::getrandom(buf).expect("getrandom");
}
