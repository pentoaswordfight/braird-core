//! braird-core — shared crypto core for Braird's native + web clients.
//!
//! Mirrors `surfc/src/crypto/{keyManager,noteEncryption,contentTag,deviceTransfer,
//! byteEncryption}.js` + `src/lib/text.js` (`normalizeForTag`) byte-for-byte; the JS
//! WebCrypto implementation is the source of truth and the frozen
//! `vendored/crypto-parity` vectors are the contract.
//!
//! Architecture: a stateful [`Vault`] owns the 256-bit Master Key in `Zeroizing`
//! memory and never returns it across the FFI — hosts call `vault.encrypt_note(...)`
//! and receive only ciphertext / wrapped blobs (Option B, criterion #7).

mod byte_encryption;
mod content_tag;
mod key_manager;
mod normalize;
mod note_encryption;
mod primitives;
mod vault;

// The native SQLite local store (SUR-723, Phase 2). Native-only — gated off wasm32,
// where the PWA uses Dexie and `bundled` SQLite cannot compile. Not a UniFFI binding
// (no `#[uniffi::export]`); the sync methods that hang off it arrive in SUR-724/725.
#[cfg(not(target_arch = "wasm32"))]
pub mod store;

pub use vault::Vault;

uniffi::setup_scaffolding!();

/// Errors that cross the FFI. Deliberately coarse: never leak key material or
/// distinguish auth-tag failure modes to callers.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CryptoError {
    #[error("decrypt failed (auth tag / key / aad mismatch)")]
    DecryptFailed,
    #[error("bad input: {0}")]
    BadInput(String),
}

/// A stored `prf-v1` / PIN-transfer wrapped-key blob — base64 fields, exactly the
/// JS `{ wrappedKey, iv, salt }` shape. Standard base64, NOT url-safe (frozen).
#[derive(Debug, Clone, uniffi::Record)]
pub struct WrappedBlob {
    pub wrapped_key: String,
    pub iv: String,
    pub salt: String,
}
