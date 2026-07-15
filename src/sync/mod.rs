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

mod export_import;
pub mod http;
pub mod outbox;
pub mod pull;
pub mod push;
mod read;
mod reconcile;

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::search::SearchHit;
use crate::store::{synced_table_names, Store};
use crate::vault::Vault;
use http::{user_id_from_jwt, PostgrestClient};
use read::{
    BookRecord, CollectionRecord, CustomIdeaRecord, IdeaCount, LensRecord, NoteRecord, StoreCounts,
};
use reconcile::BookMergeUndo;

/// Errors that cross the FFI from the sync engine. Coarse like [`crate::CryptoError`]: enough
/// for a host to distinguish "couldn't open the store" from "the flush hit the network", never
/// leaking key material or per-record server detail.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SyncError {
    #[error("store error: {0}")]
    Store(String),
    #[error("flush error: {0}")]
    Flush(String),
    #[error("invalid import: {0}")]
    InvalidImport(String),
}

/// Per-table row counts reported by a snapshot import.
#[derive(Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct ImportCounts {
    pub books: u32,
    pub notes: u32,
    pub custom_ideas: u32,
    pub note_links: u32,
    pub lenses: u32,
    pub collections: u32,
    pub collection_memberships: u32,
    pub note_signals: u32,
}

/// The result of a snapshot import across the FFI.
#[derive(Debug, uniffi::Record)]
pub struct ImportSummary {
    pub schema_version: u32,
    pub imported: ImportCounts,
    pub skipped_stale: ImportCounts,
}

/// The result of a flush across the FFI: how many outbox ids were pushed vs. left queued.
#[derive(Debug, uniffi::Record)]
pub struct FlushSummary {
    pub pushed: u32,
    pub still_queued: u32,
}

/// One local edit the pull dropped because a strictly-newer remote row won last-write-wins
/// (SUR-736/738) — so a host can tell the user their offline edit was superseded. Not an
/// *unresolved* conflict: the remote already won under LWW. `discarded_updated_at` is the newest
/// dropped outbox stamp; `winning_updated_at` is the remote stamp that beat it. Ids + timestamps
/// only — never payload contents (E2EE: nothing decrypted or logged here).
#[derive(Debug, PartialEq, uniffi::Record)]
pub struct SupersededEdit {
    pub table: String,
    pub record_id: String,
    pub discarded_updated_at: i64,
    pub winning_updated_at: i64,
}

/// The result of the post-pull reconciliation pass across the FFI (SUR-820): books backfilled by
/// id (a note's `book_id` referenced a book absent locally), notes rehomed to a known
/// offline-merge survivor vs. detached locally-only when no survivor is known, custom ideas
/// created for a note tag orphaned from the current canon, and duplicate notes collapsed by shared
/// `content_tag` (SUR-835). Nested onto [`PullSummary`] (not flattened) — a pull-mechanics count
/// (`pulled`/`merged`) and a reconciliation-outcome count are different concerns. A reconciliation
/// failure never fails the `pull`/`sync` it's attached to (best-effort — see [`reconcile`]); this
/// summary is all-zero in that case.
/// offline-merge survivor vs. detached locally-only when no survivor is known, and custom ideas
/// created for a note tag orphaned from the current canon, and book covers resolved via Open
/// Library for natively-created books (SUR-828). Nested onto [`PullSummary`] (not flattened) — a
/// pull-mechanics count (`pulled`/`merged`) and a reconciliation-outcome count are different
/// concerns. A reconciliation failure never fails the `pull`/`sync` it's attached to (best-effort —
/// see [`reconcile`]); this summary is all-zero in that case.
#[derive(Debug, Default, uniffi::Record)]
pub struct ReconcileSummary {
    pub books_backfilled: u32,
    pub notes_rehomed: u32,
    pub notes_detached: u32,
    pub ideas_created: u32,
    pub dupes_collapsed: u32,
    pub covers_resolved: u32,
}

impl From<reconcile::ReconcileResult> for ReconcileSummary {
    fn from(r: reconcile::ReconcileResult) -> Self {
        ReconcileSummary {
            books_backfilled: r.books_backfilled as u32,
            notes_rehomed: r.notes_rehomed as u32,
            notes_detached: r.notes_detached as u32,
            ideas_created: r.ideas_created as u32,
            dupes_collapsed: r.dupes_collapsed as u32,
            covers_resolved: r.covers_resolved as u32,
        }
    }
}

/// The result of a pull across the FFI: rows seen, rows merged (last-write-wins winners +
/// applied tombstones), incoming deletes skipped as "don't-resurrect" (a delete for a row this
/// device never had), and the local edits dropped as stale by the outbox rebase (SUR-736/738 —
/// hosts read `superseded.len()` for the count). `reconcile` is the post-pull reconciliation
/// pass (SUR-820) that runs automatically after every pull.
#[derive(Debug, uniffi::Record)]
pub struct PullSummary {
    pub pulled: u32,
    pub merged: u32,
    pub skipped_tombstones: u32,
    pub superseded: Vec<SupersededEdit>,
    pub reconcile: ReconcileSummary,
}

/// The result of [`SyncEngine::sync`] — one pull then one flush, reported together.
#[derive(Debug, uniffi::Record)]
pub struct SyncSummary {
    pub pull: PullSummary,
    pub flush: FlushSummary,
}

/// Every field a note upsert can carry, passed to [`SyncEngine::enqueue_note`] as ONE record.
///
/// This is a bug fix, not just ergonomics (SUR-770): the old 14-positional-arg signature lowered to
/// ~16 UniFFI FFI slots, and on arm64 (AAPCS64) the args past the 8th spill onto the stack, where
/// JNA's bundled libffi mis-marshals the by-value `RustBuffer` args — the first byte-validated stack
/// arg (`deleted`) then fails with "unexpected byte for Boolean" (java-native-access/jna#1259 is the
/// same class of defect). A record lowers as a SINGLE `RustBuffer` (3 FFI slots, all in registers),
/// so nothing spills. x86-64 (SysV) tolerated the wide call, so the `:core-roundtrip` desktop jar
/// never caught it — the arm64 regression net is braird-android's on-device `EnqueueNoteOnDeviceTest`.
/// Field semantics are byte-for-byte the old positional signature (see [`SyncEngine::enqueue_note`]).
/// Named to pair with the read model [`NoteRecord`] — `NoteUpsert` in, `NoteRecord` out.
#[derive(Debug, uniffi::Record)]
pub struct NoteUpsert {
    pub id: String,
    pub book_id: Option<String>,
    pub plaintext: String,
    pub page: Option<String>,
    pub tags: Vec<String>,
    pub source: Option<String>,
    pub source_id: Option<String>,
    pub source_meta_json: Option<String>,
    pub chapter: Option<String>,
    pub image_path: Option<String>,
    pub ink_crop_path: Option<String>,
    pub created_at: i64,
    pub deleted: bool,
    pub clear_nullable_fields: Vec<String>,
}

/// A book upsert draft (SUR-843) — the record form of [`SyncEngine::enqueue_book`]'s arguments.
///
/// Collapsed from 10 positional args to a single `uniffi::Record` for the SAME arm64 reason as
/// [`NoteUpsert`] (SUR-770): the positional signature lowered its trailing `clear_nullable_fields:
/// Vec<String>` to a by-value `RustBuffer` at FFI slot 11 — past x7, so it spilled onto the stack,
/// where JNA's bundled libffi mis-marshals struct-by-value args on arm64 (java-native-access/jna#1259).
/// x86-64 (SysV) tolerated the wide call, so CI + the desktop `:core-roundtrip` jar were blind to it;
/// iOS (Swift backend, no JNA) was unaffected. A record lowers as a SINGLE `RustBuffer` (3 FFI slots,
/// all in registers), so nothing spills. This was LATENT — no host called `enqueue_book` on arm64 yet
/// (book creation is deferred to SUR-819) — but converted now, at the cheapest moment (zero call-sites
/// to churn). The `scripts/check-ffi-arg-slots.mjs` guard now fails the build on any future wide export.
/// Field semantics are byte-for-byte the old positional signature (see [`SyncEngine::enqueue_book`]).
/// Named to pair with the read model [`BookRecord`] — `BookUpsert` in, `BookRecord` out.
#[derive(Debug, uniffi::Record)]
pub struct BookUpsert {
    pub id: String,
    pub title: String,
    pub author: Option<String>,
    pub isbn: Option<String>,
    pub cover_url: Option<String>,
    pub cover_source: Option<String>,
    pub cover_resolved_at: Option<i64>,
    pub created_at: i64,
    pub deleted: bool,
    pub clear_nullable_fields: Vec<String>,
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
    /// `upsertBook`). Column NAMES mirror `upsertBook` in surfc `src/supabase.js` exactly.
    ///
    /// TRI-STATE PATCH SEMANTICS (SUR-741 keep/set + SUR-775 clear). Each optional is `None` → the
    /// column is OMITTED from the payload, so the server upsert (`merge-duplicates`) and the local
    /// `stage_write` merge patch only the columns actually supplied — a `None` never clobbers a
    /// pulled-only column (a title-only rename keeps the server's cover). `Some(v)` sets it to `v`
    /// (incl. `Some("")`). To CLEAR a column back to NULL, name it in `clear_nullable_fields`: it is written
    /// as an explicit JSON `null` (→ SQL NULL locally, → server column NULLed on flush). Only the
    /// `?? null` columns are clearable ([`clearable_columns`] — `isbn`/covers here); a column both
    /// set and cleared, or a non-clearable name, is rejected and nothing is staged.
    ///
    /// Takes a single [`BookUpsert`] record, not positional args (SUR-843 — arm64 FFI stack-spill fix;
    /// see the [`BookUpsert`] doc). Field semantics are unchanged.
    pub fn enqueue_book(&self, draft: BookUpsert) -> Result<(), SyncError> {
        let BookUpsert {
            id,
            title,
            author,
            isbn,
            cover_url,
            cover_source,
            cover_resolved_at,
            created_at,
            deleted,
            clear_nullable_fields,
        } = draft;
        let now = epoch_ms();
        let mut row = Map::new();
        row.insert("id".into(), json!(id));
        row.insert("title".into(), json!(title));
        insert_opt(&mut row, "author", author);
        insert_opt(&mut row, "isbn", isbn);
        insert_opt(&mut row, "cover_url", cover_url);
        insert_opt(&mut row, "cover_source", cover_source);
        insert_opt(&mut row, "cover_resolved_at", cover_resolved_at);
        row.insert("created_at".into(), json!(created_at));
        row.insert("updated_at".into(), json!(now));
        row.insert("deleted".into(), json!(deleted));
        // Tri-state clears — validated against the clearable allowlist; nothing staged on reject.
        apply_clears("books", &mut row, &clear_nullable_fields)?;
        self.stage_write("books", &id, row)
    }

