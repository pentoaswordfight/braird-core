//! Embedding-vector seal (SUR-527): `[0x02][12B IV][AES-GCM ct]`, AAD = the caller's
//! context string (SUR-997's pipeline passes `emb:{noteId}` — domain-separated from
//! enc:v2's bare-noteId AAD under the shared MK; see `embeddings::embed_aad`).
//! Device-local at-rest protection — the `embeddings` store never syncs, so this is
//! not a cross-client parity surface, but it shares the MK and AES-GCM/AAD machinery.

use crate::primitives::{aes_decrypt, aes_encrypt};
use crate::CryptoError;

const VERSION_AAD: u8 = 0x02;
const IV_LEN: usize = 12;

/// `version(0x02) || iv || ciphertext`, AAD-bound to `aad` (the noteId).
pub fn seal_bytes(mk: &[u8], bytes: &[u8], aad: &str, iv: &[u8]) -> Vec<u8> {
    let ct = aes_encrypt(mk, iv, bytes, Some(aad.as_bytes()));
    let mut out = Vec::with_capacity(1 + IV_LEN + ct.len());
    out.push(VERSION_AAD);
    out.extend_from_slice(iv);
    out.extend_from_slice(&ct);
    out
}

/// Reverse [`seal_bytes`]. Rejects a wrong version byte, a short blob, a wrong key, a
/// wrong AAD, or any tamper (AES-GCM auth-tag failure).
pub fn open_bytes(mk: &[u8], sealed: &[u8], aad: &str) -> Result<Vec<u8>, CryptoError> {
    if sealed.len() < 1 + IV_LEN || sealed[0] != VERSION_AAD {
        return Err(CryptoError::BadInput("unrecognised sealed blob".into()));
    }
    let iv = &sealed[1..1 + IV_LEN];
    let ct = &sealed[1 + IV_LEN..];
    aes_decrypt(mk, iv, ct, Some(aad.as_bytes()))
}
