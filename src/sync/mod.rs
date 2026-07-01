//! The sync engine (SUR-724 / SUR-659b): outbox enqueue + push/flush + token handoff, proven
//! on notes + books. Native-only (see the `#[cfg]` in `lib.rs`) — its deps (rusqlite, reqwest,
//! tokio) don't compile to wasm32, where the PWA keeps its own `supabase.js` flush.
//!
//! Founder-decided model (resolved at the Phase-2 gates):
//!   - **Seal at write.** [`SyncEngine::enqueue_note`] seals `text` (enc:v2, bound to the note
//!     id) and computes `content_tag` FROM PLAINTEXT, both at enqueue. The outbox row holds
//!     ciphertext + the tag; no plaintext note text is ever persisted. The flush sends the
//!     ciphertext as-is (`isEncrypted` guard, mirroring the JS double-encrypt guard).
//!   - **`updated_at` in epoch MILLISECONDS**, stamped at enqueue (matching the PWA `Date.now()`
//!     and the existing cloud data; the migration default is 0, there is no server trigger).
//!   - **`bookIdRemap` persisted in `meta`** (not in-memory) so an offline book-merge survives a
//!     restart between the book flush and a later note flush.
//!   - **Sync FFI, async inside.** The engine owns a tokio current-thread runtime and
//!     `block_on`s the async PostgREST calls inside its SYNCHRONOUS UniFFI methods — the FFI
//!     surface stays sync, exactly like `Vault`.
//!
//! Source of truth: surfc `src/supabase.js` (`collapseOutboxItems`, `flushOutbox`, `upsertBook`,
//! `upsertNote`) — mirrored faithfully in `outbox.rs` / `push.rs` / `http.rs`.

pub mod http;
pub mod outbox;
pub mod push;

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::store::Store;
use crate::vault::Vault;
use http::PostgrestClient;

/// Errors that cross the FFI from the sync engine. Coarse like [`crate::CryptoError`]: enough
/// for a host to distinguish "couldn't open the store" from "the flush hit the network", never
/// leaking key material or per-record server detail.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SyncError {
    #[error("store error: {0}")]
    Store(String),
    #[error("flush error: {0}")]
    Flush(String),
}

/// The result of a flush across the FFI: how many outbox ids were pushed vs. left queued.
#[derive(Debug, uniffi::Record)]
pub struct FlushSummary {
    pub pushed: u32,
    pub still_queued: u32,
}

/// The on-device sync engine. Owns the SQLite [`Store`], the [`PostgrestClient`], the crypto
/// [`Vault`] (for seal-at-write), and a tokio current-thread runtime. `Arc<SyncEngine>` is the
/// UniFFI handle; the interior `Mutex`es make it `Send + Sync` for Swift/Kotlin callers on any
/// thread (same shape as `Vault`).
#[derive(uniffi::Object)]
pub struct SyncEngine {
    store: Mutex<Store>,
    client: Mutex<PostgrestClient>,
    vault: Arc<Vault>,
    runtime: tokio::runtime::Runtime,
}

macro_rules! lock {
    ($self:ident . $field:ident) => {
        $self.$field.lock().expect("sync engine mutex poisoned")
    };
}

#[uniffi::export]
impl SyncEngine {
    /// Open the engine over a store at `db_path`, targeting the Supabase project at
    /// `supabase_url` with the public `anon_key`. The [`Vault`] is the caller's unlocked handle
    /// (seal-at-write needs the MK). No access token yet — the host hands one over via
    /// [`SyncEngine::set_access_token`] once GoTrue has issued it.
    #[uniffi::constructor]
    pub fn open(
        db_path: String,
        supabase_url: String,
        anon_key: String,
        vault: Arc<Vault>,
    ) -> Result<Arc<SyncEngine>, SyncError> {
        let store = Store::open(&db_path).map_err(|e| SyncError::Store(e.to_string()))?;
        // Current-thread runtime: one flush at a time, no worker-thread pool to schedule across
        // the FFI. `rt` + `net` + `time` are all reqwest needs; no `macros`/`rt-multi-thread`.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .map_err(|e| SyncError::Store(format!("tokio runtime: {e}")))?;
        Ok(Arc::new(SyncEngine {
            store: Mutex::new(store),
            client: Mutex::new(PostgrestClient::new(supabase_url, anon_key)),
            vault,
            runtime,
        }))
    }