    /// Enqueue a note upsert — the seal-at-write path. `plaintext` is the note text; it is sealed
    /// here (enc:v2, AAD = note id) and `content_tag` is computed here FROM the plaintext (both
    /// while the plaintext is in hand). The stored outbox payload holds only the ciphertext + the
    /// tag. Column NAMES mirror `upsertNote` in surfc `src/supabase.js` exactly.
    ///
    /// WIDENED (SUR-741). Carries the full authoring surface: `source`/`source_id`/`source_meta`/
    /// `chapter`/`image_path`/`ink_crop_path`. `source_meta_json` takes a serialized JSON **object**
    /// string for the `source_meta` jsonb column — the `…Json` suffix is the stated convention for any
    /// param that crosses the FFI as a serialized-JSON string (UniFFI has no jsonb type; the type
    /// alone can't say "this String is JSON, not a scalar"). It is parse-validated up front — invalid
    /// JSON or a non-object → `SyncError::Store` and **nothing is staged** (no seal, no write). None of
    /// the new fields touch the Vault — only `plaintext` is ever sealed.
    ///
    /// TRI-STATE PATCH SEMANTICS (SUR-741 keep/set + SUR-775 clear): every optional is `None` →
    /// column OMITTED (patch, never clobbers a pulled-only column; see [`SyncEngine::enqueue_book`]).
    /// `source` is the one exception — `None` → `"manual"` (the PWA's `|| 'manual'` / the prior
    /// hardcode), always sent, so it is not clearable. To clear a `?? null` column to NULL name it
    /// in `clear_nullable_fields` (notes: `book_id`/`chapter`/`image_path`/`ink_crop_path`/`source_id` —
    /// [`clearable_columns`]). `page` is `|| ''`, not NULL-clearable — clearing it is `Some("")`.
    /// `text` (sealed) and `content_tag` (derived) are never clearable; a bad/contradictory
    /// `clear_nullable_fields` is rejected and nothing is staged.
    ///
    /// STALE-TAG EDGE (deliberate, mirrors surfc — do not "fix"): the content_tag bakes in the
    /// note's `book_id`, but the flush repoints `book_id` via `bookIdRemap` after an offline
    /// book-merge. So a merged note's tag reflects the PRE-merge book_id. The JS never recomputes
    /// the tag at flush (`flushOutbox` doesn't touch it), and we CAN'T recompute at flush anyway —
    /// under seal-at-write there is no plaintext left. We leave the tag as-is: the rare
    /// stale-tag-after-offline-merge self-heals on the note's next edit (which re-enqueues with a
    /// freshly-computed tag). The tag is never NULL because it is computed pre-seal, from plaintext.
    pub fn enqueue_note(&self, draft: NoteUpsert) -> Result<(), SyncError> {
        let NoteUpsert {
            id,
            book_id,
            plaintext,
            page,
            tags,
            source,
            source_id,
            source_meta_json,
            chapter,
            image_path,
            ink_crop_path,
            created_at,
            deleted,
            clear_nullable_fields,
        } = draft;
        // Validate source_meta_json BEFORE any seal/stage — a bad payload stages nothing.
        let source_meta = match source_meta_json {
            None => None,
            Some(s) => {
                // Do NOT interpolate the serde error — it can echo a fragment of the caller's
                // input into a string that crosses the FFI and may be host-logged (crypto-reviewer:
                // keep host-supplied content out of error messages).
                let v: Value = serde_json::from_str(&s)
                    .map_err(|_| SyncError::Store("source_meta_json is not valid JSON".into()))?;
                if !v.is_object() {
                    return Err(SyncError::Store(
                        "source_meta_json must be a JSON object".into(),
                    ));
                }
                Some(v)
            }
        };

        let now = epoch_ms();
        // Seal-at-write: enc:v2 ciphertext (AAD = note id) + the tag from PLAINTEXT.
        let ciphertext = self.vault.encrypt_note(Some(id.clone()), plaintext.clone());
        let content_tag = self.vault.content_tag(plaintext, book_id.clone());

        let mut row = Map::new();
        row.insert("id".into(), json!(id));
        insert_opt(&mut row, "book_id", book_id);
        row.insert("text".into(), json!(ciphertext)); // ciphertext, never plaintext
        insert_opt(&mut row, "page", page);
        row.insert("tags".into(), json!(tags));
        // source is the one always-sent optional: None → "manual" (PWA's `|| 'manual'`).
        row.insert(
            "source".into(),
            json!(source.unwrap_or_else(|| "manual".into())),
        );
        insert_opt(&mut row, "source_id", source_id);
        if let Some(v) = source_meta {
            row.insert("source_meta".into(), v);
        }
        insert_opt(&mut row, "chapter", chapter);
        insert_opt(&mut row, "image_path", image_path);
        insert_opt(&mut row, "ink_crop_path", ink_crop_path);
        row.insert("content_tag".into(), json!(content_tag));
        row.insert("created_at".into(), json!(created_at));
        row.insert("updated_at".into(), json!(now));
        row.insert("deleted".into(), json!(deleted));
        // Tri-state clears — validated against the clearable allowlist; on reject nothing is staged
        // (the seal above is discarded, never persisted). `text`/`content_tag` aren't clearable.
        apply_clears("notes", &mut row, &clear_nullable_fields)?;
        self.stage_write("notes", &id, row)
    }

    /// Enqueue a custom-idea upsert (SUR-726). Plaintext metadata only (mirrors `upsertIdea`);
    /// `description` defaults to `""` when absent (the PWA's `|| ''`). `updated_at` stamped at enqueue.
    pub fn enqueue_custom_idea(
        &self,
        id: String,
        name: String,
        description: Option<String>,
        created_at: i64,
        deleted: bool,
    ) -> Result<(), SyncError> {
        let now = epoch_ms();
        let mut row = Map::new();
        row.insert("id".into(), json!(id));
        row.insert("name".into(), json!(name));
        row.insert("description".into(), json!(description.unwrap_or_default()));
        row.insert("created_at".into(), json!(created_at));
        row.insert("updated_at".into(), json!(now));
        row.insert("deleted".into(), json!(deleted));
        self.stage_write("custom_ideas", &id, row)
    }

    /// Enqueue a note-link upsert (SUR-726) — a parent→child annotation edge. Plaintext only;
    /// `relation_type` defaults to `"handwritten_annotation"` (mirrors the surfc column default). A
    /// remove is the same call with `deleted: true` (tombstone). Row-per-edge on a random pk (a
    /// "bag" in the SUR-737 convergence contract): concurrent adds of the same logical edge do NOT
    /// dedup — unlike memberships' deterministic pk.
    pub fn enqueue_note_link(
        &self,
        id: String,
        from_note_id: String,
        to_note_id: String,
        relation_type: Option<String>,
        created_at: i64,
        deleted: bool,
    ) -> Result<(), SyncError> {
        let now = epoch_ms();
        let mut row = Map::new();
        row.insert("id".into(), json!(id));
        row.insert("from_note_id".into(), json!(from_note_id));
        row.insert("to_note_id".into(), json!(to_note_id));
        row.insert(
            "relation_type".into(),
            json!(relation_type.unwrap_or_else(|| "handwritten_annotation".into())),
        );
        row.insert("created_at".into(), json!(created_at));
        row.insert("updated_at".into(), json!(now));
        row.insert("deleted".into(), json!(deleted));
        self.stage_write("note_links", &id, row)
    }

