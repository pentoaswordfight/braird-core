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
pub mod pull;
pub mod push;

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::store::Store;
use crate::vault::Vault;
use http::{user_id_from_jwt, PostgrestClient};

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

/// The result of a pull across the FFI: rows seen, rows merged (last-write-wins winners +
/// applied tombstones), and incoming deletes skipped as "don't-resurrect" (a delete for a row
/// this device never had).
#[derive(Debug, uniffi::Record)]
pub struct PullSummary {
    pub pulled: u32,
    pub merged: u32,
    pub skipped_tombstones: u32,
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
            client: Mutex::new(
                PostgrestClient::new(supabase_url, anon_key).map_err(SyncError::Store)?,
            ),
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
        self.stage_write("books", &id, row)
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
        self.stage_write("notes", &id, row)
    }

    /// Push every queued write to Supabase (books-first, remap, notes; failed stay queued).
    /// Synchronous FFI — the async PostgREST calls run on the owned runtime via `block_on`.
    pub fn flush(&self) -> Result<FlushSummary, SyncError> {
        let store = lock!(self.store);
        let client = lock!(self.client);
        let token = client.access_token().ok_or_else(|| {
            SyncError::Flush("no access token set — call set_access_token before flush".into())
        })?;
        let user_id = user_id_from_jwt(token)
            .map_err(|e| SyncError::Flush(format!("bad access token: {e}")))?;
        let result = self
            .runtime
            .block_on(push::flush(&store, &*client, &user_id))
            .map_err(SyncError::Flush)?;
        Ok(FlushSummary {
            pushed: result.ok.len() as u32,
            still_queued: result.failed.len() as u32,
        })
    }

    /// Pull incrementally from Supabase for the in-scope tables (`books` + `notes` this slice; the
    /// other six follow in SUR-726 by extending `TABLES`). Merges last-write-wins by `updated_at`,
    /// applies tombstones without resurrecting soft-deleted rows, and advances each per-table
    /// cursor to this call's pre-fetch `now()`. Synchronous FFI — the async GETs run on the owned
    /// runtime via `block_on`, exactly like `flush`. Note text stays ciphertext at rest (never
    /// decrypted on pull); the host decrypts on demand via `Vault::decrypt_note`.
    pub fn pull(&self) -> Result<PullSummary, SyncError> {
        const TABLES: &[&str] = &["books", "notes"];
        let store = lock!(self.store);
        let client = lock!(self.client);
        if client.access_token().is_none() {
            return Err(SyncError::Flush(
                "no access token set — call set_access_token before pull".into(),
            ));
        }
        // One pre-fetch watermark for the whole pull (mirrors the JS single `nextCheckpoint`);
        // each table that succeeds advances its cursor to it.
        let now = epoch_ms();
        let result = self
            .runtime
            .block_on(pull::pull(&store, &*client, TABLES, now))
            .map_err(SyncError::Flush)?;
        // Every requested table failing (e.g. offline / bad token) is a real error — surface it
        // rather than a misleading "pulled 0". A PARTIAL failure stays Ok (per-table isolation:
        // the failed table's cursor is untouched and re-pulls next call).
        if result.failed_tables.len() == TABLES.len() {
            return Err(SyncError::Flush(format!(
                "pull failed for all tables: {}",
                result.failed_tables.join(", ")
            )));
        }
        Ok(PullSummary {
            pulled: result.pulled as u32,
            merged: result.merged as u32,
            skipped_tombstones: result.skipped_tombstones as u32,
        })
    }
}