    /// Hand the core a GoTrue-issued access token (JWT). The core makes its OWN authenticated
    /// PostgREST calls with it; the `user_id` stamped on each row is the token's `sub` claim.
    pub fn set_access_token(&self, jwt: String) {
        lock!(self.client).set_access_token(jwt);
    }

    /// Enqueue a book upsert. `updated_at` is stamped in epoch ms at enqueue (never omitted —
    /// the migration default is 0). Plaintext metadata only, no encryption branch (like the PWA
    /// `upsertBook`).
    pub fn enqueue_book(
        &self,
        id: String,
        title: String,
        author: Option<String>,
        created_at: i64,
        deleted: bool,
    ) -> Result<(), SyncError> {
        let now = epoch_ms();
        let mut row = Map::new();
        row.insert("id".into(), json!(id));
        row.insert("title".into(), json!(title));
        row.insert("author".into(), json!(author.unwrap_or_default()));
        row.insert("created_at".into(), json!(created_at));
        row.insert("updated_at".into(), json!(now));
        row.insert("deleted".into(), json!(deleted));
        self.enqueue_row("books", &id, row)
    }

    /// Enqueue a note upsert — the seal-at-write path. `text` is the PLAINTEXT; it is sealed here
    /// (enc:v2, AAD = note id) and `content_tag` is computed here FROM the plaintext (both while
    /// the plaintext is in hand). The stored outbox payload holds only the ciphertext + the tag.
    ///
    /// STALE-TAG EDGE (deliberate, mirrors surfc — do not "fix"): the content_tag bakes in the
    /// note's `book_id`, but the flush repoints `book_id` via `bookIdRemap` after an offline
    /// book-merge. So a merged note's tag reflects the PRE-merge book_id. The JS never recomputes
    /// the tag at flush (`flushOutbox` doesn't touch it), and we CAN'T recompute at flush anyway —
    /// under seal-at-write there is no plaintext left. We leave the tag as-is: the rare
    /// stale-tag-after-offline-merge self-heals on the note's next edit (which re-enqueues with a
    /// freshly-computed tag). The tag is never NULL because it is computed pre-seal, from plaintext.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_note(
        &self,
        id: String,
        book_id: Option<String>,
        plaintext: String,
        page: Option<String>,
        tags: Vec<String>,
        created_at: i64,
        deleted: bool,
    ) -> Result<(), SyncError> {
        let now = epoch_ms();
        // Seal-at-write: enc:v2 ciphertext (AAD = note id) + the tag from PLAINTEXT.
        let ciphertext = self.vault.encrypt_note(Some(id.clone()), plaintext.clone());
        let content_tag = self.vault.content_tag(plaintext, book_id.clone());

        let mut row = Map::new();
        row.insert("id".into(), json!(id));
        row.insert("book_id".into(), json!(book_id));
        row.insert("text".into(), json!(ciphertext)); // ciphertext, never plaintext
        row.insert("page".into(), json!(page.unwrap_or_default()));
        row.insert("tags".into(), json!(tags));
        row.insert("source".into(), json!("manual"));
        row.insert("content_tag".into(), json!(content_tag));
        row.insert("created_at".into(), json!(created_at));
        row.insert("updated_at".into(), json!(now));
        row.insert("deleted".into(), json!(deleted));
        self.enqueue_row("notes", &id, row)
    }

    /// Push every queued write to Supabase (books-first, remap, notes; failed stay queued).
    /// Synchronous FFI — the async PostgREST calls run on the owned runtime via `block_on`.
    pub fn flush(&self) -> Result<FlushSummary, SyncError> {
        let store = lock!(self.store);
        let client = lock!(self.client);
        let result = self
            .runtime
            .block_on(push::flush(&store, &client))
            .map_err(SyncError::Flush)?;
        Ok(FlushSummary {
            pushed: result.ok.len() as u32,
            still_queued: result.failed.len() as u32,
        })
    }
}

impl SyncEngine {
    fn enqueue_row(
        &self,
        table: &str,
        record_id: &str,
        row: Map<String, Value>,
    ) -> Result<(), SyncError> {
        let payload = Value::Object(row).to_string();
        lock!(self.store)
            .enqueue(table, record_id, &payload, epoch_ms())
            .map(|_| ())
            .map_err(|e| SyncError::Store(e.to_string()))
    }
}

/// Epoch milliseconds — the PWA `Date.now()` unit the cloud data is stamped in. `SystemTime`
/// before the epoch is impossible on a sane clock; clamp to 0 rather than panic.
fn epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