    /// Enqueue a lens upsert (SUR-726) — ONE authored query. Plaintext; `leaf_ids` is a cloud
    /// `text[]` (JSON array on the wire), whole-row LWW (SUR-737 — no leaf union). `combinator` /
    /// `threshold` default to `"AND"` / `100` (mirrors `upsertLens`'s `|| 'AND'` / `?? 100`). No
    /// client-side range check on threshold — the server CHECK (0..=100) enforces it, like the PWA.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_lens(
        &self,
        id: String,
        name: String,
        leaf_ids: Vec<String>,
        combinator: Option<String>,
        threshold: Option<i64>,
        created_at: i64,
        deleted: bool,
    ) -> Result<(), SyncError> {
        let now = epoch_ms();
        let mut row = Map::new();
        row.insert("id".into(), json!(id));
        row.insert("name".into(), json!(name));
        row.insert("leaf_ids".into(), json!(leaf_ids));
        row.insert(
            "combinator".into(),
            json!(combinator.unwrap_or_else(|| "AND".into())),
        );
        row.insert("threshold".into(), json!(threshold.unwrap_or(100)));
        row.insert("created_at".into(), json!(created_at));
        row.insert("updated_at".into(), json!(now));
        row.insert("deleted".into(), json!(deleted));
        self.stage_write("lenses", &id, row)
    }

    /// Enqueue a collection upsert (SUR-726). Plaintext metadata only.
    pub fn enqueue_collection(
        &self,
        id: String,
        name: String,
        created_at: i64,
        deleted: bool,
    ) -> Result<(), SyncError> {
        let now = epoch_ms();
        let mut row = Map::new();
        row.insert("id".into(), json!(id));
        row.insert("name".into(), json!(name));
        row.insert("created_at".into(), json!(created_at));
        row.insert("updated_at".into(), json!(now));
        row.insert("deleted".into(), json!(deleted));
        self.stage_write("collections", &id, row)
    }

    /// Enqueue a collection-membership upsert (SUR-726) — a note↔collection pair. The pk is DERIVED
    /// here via [`membership_id`] (collection first), never taken from the host, so two devices
    /// adding the same pair converge to ONE row (SUR-737 OR-set add). A remove is the same call with
    /// `deleted: true`. `created_at` is always carried (the server column is NOT NULL, no default).
    pub fn enqueue_collection_membership(
        &self,
        note_id: String,
        collection_id: String,
        created_at: i64,
        deleted: bool,
    ) -> Result<(), SyncError> {
        let now = epoch_ms();
        let id = crate::store::membership_id(&collection_id, &note_id);
        let mut row = Map::new();
        row.insert("id".into(), json!(id));
        row.insert("note_id".into(), json!(note_id));
        row.insert("collection_id".into(), json!(collection_id));
        row.insert("created_at".into(), json!(created_at));
        row.insert("updated_at".into(), json!(now));
        row.insert("deleted".into(), json!(deleted));
        self.stage_write("collection_memberships", &id, row)
    }

    /// Enqueue a note-signals upsert (SUR-726) — per-note behavioural counters, keyed by `note_id`
    /// (there is NO separate `id` column; the payload carries `note_id` only, matching
    /// `upsertNoteSignals`). Whole-row LWW; concurrent increments are lossy but self-heal (SUR-737,
    /// ratified — derived data). Params follow the descriptor column order.
    ///
    /// CONTRACT (mirror of surfc's `ensureNoteSignals`): hosts must NOT enqueue a fresh "birth" row.
    /// A birth row is local-only lazy-init; pushing one would clobber another device's earned counters
    /// under whole-row LWW. Enqueue only on a genuine behavioural change.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_note_signals(
        &self,
        note_id: String,
        source_prior: f64,
        return_visits: i64,
        has_annotation: bool,
        stitch_spawns: i64,
        exposure_recency_at: i64,
        engagement_recency_at: i64,
        importance: f64,
        created_at: i64,
        deleted: bool,
    ) -> Result<(), SyncError> {
        let now = epoch_ms();
        let mut row = Map::new();
        row.insert("note_id".into(), json!(note_id));
        row.insert("source_prior".into(), json!(source_prior));
        row.insert("return_visits".into(), json!(return_visits));
        row.insert("has_annotation".into(), json!(has_annotation));
        row.insert("stitch_spawns".into(), json!(stitch_spawns));
        row.insert("exposure_recency_at".into(), json!(exposure_recency_at));
        row.insert("engagement_recency_at".into(), json!(engagement_recency_at));
        row.insert("importance".into(), json!(importance));
        row.insert("created_at".into(), json!(created_at));
        row.insert("updated_at".into(), json!(now));
        row.insert("deleted".into(), json!(deleted));
        self.stage_write("note_signals", &note_id, row)
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

    /// Pull incrementally from Supabase for **all eight synced tables** (SUR-726 —
    /// [`synced_table_names`] is the one source of the pull scope). Merges last-write-wins by
    /// `updated_at`, applies tombstones without resurrecting soft-deleted rows, **rebases the outbox**
    /// (drops a queued local edit a newer remote row beat — SUR-736 — and reports it in `superseded`,
    /// SUR-738), and advances each per-table cursor to the max server `change_seq` it merged
    /// (SUR-739 visibility watermark), paging by `change_seq` until a short page (SUR-652). The
    /// watermark replaces the old client-clock lookback: a delayed/offline flush is now delivered the
    /// moment the server makes it visible, not skipped. Synchronous FFI — the async GETs run on the
    /// owned runtime via `block_on`, exactly like `flush`. Note text stays ciphertext at rest (never
    /// decrypted on pull); the host decrypts via `Vault::decrypt_note`.
    ///
    /// Call order is now safe either way for SUR-736: the rebase drops a stale queued edit as it
    /// merges the newer remote row, so a following `flush()` can't re-push it. Prefer
    /// [`SyncEngine::sync`] (pull-then-flush) for the one-call path. (This does NOT fix SUR-740 — a
    /// flush destroying a newer SERVER row before a pull can see it is the server's job, PR-3.)
    pub fn pull(&self) -> Result<PullSummary, SyncError> {
        let tables = synced_table_names();
        let store = lock!(self.store);
        let client = lock!(self.client);
        let token = match client.access_token() {
            Some(t) => t,
            None => {
                return Err(SyncError::Flush(
                    "no access token set — call set_access_token before pull".into(),
                ))
            }
        };
        let (result, reconciled) = self
            .runtime
            .block_on(pull_and_reconcile(
                &store,
                &*client,
                token,
                &tables,
                &self.vault,
            ))
            .map_err(SyncError::Flush)?;
        // Every requested table failing (e.g. offline / bad token) is a real error — surface it
        // rather than a misleading "pulled 0". A PARTIAL failure stays Ok (per-table isolation:
        // the failed table's cursor is untouched and re-pulls next call) — reconciliation was
        // already skipped for a partial failure inside `pull_and_reconcile`.
        if result.failed_tables.len() == tables.len() {
            return Err(SyncError::Flush(format!(
                "pull failed for all tables: {}",
                result.failed_tables.join(", ")
            )));
        }
        Ok(PullSummary {
            pulled: result.pulled as u32,
            merged: result.merged as u32,
            skipped_tombstones: result.skipped_tombstones as u32,
            superseded: result.superseded,
            reconcile: reconciled.into(),
        })
    }

    /// Pull, then flush — the one-call convergence path (SUR-736). Pulls FIRST, then flushes.
    ///
    /// **Deliberate divergence from the oracle** (surfc's `syncFromCloud` flushes first): with the
    /// outbox rebase (SUR-736), pulling first fetches the server's newer row and rebases the stale
    /// local edit out of the outbox, so the following flush pushes nothing stale. Flushing FIRST
    /// would re-push the stale edit over the newer server row before the pull could see it — the 736
    /// lost edit. Same class of documented divergence as the per-table cursor.
    ///
    /// **The flush is aborted unless the pull was fully clean.** If ANY table's pull failed (partial
    /// OR total), `sync()` returns an error and does NOT flush. A failed table never rebased its
    /// outbox (its cursor is unadvanced), so flushing it could re-push a stale edit over a newer
    /// server row this pull didn't fetch — reopening SUR-736 for that table. This is stricter than
    /// calling `pull()` + `flush()` separately (where a partial pull is `Ok` and a subsequent flush
    /// runs) — the strictness is the point: `sync()` guarantees rebase-protected convergence or
    /// nothing, and the host retries. (This still does NOT fix SUR-740 — a flush destroying a newer
    /// SERVER row before this pull could see it is the server's job, PR-3.)
    pub fn sync(&self) -> Result<SyncSummary, SyncError> {
        let tables = synced_table_names();
        let store = lock!(self.store);
        let client = lock!(self.client);
        let token = client.access_token().ok_or_else(|| {
            SyncError::Flush("no access token set — call set_access_token before sync".into())
        })?;
        let user_id = user_id_from_jwt(token)
            .map_err(|e| SyncError::Flush(format!("bad access token: {e}")))?;
        let (pull, reconcile, flush) = self
            .runtime
            .block_on(pull_then_flush(
                &store,
                &*client,
                &user_id,
                &tables,
                &self.vault,
            ))
            .map_err(SyncError::Flush)?;
        Ok(SyncSummary {
            pull: PullSummary {
                pulled: pull.pulled as u32,
                merged: pull.merged as u32,
                skipped_tombstones: pull.skipped_tombstones as u32,
                superseded: pull.superseded,
                reconcile: reconcile.into(),
            },
            flush: FlushSummary {
                pushed: flush.ok.len() as u32,
                still_queued: flush.failed.len() as u32,
            },
        })
    }

    /// Export a plaintext, PWA-compatible snapshot of every live synced row. Note ciphertext is
    /// decrypted inside the core; a single decryption failure aborts the entire export so neither
    /// ciphertext nor a partial archive can cross the FFI. Local-only tables are never included.
    pub fn export_snapshot(&self) -> Result<String, SyncError> {
        let store = lock!(self.store);
        export_import::build_snapshot_at(&store, &self.vault, epoch_ms())
    }

    /// Protectively merge a plaintext PWA snapshot into the local mirror. Parsing happens before
    /// any operational lock or token check. A valid archive then performs a clean all-table pull,
    /// direct server LWW preflight, in-core note sealing, and one atomic local+outbox batch. The
    /// staged batch is deliberately not flushed; the next normal [`SyncEngine::sync`] uploads it.
    pub fn import_merge(&self, json: String) -> Result<ImportSummary, SyncError> {
        let import_now = epoch_ms();
        export_import::with_parsed_import_at(&json, import_now, |parsed| {
            let store = lock!(self.store);
            let client = lock!(self.client);
            if client.access_token().is_none() {
                return Err(SyncError::Flush(
                    "no access token set — call set_access_token before importing".into(),
                ));
            }
            self.runtime.block_on(export_import::merge_parsed_with_sink(
                &store,
                &*client,
                &self.vault,
                parsed,
                import_now,
            ))
        })
    }

    // ── read/query surface (SUR-744) ─────────────────────────────────────────
    // Decrypt-in-core reads for the native list/search screens (SUR-754). Every method
    // excludes soft-deleted rows and orders newest-first; note text is decrypted on the way
    // out (`read::note_record` → `Vault::decrypt_note`), never written back to the store.
    // The shape + the crypto boundary live in `sync::read`; these are thin locks-and-maps.

    /// Books for the Library / Sources grid, newest-first, each with its live `note_count`.
    pub fn list_books(&self, limit: u32, offset: u32) -> Result<Vec<BookRecord>, SyncError> {
        let store = lock!(self.store);
        read::list_books(&store, limit as i64, offset as i64).map_err(store_err)
    }

    /// One book by id, or `None` if absent or soft-deleted.
    pub fn get_book(&self, id: String) -> Result<Option<BookRecord>, SyncError> {
        let store = lock!(self.store);
        read::get_book(&store, &id).map_err(store_err)
    }

    /// Notes newest-first. `book_id = None` → the Commonplace flat list (all notes); `Some` →
    /// that book's notes. `text` is decrypted plaintext, or `None` with `decrypt_failed = true`.
    pub fn list_notes(
        &self,
        book_id: Option<String>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<NoteRecord>, SyncError> {
        let store = lock!(self.store);
        read::list_notes(
            &store,
            &self.vault,
            book_id.as_deref(),
            limit as i64,
            offset as i64,
        )
        .map_err(store_err)
    }

    /// One note by id, decrypted, or `None` if absent or soft-deleted.
    pub fn get_note(&self, id: String) -> Result<Option<NoteRecord>, SyncError> {
        let store = lock!(self.store);
        read::get_note(&store, &self.vault, &id).map_err(store_err)
    }

    /// Custom ideas for the AddIdeaSheet "Your Ideas" section, newest-first.
    pub fn list_custom_ideas(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<CustomIdeaRecord>, SyncError> {
        let store = lock!(self.store);
        read::list_custom_ideas(&store, limit as i64, offset as i64).map_err(store_err)
    }

    /// Live (non-deleted) row totals for books / notes / custom ideas, plus `active_ideas` — the
    /// count of distinct idea tags on live notes (the Home stat row, SUR-806).
    pub fn counts(&self) -> Result<StoreCounts, SyncError> {
        let store = lock!(self.store);
        read::counts(&store).map_err(store_err)
    }

    /// Home "this week" count (SUR-806) — live notes created within the last 7 days whose decrypted
    /// text is non-empty (the PWA's `notesThisWeek`). `now_ms` is the host's `Date.now()` (epoch ms).
    pub fn notes_this_week(&self, now_ms: i64) -> Result<u32, SyncError> {
        let store = lock!(self.store);
        read::notes_this_week(&store, &self.vault, now_ms).map_err(store_err)
    }

    /// Home "Recently surfaced" card (SUR-806) — a pseudo-random note from that same "this week"
    /// set, decrypted in core, or `None` when nothing is fresh. `seed` is the host's random draw
    /// (the pick is deterministic in it, and the host re-rolls it to re-surface); `now_ms` is the
    /// host's `Date.now()`.
    pub fn recent_note(&self, now_ms: i64, seed: u64) -> Result<Option<NoteRecord>, SyncError> {
        let store = lock!(self.store);
        read::recent_note(&store, &self.vault, now_ms, seed).map_err(store_err)
    }

    /// Lexical search over decrypted note text + custom-idea name/description (SUR-527 parity).
    /// Rebuilds the in-memory index from the live store per call — no plaintext touches disk —
    /// and returns up to `limit` hits, best-first.
    pub fn search(&self, query: String, limit: u32) -> Result<Vec<SearchHit>, SyncError> {
        let store = lock!(self.store);
        let docs = read::build_search_docs(&store, &self.vault).map_err(store_err)?;
        Ok(crate::search::search(&docs, &query, limit as usize))
    }

    // ── organise reads (SUR-858) ─────────────────────────────────────────────
    // Extension #2 of the read surface (after SUR-744/806) for the native browse/organise
    // screens: notes-by-idea, per-idea counts, the collections/lenses lists, and the untagged
    // work queue. Same decrypt-in-core + soft-delete-excluding + newest-first contract.

    /// Live notes carrying `idea` as an idea tag, newest-first, decrypted in core (SUR-858) — the
    /// Commonplace idea filter / IdeaDetail / RelatedNotes. `idea` is the raw tag string (== a
    /// `CustomIdeaRecord.name`, == an `IdeaCount.idea`); the match is exact.
    pub fn notes_by_idea(
        &self,
        idea: String,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<NoteRecord>, SyncError> {
        let store = lock!(self.store);
        read::notes_by_idea(&store, &self.vault, &idea, limit as i64, offset as i64)
            .map_err(store_err)
    }

    /// Per-idea live-note counts (SUR-858) — the tree's counts, `{idea, count}` sorted by idea name,
    /// only for tags on ≥1 live note (the client overlays these onto its generated canon structure).
    pub fn idea_counts(&self) -> Result<Vec<IdeaCount>, SyncError> {
        let store = lock!(self.store);
        read::idea_counts(&store).map_err(store_err)
    }

    /// Collections for the Lexicon list (SUR-858), newest-first. Bare metadata rows, no crypto.
    pub fn list_collections(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<CollectionRecord>, SyncError> {
        let store = lock!(self.store);
        read::list_collections(&store, limit as i64, offset as i64).map_err(store_err)
    }

    /// Lenses (authored saved-queries) for the Lexicon list (SUR-858), newest-first. No crypto.
    pub fn list_lenses(&self, limit: u32, offset: u32) -> Result<Vec<LensRecord>, SyncError> {
        let store = lock!(self.store);
        read::list_lenses(&store, limit as i64, offset as i64).map_err(store_err)
    }

    /// Live notes with NO idea tags, newest-first, decrypted in core (SUR-858) — BulkDiscovery's
    /// work queue.
    pub fn untagged_notes(&self, limit: u32, offset: u32) -> Result<Vec<NoteRecord>, SyncError> {
        let store = lock!(self.store);
        read::untagged_notes(&store, &self.vault, limit as i64, offset as i64).map_err(store_err)
    }

    /// Count of the whole untagged-notes queue (SUR-858) — BulkDiscovery's badge. No decryption.
    pub fn untagged_notes_count(&self) -> Result<u32, SyncError> {
        let store = lock!(self.store);
        read::untagged_notes_count(&store).map_err(store_err)
    }

    // ── duplicate-resolution merge contract (SUR-915) ─────────────────────────
    // Host-invoked merge verbs (consumers SUR-863 iOS / SUR-877 Android). Key-free store-level
    // patches under the store lock; the shape + replay-safety live in `sync::reconcile`.

    /// Merge duplicate source books into `survivor_id` (SUR-915): rehome the losers' notes, keep the
    /// earliest `created_at`, tombstone the losers, and record the redirects so the fleet converges.
    /// Returns the undo token for the host's 10-second window.
    pub fn merge_books(
        &self,
        survivor_id: String,
        loser_ids: Vec<String>,
    ) -> Result<BookMergeUndo, SyncError> {
        let store = lock!(self.store);
        reconcile::merge_books(&store, &survivor_id, &loser_ids).map_err(SyncError::Store)
    }

    /// Reverse a `merge_books` within the host's undo window (SUR-915). Idempotent.
    pub fn unmerge_books(&self, undo: BookMergeUndo) -> Result<(), SyncError> {
        let store = lock!(self.store);
        reconcile::unmerge_books(&store, &undo).map_err(SyncError::Store)
    }

    /// Manual/user-selected content-duplicate merge into an explicit `survivor_id` (SUR-915). When
    /// `allow_cross_cluster` is false, every selected note must share one non-empty `content_tag`;
    /// set it for the host's fuzzy (0.92) path, which spans clusters. Returns the losers collapsed.
    pub fn merge_content_duplicates(
        &self,
        survivor_id: String,
        loser_ids: Vec<String>,
        allow_cross_cluster: bool,
    ) -> Result<u32, SyncError> {
        let store = lock!(self.store);
        reconcile::merge_content_duplicates(&store, &survivor_id, &loser_ids, allow_cross_cluster)
            .map(|n| n as u32)
            .map_err(SyncError::Store)
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
    /// cover fields, a note's `image_path`/`source_meta`/`chapter`). The local synced row is the
    /// partial edit **merged** onto any existing row (so it can't null pulled-only columns), while
    /// the outbox keeps the partial payload (the server upsert patches only the changed columns).
    /// Both writes happen in ONE transaction ([`Store::stage_local_write`]) — a partial failure
    /// can't leave a locally-visible edit with no queued outbox row (SUR-725 review).
    fn stage_write(
        &self,
        table: &str,
        record_id: &str,
        row: Map<String, Value>,
    ) -> Result<(), SyncError> {
        lock!(self.store)
            .stage_local_write(table, record_id, row, epoch_ms())
            .map_err(|e| SyncError::Store(e.to_string()))
    }
}

/// Pull then flush, aborting the flush unless the pull was **fully clean** (SUR-736). Shared by
/// [`SyncEngine::sync`] and the sync integration test (which drives it with a stub sink — the
/// concrete `PostgrestClient` inside `SyncEngine` can't be made to fail one table but not another).
/// If ANY table's pull failed (partial or total), returns `Err` and does NOT flush: a failed table
/// never rebased its outbox, so flushing it could re-push a stale edit over a newer server row this
/// pull didn't fetch (reopening SUR-736). On a fully-clean pull, flushes and returns both results.
pub async fn pull_then_flush<S: http::PostgrestSink + http::CoverEgress>(
    store: &Store,
    sink: &S,
    user_id: &str,
    tables: &[&str],
    vault: &Vault,
) -> Result<
    (
        pull::PullResult,
        reconcile::ReconcileResult,
        push::FlushResult,
    ),
    String,
> {
    let pulled = pull::pull(store, sink, tables).await?;
    if !pulled.failed_tables.is_empty() {
        return Err(format!(
            "pull failed for {} — aborting flush so a stale edit can't re-push over a newer server \
             row (SUR-736); retry sync",
            pulled.failed_tables.join(", ")
        ));
    }
    // Best-effort (SUR-820): a reconciliation hiccup must never abort an otherwise-clean
    // pull+flush — logged and zeroed here, retried silently on the next sync.
    let reconciled = reconcile::reconcile(store, sink, user_id, vault)
        .await
        .unwrap_or_else(|e| {
            eprintln!("sync: post-pull reconciliation failed (non-fatal, retries next pull): {e}");
            reconcile::ReconcileResult::default()
        });
    let flushed = push::flush(store, sink, user_id).await?;
    Ok((pulled, reconciled, flushed))
}

/// Run the post-pull reconciliation pass for [`SyncEngine::pull`], never failing the pull it
/// follows — the same best-effort guarantee [`pull_then_flush`] gives `sync()`. `pull()` has no
/// independent need for `user_id` (unlike `flush`/`sync`, which stamp it on pushed rows); it's
/// derived here purely for reconciliation's dropped-tag pass, so a bad/unparseable token
/// degrades reconciliation only, not the pull itself.
/// Pull, then reconcile — but ONLY if the pull was fully clean, mirroring [`pull_then_flush`]'s
/// SUR-736 guard for the same reason: a table that failed to pull this round is stale (its
/// cursor didn't advance), and reconciling against stale data is unsafe. Concretely,
/// `reconcile_dropped_tags` reads the local `custom_ideas` mirror to decide whether a note tag
/// is already a known custom idea — if `custom_ideas` failed to pull while `notes` succeeded,
/// that mirror is stale, and the pass could recreate/overwrite a custom idea another device
/// already pushed under the same deterministic id, because this device just doesn't know about
/// it yet. `pull_then_flush` already has this exact guard (it aborts the whole call on ANY
/// failed table, for the pre-existing SUR-736 flush reason); this gives `SyncEngine::pull`'s
/// standalone path the matching guard, without forcing it to also abort the pull-result
/// reporting itself — a partial pull failure still returns `Ok` with real counts (per-table
/// isolation, unchanged), only reconciliation is skipped and zeroed.
///
/// `pull()` has no independent need for `user_id` (unlike `flush`/`sync`, which stamp it on
/// pushed rows); it's derived here purely for reconciliation's dropped-tag pass, so a
/// bad/unparseable token degrades reconciliation only, not the pull itself. Shared by
/// [`SyncEngine::pull`] and covered directly by a stub-sink test (mirrors [`pull_then_flush`]'s
/// own testability shape — the concrete `PostgrestClient` inside `SyncEngine` can't be made to
/// fail one table but not another).
pub async fn pull_and_reconcile<S: http::PostgrestSink + http::CoverEgress>(
    store: &Store,
    sink: &S,
    token: &str,
    tables: &[&str],
    vault: &Vault,
) -> Result<(pull::PullResult, reconcile::ReconcileResult), String> {
    let pulled = pull::pull(store, sink, tables).await?;
    if !pulled.failed_tables.is_empty() {
        eprintln!(
            "pull: skipping reconciliation — {} failed to pull this round (stale-data risk); \
             retries next pull",
            pulled.failed_tables.join(", ")
        );
        return Ok((pulled, reconcile::ReconcileResult::default()));
    }
    let outcome = match http::user_id_from_jwt(token) {
        Ok(user_id) => reconcile::reconcile(store, sink, &user_id, vault).await,
        Err(e) => Err(format!("bad access token: {e}")),
    };
    let reconciled = outcome.unwrap_or_else(|e| {
        eprintln!("pull: post-pull reconciliation failed (non-fatal, retries next pull): {e}");
        reconcile::ReconcileResult::default()
    });
    Ok((pulled, reconciled))
}

/// Derive a `collection_memberships` primary key from its `(collection_id, note_id)` pair — the
/// FFI-exported mirror of surfc's `membershipId(collectionId, noteId)`, so a host can look up or
/// join local membership rows by the same deterministic id the sync layer writes (SUR-726). Thin
/// wrapper over [`crate::store::membership_id`]; collection id first.
#[uniffi::export]
pub fn membership_id(collection_id: String, note_id: String) -> String {
    crate::store::membership_id(&collection_id, &note_id)
}

/// Wrap a `rusqlite` read error as the coarse FFI `SyncError::Store` — the same mapping the
/// write path uses (`open`), so the read surface leaks no per-row SQL detail across the FFI.
fn store_err(e: rusqlite::Error) -> SyncError {
    SyncError::Store(e.to_string())
}

/// Insert `key` into an outbox payload only when `Some` — the SUR-741 partial-patch rule: an
/// absent optional is OMITTED (so the server `merge-duplicates` upsert + the local merge patch
/// only supplied columns), never emitted as an explicit `null` that would clobber a pulled-only
/// column. `Some(v)` sets the column to `v` (incl. `Some("")`).
fn insert_opt<T: Into<Value>>(row: &mut Map<String, Value>, key: &str, val: Option<T>) {
    if let Some(v) = val {
        row.insert(key.to_string(), v.into());
    }
}

/// The columns a host may clear to NULL via `clear_nullable_fields` (SUR-775) — the third state past the
/// SUR-741 keep (`None`) / set (`Some`) pair. Kept to **exactly** the surfc `upsert*` columns
/// written with `?? null` (`isbn`/covers on books; `book_id`/`chapter`/`image_path`/
/// `ink_crop_path`/`source_id` on notes), so a clear stays a wire shape the PWA can also produce
/// and merge (byte-for-byte parity). A `|| ''`/`|| {}`/defaulted column (`page`, `author`,
/// `source_meta`, `source`) can't be NULL under parity — "clearing" those is `Some("")`/the
/// default, not a NULL clear — so they are deliberately absent. `text` (sealed, not a field),
/// `content_tag` (derived), and every pk/timestamp/`deleted` are never clearable.
fn clearable_columns(table: &str) -> &'static [&'static str] {
    match table {
        "books" => &["isbn", "cover_url", "cover_source", "cover_resolved_at"],
        "notes" => &[
            "book_id",
            "chapter",
            "image_path",
            "ink_crop_path",
            "source_id",
        ],
        _ => &[],
    }
}

/// Apply the SUR-775 `clear_nullable_fields` directive to a partial outbox `row`: set each named column to
/// an explicit JSON `null` (the tri-state "clear" — vs an omitted key's "keep"). The null then
/// flows unchanged through `stage_local_write` (→ SQL NULL locally) and the flush (→ server column
/// patched NULL under `merge-duplicates`); both seams are already null-transparent, so this enqueue
/// step is the whole feature. Rejected — nothing staged, mirroring the up-front `source_meta_json`
/// reject — when a name is not a clearable column for `table`, or is ALSO supplied as a value (a
/// set-and-clear contradiction, detected by the key already being present from `insert_opt`).
/// Host-supplied names are NOT echoed into the error (crypto-reviewer: host content must stay out
/// of FFI error strings that may be host-logged); `table` is a caller-side literal, so it is safe.
fn apply_clears(
    table: &str,
    row: &mut Map<String, Value>,
    clear_nullable_fields: &[String],
) -> Result<(), SyncError> {
    let allowed = clearable_columns(table);
    for field in clear_nullable_fields {
        if !allowed.contains(&field.as_str()) {
            return Err(SyncError::Store(format!(
                "a column requested for clearing is not clearable on {table}"
            )));
        }
        if row.contains_key(field) {
            return Err(SyncError::Store(format!(
                "a {table} column is both set and cleared — pass a value or clear it, not both"
            )));
        }
        row.insert(field.clone(), Value::Null);
    }
    Ok(())
}

/// Epoch milliseconds — the PWA `Date.now()` unit the cloud data is stamped in. `SystemTime`
/// before the epoch is impossible on a sane clock; clamp to 0 rather than panic.
///
/// `pub(crate)`: also used by [`reconcile`], whose local-only mutations (rehome, dropped-tag
/// custom ideas) stamp `updated_at`/`created_at` the same way every other write path does.
pub(crate) fn epoch_ms() -> i64 {
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
    fn invalid_import_returns_before_token_network_or_store_locks() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("invalid-import.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = SyncEngine::open(
            db_path.into(),
            "https://x.supabase.co".into(),
            "anon".into(),
            Vault::generate(),
        )
        .unwrap();

        let poison = engine.clone();
        std::thread::spawn(move || {
            let _store = poison.store.lock().unwrap();
            let _client = poison.client.lock().unwrap();
            panic!("poison both operational locks");
        })
        .join()
        .unwrap_err();

        let error = engine.import_merge("not json".into()).unwrap_err();
        assert!(matches!(error, SyncError::InvalidImport(_)));
        assert!(Store::open(db_path)
            .unwrap()
            .outbox_items()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn valid_import_requires_an_access_token_without_staging_anything() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("token-required-import.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = SyncEngine::open(
            db_path.into(),
            "https://x.supabase.co".into(),
            "anon".into(),
            Vault::generate(),
        )
        .unwrap();
        let archive = serde_json::json!({
            "_syntopicon":true,"schemaVersion":19,
            "books":[],"notes":[],"customIdeas":[],"noteLinks":[],
            "lenses":[],"collections":[],"collectionMemberships":[],"noteSignals":[]
        })
        .to_string();

        let error = engine.import_merge(archive).unwrap_err();
        assert!(matches!(error, SyncError::Flush(_)));
        assert!(Store::open(db_path)
            .unwrap()
            .outbox_items()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn search_writes_no_plaintext_note_text_to_disk() {
        // AC #4 (SUR-744): the search path decrypts into memory only — no plaintext note text is
        // ever persisted. Enqueue a note carrying a distinctive plaintext marker (sealed at write),
        // run search (which decrypts + indexes in memory), then scan the on-disk DB bytes: only
        // ciphertext must be there. There is no separate on-disk index — the in-memory `SearchDoc`
        // corpus IS the index (rebuilt per call), so "index target is :memory:" holds by design.
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
        let marker = "plaintextmarkerzzz";
        engine
            .enqueue_note(note_upsert("n1", &format!("a note about {marker}")))
            .unwrap();

        // The search round-trip works (decrypted plaintext is searchable)…
        let hits = engine.search(marker.into(), 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ref_id, "n1");

        // …yet the marker never reaches the SQLite file — only enc:v2 ciphertext is at rest.
        let bytes = std::fs::read(db_path).unwrap();
        let leaked = bytes.windows(marker.len()).any(|w| w == marker.as_bytes());
        assert!(!leaked, "plaintext note text must never be written to disk");
    }

    #[test]
    fn a_note_enqueued_on_one_engine_reads_back_as_plaintext_on_a_second() {
        // AC #6 (SUR-744): enqueue on instance A seals the note; a second instance that receives
        // the row via pull (apply_row = the pull sink, storing ciphertext verbatim) reads it back
        // to plaintext — the same user's MK on both devices. The full networked
        // enqueue→flush→pull path stays in the #[ignore]d Docker suite; this proves the read leg.
        let vault = Vault::generate();
        let dir = tempfile::tempdir().unwrap();
        let a_path = dir.path().join("a.sqlite");
        let a_path = a_path.to_str().unwrap();
        let engine_a = SyncEngine::open(
            a_path.into(),
            "https://x.supabase.co".into(),
            "anon".into(),
            vault.clone(),
        )
        .unwrap();
        engine_a
            .enqueue_note(NoteUpsert {
                book_id: Some("b1".into()),
                tags: vec!["tag".into()],
                ..note_upsert("n1", "cross-device plaintext")
            })
            .unwrap();

        // A's local synced row is ciphertext at rest — exactly what a flush pushes and a pull delivers.
        let a_store = Store::open(a_path).unwrap();
        let row = a_store.get_row("notes", "n1").unwrap().unwrap();
        assert!(row["text"].as_str().unwrap().starts_with("enc:v2:"));

        // Second instance B pulls the row, then reads it back to plaintext.
        let b_store = Store::open_in_memory().unwrap();
        b_store.apply_row("notes", &row).unwrap();
        let note = read::get_note(&b_store, &vault, "n1")
            .unwrap()
            .expect("row reflected on B");
        assert_eq!(note.text.as_deref(), Some("cross-device plaintext"));
        assert!(!note.decrypt_failed);
        assert_eq!(note.book_id.as_deref(), Some("b1"));
        assert_eq!(note.tags, vec!["tag"]);
    }

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
            .enqueue_note(note_upsert("n1", "the secret plaintext"))
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
            .enqueue_note(NoteUpsert {
                book_id: Some("b1".into()),
                page: Some("5".into()),
                tags: vec!["philosophy".into()],
                ..note_upsert("n1", "the secret plaintext")
            })
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
            .enqueue_book(BookUpsert {
                author: Some("A".into()),
                created_at: 1,
                ..book_upsert("b1", "New Title")
            })
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

    // ── SUR-741: widened enqueue_book / enqueue_note authoring surface ──────────

    fn outbox_payload(db_path: &str, idx: usize) -> Value {
        let rows = Store::open(db_path).unwrap().outbox_items().unwrap();
        serde_json::from_str(&rows[idx].3).unwrap()
    }

    #[test]
    fn enqueue_book_authors_cover_fields_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        engine_at(db_path)
            .enqueue_book(BookUpsert {
                author: Some("Aurelius".into()),
                isbn: Some("978-0140449334".into()),
                cover_url: Some("https://cover".into()),
                cover_source: Some("openlibrary".into()),
                cover_resolved_at: Some(1_700_000_000_000),
                created_at: 1,
                ..book_upsert("b1", "Meditations")
            })
            .unwrap();
        let row = Store::open(db_path)
            .unwrap()
            .get_row("books", "b1")
            .unwrap()
            .unwrap();
        assert_eq!(row["isbn"], json!("978-0140449334"));
        assert_eq!(row["cover_url"], json!("https://cover"));
        assert_eq!(row["cover_source"], json!("openlibrary"));
        assert_eq!(row["cover_resolved_at"], json!(1_700_000_000_000_i64));
        // the outbox payload carries the cover too — native can now AUTHOR it, not just preserve it.
        assert_eq!(
            outbox_payload(db_path, 0)["cover_url"],
            json!("https://cover")
        );
    }

    #[test]
    fn enqueue_book_omits_absent_optionals() {
        // None → key OMITTED (patch), never an explicit null that would clobber a server column.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        engine_at(db_path)
            .enqueue_book(BookUpsert {
                created_at: 1,
                ..book_upsert("b1", "T")
            })
            .unwrap();
        let p = outbox_payload(db_path, 0);
        let obj = p.as_object().unwrap();
        for k in [
            "author",
            "isbn",
            "cover_url",
            "cover_source",
            "cover_resolved_at",
        ] {
            assert!(!obj.contains_key(k), "{k} must be omitted when None");
        }
        assert_eq!(p["title"], json!("T"));
    }

    #[test]
    fn enqueue_note_authors_widened_fields_and_parses_source_meta() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        engine_at(db_path)
            .enqueue_note(NoteUpsert {
                book_id: Some("b1".into()),
                page: Some("12".into()),
                tags: vec!["stoicism".into()],
                source: Some("readwise".into()),
                source_id: Some("rw-42".into()),
                source_meta_json: Some(r#"{"highlight_id":"h1","location":42}"#.into()),
                chapter: Some("On Anger".into()),
                image_path: Some("images/n1.jpg".into()),
                ink_crop_path: Some("images/n1-ink.jpg".into()),
                created_at: 7,
                ..note_upsert("n1", "highlighted line")
            })
            .unwrap();
        let p = outbox_payload(db_path, 0);
        assert_eq!(p["source"], json!("readwise"));
        assert_eq!(p["source_id"], json!("rw-42"));
        assert_eq!(p["chapter"], json!("On Anger"));
        assert_eq!(p["image_path"], json!("images/n1.jpg"));
        assert_eq!(p["ink_crop_path"], json!("images/n1-ink.jpg"));
        // source_meta_json is parsed into a JSON OBJECT stored under the `source_meta` column
        // (mirrors the PWA's jsonb column name, not the FFI param name).
        assert!(p["source_meta"].is_object());
        assert_eq!(p["source_meta"]["highlight_id"], json!("h1"));
        assert_eq!(p["source_meta"]["location"], json!(42));
        // Seal-at-write invariants STILL hold on the widened path: text sealed, tag present.
        let text = p["text"].as_str().unwrap();
        assert!(text.starts_with("enc:v2:"), "text is enc:v2 ciphertext");
        assert!(
            !text.contains("highlighted line"),
            "plaintext never reaches the outbox"
        );
        assert!(p["content_tag"].as_str().is_some_and(|t| !t.is_empty()));
    }

    #[test]
    fn enqueue_note_source_none_defaults_to_manual_and_omits_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        engine_at(db_path)
            .enqueue_note(note_upsert("n1", "x"))
            .unwrap();
        let p = outbox_payload(db_path, 0);
        // source is the one always-sent optional (the PWA's `|| 'manual'` / the prior hardcode).
        assert_eq!(p["source"], json!("manual"));
        let obj = p.as_object().unwrap();
        for k in [
            "book_id",
            "page",
            "source_id",
            "source_meta",
            "chapter",
            "image_path",
            "ink_crop_path",
        ] {
            assert!(!obj.contains_key(k), "{k} must be omitted when None");
        }
    }

    #[test]
    fn enqueue_note_invalid_source_meta_json_is_rejected_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        // (a) not JSON at all, and (b) valid JSON but NOT an object — both rejected.
        assert!(
            engine
                .enqueue_note(NoteUpsert {
                    source_meta_json: Some("not json".into()),
                    ..note_upsert("n1", "x")
                })
                .is_err(),
            "invalid JSON rejected"
        );
        assert!(
            engine
                .enqueue_note(NoteUpsert {
                    source_meta_json: Some("[1,2,3]".into()),
                    ..note_upsert("n2", "x")
                })
                .is_err(),
            "non-object JSON rejected"
        );
        // Atomic: the reject happens BEFORE any seal/stage — nothing queued, no local rows.
        let store = Store::open(db_path).unwrap();
        assert_eq!(store.outbox_items().unwrap().len(), 0, "nothing queued");
        assert!(
            store.get_row("notes", "n1").unwrap().is_none(),
            "no local row for n1"
        );
        assert!(
            store.get_row("notes", "n2").unwrap().is_none(),
            "no local row for n2"
        );
    }

    #[test]
    fn enqueue_note_edit_preserves_omitted_columns_but_resets_always_sent_source() {
        // The note analogue of enqueue_book_edit_preserves_pulled_only_columns: a text-only edit
        // OMITS image_path/source_id/chapter (None→omit) so they survive the merge; `source` is the
        // deliberate always-sent exception, so a source-less edit resets it to "manual".
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        {
            let store = Store::open(db_path).unwrap();
            store
                .apply_row(
                    "notes",
                    json!({
                        "id": "n1", "book_id": "b1", "text": "enc:v2:seed", "page": "1",
                        "tags": [], "image_path": "images/keep.jpg", "ink_crop_path": null,
                        "source": "readwise", "source_id": "rw-1", "source_meta": {"k": "v"},
                        "chapter": "C", "content_tag": "tag", "created_at": 1, "updated_at": 1,
                        "deleted": false
                    })
                    .as_object()
                    .unwrap(),
                )
                .unwrap();
        }
        engine_at(db_path)
            .enqueue_note(NoteUpsert {
                book_id: Some("b1".into()),
                created_at: 2,
                ..note_upsert("n1", "edited text")
            })
            .unwrap();
        let row = Store::open(db_path)
            .unwrap()
            .get_row("notes", "n1")
            .unwrap()
            .unwrap();
        assert_eq!(
            row["image_path"],
            json!("images/keep.jpg"),
            "image_path (None→omit) survives"
        );
        assert_eq!(
            row["source_id"],
            json!("rw-1"),
            "source_id (None→omit) survives"
        );
        assert_eq!(row["chapter"], json!("C"), "chapter (None→omit) survives");
        assert_eq!(
            row["source"],
            json!("manual"),
            "source is always sent (None→manual) — a source-less edit resets it (host must pass it)"
        );
    }

    // ── SUR-775: tri-state field clearing (clear_nullable_fields → explicit NULL) ─────────

    #[test]
    fn enqueue_book_clear_nullable_fields_emits_explicit_null_and_leaves_omitted_absent() {
        // The tri-state trio in one call: CLEAR isbn (→ explicit null), SET cover_url, KEEP the
        // rest (omitted). The cleared key must be PRESENT-and-null (the wire signal PostgREST
        // patches to NULL); an omitted optional must stay absent (never a stray null that clobbers).
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        engine_at(db_path)
            .enqueue_book(BookUpsert {
                cover_url: Some("https://cover".into()),
                created_at: 1,
                clear_nullable_fields: vec!["isbn".into()],
                ..book_upsert("b1", "T")
            })
            .unwrap();
        let p = outbox_payload(db_path, 0);
        let obj = p.as_object().unwrap();
        assert!(
            obj.contains_key("isbn"),
            "a cleared column is PRESENT in the payload"
        );
        assert_eq!(
            p["isbn"],
            Value::Null,
            "…as an explicit null (clear), not omitted"
        );
        assert_eq!(
            p["cover_url"],
            json!("https://cover"),
            "a set column still sets"
        );
        for k in ["author", "cover_source", "cover_resolved_at"] {
            assert!(
                !obj.contains_key(k),
                "{k} stays omitted (keep), never a stray null"
            );
        }
        // The local synced row's column is SQL NULL too (reads back as JSON null).
        let row = Store::open(db_path)
            .unwrap()
            .get_row("books", "b1")
            .unwrap()
            .unwrap();
        assert_eq!(row["isbn"], Value::Null, "local column nulled by the clear");
    }

    #[test]
    fn enqueue_note_clear_nullable_fields_nulls_columns_without_touching_the_seal() {
        // Clearing `?? null` note columns (book_id = unlink, chapter) must NOT disturb seal-at-write:
        // text stays enc:v2 ciphertext and content_tag stays present.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        engine_at(db_path)
            .enqueue_note(NoteUpsert {
                clear_nullable_fields: vec!["book_id".into(), "chapter".into()],
                ..note_upsert("n1", "plaintext")
            })
            .unwrap();
        let p = outbox_payload(db_path, 0);
        assert_eq!(
            p["book_id"],
            Value::Null,
            "book_id cleared (note unlinked from its book)"
        );
        assert_eq!(p["chapter"], Value::Null, "chapter cleared");
        let text = p["text"].as_str().unwrap();
        assert!(
            text.starts_with("enc:v2:"),
            "text still sealed — a clear never touches it"
        );
        assert!(
            !text.contains("plaintext"),
            "plaintext never reaches the outbox"
        );
        assert!(
            p["content_tag"].as_str().is_some_and(|t| !t.is_empty()),
            "content_tag intact"
        );
    }

    #[test]
    fn enqueue_note_clearing_a_non_clearable_column_is_rejected_atomically() {
        // `text` (sealed, not a field), `page` (`|| ''` → clear is `Some("")`, not NULL), and an
        // unknown name are all rejected BEFORE any stage — nothing queued, no local row.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        for bad in [
            vec!["text".to_string()],
            vec!["page".to_string()],
            vec!["nonsense".to_string()],
        ] {
            assert!(
                engine
                    .enqueue_note(NoteUpsert {
                        clear_nullable_fields: bad.clone(),
                        ..note_upsert("n1", "x")
                    })
                    .is_err(),
                "clearing {bad:?} must be rejected"
            );
        }
        let store = Store::open(db_path).unwrap();
        assert_eq!(
            store.outbox_items().unwrap().len(),
            0,
            "nothing queued on reject"
        );
        assert!(
            store.get_row("notes", "n1").unwrap().is_none(),
            "no local row staged on reject"
        );
    }

    #[test]
    fn enqueue_book_set_and_clear_of_one_column_is_rejected() {
        // A column supplied as BOTH a value (Some) and in clear_nullable_fields is a contradiction — reject
        // and stage nothing, so a host bug can't silently resolve to one interpretation.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        assert!(
            engine
                .enqueue_book(BookUpsert {
                    isbn: Some("978-x".into()), // isbn set…
                    created_at: 1,
                    clear_nullable_fields: vec!["isbn".into()], // …and cleared → contradiction
                    ..book_upsert("b1", "T")
                })
                .is_err(),
            "set-and-clear of the same column is rejected"
        );
        assert_eq!(
            Store::open(db_path).unwrap().outbox_items().unwrap().len(),
            0,
            "nothing staged on the contradiction"
        );
    }

    #[test]
    fn sync_aborts_with_the_pull_error_and_does_not_swallow_it() {
        // sync() must surface a failure rather than swallow it into an Ok flush. With no access token
        // it errors immediately (no network), before any flush, so this is deterministic; the queued
        // write is left untouched. (The partial-pull-failure abort — a table failing mid-pull — is
        // tested in tests/sync_736_integration.rs via `pull_then_flush` with a stub sink, since a
        // real PostgrestClient can't be made to fail one table but not another.)
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
        engine.enqueue_book(book_upsert("b1", "T")).unwrap();

        assert!(
            engine.sync().is_err(),
            "sync surfaces the pull error rather than proceeding to flush"
        );
        assert_eq!(
            Store::open(db_path).unwrap().outbox_items().unwrap().len(),
            1,
            "the queued write is untouched — sync returned before it could flush"
        );
    }

    // ── SUR-726 enqueue wire shapes (mirror the surfc upsert* payloads) ────────

    fn engine_at(db_path: &str) -> Arc<SyncEngine> {
        SyncEngine::open(
            db_path.into(),
            "https://x.supabase.co".into(),
            "anon".into(),
            Vault::generate(),
        )
        .unwrap()
    }

    /// Test builder for a note upsert — `id` + `plaintext` with every other field at its "unset"
    /// default (no book/page/tags/source/meta, `created_at` 0, live). Override specific fields with
    /// struct-update: `NoteUpsert { book_id: Some("b1".into()), ..note_upsert("n1", "text") }`.
    fn note_upsert(id: &str, plaintext: &str) -> NoteUpsert {
        NoteUpsert {
            id: id.into(),
            book_id: None,
            plaintext: plaintext.into(),
            page: None,
            tags: vec![],
            source: None,
            source_id: None,
            source_meta_json: None,
            chapter: None,
            image_path: None,
            ink_crop_path: None,
            created_at: 0,
            deleted: false,
            clear_nullable_fields: vec![],
        }
    }

    /// Test builder for a book upsert — `id` + `title` with every other field at its "unset"
    /// default (no author/isbn/covers, `created_at` 0, live). Override specific fields with
    /// struct-update: `BookUpsert { author: Some("A".into()), ..book_upsert("b1", "Title") }`.
    fn book_upsert(id: &str, title: &str) -> BookUpsert {
        BookUpsert {
            id: id.into(),
            title: title.into(),
            author: None,
            isbn: None,
            cover_url: None,
            cover_source: None,
            cover_resolved_at: None,
            created_at: 0,
            deleted: false,
            clear_nullable_fields: vec![],
        }
    }

    /// The single queued outbox row (fails if there isn't exactly one). See [`crate::store::OutboxRow`].
    fn only_row(db_path: &str) -> crate::store::OutboxRow {
        let mut rows = Store::open(db_path).unwrap().outbox_items().unwrap();
        assert_eq!(rows.len(), 1, "expected exactly one queued row");
        rows.pop().unwrap()
    }

    #[test]
    fn enqueue_note_signals_keys_note_id_and_omits_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        engine_at(db_path)
            .enqueue_note_signals("nA".into(), 0.5, 3, true, 1, 100, 200, 0.9, 10, false)
            .unwrap();

        let (_, table, record_id, payload_json, _) = only_row(db_path);
        assert_eq!(table, "note_signals");
        assert_eq!(
            record_id.as_deref(),
            Some("nA"),
            "outbox record_id = note_id (the collapse key)"
        );
        let payload: Value = serde_json::from_str(&payload_json).unwrap();
        assert_eq!(payload["note_id"], json!("nA"));
        assert!(
            payload.get("id").is_none(),
            "note_signals payload must NOT carry an `id` key (PostgREST rejects unknown columns)"
        );
        assert_eq!(payload["return_visits"], json!(3));
        assert_eq!(payload["has_annotation"], json!(true));
        assert!(
            Store::open(db_path)
                .unwrap()
                .get_row("note_signals", "nA")
                .unwrap()
                .is_some(),
            "the local synced row is keyed by note_id too"
        );
    }

    #[test]
    fn enqueue_collection_membership_derives_the_deterministic_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        engine_at(db_path)
            .enqueue_collection_membership("noteX".into(), "colY".into(), 10, false)
            .unwrap();

        let (_, table, record_id, payload_json, _) = only_row(db_path);
        assert_eq!(table, "collection_memberships");
        assert_eq!(
            record_id.as_deref(),
            Some("colY:noteX"),
            "id derived collection-first — the host can't supply a divergent one"
        );
        let payload: Value = serde_json::from_str(&payload_json).unwrap();
        assert_eq!(payload["id"], json!("colY:noteX"));
        assert_eq!(payload["note_id"], json!("noteX"));
        assert_eq!(payload["collection_id"], json!("colY"));
    }

    #[test]
    fn enqueue_lens_applies_wire_defaults_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        engine_at(db_path)
            .enqueue_lens(
                "l1".into(),
                "L".into(),
                vec!["a".into(), "b".into()],
                None,
                None,
                10,
                false,
            )
            .unwrap();

        let payload: Value = serde_json::from_str(&only_row(db_path).3).unwrap();
        assert_eq!(
            payload["leaf_ids"],
            json!(["a", "b"]),
            "leaf_ids rides as a JSON array (cloud text[])"
        );
        assert_eq!(payload["combinator"], json!("AND"), "default combinator");
        assert_eq!(payload["threshold"], json!(100), "default threshold");
    }

    #[test]
    fn enqueue_note_link_defaults_relation_type() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        engine_at(db_path)
            .enqueue_note_link("nl1".into(), "from1".into(), "to1".into(), None, 10, false)
            .unwrap();

        let payload: Value = serde_json::from_str(&only_row(db_path).3).unwrap();
        assert_eq!(payload["relation_type"], json!("handwritten_annotation"));
        assert_eq!(payload["from_note_id"], json!("from1"));
        assert_eq!(payload["to_note_id"], json!("to1"));
    }

    #[test]
    fn enqueue_custom_idea_defaults_empty_description() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        engine_at(db_path)
            .enqueue_custom_idea("ci1".into(), "Idea".into(), None, 10, false)
            .unwrap();

        let payload: Value = serde_json::from_str(&only_row(db_path).3).unwrap();
        assert_eq!(payload["name"], json!("Idea"));
        assert_eq!(
            payload["description"],
            json!(""),
            "absent description → \"\""
        );
    }

    #[test]
    fn membership_id_matches_the_oracle_colon_join() {
        // Byte-exact mirror of surfc `membershipId(collectionId, noteId)`: `${collection}:${note}`.
        assert_eq!(membership_id("c1".into(), "n1".into()), "c1:n1");
        assert_eq!(
            membership_id(
                "11111111-1111-4111-8111-111111111111".into(),
                "22222222-2222-4222-8222-222222222222".into()
            ),
            "11111111-1111-4111-8111-111111111111:22222222-2222-4222-8222-222222222222"
        );
        // Argument order is load-bearing (collection FIRST) — the reversed pair differs.
        assert_ne!(
            membership_id("a".into(), "b".into()),
            membership_id("b".into(), "a".into())
        );
    }
}