impl SyncEngine {
    /// Offline-first (§4): stage a local write to BOTH the synced table and the outbox — the local
    /// synced row first (so a read, and pull's LWW compare, see it immediately), then the outbox
    /// (so a later flush pushes it). Both hit SQLite before any cloud call. Mirrors the PWA writing
    /// Dexie + the outbox together. For notes, `row["text"]` is ALREADY enc:v2 ciphertext
    /// (seal-at-write), so nothing plaintext is ever persisted here either.
    ///
    /// `enqueue_*` payloads are PARTIAL (this FFI doesn't yet carry every column — e.g. a book's
    /// cover fields, a note's `image_path`/`source_meta`/`chapter`). `apply_row` is a FULL-row
    /// replace (correct for pull's `select('*')`, destructive for a partial write — it would null
    /// the omitted columns). So the local write **merges** the partial payload onto any existing
    /// row before applying; a pulled book keeps its cover when the user renames it. The OUTBOX,
    /// however, carries the partial payload as-is — the server upsert (`merge-duplicates`) patches
    /// only the changed columns, and sending the merged full row could clobber a newer server field.
    fn stage_write(
        &self,
        table: &str,
        record_id: &str,
        row: Map<String, Value>,
    ) -> Result<(), SyncError> {
        let store = lock!(self.store);
        // Local synced row = the existing row (if any) with the partial edit overlaid.
        let mut local_row = store
            .get_row(table, record_id)
            .map_err(|e| SyncError::Store(e.to_string()))?
            .unwrap_or_default();
        for (k, v) in &row {
            local_row.insert(k.clone(), v.clone());
        }
        store
            .apply_row(table, &local_row)
            .map_err(|e| SyncError::Store(e.to_string()))?;
        // Outbox: the PARTIAL payload, unchanged (SUR-724 flush semantics).
        let payload = Value::Object(row).to_string();
        store
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn enqueue_note_stores_ciphertext_not_plaintext() {
        // Seal-at-write fast-gate guard (SUR-724 Gate-2): a plaintext-storage regression must
        // fail `cargo test`, not slip past to the #[ignore]d Docker integration test. This is
        // the structural E2EE invariant — plaintext note text must never reach the outbox.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = SyncEngine::open(
            db_path.into(),
            "https://x.supabase.co".into(),
            "anon".into(),
            Vault::generate(),
        )
        .unwrap();
        engine
            .enqueue_note(
                "n1".into(),
                None,
                "the secret plaintext".into(),
                None,
                vec![],
                0,
                false,
            )
            .unwrap();

        // Read the outbox back through a fresh Store on the same file.
        let rows = Store::open(db_path).unwrap().outbox_items().unwrap();
        assert_eq!(rows.len(), 1);
        let payload: Value = serde_json::from_str(&rows[0].3).unwrap();
        let text = payload["text"].as_str().unwrap();
        assert!(
            text.starts_with("enc:v2:"),
            "note text must be enc:v2 ciphertext, got {text}"
        );
        assert!(
            !text.contains("the secret plaintext"),
            "plaintext must never reach the outbox"
        );
        assert!(
            payload["content_tag"]
                .as_str()
                .is_some_and(|t| !t.is_empty()),
            "content_tag must be present (computed pre-seal, from plaintext)"
        );
    }

    #[test]
    fn enqueue_note_writes_local_synced_row_and_outbox() {
        // Offline-first (§4): a local write must hit BOTH the synced `notes` table (so reads + the
        // pull LWW compare see it) AND the outbox (so it flushes) — before any cloud call. The
        // local row's text is ciphertext at rest, never plaintext.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = SyncEngine::open(
            db_path.into(),
            "https://x.supabase.co".into(),
            "anon".into(),
            Vault::generate(),
        )
        .unwrap();
        engine
            .enqueue_note(
                "n1".into(),
                Some("b1".into()),
                "the secret plaintext".into(),
                Some("5".into()),
                vec!["philosophy".into()],
                0,
                false,
            )
            .unwrap();

        let store = Store::open(db_path).unwrap();
        let row = store
            .get_row("notes", "n1")
            .unwrap()
            .expect("local synced row written");
        let text = row["text"].as_str().unwrap();
        assert!(text.starts_with("enc:v2:"), "local text is ciphertext");
        assert!(
            !text.contains("the secret plaintext"),
            "plaintext must never be at rest"
        );
        assert_eq!(row["book_id"], json!("b1"));
        assert_eq!(
            store.outbox_items().unwrap().len(),
            1,
            "the write is also queued for flush"
        );
    }

    #[test]
    fn enqueue_book_edit_preserves_pulled_only_columns() {
        // Regression (SUR-725 review): a partial local edit must NOT null the columns this FFI
        // doesn't carry. A book pulled WITH a cover, then renamed locally, keeps its cover —
        // stage_write merges the edit onto the existing row rather than full-replacing it.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();

        // Seed a "pulled" book row with cover fields (own connection, dropped before the engine).
        {
            let store = Store::open(db_path).unwrap();
            store
                .apply_row(
                    "books",
                    json!({
                        "id": "b1", "title": "Old", "author": "A",
                        "cover_url": "https://cover", "cover_source": "openlibrary",
                        "cover_resolved_at": 123, "created_at": 1, "updated_at": 1, "deleted": false
                    })
                    .as_object()
                    .unwrap(),
                )
                .unwrap();
        }

        // The user renames the book — enqueue_book carries only id/title/author/created_at/deleted.
        let engine = SyncEngine::open(
            db_path.into(),
            "https://x.supabase.co".into(),
            "anon".into(),
            Vault::generate(),
        )
        .unwrap();
        engine
            .enqueue_book("b1".into(), "New Title".into(), Some("A".into()), 1, false)
            .unwrap();

        let store = Store::open(db_path).unwrap();
        let row = store.get_row("books", "b1").unwrap().unwrap();
        assert_eq!(row["title"], json!("New Title"), "edit applied");
        assert_eq!(
            row["cover_url"],
            json!("https://cover"),
            "cover survives a partial edit (merge, not full-replace)"
        );
    }
}
