//! Content-dedup fingerprint (SUR-638): `HMAC-SHA256(subkey, normalizeForTag(text) +
//! '\x00' + (bookId ?? ''))`, where `subkey` derives from the raw MK via HKDF.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::normalize::normalize_for_tag;
use crate::primitives::hkdf_expand;

type HmacSha256 = Hmac<Sha256>;

/// HKDF info — cryptographically independent of the MK-wrap key. Frozen wire constant
/// (SUR-680 allowlist), verbatim despite the Braird rename.
const TAG_INFO: &[u8] = b"surfc-content-tag-v1";

/// 64-char lowercase hex content tag. `book_id` of `None` encodes as the empty string,
/// exactly like JS `bookId ?? ''`. The HKDF salt is a fixed 32 zero bytes (the subkey
/// must be deterministic per-user across devices, so domain separation comes from the
/// distinct info string alone) and the HMAC key is 64 bytes (the SHA-256 block size).
pub fn content_tag(raw_mk: &[u8], text: &str, book_id: Option<&str>) -> String {
    let key = hkdf_expand(&[0u8; 32], raw_mk, TAG_INFO, 64);
    let message = format!("{}\u{0}{}", normalize_for_tag(text), book_id.unwrap_or(""));
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key.as_slice()).expect("hmac key length");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}
