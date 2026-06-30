//! Master-Key wrap/unwrap (PRF + PIN). Blobs are base64 `WrappedBlob`s matching the
//! JS `{ wrappedKey, iv, salt }` shape; recovered MKs are returned in `Zeroizing` and
//! never logged. Re-wrap (multi-device) is expressed on the `Vault` (it already owns
//! the MK), so it is a fresh `wrap_with_prf` rather than a decrypt-then-encrypt here.

use zeroize::Zeroizing;

use crate::primitives::{aes_decrypt, aes_encrypt, b64decode, b64encode, hkdf32, pbkdf2_sha256};
use crate::{CryptoError, WrappedBlob};

/// HKDF info for MK wrapping — distinct from the legacy note key and the content-tag
/// subkey. Frozen wire constant (SUR-680 allowlist), verbatim.
const WRAP_INFO: &[u8] = b"surfc-master-key-wrap-v1";

/// Frozen PBKDF2 iteration count for PIN transfer (SUR-680 allowlist).
pub const PIN_ITERATIONS: u32 = 600_000;

fn to_blob(wrapped_key: &[u8], iv: &[u8], salt: &[u8]) -> WrappedBlob {
    WrappedBlob {
        wrapped_key: b64encode(wrapped_key),
        iv: b64encode(iv),
        salt: b64encode(salt),
    }
}

fn to_mk_array(raw: Vec<u8>) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    if raw.len() != 32 {
        return Err(CryptoError::BadInput("unwrapped MK is not 32 bytes".into()));
    }
    let mut mk = Zeroizing::new([0u8; 32]);
    mk.copy_from_slice(&raw);
    Ok(mk)
}

/// Wrap raw MK bytes with a PRF-derived AES-256-GCM key.
pub fn wrap_with_prf(raw_mk: &[u8], prf: &[u8], salt: &[u8], iv: &[u8]) -> WrappedBlob {
    let wk = hkdf32(salt, prf, WRAP_INFO);
    let ct = aes_encrypt(wk.as_slice(), iv, raw_mk, None);
    to_blob(&ct, iv, salt)
}

/// Unwrap an MK from a PRF-wrapped blob → 32 raw bytes.
pub fn unwrap_with_prf(blob: &WrappedBlob, prf: &[u8]) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    let salt = b64decode(&blob.salt)?;
    let iv = b64decode(&blob.iv)?;
    let ct = b64decode(&blob.wrapped_key)?;
    let wk = hkdf32(&salt, prf, WRAP_INFO);
    to_mk_array(aes_decrypt(wk.as_slice(), &iv, &ct, None)?)
}

/// Wrap raw MK bytes with a PIN-derived key (PBKDF2-SHA256 @ 600k → AES-256-GCM).
pub fn wrap_with_pin(raw_mk: &[u8], pin: &str, salt: &[u8], iv: &[u8]) -> WrappedBlob {
    let dk = pbkdf2_sha256(pin.as_bytes(), salt, PIN_ITERATIONS);
    let ct = aes_encrypt(dk.as_slice(), iv, raw_mk, None);
    to_blob(&ct, iv, salt)
}

/// Unwrap raw MK bytes from a PIN-wrapped blob.
pub fn unwrap_with_pin(blob: &WrappedBlob, pin: &str) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    let salt = b64decode(&blob.salt)?;
    let iv = b64decode(&blob.iv)?;
    let ct = b64decode(&blob.wrapped_key)?;
    let dk = pbkdf2_sha256(pin.as_bytes(), &salt, PIN_ITERATIONS);
    to_mk_array(aes_decrypt(dk.as_slice(), &iv, &ct, None)?)
}
