//! enc:v1 / enc:v2 note-text sealing. Both formats are
//! `enc:vN:<base64(iv)>.<base64(ct)>` with the 16-byte GCM tag folded into the
//! ciphertext. v2 binds the noteId as AAD so ciphertext can't be transplanted
//! between records; v1 (legacy reads) has no AAD.

use crate::primitives::{aes_decrypt, aes_encrypt, b64decode, b64encode};
use crate::CryptoError;

const SENTINEL_V1: &str = "enc:v1:";
const SENTINEL_V2: &str = "enc:v2:";

/// Seal plaintext. `note_id = Some` → enc:v2 (AAD = UTF-8 noteId); `None` → enc:v1.
pub fn encrypt_note(mk: &[u8], note_id: Option<&str>, plaintext: &str, iv: &[u8]) -> String {
    let (aad, sentinel) = match note_id {
        Some(id) => (Some(id.as_bytes()), SENTINEL_V2),
        None => (None, SENTINEL_V1),
    };
    let ct = aes_encrypt(mk, iv, plaintext.as_bytes(), aad);
    format!("{sentinel}{}.{}", b64encode(iv), b64encode(&ct))
}

/// Open an enc:v1/enc:v2 payload. enc:v2 requires the matching noteId.
pub fn decrypt_note(mk: &[u8], note_id: Option<&str>, value: &str) -> Result<String, CryptoError> {
    let (is_v2, payload) = if let Some(p) = value.strip_prefix(SENTINEL_V2) {
        (true, p)
    } else if let Some(p) = value.strip_prefix(SENTINEL_V1) {
        (false, p)
    } else {
        return Err(CryptoError::BadInput("unrecognised sentinel".into()));
    };
    let (iv_b64, ct_b64) = payload
        .split_once('.')
        .ok_or_else(|| CryptoError::BadInput("missing '.' separator".into()))?;
    let iv = b64decode(iv_b64)?;
    let ct = b64decode(ct_b64)?;
    let aad: Option<&[u8]> = if is_v2 {
        Some(
            note_id
                .ok_or_else(|| CryptoError::BadInput("noteId required to decrypt enc:v2".into()))?
                .as_bytes(),
        )
    } else {
        None
    };
    let pt = aes_decrypt(mk, &iv, &ct, aad)?;
    String::from_utf8(pt).map_err(|e| CryptoError::BadInput(format!("utf8: {e}")))
}
