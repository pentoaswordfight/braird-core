//! [`Vault`] — the stateful FFI handle (Option B). Owns the 256-bit Master Key in
//! `Zeroizing` memory behind a `Mutex` (so `Arc<Vault>` is `Send + Sync` for Swift /
//! Kotlin callers on any thread). No method returns the raw MK across the FFI; the
//! only outputs are ciphertext, wrapped blobs, and content tags.

use std::sync::{Arc, Mutex};

use zeroize::Zeroizing;

use crate::primitives::fill_random;
use crate::{byte_encryption, content_tag, key_manager, note_encryption};
use crate::{CryptoError, WrappedBlob};

#[derive(uniffi::Object)]
pub struct Vault {
    mk: Mutex<Zeroizing<[u8; 32]>>,
}

fn new_vault(mk: Zeroizing<[u8; 32]>) -> Arc<Vault> {
    Arc::new(Vault { mk: Mutex::new(mk) })
}

/// Lock the MK mutex. `expect` is correct: a poisoned mutex means another thread
/// panicked mid-crypto, and continuing with possibly-inconsistent key state is worse
/// than aborting.
macro_rules! mk {
    ($self:ident) => {
        $self.mk.lock().expect("vault mutex poisoned")
    };
}

#[uniffi::export]
impl Vault {
    /// Generate a fresh random 256-bit Master Key in-core. The MK never leaves the
    /// handle; persist it with [`Vault::wrap_with_prf`].
    #[uniffi::constructor]
    pub fn generate() -> Arc<Vault> {
        let mut mk = Zeroizing::new([0u8; 32]);
        fill_random(mk.as_mut_slice());
        new_vault(mk)
    }

    /// Unlock by decrypting a stored prf-v1 blob with raw PRF bytes (the WebAuthn PRF
    /// output, fed to HKDF unchanged). The recovered MK stays inside the handle.
    #[uniffi::constructor]
    pub fn unlock(prf: Vec<u8>, blob: WrappedBlob) -> Result<Arc<Vault>, CryptoError> {
        Ok(new_vault(key_manager::unwrap_with_prf(&blob, &prf)?))
    }

    /// Redeem a PIN-encrypted device-transfer blob on a new device (PBKDF2 @ 600k).
    #[uniffi::constructor]
    pub fn redeem_pin_transfer(
        transfer_blob: WrappedBlob,
        pin: String,
    ) -> Result<Arc<Vault>, CryptoError> {
        Ok(new_vault(key_manager::unwrap_with_pin(
            &transfer_blob,
            &pin,
        )?))
    }

    /// Wrap the owned MK with a PRF-derived key → a storable prf-v1 blob. Salt (32B)
    /// and IV (12B) are generated in-core with the CSPRNG — no nonce-reuse footgun.
    pub fn wrap_with_prf(&self, prf: Vec<u8>) -> WrappedBlob {
        let (salt, iv) = fresh_salt_iv();
        key_manager::wrap_with_prf(mk!(self).as_slice(), &prf, &salt, &iv)
    }

    /// Re-wrap the owned MK for a different credential/device. The Vault already holds
    /// the MK, so this is a fresh wrap under `new_prf` (multi-device add).
    pub fn rewrap(&self, new_prf: Vec<u8>) -> WrappedBlob {
        self.wrap_with_prf(new_prf)
    }

    /// PIN-wrap the owned MK for device transfer (PBKDF2-SHA256 @ 600k → AES-256-GCM).
    pub fn pin_wrap(&self, pin: String) -> WrappedBlob {
        let (salt, iv) = fresh_salt_iv();
        key_manager::wrap_with_pin(mk!(self).as_slice(), &pin, &salt, &iv)
    }

    /// enc:v2 when `note_id` is `Some` (AAD = UTF-8 noteId); enc:v1 when `None`. Fresh
    /// random 12-byte IV per call.
    pub fn encrypt_note(&self, note_id: Option<String>, plaintext: String) -> String {
        let mut iv = [0u8; 12];
        fill_random(&mut iv);
        note_encryption::encrypt_note(mk!(self).as_slice(), note_id.as_deref(), &plaintext, &iv)
    }

    /// Decrypt an enc:v1/enc:v2 payload produced by this core OR by the PWA.
    pub fn decrypt_note(
        &self,
        note_id: Option<String>,
        ciphertext: String,
    ) -> Result<String, CryptoError> {
        note_encryption::decrypt_note(mk!(self).as_slice(), note_id.as_deref(), &ciphertext)
    }

    /// 64-char lowercase hex content-dedup tag (HMAC-SHA256, 64-byte subkey).
    pub fn content_tag(&self, text: String, book_id: Option<String>) -> String {
        content_tag::content_tag(mk!(self).as_slice(), &text, book_id.as_deref())
    }

    /// Seal arbitrary bytes (e.g. an embedding vector) at rest: `[0x02][IV][ct]`,
    /// AAD = noteId. Fresh random IV per call.
    pub fn seal_bytes(&self, bytes: Vec<u8>, aad: String) -> Vec<u8> {
        let mut iv = [0u8; 12];
        fill_random(&mut iv);
        byte_encryption::seal_bytes(mk!(self).as_slice(), &bytes, &aad, &iv)
    }

    /// Open a blob produced by [`Vault::seal_bytes`].
    pub fn open_bytes(&self, sealed: Vec<u8>, aad: String) -> Result<Vec<u8>, CryptoError> {
        byte_encryption::open_bytes(mk!(self).as_slice(), &sealed, &aad)
    }
}

fn fresh_salt_iv() -> ([u8; 32], [u8; 12]) {
    let mut salt = [0u8; 32];
    let mut iv = [0u8; 12];
    fill_random(&mut salt);
    fill_random(&mut iv);
    (salt, iv)
}

// ── Test / parity seams ──────────────────────────────────────────────────────
// Built ONLY under `--features test-seams`, and NEVER `#[uniffi::export]`ed, so the
// with-raw-MK constructor, the fixed-salt/IV determinism overrides, and the raw-MK
// readback are all absent from the production cdylib + the generated Swift/Kotlin
// bindings (naming-reviewer / crypto-reviewer BLOCKER: a public fixed-IV path is a
// catastrophic GCM nonce-reuse footgun). The parity harness (`tests/parity.rs`)
// drives these to reproduce the frozen golden vectors byte-for-byte.
#[cfg(feature = "test-seams")]
impl Vault {
    /// Construct from a known MK (hex). Mirrors the JS vectors that fix `mk = 0x11*32`.
    pub fn __with_raw_mk_hex(mk_hex: &str) -> Result<Arc<Vault>, CryptoError> {
        let bytes =
            hex::decode(mk_hex).map_err(|e| CryptoError::BadInput(format!("mk hex: {e}")))?;
        if bytes.len() != 32 {
            return Err(CryptoError::BadInput("mk must be 32 bytes".into()));
        }
        let mut mk = Zeroizing::new([0u8; 32]);
        mk.copy_from_slice(&bytes);
        Ok(new_vault(mk))
    }

    /// Read back the raw MK as hex — proves mk-unwrap without exporting the MK across
    /// the FFI (this accessor is not in the binding).
    pub fn __raw_mk_hex(&self) -> String {
        hex::encode(mk!(self).as_slice())
    }

    pub fn __wrap_with_prf_fixed(&self, prf: &[u8], salt: &[u8], iv: &[u8]) -> WrappedBlob {
        key_manager::wrap_with_prf(mk!(self).as_slice(), prf, salt, iv)
    }

    pub fn __pin_wrap_fixed(&self, pin: &str, salt: &[u8], iv: &[u8]) -> WrappedBlob {
        key_manager::wrap_with_pin(mk!(self).as_slice(), pin, salt, iv)
    }

    pub fn __encrypt_note_fixed(
        &self,
        note_id: Option<&str>,
        plaintext: &str,
        iv: &[u8],
    ) -> String {
        note_encryption::encrypt_note(mk!(self).as_slice(), note_id, plaintext, iv)
    }
}
