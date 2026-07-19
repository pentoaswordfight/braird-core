//! The sync engine (SUR-724 / SUR-659b): outbox enqueue + push/flush + token handoff, proven
//! on notes + books. Native-only (see the `#[cfg]` in `lib.rs`) — its deps (rusqlite, reqwest,
//! tokio) don't compile to wasm32, where the PWA keeps its own `supabase.js` flush.
//!
//! Founder-decided model (resolved at the Phase-2 gates):
//!   - **Seal at plaintext-bearing write.** [`SyncEngine::enqueue_note`] seals `text` (enc:v2,
//!     bound to the note id) and computes `content_tag` FROM PLAINTEXT when plaintext is supplied.
//!     A plaintext-absent patch never calls the Vault and preserves both sealed columns. The outbox
//!     never persists plaintext; flush sends ciphertext as-is (`isEncrypted` guard, mirroring the
//!     JS double-encrypt guard).
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
use crate::store::{synced_table_names, StageExistingWriteError, Store};
use crate::vault::Vault;
use export_import::import::{compute_importance, source_prior};
use http::{user_id_from_jwt, PostgrestClient};
use read::{
    BookRecord, CollectionNoteCount, CollectionRecord, CustomIdeaRecord, IdeaCount, LensRecord,
    NoteLinkRecord, NoteRecord, StoreCounts,
};
use reconcile::BookMergeUndo;

/// Errors that cross the FFI from the sync engine. Coarse like [`crate::CryptoError`]: enough
/// for a host to distinguish store failures, network/sync failures, invalid snapshot input, and
/// an expected note-patch target race, never leaking key material or per-record server detail.
/// Invalid snapshot-input reasons are additionally sanitized so they never echo archive content.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SyncError {
    #[error("store error: {0}")]
    Store(String),
    #[error("flush error: {0}")]
    Flush(String),
    #[error("invalid import: {0}")]
    InvalidImport(String),
    /// A plaintext-free note patch lost its live local target between the host's read and write.
    /// Bulk patch flows may skip this note and re-query; this is not a generic store corruption.
    #[error("note patch requires an existing live row")]
    PatchTargetMissing,
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
/// `plaintext: Some` retains the prior full-write semantics. `plaintext: None` is a narrow patch
/// for an existing live note and deliberately omits sealed text, content tag, and `created_at`
/// (see [`SyncEngine::enqueue_note`]). Named to pair with the read model [`NoteRecord`] —
/// `NoteUpsert` in, `NoteRecord` out.
#[derive(Debug, uniffi::Record)]
pub struct NoteUpsert {
    pub id: String,
    pub book_id: Option<String>,
    pub plaintext: Option<String>,
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

/// One margin to file under a parent note (SUR-952, the "Add the margins" / capture-time handwriting
/// features). The host mints both ids and trims the text; core seals [`text`] under the parent's live
/// book and stages the child note + its parent→child link atomically. [`id`] is the child note's id,
/// [`link_id`] the parent→child `handwritten_annotation` edge's id — both host-supplied so core needs no
/// uuid source, matching the note-link API where the host already owns id generation.
///
/// [`ink_crop_path`] is the storage path of the handwriting's cropped image when the host has one (the
/// capture-time detection path uploads the crop first, mirroring the PWA's `replaceHandwrittenAnnotations`
/// `{text, cropDataUrl}` items → `inkCropPath`). It is plaintext metadata (a storage key, like
/// `image_path` — not sealed), stored verbatim on the child. Android's action-sheet "Add the margins" is
/// text-only (`transcribe_handwriting` returns no crops) and passes `None`.
#[derive(Debug, uniffi::Record)]
pub struct MarginChild {
    pub id: String,
    pub link_id: String,
    pub text: String,
    pub ink_crop_path: Option<String>,
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

    /// Enqueue a full note write or a plaintext-free patch. `plaintext: Some` is the existing
    /// seal-at-write path: the text is sealed here (enc:v2, AAD = note id), `content_tag` is
    /// computed from the plaintext, and the outbox holds only ciphertext plus the tag.
    /// `plaintext: None` is valid only for an existing live local row: it makes no Vault call and
    /// omits `text`, `content_tag`, and `created_at`, preserving their stored bytes. A missing or
    /// already-tombstoned target returns [`SyncError::PatchTargetMissing`]. Bulk patch hosts should
    /// treat that typed error as a normal per-note race, skip the note, and re-query their live
    /// work list. Column NAMES mirror `upsertNote` in surfc `src/supabase.js` exactly.
    ///
    /// WIDENED (SUR-741). Carries the full authoring surface: `source`/`source_id`/`source_meta`/
    /// `chapter`/`image_path`/`ink_crop_path`. `source_meta_json` takes a serialized JSON **object**
    /// string for the `source_meta` jsonb column — the `…Json` suffix is the stated convention for any
    /// param that crosses the FFI as a serialized-JSON string (UniFFI has no jsonb type; the type
    /// alone can't say "this String is JSON, not a scalar"). It is parse-validated up front — invalid
    /// JSON or a non-object → `SyncError::Store` and **nothing is staged** (no seal, no write). None of
    /// the new fields touch the Vault — only a supplied `plaintext` is ever sealed.
    ///
    /// TRI-STATE PATCH SEMANTICS (SUR-741 keep/set + SUR-775 clear): every optional is `None` →
    /// column OMITTED (patch, never clobbers a pulled-only column; see [`SyncEngine::enqueue_book`]).
    /// On a full write, `source` is the one exception — `None` → `"manual"` (the PWA's
    /// `|| 'manual'` / the prior hardcode). On a plaintext-free patch, `source: None` is omitted
    /// like every other optional so it cannot clobber an existing source; `Some` explicitly
    /// updates it. A plaintext-free patch cannot set or clear `book_id`: the content tag includes
    /// that id, and without plaintext the patch can neither recompute the tag nor safely retain it.
    /// Full writes may clear a `?? null` column by naming it in `clear_nullable_fields` (notes:
    /// `book_id`/`chapter`/`image_path`/`ink_crop_path`/`source_id` — [`clearable_columns`]).
    /// `page` is `|| ''`, not NULL-clearable — clearing it is `Some("")`. `text` (sealed) and
    /// `content_tag` (derived) are never clearable; a bad/contradictory clear list is rejected and
    /// nothing is staged. Patch-mode `created_at` is ignored and immutable; both paths stamp a
    /// fresh `updated_at`.
    ///
    /// STALE-TAG EDGE (deliberate, mirrors surfc — do not "fix"): the content_tag bakes in the
    /// note's `book_id`, but the flush repoints `book_id` via `bookIdRemap` after an offline
    /// book-merge. So a merged note's tag reflects the PRE-merge book_id. The JS never recomputes
    /// the tag at flush (`flushOutbox` doesn't touch it), and we CAN'T recompute at flush anyway —
    /// under seal-at-write there is no plaintext left. We leave the tag as-is: the rare
    /// stale-tag-after-offline-merge self-heals on the note's next plaintext-bearing edit (which
    /// re-enqueues with a freshly-computed tag). The tag is never NULL because it is computed
    /// pre-seal, from plaintext.
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
        let mut row = Map::new();
        row.insert("id".into(), json!(id));
        insert_opt(&mut row, "book_id", book_id.clone());
        insert_opt(&mut row, "page", page);
        row.insert("tags".into(), json!(tags));
        insert_opt(&mut row, "source_id", source_id);
        if let Some(v) = source_meta {
            row.insert("source_meta".into(), v);
        }
        insert_opt(&mut row, "chapter", chapter);
        insert_opt(&mut row, "image_path", image_path);
        insert_opt(&mut row, "ink_crop_path", ink_crop_path);
        row.insert("updated_at".into(), json!(now));
        row.insert("deleted".into(), json!(deleted));
        // Validate clears before either stage path. `text`/`content_tag` remain non-clearable.
        apply_clears("notes", &mut row, &clear_nullable_fields)?;
        match plaintext {
            Some(plaintext) => {
                // Full write: seal/tag and keep the PWA's create-time source default.
                let ciphertext = self.vault.encrypt_note(Some(id.clone()), plaintext.clone());
                let content_tag = self.vault.content_tag(plaintext, book_id);
                row.insert("text".into(), json!(ciphertext));
                row.insert("content_tag".into(), json!(content_tag));
                row.insert(
                    "source".into(),
                    json!(source.unwrap_or_else(|| "manual".into())),
                );
                row.insert("created_at".into(), json!(created_at));
                self.stage_write("notes", &id, row)
            }
            None => {
                // Existing-row patch: no Vault call; absent source means keep.
                if book_id.is_some()
                    || clear_nullable_fields
                        .iter()
                        .any(|column| column == "book_id")
                {
                    return Err(SyncError::Store(
                        "plaintext-free note patches cannot change book_id".into(),
                    ));
                }
                insert_opt(&mut row, "source", source);
                self.stage_existing_live_note_patch(&id, row)
            }
        }
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

    /// Atomically replace a note's handwritten margins (SUR-952, the SUR-928 "Add the margins"
    /// feature; the PWA's `replaceHandwrittenAnnotations`). Seals each [`MarginChild::text`] under the
    /// parent's LIVE book, creates the new child notes + their parent→child `handwritten_annotation`
    /// links, and retires the parent's prior handwritten children + edges — every row staged in ONE
    /// transaction ([`Store::stage_local_writes`]).
    ///
    /// This exists because the host's per-item `enqueue_note` + `enqueue_note_link` were two separate
    /// transactions: a crash between them orphaned a child note with no edge, which never converged (a
    /// re-run reads prior children from live edges, so an edgeless orphan is invisible to cleanup). One
    /// transaction closes that window — the whole replace commits or rolls back, and a retry re-does it.
    ///
    /// - Texts are trimmed and blank items dropped IN CORE (the PWA filters before its length check);
    ///   an empty or all-blank [`children`] is a no-op that leaves existing margins intact — guarded
    ///   before any read so it can't error on a missing/locked parent.
    /// - Host-minted ids are validated fail-loud before staging: reusing an existing id is legal ONLY
    ///   for THIS parent's prior handwritten margin (retry/repoint/restore) that NO OTHER live edge
    ///   still touches. A child/link id equal to the parent, a duplicate within the call, a child id
    ///   on any non-margin note or on ANOTHER parent's margin, a reused child id that any foreign
    ///   live edge — any relation type, either direction, and whether or not the child's notes row
    ///   exists locally (pull can skip a never-seen row's tombstone while its edges apply, so a
    ///   fleet-deleted child can stand as dangling live edges; reusing it would resurrect the note
    ///   over its server tombstone) — still references (the retire loop deliberately KEEPS such
    ///   shared/entangled children, so the create loop must never overwrite one; the host mints a
    ///   fresh id instead), or a link id on any non-handwritten edge —
    ///   including this parent's own `related`/`duplicate_of` edges — rejects the WHOLE call; each
    ///   would silently corrupt, steal, or orphan a row the create loop would otherwise overwrite.
    /// - The parent must exist and be live; its CURRENT `book_id` is read here, so children file where
    ///   the parent lives now, not where a host snapshot thought it did.
    /// - Allowed on a decrypt-failed parent: only the NEW child bodies are sealed; the parent's
    ///   ciphertext is never read or re-sealed.
    /// - Children carry `source = "handwritten"`, empty tags, the parent's book, each
    ///   [`MarginChild::ink_crop_path`] verbatim (`None` on Android's text-only path; a storage key on the
    ///   capture-with-crops path), and `created_at`/`updated_at` staggered by index so review order
    ///   survives LWW (the PWA child writes `createdAt: now + i, updatedAt: now + i`). EVERY other
    ///   synced notes column is written as the PWA child literal's explicit cleared shape (empty
    ///   `page`, null `chapter`/`image_path`/`source_id`, `{}` `source_meta`), so an id reuse can
    ///   never resurrect a stale field through the staging merge or the server's column-list upsert.
    ///   Note-links are a random-pk bag (host ids), so
    ///   a re-run with fresh ids adds a new set and tombstones the prior one; a retry re-sending the SAME
    ///   ids is idempotent — a row in the new set is NEVER retired, so the batch can't stage a create then
    ///   a sticky delete for it (SUR-724 collapse) and destroy the margins it meant to preserve. The same
    ///   holds ACROSS batches: a live write in this batch drops any still-queued tombstone for its id from
    ///   a PREVIOUS (offline, un-flushed) replace ([`Store::stage_local_writes`]' resurrect rule), so a
    ///   retry/restore that re-creates a previously retired id flushes live, not as a sticky delete the
    ///   strict-tie LWW pull could never repair.
    /// - HOST CONTRACT (the flip side of that resurrect rule): minted child/link ids are single-shot per
    ///   user-initiated replace. A host must NEVER persist a `children` set and replay it after the
    ///   reader could have touched the results — the replay would faithfully re-assert those exact ids
    ///   live, silently undoing an intervening reader delete or edit of a margin. Replay the same ids
    ///   only within one unacknowledged write attempt; any later retry mints fresh ids.
    /// - Retiring the prior set ALWAYS tombstones this parent's edges — as the STORED row's full
    ///   NOT-NULL shape with its `created_at` preserved (the SUR-942 membership convention; note_links
    ///   has no sparse-PATCH flush fallback, so a bare tombstone would 23502 and wedge the outbox) —
    ///   but tombstones a child NOTE only when it is still a live handwritten note that NO OTHER live
    ///   edge — any relation type, either direction — still touches, and never the parent itself (a
    ///   corrupt self-edge retires the edge only). `note_links` are generic and the reconciler
    ///   preserves/repoints every type, so a margin child can be a repointed regular survivor, a shared
    ///   child of several parents, or carry a non-handwritten edge (e.g. an imported `related` row); in
    ///   each case deleting the note would dangle another edge or destroy a regular note, so only the
    ///   edge is retired.
    /// - The parent's `note_signals.has_annotation` rides the SAME batch (SUR-956; the PWA fires
    ///   `refreshAnnotationSignal` on every margin save, and importance scoring weights the flag at
    ///   0.3): the stored signals row — or a birth-defaults row with the prior derived from the
    ///   parent's `source` — is re-staged WHOLE with `has_annotation: true` and `importance`
    ///   recomputed, preserving earned behavioural counters verbatim. An existing live row already
    ///   flagged is left untouched (the PWA's change-detection no-op: no `updated_at` bump, no
    ///   outbox churn). Dropping the flag to false (a margins-delete recompute) is deliberately NOT
    ///   here — this op never ends with zero margins (SUR-959 owns that path).
    ///
    /// Returns the count of margin children created.
    pub fn replace_handwritten_annotations(
        &self,
        parent_id: String,
        children: Vec<MarginChild>,
    ) -> Result<u32, SyncError> {
        // Trim + drop blank texts BEFORE the emptiness guard (the PWA filters blanks before its length
        // check, useNoteActions.js:123-128) — an all-blank call must preserve existing margins, never
        // retire them behind a set of empty children.
        let children: Vec<MarginChild> = children
            .into_iter()
            .map(|mut c| {
                c.text = c.text.trim().to_string();
                c
            })
            .filter(|c| !c.text.is_empty())
            .collect();
        if children.is_empty() {
            return Ok(0); // PWA parity: an empty (or all-blank) replace leaves existing margins alone
        }
        let store = lock!(self.store);

        // Parent must exist + be live; children inherit its CURRENT book (not a host snapshot).
        let parent = store
            .get_row("notes", &parent_id)
            .map_err(store_err)?
            .filter(|r| !matches!(r.get("deleted"), Some(Value::Bool(true))))
            .ok_or_else(|| {
                SyncError::Store(format!(
                    "replace_handwritten_annotations: parent {parent_id} not found or deleted"
                ))
            })?;
        let book_id = parent
            .get("book_id")
            .and_then(Value::as_str)
            .map(str::to_string);

        // Host-minted ids must be coherent — reject the whole call BEFORE staging anything. Reusing an
        // existing id is legal ONLY for THIS parent's prior handwritten margin (the sanctioned
        // retry/repoint/restore paths); anything else the create loop would overwrite:
        //  - a child id on the parent or any non-margin note re-seals margin text over that row;
        //  - a child id on ANOTHER parent's margin steals + rewrites that margin;
        //  - a link id on any non-handwritten edge — INCLUDING this same parent's own `related`/
        //    `duplicate_of` edge, which only a from-check would wave through — rewrites its
        //    to_note_id/relation_type, corrupting the very generic edges the retire loop preserves;
        //  - a duplicate id within the call leaves a child edgeless (the orphan class this op closes).
        let mut seen_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for child in &children {
            if child.id == parent_id || child.link_id == parent_id {
                return Err(SyncError::Store(
                    "replace_handwritten_annotations: child/link id collides with the parent id"
                        .into(),
                ));
            }
            if !seen_ids.insert(child.id.as_str()) || !seen_ids.insert(child.link_id.as_str()) {
                return Err(SyncError::Store(
                    "replace_handwritten_annotations: duplicate child/link ids in one call".into(),
                ));
            }
            // A reused link id must be this parent's prior HANDWRITTEN edge (live or tombstoned —
            // the restore path re-sends a retired pair). Same-parent alone is NOT enough: this
            // parent's own generic edges must never be repointed into margins.
            let link_row = store
                .get_row("note_links", &child.link_id)
                .map_err(store_err)?;
            if let Some(ref existing) = link_row {
                let same_parent = existing.get("from_note_id").and_then(Value::as_str)
                    == Some(parent_id.as_str());
                let handwritten = existing
                    .get("relation_type")
                    .and_then(Value::as_str)
                    .is_none_or(|r| r == "handwritten_annotation");
                if !(same_parent && handwritten) {
                    return Err(SyncError::Store(
                        "replace_handwritten_annotations: link id collides with a non-margin edge"
                            .into(),
                    ));
                }
            }
            // A reused child id must be a handwritten note that is THIS parent's margin: either a
            // live handwritten edge parent→child exists, or this call's paired link row already
            // points parent→child (the tombstoned-restore pair). A handwritten child of ANOTHER
            // parent — reachable by no edge of ours — is someone else's margin, not reusable.
            // The edge scan runs for EVERY child id, row or no row: pull skips a tombstone for a
            // row this device never had (`pull.rs` incoming-delete-without-local) while edges apply
            // independently (no local FK), so a fleet-tombstoned child can stand locally as live
            // edges with NO notes row — gating the edge checks on row existence would let exactly
            // that id bypass them.
            let live_touching = read::note_links_for_note(&store, &child.id).map_err(store_err)?;
            let own_margin_edge = |l: &NoteLinkRecord| {
                l.from_note_id == parent_id
                    && l.to_note_id == child.id
                    && l.relation_type
                        .as_deref()
                        .unwrap_or("handwritten_annotation")
                        == "handwritten_annotation"
            };
            if let Some(existing) = store.get_row("notes", &child.id).map_err(store_err)? {
                if existing.get("source").and_then(Value::as_str) != Some("handwritten") {
                    return Err(SyncError::Store(
                        "replace_handwritten_annotations: child id collides with an existing non-margin note"
                            .into(),
                    ));
                }
                let paired = link_row.as_ref().is_some_and(|l| {
                    l.get("to_note_id").and_then(Value::as_str) == Some(child.id.as_str())
                });
                if !(paired || live_touching.iter().any(own_margin_edge)) {
                    return Err(SyncError::Store(
                        "replace_handwritten_annotations: child id collides with another parent's margin"
                            .into(),
                    ));
                }
            }
            // Ownership is NOT sufficiency: the retire loop deliberately KEEPS a child that any
            // other live edge still touches (a shared dedupe survivor, a generic `related` row —
            // even this parent's own), and the create loop below OVERWRITES the notes row for
            // every id in the set. Accepting the reuse would rewrite the text/book/source that the
            // preserved edge still renders — and when the notes row is locally ABSENT (the dangling
            // state above), the create would additionally resurrect a fleet-deleted note over its
            // server tombstone. So while any foreign live edge touches the child id, reuse rejects
            // and the host mints a fresh child id instead. (An id whose only live edges are this
            // parent's own margin edges stays legal with or without the row — the row-less form is
            // the same restore, just seen from a device that skipped the note's tombstone.)
            if live_touching.iter().any(|l| !own_margin_edge(l)) {
                return Err(SyncError::Store(
                    "replace_handwritten_annotations: reused child id is still referenced by another live edge"
                        .into(),
                ));
            }
        }

        // Prior handwritten children of THIS parent (parent is `from`), captured before any write —
        // full records, because retiring an edge needs its whole NOT-NULL row shape (see the tombstone
        // comment below).
        let old_edges: Vec<NoteLinkRecord> = read::note_links_for_note(&store, &parent_id)
            .map_err(store_err)?
            .into_iter()
            .filter(|l| {
                l.from_note_id == parent_id
                    && l.relation_type
                        .as_deref()
                        .unwrap_or("handwritten_annotation")
                        == "handwritten_annotation"
            })
            .collect();

        let now = epoch_ms();
        let mut writes: Vec<(&str, String, Map<String, Value>)> = Vec::new();

        // Create new children + links FIRST (staggered created_at preserves review order). Ordering is
        // create-before-retire so the outbox flush duplicates-not-loses if interrupted, though the local
        // transaction is atomic regardless.
        for (i, child) in children.iter().enumerate() {
            let created = now + i as i64;
            let ciphertext = self
                .vault
                .encrypt_note(Some(child.id.clone()), child.text.clone());
            let content_tag = self.vault.content_tag(child.text.clone(), book_id.clone());
            // EVERY synced notes column gets an EXPLICIT value — MarginChild-owned, or the PWA
            // child literal's cleared shape (`useNoteActions.js`: `page: ''`, `chapter: null`,
            // `imagePath: null`, `sourceId: null`, `sourceMeta: {}`) — never an omitted key.
            // Staging MERGES the partial onto any existing row, and the server upsert only sets the
            // columns the payload names, so a restore/retry reusing a prior child id would
            // otherwise resurrect that row's stale fields — a crop or whole-page photo rendered
            // against unrelated text, a stale source/page/chapter riding a text-only margin —
            // locally AND on the cloud row. The schema-completeness test pins this shape to
            // `vendored/schema/sync-schema.json`, so a new synced notes column fails the build
            // until it is covered here.
            let mut note = Map::new();
            note.insert("id".into(), json!(child.id));
            note.insert("book_id".into(), json!(book_id.clone()));
            note.insert("text".into(), json!(ciphertext));
            note.insert("content_tag".into(), json!(content_tag));
            note.insert("tags".into(), json!(Vec::<String>::new()));
            note.insert("source".into(), json!("handwritten"));
            note.insert("page".into(), json!(""));
            note.insert("chapter".into(), Value::Null);
            note.insert("image_path".into(), Value::Null);
            note.insert("ink_crop_path".into(), json!(child.ink_crop_path.clone()));
            note.insert("source_id".into(), Value::Null);
            note.insert("source_meta".into(), json!({}));
            // created_at staggered by index (review order survives LWW); updated_at mirrors it on
            // both rows — the PWA writes `createdAt: now + i, updatedAt: now + i` for the child AND
            // its edge.
            note.insert("created_at".into(), json!(created));
            note.insert("updated_at".into(), json!(created));
            note.insert("deleted".into(), json!(false));
            writes.push(("notes", child.id.clone(), note));

            let mut link = Map::new();
            link.insert("id".into(), json!(child.link_id));
            link.insert("from_note_id".into(), json!(parent_id));
            link.insert("to_note_id".into(), json!(child.id));
            link.insert("relation_type".into(), json!("handwritten_annotation"));
            link.insert("created_at".into(), json!(created));
            link.insert("updated_at".into(), json!(created));
            link.insert("deleted".into(), json!(false));
            writes.push(("note_links", child.link_id.clone(), link));
        }

        // THEN retire the prior set. ALWAYS tombstone this parent's edges, but tombstone the CHILD NOTE
        // only when it's genuinely a spent, unentangled margin: a LIVE `source="handwritten"` note that
        // NO OTHER live edge — of ANY relation type, in EITHER direction — still touches. `note_links`
        // are generic (`normalize_note_link` accepts any non-empty relation) and the reconciler
        // preserves/repoints every type, so a margin child can also carry a non-handwritten edge (e.g. a
        // snapshot-imported `related` row) or be a shared survivor of a content-dedupe merge. Deleting
        // the note in any of those cases would leave another live edge dangling at a tombstone or destroy
        // a regular survivor — so we drop only the edge and keep the note. Reads see pre-op state (the
        // batch hasn't committed), so exclude our OWN retire set when checking for other references.
        let retiring: std::collections::HashSet<String> =
            old_edges.iter().map(|e| e.id.clone()).collect();
        // Ids the create loop just staged LIVE. An idempotent retry re-sends the same MarginChild ids, so
        // a prior child/edge sits in BOTH old_edges and the new set; retiring it here would stage a
        // tombstone for a row this same batch just created, and the outbox collapse makes deletes sticky
        // (SUR-724 "delete wins, never resurrect") — turning the margins we meant to preserve into
        // tombstones. Never retire a row that's part of the new set.
        let new_child_ids: std::collections::HashSet<&str> =
            children.iter().map(|c| c.id.as_str()).collect();
        let new_link_ids: std::collections::HashSet<&str> =
            children.iter().map(|c| c.link_id.as_str()).collect();
        let mut tombstoned: std::collections::HashSet<String> = std::collections::HashSet::new();
        for edge in &old_edges {
            // Tombstone this parent's edge UNLESS the new set re-creates/repoints it live (the create loop
            // keeps it). But do NOT `continue` on a reused edge: the create loop may have repointed that
            // edge to a NEW child, orphaning THIS old child — so the old-child retirement check below must
            // still run (skipping only the edge tombstone, never the child cleanup).
            if !new_link_ids.contains(edge.id.as_str()) {
                // The tombstone carries the STORED row's full NOT-NULL shape (the SUR-942 membership
                // convention; the PWA cloudWrites `{...l, deleted: 1}`): the server's from/to/created_at
                // are NOT NULL with no default, and — unlike notes — note_links has no sparse-payload
                // PATCH fallback, so a bare `{id, deleted}` tombstone would 23502 on every flush and
                // wedge the outbox forever. `created_at` is the stored edge's, preserved (SUR-942).
                let mut edge_tomb = Map::new();
                edge_tomb.insert("id".into(), json!(edge.id));
                edge_tomb.insert("from_note_id".into(), json!(edge.from_note_id));
                edge_tomb.insert("to_note_id".into(), json!(edge.to_note_id));
                edge_tomb.insert(
                    "relation_type".into(),
                    json!(edge
                        .relation_type
                        .clone()
                        .unwrap_or_else(|| "handwritten_annotation".into())),
                );
                edge_tomb.insert("created_at".into(), json!(edge.created_at));
                edge_tomb.insert("updated_at".into(), json!(now));
                edge_tomb.insert("deleted".into(), json!(true));
                writes.push(("note_links", edge.id.clone(), edge_tomb));
            }

            let child_id = &edge.to_note_id;
            if tombstoned.contains(child_id) {
                continue; // this parent has >1 edge to the same child — tombstone the note once
            }
            if new_child_ids.contains(child_id.as_str()) {
                continue; // the new set re-creates this child live — never tombstone it (retry preservation)
            }
            if child_id == &parent_id {
                continue; // a corrupt p→p self-edge: retire the edge above, never the parent itself
            }
            // Target still a live handwritten child? (A repointed edge can sit on a regular survivor.)
            let is_live_handwritten = store
                .get_row("notes", child_id)
                .map_err(store_err)?
                .filter(|r| !matches!(r.get("deleted"), Some(Value::Bool(true))))
                .and_then(|r| r.get("source").and_then(Value::as_str).map(str::to_string))
                .as_deref()
                == Some("handwritten");
            // Still touched by ANY other live edge we are NOT retiring (any relation, either direction)?
            // If so, deleting the note would dangle that edge — keep the note, drop only our edge.
            let still_referenced = read::note_links_for_note(&store, child_id)
                .map_err(store_err)?
                .iter()
                .any(|l| !retiring.contains(&l.id));
            if is_live_handwritten && !still_referenced {
                // Sparse is CORRECT here (unlike the edge tombstone above): a text-less notes payload
                // dispatches through the flush's notes-only PATCH fallback (push.rs `patch_group`, the
                // SUR-921 sparse-retag path), which updates the existing cloud row without an INSERT
                // shape — so no NOT-NULL trap. Local merge keeps the stored row's other columns under
                // the tombstone, matching the PWA's `{...child, deleted: 1}`.
                let mut note_tomb = Map::new();
                note_tomb.insert("id".into(), json!(child_id));
                note_tomb.insert("deleted".into(), json!(true));
                note_tomb.insert("updated_at".into(), json!(now));
                writes.push(("notes", child_id.clone(), note_tomb));
                tombstoned.insert(child_id.clone());
            }
        }

        // SUR-956: the op ends with ≥1 live margin, so the parent's `note_signals.has_annotation`
        // must ride the same batch (the PWA's `record-annotation` parity behavior). Read-merge-stage
        // IN CORE: `enqueue_note_signals` is a blind whole-row LWW write a host must never point at
        // this (it would clobber earned counters), and the FFI has no signals read. Mirrors the
        // PWA's `applyNoteSignal`:
        //  - change-detection no-op — an existing LIVE row already flagged stays untouched;
        //  - otherwise the FULL row is staged: existing columns verbatim, or birth defaults with
        //    the prior from the parent's `source` (`freshNoteSignals`). Full-row on purpose — the
        //    enqueued payload is the staged partial, and the server upsert only sets the columns
        //    the payload names, so a minimal `{has_annotation}` payload would leave a NEW cloud
        //    row's other columns to server defaults while the PWA enqueues whole rows;
        //  - `deleted: false` — a live write, so `stage_local_writes` drops any queued signals
        //    tombstone (resurrect rule).
        // The recompute-to-FALSE half lives in the future margins-delete path (SUR-959), not here.
        let signals = store
            .get_row("note_signals", &parent_id)
            .map_err(store_err)?;
        let already_flagged = signals.as_ref().is_some_and(|s| {
            matches!(s.get("has_annotation"), Some(Value::Bool(true)))
                && !matches!(s.get("deleted"), Some(Value::Bool(true)))
        });
        if !already_flagged {
            let existing = signals.unwrap_or_default();
            let prior = existing
                .get("source_prior")
                .and_then(Value::as_f64)
                .unwrap_or_else(|| source_prior(parent.get("source").and_then(Value::as_str)));
            let int_or = |field: &str, default: i64| {
                existing
                    .get(field)
                    .and_then(Value::as_i64)
                    .unwrap_or(default)
            };
            let return_visits = int_or("return_visits", 0);
            let stitch_spawns = int_or("stitch_spawns", 0);
            let mut sig = Map::new();
            sig.insert("note_id".into(), json!(parent_id));
            sig.insert("source_prior".into(), json!(prior));
            sig.insert("return_visits".into(), json!(return_visits));
            sig.insert("has_annotation".into(), json!(true));
            sig.insert("stitch_spawns".into(), json!(stitch_spawns));
            sig.insert(
                "exposure_recency_at".into(),
                json!(int_or("exposure_recency_at", 0)),
            );
            sig.insert(
                "engagement_recency_at".into(),
                json!(int_or("engagement_recency_at", 0)),
            );
            sig.insert(
                "importance".into(),
                json!(compute_importance(
                    prior,
                    return_visits,
                    true,
                    stitch_spawns
                )),
            );
            sig.insert("created_at".into(), json!(int_or("created_at", now)));
            sig.insert("updated_at".into(), json!(now));
            sig.insert("deleted".into(), json!(false));
            writes.push(("note_signals", parent_id.clone(), sig));
        }

        store.stage_local_writes(writes, now).map_err(store_err)?;
        Ok(children.len() as u32)
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
        // A re-add (deleted=false) must RESURRECT: atomically drop any un-flushed tombstone for this
        // deterministic membership id, so the outbox collapse can't eat the un-delete. Without it, an
        // offline (or between-flush) file→off→on collapses to a sticky `deleted:true` (SUR-724 "delete
        // wins") and the note is silently dropped from the collection on push, while the local mirror
        // still shows it filed (SUR-940). A soft-delete (deleted=true) stays on the sticky path — a
        // delete SHOULD win. Memberships are the only reachable resurrection case:
        //  - note_links use a random per-edge pk (a re-add is a new row, not a same-pk un-delete);
        //  - custom_ideas re-create with a fresh host uuid (a new row, same reason) — the only same-id
        //    re-create is the reconcile pass's deterministic `cidea_sur597_*`, which runs POST-PULL
        //    (across a flush), so the prior soft-delete is already pushed and its tombstone gone;
        //  - collections/lenses have no re-add-after-delete host UI yet.
        // Extend the split here if any of those grows a same-pk, same-batch re-add path.
        if deleted {
            // A tombstone PRESERVES the membership's filed-at `created_at`, mirroring surfc's
            // `removeNoteFromCollection` → `softDeleteMembershipRows` (which tombstones the *stored*
            // row, `{ ...m, deleted: 1 }`, not a reconstruct-from-ids): the host can't supply it
            // (`collection_ids_for_note` exposes no timestamp) and the pushed payload IS the outbox
            // partial (server column NOT NULL), so read it here and overwrite the host stand-in;
            // fall back to the host value when no row exists (parity with the PWA's
            // `?? { createdAt: now }`). The same preserve reconcile's `repoint_memberships` does. The
            // lookup and the stage share ONE held guard — `SyncEngine` is called from any host thread,
            // and a released-then-reacquired lock would let a concurrent re-add stage `deleted:false`
            // between them, with this tombstone landing after it and collapsing sticky-deleted (the
            // SUR-940 loss, re-opened as a race). `stage_local_write` on the held guard does not
            // re-lock.
            let store = lock!(self.store);
            if let Some(existing) = store
                .get_row("collection_memberships", &id)
                .map_err(|e| SyncError::Store(e.to_string()))?
                .and_then(|r| r.get("created_at").and_then(Value::as_i64))
            {
                row.insert("created_at".into(), json!(existing));
            }
            store
                .stage_local_write("collection_memberships", &id, row, now)
                .map_err(|e| SyncError::Store(e.to_string()))
        } else {
            lock!(self.store)
                .stage_local_write_resurrecting("collection_memberships", &id, row, now)
                .map_err(|e| SyncError::Store(e.to_string()))
        }
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
    ///
    /// **Security:** the returned string contains plaintext note text. The host must never log it
    /// or attach it to telemetry/crash reports, and must write it only through a restrictively
    /// protected temporary file on the destination filesystem before verified atomic install and
    /// cleanup. See `docs/snapshots.md` for the durable host-storage contract.
    pub fn export_snapshot(&self) -> Result<String, SyncError> {
        let store = lock!(self.store);
        export_import::build_snapshot_at(&store, &self.vault, epoch_ms())
    }

    /// Protectively merge a plaintext PWA snapshot into the local mirror. Parsing happens before
    /// any operational lock or token check. A valid archive then performs a clean all-table pull,
    /// direct server LWW preflight, in-core note sealing, and one atomic local+outbox batch. The
    /// staged batch is deliberately not flushed; the next normal [`SyncEngine::sync`] uploads it.
    ///
    /// **Security:** `json` contains plaintext note text. The host must source it only from
    /// restrictively protected storage, never log or report it, and remove temporary plaintext on
    /// every success/failure/cancellation path. See `docs/snapshots.md` for the full contract.
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

    // ── relation reads (SUR-923) ─────────────────────────────────────────────
    // Extension #3 of the read surface: the membership + note-link relations, traversed in both
    // directions, for the note action sheets (Add-to-collection, Add-the-margins) and the Lexicon
    // manage flows (collection delete cascade, per-collection counts). Same soft-delete-excluding
    // contract; no decryption anywhere — no note text is involved.

    /// Ids of the live collections containing the note (SUR-923) — the AddToCollectionSheet's
    /// member set. Live membership rows only; no collection-liveness or notes join (the PWA
    /// `memberIds` oracle — the sheet filters its rendered rows to live collections itself).
    pub fn collection_ids_for_note(&self, note_id: String) -> Result<Vec<String>, SyncError> {
        let store = lock!(self.store);
        read::collection_ids_for_note(&store, &note_id).map_err(store_err)
    }

    /// Live note-link edges where the note is either endpoint (SUR-923) — one hop, both
    /// directions; the host filters direction ("children of this parent" = the note is the
    /// link's `from` side; "parent of this child" = it is the `to` side) and by relation type
    /// (Add-the-margins / parent-aware sheet options).
    pub fn note_links_for_note(&self, note_id: String) -> Result<Vec<NoteLinkRecord>, SyncError> {
        let store = lock!(self.store);
        read::note_links_for_note(&store, &note_id).map_err(store_err)
    }

    /// Live member note ids of a collection (SUR-923) — feeds the host-side collection-delete
    /// cascade (which must see memberships of already-deleted notes, so: deliberately no notes
    /// join) and the collection-scoped note list (which re-checks note liveness host-side, as
    /// the PWA's `notesInCollection` does).
    pub fn note_ids_for_collection(&self, collection_id: String) -> Result<Vec<String>, SyncError> {
        let store = lock!(self.store);
        read::note_ids_for_collection(&store, &collection_id).map_err(store_err)
    }

    /// Per-collection live-note counts (SUR-923) — the Lexicon Collections tab subtitles, one
    /// row per collection sorted by collection id, only counts ≥ 1. Counts memberships
    /// whose note is present and live — a founder-decided (2026-07-17) divergence from the PWA's
    /// raw membership tally, keeping the subtitle consistent with the scoped note list.
    pub fn collection_note_counts(&self) -> Result<Vec<CollectionNoteCount>, SyncError> {
        let store = lock!(self.store);
        read::collection_note_counts(&store).map_err(store_err)
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

    fn stage_existing_live_note_patch(
        &self,
        record_id: &str,
        row: Map<String, Value>,
    ) -> Result<(), SyncError> {
        lock!(self.store)
            .stage_local_write_existing_live("notes", record_id, row, epoch_ms())
            .map_err(|error| match error {
                StageExistingWriteError::TargetMissing => SyncError::PatchTargetMissing,
                StageExistingWriteError::Sql(error) => SyncError::Store(error.to_string()),
            })
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
    fn note_patch_preserves_sealed_and_immutable_fields_and_queues_a_narrow_payload() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let original_text = "enc:v2:foreign-ciphertext";
        let original_tag = "foreign-content-tag";
        {
            let store = Store::open(db_path).unwrap();
            store
                .apply_row(
                    "notes",
                    json!({
                        "id": "n1",
                        "text": original_text,
                        "tags": ["before"],
                        "source": "kindle",
                        "content_tag": original_tag,
                        "created_at": 10,
                        "updated_at": 1,
                        "deleted": false
                    })
                    .as_object()
                    .unwrap(),
                )
                .unwrap();
        }
        let engine = engine_at(db_path);
        assert!(
            engine
                .get_note("n1".into())
                .unwrap()
                .unwrap()
                .decrypt_failed,
            "the seeded foreign ciphertext must be undecryptable before the patch"
        );

        engine
            .enqueue_note(NoteUpsert {
                tags: vec!["after".into()],
                created_at: 999,
                ..note_patch("n1")
            })
            .unwrap();

        let store = Store::open(db_path).unwrap();
        let row = store.get_row("notes", "n1").unwrap().unwrap();
        assert_eq!(row["text"], json!(original_text));
        assert_eq!(row["content_tag"], json!(original_tag));
        assert_eq!(row["source"], json!("kindle"));
        assert_eq!(row["created_at"], json!(10));
        assert_eq!(row["tags"], json!(["after"]));
        assert!(row["updated_at"].as_i64().unwrap() > 1);
        assert!(
            engine
                .get_note("n1".into())
                .unwrap()
                .unwrap()
                .decrypt_failed,
            "the patch must not replace foreign ciphertext with synthesized plaintext"
        );

        let queued = store.outbox_items().unwrap();
        assert_eq!(queued.len(), 1);
        let payload: Value = serde_json::from_str(&queued[0].3).unwrap();
        let object = payload.as_object().unwrap();
        for key in ["text", "content_tag", "source", "created_at"] {
            assert!(
                !object.contains_key(key),
                "{key} must be absent from a plaintext-free patch"
            );
        }
        assert_eq!(payload["tags"], json!(["after"]));
        assert!(payload["updated_at"].as_i64().unwrap() > 1);
    }

    #[test]
    fn note_patch_rejects_missing_and_tombstoned_targets_with_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);

        let missing = engine.enqueue_note(note_patch("missing")).unwrap_err();
        assert!(matches!(&missing, SyncError::PatchTargetMissing));
        assert_eq!(
            missing.to_string(),
            "note patch requires an existing live row"
        );
        assert!(Store::open(db_path)
            .unwrap()
            .outbox_items()
            .unwrap()
            .is_empty());
        assert!(Store::open(db_path)
            .unwrap()
            .get_row("notes", "missing")
            .unwrap()
            .is_none());

        {
            let store = Store::open(db_path).unwrap();
            store
                .apply_row(
                    "notes",
                    json!({
                        "id": "dead",
                        "text": "enc:v2:foreign-ciphertext",
                        "tags": ["before"],
                        "source": "kindle",
                        "content_tag": "foreign-content-tag",
                        "created_at": 10,
                        "updated_at": 1,
                        "deleted": true
                    })
                    .as_object()
                    .unwrap(),
                )
                .unwrap();
        }
        let before = Store::open(db_path)
            .unwrap()
            .get_row("notes", "dead")
            .unwrap()
            .unwrap();
        let tombstoned = engine
            .enqueue_note(NoteUpsert {
                tags: vec!["after".into()],
                ..note_patch("dead")
            })
            .unwrap_err();
        assert!(matches!(&tombstoned, SyncError::PatchTargetMissing));
        assert_eq!(
            tombstoned.to_string(),
            "note patch requires an existing live row"
        );
        let store = Store::open(db_path).unwrap();
        assert_eq!(store.get_row("notes", "dead").unwrap().unwrap(), before);
        assert!(store.outbox_items().unwrap().is_empty());
    }

    #[test]
    fn note_patch_can_tombstone_a_live_undecryptable_note_without_resealing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let original_text = "enc:v2:foreign-ciphertext";
        let original_tag = "foreign-content-tag";
        {
            let store = Store::open(db_path).unwrap();
            store
                .apply_row(
                    "notes",
                    json!({
                        "id": "n1",
                        "text": original_text,
                        "tags": ["before"],
                        "source": "kindle",
                        "content_tag": original_tag,
                        "created_at": 10,
                        "updated_at": 1,
                        "deleted": false
                    })
                    .as_object()
                    .unwrap(),
                )
                .unwrap();
        }
        let engine = engine_at(db_path);
        assert!(
            engine
                .get_note("n1".into())
                .unwrap()
                .unwrap()
                .decrypt_failed
        );

        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_patch("n1")
            })
            .unwrap();

        let store = Store::open(db_path).unwrap();
        let row = store.get_row("notes", "n1").unwrap().unwrap();
        assert_eq!(row["deleted"], json!(true));
        assert_eq!(row["text"], json!(original_text));
        assert_eq!(row["content_tag"], json!(original_tag));
        let payload: Value = serde_json::from_str(&store.outbox_items().unwrap()[0].3).unwrap();
        assert_eq!(payload["deleted"], json!(true));
        assert!(payload.get("text").is_none());
        assert!(payload.get("content_tag").is_none());
    }

    #[test]
    fn note_patch_rejects_book_moves_that_would_stale_the_content_tag() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        {
            let store = Store::open(db_path).unwrap();
            for id in ["set-book", "clear-book"] {
                store
                    .apply_row(
                        "notes",
                        json!({
                            "id": id,
                            "book_id": "b1",
                            "text": "enc:v2:foreign-ciphertext",
                            "tags": ["before"],
                            "source": "kindle",
                            "content_tag": "book-b1-content-tag",
                            "created_at": 10,
                            "updated_at": 1,
                            "deleted": false
                        })
                        .as_object()
                        .unwrap(),
                    )
                    .unwrap();
            }
        }
        let engine = engine_at(db_path);

        let set_error = engine
            .enqueue_note(NoteUpsert {
                book_id: Some("b2".into()),
                tags: vec!["after".into()],
                ..note_patch("set-book")
            })
            .unwrap_err();
        assert!(matches!(set_error, SyncError::Store(_)));
        assert_eq!(
            set_error.to_string(),
            "store error: plaintext-free note patches cannot change book_id"
        );

        let clear_error = engine
            .enqueue_note(NoteUpsert {
                tags: vec!["after".into()],
                clear_nullable_fields: vec!["book_id".into()],
                ..note_patch("clear-book")
            })
            .unwrap_err();
        assert!(matches!(clear_error, SyncError::Store(_)));
        assert_eq!(
            clear_error.to_string(),
            "store error: plaintext-free note patches cannot change book_id"
        );

        let store = Store::open(db_path).unwrap();
        for id in ["set-book", "clear-book"] {
            let row = store.get_row("notes", id).unwrap().unwrap();
            assert_eq!(row["book_id"], json!("b1"));
            assert_eq!(row["content_tag"], json!("book-b1-content-tag"));
            assert_eq!(row["tags"], json!(["before"]));
        }
        assert!(store.outbox_items().unwrap().is_empty());
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
            plaintext: Some(plaintext.into()),
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

    fn note_patch(id: &str) -> NoteUpsert {
        NoteUpsert {
            id: id.into(),
            book_id: None,
            plaintext: None,
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

    /// Collapse the store's whole outbox exactly as the flush would, and return the pushed `deleted`
    /// field of the sole `collection_memberships` upsert. The tests assert the COLLAPSED OUTBOX (what
    /// push sends) rather than a `collection_ids_for_note` read, because the local synced mirror is
    /// correct under the SUR-940 bug — only the pushed payload was wrong.
    fn collapsed_membership_payload(db_path: &str) -> Map<String, Value> {
        let items: Vec<crate::sync::outbox::OutboxItem> = Store::open(db_path)
            .unwrap()
            .outbox_items()
            .unwrap()
            .into_iter()
            .map(|(id, table_name, record_id, payload, created_at)| {
                crate::sync::outbox::OutboxItem {
                    id,
                    table_name,
                    record_id,
                    payload: serde_json::from_str(&payload).unwrap(),
                    created_at,
                }
            })
            .collect();
        crate::sync::outbox::collapse(items, &std::collections::BTreeMap::new())
            .iter()
            .find(|c| c.table == "collection_memberships")
            .expect("a membership upsert is queued")
            .payload
            .clone()
    }

    fn collapsed_membership_deleted(db_path: &str) -> Value {
        collapsed_membership_payload(db_path)["deleted"].clone()
    }

    /// The `created_at` the local synced mirror holds for the sole membership (what a reconcile or an
    /// export would read), distinct from the pushed outbox payload.
    fn membership_mirror_created_at(db_path: &str, id: &str) -> i64 {
        Store::open(db_path)
            .unwrap()
            .get_row("collection_memberships", id)
            .unwrap()
            .expect("the membership row exists in the local mirror")
            .get("created_at")
            .and_then(Value::as_i64)
            .expect("created_at is a number")
    }

    #[test]
    fn membership_re_add_resurrects_past_the_outbox_sticky_delete() {
        // SUR-940: file → toggle-off → toggle-on of the same membership within one un-flushed batch.
        // The re-add must drop the queued tombstone so the collapsed push payload is deleted=false —
        // otherwise the note is silently dropped from the collection server-side.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);

        engine
            .enqueue_collection_membership("n1".into(), "c1".into(), 100, false)
            .unwrap(); // file
        engine
            .enqueue_collection_membership("n1".into(), "c1".into(), 100, true)
            .unwrap(); // off
        engine
            .enqueue_collection_membership("n1".into(), "c1".into(), 100, false)
            .unwrap(); // on

        assert_eq!(
            collapsed_membership_deleted(db_path),
            json!(false),
            "re-add wins: the collapsed push payload un-deletes the membership, not a sticky tombstone",
        );
    }

    #[test]
    fn membership_delete_without_re_add_still_wins_the_collapse() {
        // The OTHER branch (guards against a refactor routing the soft-delete through the resurrecting
        // stage too): file → toggle-off with NO re-add must collapse to a sticky deleted=true — a
        // genuine removal must still push a tombstone.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);

        engine
            .enqueue_collection_membership("n1".into(), "c1".into(), 100, false)
            .unwrap(); // file
        engine
            .enqueue_collection_membership("n1".into(), "c1".into(), 100, true)
            .unwrap(); // off

        assert_eq!(
            collapsed_membership_deleted(db_path),
            json!(true),
            "a genuine remove must still win the collapse — the sticky-delete is intact",
        );
    }

    #[test]
    fn membership_tombstone_preserves_the_filed_at_created_at() {
        // The host toggle-off can't supply the original `created_at` (SUR-927): `collection_ids_for_note`
        // exposes no timestamp, so it passes the wall clock. Core must preserve the filed-at value on
        // the tombstone (parity with surfc's `removeNoteFromCollection`, which tombstones the stored
        // row). Both the local mirror AND the pushed outbox payload must carry it — the server column
        // is NOT NULL and the pushed value is what other devices see.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);

        engine
            .enqueue_collection_membership("n1".into(), "c1".into(), 100, false)
            .unwrap(); // filed at 100
        engine
            .enqueue_collection_membership("n1".into(), "c1".into(), 200, true)
            .unwrap(); // removed at 200 (host clock)

        assert_eq!(
            membership_mirror_created_at(db_path, "c1:n1"),
            100,
            "the local mirror keeps the filed-at created_at, not the removal clock",
        );
        assert_eq!(
            collapsed_membership_payload(db_path)["created_at"],
            json!(100),
            "the pushed tombstone carries the filed-at created_at, not the removal clock",
        );
    }

    #[test]
    fn membership_tombstone_of_absent_row_falls_back_to_the_host_clock() {
        // No prior row (remove of something never filed locally — e.g. removed on another device):
        // there is nothing to preserve, so the host clock stands in and the NOT-NULL column is still
        // satisfied. Parity with the PWA's `?? { createdAt: now }` fallback.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        engine_at(db_path)
            .enqueue_collection_membership("n1".into(), "c1".into(), 200, true)
            .unwrap();

        assert_eq!(membership_mirror_created_at(db_path, "c1:n1"), 200);
        assert_eq!(
            collapsed_membership_payload(db_path)["created_at"],
            json!(200),
        );
    }

    #[test]
    fn membership_add_off_re_add_converges_to_one_live_row_with_its_created_at() {
        // file → toggle-off → toggle-on, all in one un-flushed batch (SUR-940 resurrection + the
        // SUR-927 created_at preserve together): the collapse must yield ONE live row, and its
        // created_at is intact end to end. An active re-add re-stamps the host clock (parity with
        // `addNoteToCollection`); here every call shares ts=300 so the survivor's value is unambiguous.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);

        engine
            .enqueue_collection_membership("n1".into(), "c1".into(), 300, false)
            .unwrap(); // file
        engine
            .enqueue_collection_membership("n1".into(), "c1".into(), 300, true)
            .unwrap(); // off
        engine
            .enqueue_collection_membership("n1".into(), "c1".into(), 300, false)
            .unwrap(); // on

        let payload = collapsed_membership_payload(db_path);
        assert_eq!(
            payload["deleted"],
            json!(false),
            "re-add wins — one live row"
        );
        assert_eq!(payload["created_at"], json!(300));
        assert_eq!(membership_mirror_created_at(db_path, "c1:n1"), 300);
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

    fn margin(id: &str, link_id: &str, text: &str) -> MarginChild {
        MarginChild {
            id: id.into(),
            link_id: link_id.into(),
            text: text.into(),
            ink_crop_path: None,
        }
    }

    fn parent_with_book(id: &str, book_id: &str) -> NoteUpsert {
        NoteUpsert {
            book_id: Some(book_id.into()),
            ..note_upsert(id, "the parent passage")
        }
    }

    #[test]
    fn replace_handwritten_annotations_creates_linked_children_under_the_live_book() {
        // Core never reads the parent's text (only its existence + book_id), so a decrypt-failed parent
        // takes margins fine — the same path as here. Children seal their OWN text.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();

        let n = engine
            .replace_handwritten_annotations(
                "p".into(),
                vec![
                    margin("c1", "e1", "  Kahneman overstates it  "),
                    margin("c2", "e2", "cf. base rates"),
                ],
            )
            .unwrap();
        assert_eq!(n, 2);

        // Both children are live, handwritten, on the parent's book, text sealed→decrypts to input.
        let c1 = engine.get_note("c1".into()).unwrap().expect("c1 live");
        assert_eq!(c1.source.as_deref(), Some("handwritten"));
        assert_eq!(c1.book_id.as_deref(), Some("b1"));
        assert_eq!(c1.text.as_deref(), Some("Kahneman overstates it")); // core trims (PWA parity, M2)
        assert!(c1.tags.is_empty());
        let c2 = engine.get_note("c2".into()).unwrap().expect("c2 live");
        assert_eq!(c2.text.as_deref(), Some("cf. base rates"));
        assert!(
            c1.created_at < c2.created_at,
            "staggered created_at preserves review order"
        );

        // Wire stamps: updated_at mirrors the staggered created_at (PWA parity, both rows), and
        // content_tag is computed under the parent's LIVE book — not None, not a host snapshot.
        let c1_payload = collapsed_payload_for(db_path, "notes", "c1").expect("c1 queued");
        assert_eq!(
            c1_payload["updated_at"], c1_payload["created_at"],
            "updatedAt == createdAt == now + i (PWA stamp parity)"
        );
        assert_eq!(
            c1_payload["content_tag"],
            json!(engine
                .vault
                .content_tag("Kahneman overstates it".into(), Some("b1".into()))),
            "content_tag fingerprinted under the parent's live book"
        );
        let e1_payload = collapsed_payload_for(db_path, "note_links", "e1").expect("e1 queued");
        assert_eq!(
            e1_payload["updated_at"], e1_payload["created_at"],
            "edge stamps mirror the child's (PWA writes now + i on both)"
        );

        // Each child has a live parent→child handwritten edge (no orphan possible — one transaction).
        let edges = engine.note_links_for_note("p".into()).unwrap();
        let from_parent: Vec<_> = edges.iter().filter(|e| e.from_note_id == "p").collect();
        assert_eq!(from_parent.len(), 2);
        assert!(from_parent
            .iter()
            .all(|e| e.relation_type.as_deref() == Some("handwritten_annotation")));
        assert_eq!(
            from_parent
                .iter()
                .map(|e| e.to_note_id.clone())
                .collect::<std::collections::HashSet<_>>(),
            ["c1".to_string(), "c2".to_string()].into_iter().collect(),
        );
    }

    #[test]
    fn replace_handwritten_annotations_retires_prior_set_and_converges() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();

        engine
            .replace_handwritten_annotations(
                "p".into(),
                vec![margin("c1", "e1", "old one"), margin("c2", "e2", "old two")],
            )
            .unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c3", "e3", "fresh")])
            .unwrap();

        // Prior children + edges retired; exactly the fresh set survives (convergence, no accumulation).
        assert!(
            engine.get_note("c1".into()).unwrap().is_none(),
            "prior child c1 tombstoned"
        );
        assert!(
            engine.get_note("c2".into()).unwrap().is_none(),
            "prior child c2 tombstoned"
        );
        assert!(
            engine.get_note("c3".into()).unwrap().is_some(),
            "fresh child live"
        );
        let edges = engine.note_links_for_note("p".into()).unwrap();
        assert_eq!(
            edges
                .iter()
                .filter(|e| e.from_note_id == "p")
                .map(|e| e.to_note_id.clone())
                .collect::<Vec<_>>(),
            vec!["c3".to_string()]
        );
    }

    #[test]
    fn replace_handwritten_annotations_only_retires_this_parents_handwritten_edges() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("hw", "e-hw", "a margin")])
            .unwrap();
        // A non-handwritten edge leaving p, and an inbound handwritten edge into p — neither may be retired.
        engine
            .enqueue_note_link(
                "e-other".into(),
                "p".into(),
                "dup".into(),
                Some("duplicate_of".into()),
                5,
                false,
            )
            .unwrap();
        engine
            .enqueue_note_link("e-in".into(), "src".into(), "p".into(), None, 5, false)
            .unwrap();

        engine
            .replace_handwritten_annotations("p".into(), vec![margin("hw2", "e-hw2", "new margin")])
            .unwrap();

        assert!(
            engine.get_note("hw".into()).unwrap().is_none(),
            "prior handwritten child retired"
        );
        let live_ids: std::collections::HashSet<String> = engine
            .note_links_for_note("p".into())
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert!(
            live_ids.contains("e-other"),
            "non-handwritten edge untouched"
        );
        assert!(
            live_ids.contains("e-in"),
            "inbound handwritten edge untouched"
        );
        assert!(live_ids.contains("e-hw2"), "the fresh edge is live");
        assert!(
            !live_ids.contains("e-hw"),
            "only the prior handwritten edge leaving p is retired"
        );
    }

    #[test]
    fn replace_handwritten_annotations_never_deletes_a_repointed_regular_note() {
        // reconcile.rs repoint_note_links can leave a handwritten_annotation edge on a REGULAR
        // (non-handwritten) survivor after a content-dedupe merge. Replacing this parent's margins must
        // retire the edge but NEVER delete that regular note.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .enqueue_note(note_upsert("reg", "a normal note, not a margin"))
            .unwrap(); // source = manual
        engine
            .enqueue_note_link(
                "e-reg".into(),
                "p".into(),
                "reg".into(),
                Some("handwritten_annotation".into()),
                5,
                false,
            )
            .unwrap();

        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "fresh margin")])
            .unwrap();

        assert!(
            engine.get_note("reg".into()).unwrap().is_some(),
            "a repointed regular note must NOT be deleted"
        );
        let live_edges: std::collections::HashSet<String> = engine
            .note_links_for_note("p".into())
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert!(!live_edges.contains("e-reg"), "the stale edge is retired");
        assert!(live_edges.contains("e1"), "the fresh margin edge is live");
    }

    #[test]
    fn replace_handwritten_annotations_keeps_a_child_still_referenced_by_another_parent() {
        // A content-dedupe merge can leave two parents' live handwritten edges on ONE shared margin
        // survivor. Replacing one parent's margins must retire only ITS edge, not the shared child (which
        // the other parent still annotates).
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p1", "b1")).unwrap();
        engine.enqueue_note(parent_with_book("p2", "b2")).unwrap();
        // p1 creates the margin child "shared" (edge e1); p2 also annotates it (edge e2).
        engine
            .replace_handwritten_annotations(
                "p1".into(),
                vec![margin("shared", "e1", "shared margin")],
            )
            .unwrap();
        engine
            .enqueue_note_link("e2".into(), "p2".into(), "shared".into(), None, 5, false)
            .unwrap();

        engine
            .replace_handwritten_annotations(
                "p1".into(),
                vec![margin("new", "e3", "p1's new margin")],
            )
            .unwrap();

        assert!(
            engine.get_note("shared".into()).unwrap().is_some(),
            "shared child stays live — p2 still references it"
        );
        let p1_edges: std::collections::HashSet<String> = engine
            .note_links_for_note("p1".into())
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert!(
            !p1_edges.contains("e1"),
            "p1's edge to the shared child is retired"
        );
        assert!(p1_edges.contains("e3"), "p1's fresh edge is live");
        let p2_edges = engine.note_links_for_note("p2".into()).unwrap();
        assert!(
            p2_edges
                .iter()
                .any(|e| e.id == "e2" && e.to_note_id == "shared"),
            "p2's edge survives, its target intact",
        );
    }

    #[test]
    fn replace_handwritten_annotations_keeps_a_child_carrying_a_non_handwritten_edge() {
        // note_links are generic: a margin child can also carry a non-handwritten link (e.g. an imported
        // `related` row, in EITHER direction). Replacing margins must not delete such a child — that
        // would dangle the other edge. Keep the note; drop only the handwritten margin edge.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "a margin")])
            .unwrap();
        // An OUTBOUND non-handwritten edge FROM the margin child — the narrow inbound-handwritten-only
        // check would have missed it and deleted c1, dangling e-rel.
        engine
            .enqueue_note_link(
                "e-rel".into(),
                "c1".into(),
                "other".into(),
                Some("related".into()),
                5,
                false,
            )
            .unwrap();

        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c2", "e2", "new margin")])
            .unwrap();

        assert!(
            engine.get_note("c1".into()).unwrap().is_some(),
            "a child carrying a non-handwritten edge is kept"
        );
        let c1_edges: std::collections::HashSet<String> = engine
            .note_links_for_note("c1".into())
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert!(
            !c1_edges.contains("e1"),
            "the handwritten margin edge is retired"
        );
        assert!(
            c1_edges.contains("e-rel"),
            "the non-handwritten edge survives, its target intact"
        );
    }

    #[test]
    fn replace_handwritten_annotations_idempotent_retry_with_same_ids_preserves_margins() {
        // A retry re-sends the SAME MarginChild ids. The prior child/edge are in old_edges AND the new
        // set; retiring them in the same batch would stage a create then a sticky tombstone (SUR-724
        // collapse) and destroy the very margins being preserved. Rows in the new set must never retire.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        let make = || vec![margin("c1", "e1", "keep me"), margin("c2", "e2", "and me")];

        engine
            .replace_handwritten_annotations("p".into(), make())
            .unwrap();
        let n = engine
            .replace_handwritten_annotations("p".into(), make())
            .unwrap(); // idempotent retry

        assert_eq!(n, 2);
        assert!(
            engine.get_note("c1".into()).unwrap().is_some(),
            "c1 preserved across the retry"
        );
        assert!(
            engine.get_note("c2".into()).unwrap().is_some(),
            "c2 preserved across the retry"
        );
        let edges: std::collections::HashSet<String> = engine
            .note_links_for_note("p".into())
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(
            edges,
            ["e1".to_string(), "e2".to_string()]
                .into_iter()
                .collect::<std::collections::HashSet<_>>(),
            "both edges stay live — no self-inflicted tombstone",
        );
    }

    /// The collapsed outbox payload that would FLUSH for `record_id` in `table`, or None if nothing is
    /// queued for it — the margins twin of [`collapsed_membership_payload`], for asserting what the
    /// cloud would actually receive after the SUR-724 sticky collapse.
    fn collapsed_payload_for(
        db_path: &str,
        table: &str,
        record_id: &str,
    ) -> Option<Map<String, Value>> {
        let items: Vec<crate::sync::outbox::OutboxItem> = Store::open(db_path)
            .unwrap()
            .outbox_items()
            .unwrap()
            .into_iter()
            .map(
                |(id, table_name, rec, payload, created_at)| crate::sync::outbox::OutboxItem {
                    id,
                    table_name,
                    record_id: rec,
                    payload: serde_json::from_str(&payload).unwrap(),
                    created_at,
                },
            )
            .collect();
        crate::sync::outbox::collapse(items, &std::collections::BTreeMap::new())
            .iter()
            // note_signals is keyed by note_id (no `id` column) — match either pk shape.
            .find(|c| {
                c.table == table
                    && (c.payload.get("id") == Some(&json!(record_id))
                        || c.payload.get("note_id") == Some(&json!(record_id)))
            })
            .map(|c| c.payload.clone())
    }

    /// Empty the queue as a successful flush would (clear by outbox row id) — so a later batch's
    /// tombstones stand ALONE, the exact shape the wire sees on the mainline replace→sync→replace path.
    fn drain_outbox(db_path: &str) {
        let store = Store::open(db_path).unwrap();
        let ids: Vec<i64> = store
            .outbox_items()
            .unwrap()
            .into_iter()
            .map(|r| r.0)
            .collect();
        store.clear_outbox(&ids).unwrap();
    }

    #[test]
    fn replace_handwritten_annotations_edge_tombstone_flushes_with_the_full_not_null_shape() {
        // B1 (sweep, 3 lenses): after replace→flush→replace, the old edge's tombstone stands alone in
        // the outbox. note_links has no sparse-PATCH fallback, so the payload must carry the server's
        // NOT-NULL columns (from/to/created_at) or every flush 23502s and the outbox wedges forever.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "first")])
            .unwrap();
        drain_outbox(db_path); // the creates flushed; the retire batch will queue tombstones ALONE

        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c2", "e2", "second")])
            .unwrap();

        let e1 = collapsed_payload_for(db_path, "note_links", "e1").expect("edge tombstone queued");
        assert_eq!(e1["deleted"], json!(true));
        assert_eq!(
            e1["from_note_id"],
            json!("p"),
            "NOT NULL from_note_id present"
        );
        assert_eq!(e1["to_note_id"], json!("c1"), "NOT NULL to_note_id present");
        assert_eq!(e1["relation_type"], json!("handwritten_annotation"));
        assert!(
            e1.get("created_at").and_then(Value::as_i64).is_some(),
            "NOT NULL created_at present"
        );
        let c1 = collapsed_payload_for(db_path, "notes", "c1").expect("note tombstone queued");
        assert_eq!(c1["deleted"], json!(true)); // sparse is fine for notes (PATCH fallback)
    }

    #[test]
    fn replace_handwritten_annotations_restore_writes_explicit_nulls_not_stale_merges() {
        // M1 (sweep) + the round-8 completion: a restore reusing a prior child id must not resurrect
        // ANY of the tombstoned row's stale fields via the staging merge — every synced column the
        // MarginChild doesn't own is written as the PWA child's explicit cleared shape. A pull can
        // enrich a margin with fields a fresh one never has (dedupe merges, PWA-side edits, older
        // schemas), so seed them ALL and prove a text-only restore clears every one.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations(
                "p".into(),
                vec![MarginChild {
                    id: "c1".into(),
                    link_id: "e1".into(),
                    text: "inked".into(),
                    ink_crop_path: Some("u/c1.jpg".into()),
                }],
            )
            .unwrap();
        // Simulate the pull-side enrichment: the stored row now carries every clearable field.
        let enriched = json!({
            "id": "c1",
            "book_id": "b-old",
            "text": "enc:v2:stale",
            "content_tag": "stale-tag",
            "tags": ["stale-idea"],
            "source": "handwritten",
            "page": "12",
            "chapter": "Ch. 3",
            "image_path": "u/whole-page.jpg",
            "ink_crop_path": "u/c1.jpg",
            "source_id": "src-9",
            "source_meta": {"title": "Old Book"},
            "created_at": 1,
            "updated_at": 1,
            "deleted": false
        });
        Store::open(db_path)
            .unwrap()
            .apply_row("notes", enriched.as_object().unwrap())
            .unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c2", "e2", "interim")])
            .unwrap();

        // Restore c1 text-only: none of the stale fields may come back with it.
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "typed rewrite")])
            .unwrap();

        let restored = engine.get_note("c1".into()).unwrap().unwrap();
        assert_eq!(restored.ink_crop_path, None, "stale crop not resurrected");
        assert_eq!(
            restored.book_id.as_deref(),
            Some("b1"),
            "book re-derived from the live parent, not the stale row"
        );
        assert!(restored.tags.is_empty(), "stale tags not resurrected");
        let payload = collapsed_payload_for(db_path, "notes", "c1").expect("c1 queued");
        for column in ["chapter", "image_path", "ink_crop_path", "source_id"] {
            assert_eq!(
                payload.get(column),
                Some(&Value::Null),
                "{column}: explicit null on the wire"
            );
        }
        assert_eq!(
            payload["page"],
            json!(""),
            "page cleared to the PWA child's ''"
        );
        assert_eq!(
            payload["source_meta"],
            json!({}),
            "source_meta cleared to the PWA child's {{}}"
        );
        assert_eq!(payload["book_id"], json!("b1"));
        assert_eq!(payload["tags"], json!([]));
        assert_eq!(payload["deleted"], json!(false));
    }

    #[test]
    fn replace_handwritten_annotations_all_blank_call_preserves_existing_margins() {
        // M2 (sweep): the PWA drops blank items BEFORE its length check — an all-blank call is a no-op,
        // never a destroy. Mixed input trims and keeps only real texts.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "real margin")])
            .unwrap();

        let n = engine
            .replace_handwritten_annotations(
                "p".into(),
                vec![margin("c2", "e2", "   "), margin("c3", "e3", "")],
            )
            .unwrap();
        assert_eq!(n, 0);
        assert!(
            engine.get_note("c1".into()).unwrap().is_some(),
            "existing margin preserved"
        );
        assert!(
            engine.get_note("c2".into()).unwrap().is_none(),
            "no blank child created"
        );

        let n = engine
            .replace_handwritten_annotations(
                "p".into(),
                vec![margin("c4", "e4", "  keep  "), margin("c5", "e5", "\n")],
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            engine
                .get_note("c4".into())
                .unwrap()
                .unwrap()
                .text
                .as_deref(),
            Some("keep"),
            "trimmed before sealing"
        );
        assert!(engine.get_note("c5".into()).unwrap().is_none());
    }

    #[test]
    fn replace_handwritten_annotations_rejects_incoherent_host_ids_staging_nothing() {
        // L2 (sweep): id collisions must fail the whole call BEFORE any write — a child id equal to the
        // parent would re-seal margin text over the parent's passage; a duplicate link id would leave a
        // child edgeless; a link id owned by another parent's edge would steal it fleet-wide.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .enqueue_note(note_upsert("regular", "an unrelated passage"))
            .unwrap();
        // Another parent with its own margin (its edge AND child are theft targets).
        engine.enqueue_note(parent_with_book("p2", "b2")).unwrap();
        engine
            .replace_handwritten_annotations("p2".into(), vec![margin("cB", "eX", "p2's margin")])
            .unwrap();
        // This parent's OWN generic (non-handwritten) edge — a from-check alone would wave its id
        // through, and the create loop would rewrite its to/relation into a margin edge.
        engine
            .enqueue_note_link(
                "er".into(),
                "p".into(),
                "regular".into(),
                Some("related".into()),
                5,
                false,
            )
            .unwrap();
        // This parent's OWN margin — but entangled: p2 also annotates it (shared dedupe-survivor
        // shape) and an imported `related` row touches it. The retire loop would deliberately KEEP
        // such a child, so the create loop must never be allowed to overwrite it via id reuse.
        engine
            .replace_handwritten_annotations(
                "p".into(),
                vec![margin("cS", "eS", "the entangled margin")],
            )
            .unwrap();
        engine
            .enqueue_note_link("e2b".into(), "p2".into(), "cS".into(), None, 5, false)
            .unwrap();
        engine
            .enqueue_note_link(
                "eg".into(),
                "regular".into(),
                "cS".into(),
                Some("related".into()),
                5,
                false,
            )
            .unwrap();
        drain_outbox(db_path);

        for (children, why) in [
            (vec![margin("p", "e1", "x")], "child id == parent id"),
            (
                vec![margin("c1", "e1", "x"), margin("c2", "e1", "y")],
                "duplicate link id",
            ),
            (
                vec![margin("c1", "e1", "x"), margin("c1", "e2", "y")],
                "duplicate child id",
            ),
            (
                vec![margin("regular", "e1", "x")],
                "child id collides with a non-margin note",
            ),
            (
                vec![margin("c1", "eX", "x")],
                "link id owned by another parent's edge",
            ),
            (
                vec![margin("c1", "er", "x")],
                "link id owned by this parent's GENERIC edge",
            ),
            (
                vec![margin("cB", "e9", "x")],
                "child id is another parent's margin",
            ),
            (
                vec![margin("cS", "eS", "x")],
                "child id reused while another live edge still touches it",
            ),
        ] {
            let err = engine
                .replace_handwritten_annotations("p".into(), children)
                .unwrap_err();
            assert!(matches!(err, SyncError::Store(_)), "{why}: must reject");
        }
        assert!(
            collapsed_payload_for(db_path, "notes", "c1").is_none(),
            "nothing staged by rejected calls"
        );
        assert!(
            engine.get_note("regular".into()).unwrap().is_some(),
            "the colliding regular note is untouched"
        );
        assert_eq!(
            engine
                .note_links_for_note("p2".into())
                .unwrap()
                .iter()
                .filter(|e| e.id == "eX")
                .count(),
            1,
            "p2's edge not stolen",
        );
        assert_eq!(
            engine
                .get_note("cB".into())
                .unwrap()
                .unwrap()
                .text
                .as_deref(),
            Some("p2's margin"),
            "p2's margin text not rewritten",
        );
        // The generic edge is byte-untouched: still `related`, still to `regular`, still live.
        let er = engine
            .note_links_for_note("p".into())
            .unwrap()
            .into_iter()
            .find(|e| e.id == "er")
            .expect("generic edge still live");
        assert_eq!(
            (er.relation_type.as_deref(), er.to_note_id.as_str()),
            (Some("related"), "regular"),
            "this parent's generic edge not repointed into a margin",
        );
        // The entangled margin is byte-untouched: its text was not rewritten, and both foreign
        // edges (the second parent's and the imported `related` row) still point at it, live.
        assert_eq!(
            engine
                .get_note("cS".into())
                .unwrap()
                .unwrap()
                .text
                .as_deref(),
            Some("the entangled margin"),
            "the shared child's text not rewritten by the rejected reuse",
        );
        let cs_edges: std::collections::HashSet<String> = engine
            .note_links_for_note("cS".into())
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert!(
            cs_edges.contains("e2b") && cs_edges.contains("eg"),
            "both foreign edges still live on the shared child",
        );
    }

    #[test]
    fn replace_handwritten_annotations_self_edge_never_tombstones_the_parent() {
        // L2 (sweep): a corrupt p→p handwritten self-edge must retire the EDGE, never the parent — even
        // when the parent itself is source=handwritten (nothing else protects it then).
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine
            .enqueue_note(NoteUpsert {
                source: Some("handwritten".into()),
                ..parent_with_book("p", "b1")
            })
            .unwrap();
        engine
            .enqueue_note_link("es".into(), "p".into(), "p".into(), None, 5, false)
            .unwrap();

        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "a margin")])
            .unwrap();

        assert!(
            engine.get_note("p".into()).unwrap().is_some(),
            "parent survives its own self-edge"
        );
        let live: std::collections::HashSet<String> = engine
            .note_links_for_note("p".into())
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert!(!live.contains("es"), "the corrupt self-edge is retired");
        assert!(live.contains("e1"), "the real margin edge is live");
    }

    #[test]
    fn replace_handwritten_annotations_recreating_a_previously_retired_id_flushes_live_not_deleted()
    {
        // The cross-batch sticky-delete trap: an (offline, un-flushed) replace queued tombstones for
        // c1/e1; a later retry/restore re-sends c1/e1 as children. Without dropping the queued deletes,
        // the SUR-724 collapse keeps `deleted` sticky and the recreated margin FLUSHES as deleted with a
        // fresh updated_at — this device shows it live, the fleet tombstones it, and strict-tie LWW never
        // repairs the split. The live write must drop the queued tombstone (stage_local_writes resurrect
        // rule), so what flushes is live.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();

        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "first")])
            .unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c2", "e2", "second")])
            .unwrap(); // queues c1/e1 tombstones
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "restored")])
            .unwrap(); // re-creates c1/e1

        // Local mirror: the restored margin is live, the intermediate set retired.
        assert!(
            engine.get_note("c1".into()).unwrap().is_some(),
            "restored child live locally"
        );
        assert!(
            engine.get_note("c2".into()).unwrap().is_none(),
            "intermediate child retired"
        );

        // What the cloud would receive: c1/e1 collapse to LIVE (queued tombstones dropped), c2/e2 to deleted.
        let c1 = collapsed_payload_for(db_path, "notes", "c1").expect("c1 queued");
        assert_eq!(
            c1["deleted"],
            json!(false),
            "restored child flushes live — no sticky delete"
        );
        let e1 = collapsed_payload_for(db_path, "note_links", "e1").expect("e1 queued");
        assert_eq!(
            e1["deleted"],
            json!(false),
            "restored edge flushes live — no sticky delete"
        );
        assert_eq!(
            collapsed_payload_for(db_path, "notes", "c2").unwrap()["deleted"],
            json!(true)
        );
        assert_eq!(
            collapsed_payload_for(db_path, "note_links", "e2").unwrap()["deleted"],
            json!(true)
        );
    }

    #[test]
    fn replace_handwritten_annotations_reused_link_with_new_child_retires_the_old_orphan() {
        // A rebuild reuses a prior link_id but with a DIFFERENT child id. The create loop repoints that
        // edge to the new child; the OLD child would be left live + edgeless — the exact orphan this op
        // exists to prevent. Skipping the edge tombstone must NOT skip the old-child retirement.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("old", "e1", "old margin")])
            .unwrap();

        // Reuse link id e1, new child id "new".
        engine
            .replace_handwritten_annotations(
                "p".into(),
                vec![margin("new", "e1", "rebuilt margin")],
            )
            .unwrap();

        assert!(
            engine.get_note("old".into()).unwrap().is_none(),
            "the orphaned old child is retired, not leaked"
        );
        assert!(
            engine.get_note("new".into()).unwrap().is_some(),
            "the new child is live"
        );
        let from_p: Vec<_> = engine
            .note_links_for_note("p".into())
            .unwrap()
            .into_iter()
            .filter(|e| e.from_note_id == "p")
            .collect();
        assert_eq!(
            from_p.len(),
            1,
            "exactly one live edge from p — no orphan, no duplicate"
        );
        assert_eq!(
            (from_p[0].id.as_str(), from_p[0].to_note_id.as_str()),
            ("e1", "new"),
            "e1 repointed to the new child"
        );
    }

    #[test]
    fn replace_handwritten_annotations_stores_ink_crop_path_when_supplied() {
        // Capture-time handwriting detection (iOS / PWA capture card) uploads the crop first and passes
        // its storage path; core stores it verbatim on the child. Android's action-sheet path passes None.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();

        engine
            .replace_handwritten_annotations(
                "p".into(),
                vec![
                    MarginChild {
                        id: "c1".into(),
                        link_id: "e1".into(),
                        text: "a scribble".into(),
                        ink_crop_path: Some("userId/c1.jpg".into()),
                    },
                    margin("c2", "e2", "text-only margin"), // ink_crop_path None
                ],
            )
            .unwrap();

        assert_eq!(
            engine
                .get_note("c1".into())
                .unwrap()
                .unwrap()
                .ink_crop_path
                .as_deref(),
            Some("userId/c1.jpg")
        );
        assert_eq!(
            engine.get_note("c2".into()).unwrap().unwrap().ink_crop_path,
            None
        );
    }

    #[test]
    fn replace_handwritten_annotations_empty_children_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "keep me")])
            .unwrap();

        let signals_before = stored_signals(db_path, "p");

        let n = engine
            .replace_handwritten_annotations("p".into(), vec![])
            .unwrap();

        assert_eq!(n, 0);
        assert!(
            engine.get_note("c1".into()).unwrap().is_some(),
            "empty replace leaves existing margins intact"
        );
        assert_eq!(
            stored_signals(db_path, "p"),
            signals_before,
            "a no-op replace never touches the parent's signals (SUR-956)"
        );
        assert_eq!(
            engine
                .note_links_for_note("p".into())
                .unwrap()
                .iter()
                .filter(|e| e.from_note_id == "p")
                .count(),
            1
        );
    }

    #[test]
    fn replace_handwritten_annotations_missing_parent_errors_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);

        let err = engine
            .replace_handwritten_annotations("ghost".into(), vec![margin("c1", "e1", "x")])
            .unwrap_err();
        assert!(matches!(err, SyncError::Store(_)));
        assert!(
            engine.get_note("c1".into()).unwrap().is_none(),
            "no child staged when the parent is missing"
        );
        assert!(
            Store::open(db_path)
                .unwrap()
                .outbox_items()
                .unwrap()
                .is_empty(),
            "nothing enqueued"
        );
    }

    #[test]
    fn replace_handwritten_annotations_create_rows_cover_every_synced_column() {
        // The mechanical guard for the stale-resurrection class (round 8): the create row must carry
        // EVERY column the synced schema knows for notes/note_links — an omitted column survives an
        // id reuse through the staging merge locally AND through the server's column-list upsert.
        // Pinned to the drift-guarded vendored schema, so when surfc grows a synced column this test
        // fails until the op covers it — completeness is no longer a review judgment call.
        let schema: Value =
            serde_json::from_str(include_str!("../../vendored/schema/sync-schema.json")).unwrap();
        let columns = |table: &str| -> std::collections::BTreeSet<String> {
            schema[table].as_object().unwrap().keys().cloned().collect()
        };
        let keys = |m: &Map<String, Value>| -> std::collections::BTreeSet<String> {
            m.keys().cloned().collect()
        };

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "a margin")])
            .unwrap();

        assert_eq!(
            keys(&collapsed_payload_for(db_path, "notes", "c1").unwrap()),
            columns("notes"),
            "child create writes every synced notes column — nothing less, nothing more",
        );
        assert_eq!(
            keys(&collapsed_payload_for(db_path, "note_links", "e1").unwrap()),
            columns("note_links"),
            "edge create writes every synced note_links column",
        );
        assert_eq!(
            keys(&collapsed_payload_for(db_path, "note_signals", "p").unwrap()),
            columns("note_signals"),
            "the SUR-956 signals refresh writes every synced note_signals column — a sparse \
             payload would leave a NEW cloud row's unnamed columns to server defaults",
        );

        // The edge TOMBSTONE must be schema-complete too — note_links has no sparse-PATCH flush
        // fallback, so a missing NOT-NULL column would 23502 and wedge the outbox (B1).
        drain_outbox(db_path);
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c2", "e2", "next")])
            .unwrap();
        assert_eq!(
            keys(&collapsed_payload_for(db_path, "note_links", "e1").unwrap()),
            columns("note_links"),
            "edge tombstone carries the full synced shape",
        );
    }

    fn stored_signals(db_path: &str, note_id: &str) -> Option<Map<String, Value>> {
        Store::open(db_path)
            .unwrap()
            .get_row("note_signals", note_id)
            .unwrap()
    }

    #[test]
    fn replace_handwritten_annotations_stages_birth_signals_row_in_the_same_batch() {
        // SUR-956: a first margin on a signals-less parent births the row exactly as the PWA's
        // `refreshAnnotationSignal` → `applyNoteSignal` does — defaults + the prior derived from the
        // parent's `source`, `has_annotation: true`, importance recomputed — and it rides the SAME
        // outbox batch as the child rows (one enqueue stamp = one transaction).
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine
            .enqueue_note(NoteUpsert {
                source: Some("manual".into()),
                ..parent_with_book("p", "b1")
            })
            .unwrap();

        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "a margin")])
            .unwrap();

        let sig = stored_signals(db_path, "p").expect("birth signals row staged");
        assert_eq!(sig["has_annotation"], json!(true));
        assert_eq!(sig["deleted"], json!(false));
        assert_eq!(sig["return_visits"], json!(0));
        assert_eq!(sig["stitch_spawns"], json!(0));
        assert_eq!(sig["exposure_recency_at"], json!(0));
        assert_eq!(sig["engagement_recency_at"], json!(0));
        assert_eq!(
            sig["source_prior"],
            json!(0.7),
            "prior derived from the parent's source (`manual`), not a flat default"
        );
        assert!(
            (sig["importance"].as_f64().unwrap() - 0.909_385_394_307_286_9).abs() < 1e-12,
            "importance = 0.7 * 2^(-0.3/1.5) + 0.3 — the annotation's 0.3 evidence applied"
        );
        assert_eq!(sig["updated_at"], sig["created_at"], "birth stamps");

        let items = Store::open(db_path).unwrap().outbox_items().unwrap();
        let enqueue_stamp = |table: &str, rec: &str| {
            items
                .iter()
                .find(|(_, t, r, _, _)| t == table && r.as_deref() == Some(rec))
                .unwrap_or_else(|| panic!("{table}/{rec} not enqueued"))
                .4
        };
        assert_eq!(
            enqueue_stamp("note_signals", "p"),
            enqueue_stamp("notes", "c1"),
            "signals row rides the same batch (one enqueue stamp = one transaction)"
        );
    }

    #[test]
    fn replace_handwritten_annotations_preserves_earned_signal_counters() {
        // The whole point of read-merge-stage IN CORE (vs the host's blind `enqueue_note_signals`):
        // flipping `has_annotation` must not clobber behavioural counters another surface earned.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .enqueue_note_signals("p".into(), 0.9, 5, false, 2, 111, 222, 1.23, 100, false)
            .unwrap();

        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "a margin")])
            .unwrap();

        let sig = stored_signals(db_path, "p").expect("signals row live");
        assert_eq!(sig["has_annotation"], json!(true));
        assert_eq!(sig["return_visits"], json!(5), "earned counter preserved");
        assert_eq!(sig["stitch_spawns"], json!(2), "earned counter preserved");
        assert_eq!(sig["exposure_recency_at"], json!(111));
        assert_eq!(sig["engagement_recency_at"], json!(222));
        assert_eq!(
            sig["source_prior"],
            json!(0.9),
            "stored prior kept, not re-derived"
        );
        assert_eq!(sig["created_at"], json!(100), "created_at preserved");
        assert!(
            (sig["importance"].as_f64().unwrap() - 2.191_747_753_483_256).abs() < 1e-12,
            "importance recomputed over the PRESERVED counters: 0.9 * 2^(-1.8/1.5) + 1.8"
        );
    }

    #[test]
    fn replace_handwritten_annotations_already_flagged_signals_are_untouched() {
        // PWA change-detection parity (`applyNoteSignal`'s early return): an already-annotated live
        // row gets NO write — no `updated_at` bump, no outbox churn, zero LWW clobber exposure.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .enqueue_note_signals("p".into(), 0.9, 5, true, 2, 111, 222, 1.23, 100, false)
            .unwrap();
        let before = stored_signals(db_path, "p").expect("seeded");
        drain_outbox(db_path);

        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "a margin")])
            .unwrap();

        assert_eq!(
            stored_signals(db_path, "p").unwrap(),
            before,
            "already-flagged row byte-identical — no updated_at bump"
        );
        assert!(
            !Store::open(db_path)
                .unwrap()
                .outbox_items()
                .unwrap()
                .iter()
                .any(|(_, t, _, _, _)| t == "note_signals"),
            "no signals outbox row enqueued"
        );
    }

    #[test]
    fn replace_handwritten_annotations_resurrects_a_tombstoned_signals_row() {
        // A tombstoned signals row is a live upsert target here: the batch's live write drops the
        // queued tombstone (the resurrect rule), so the flush can't push a sticky delete.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .enqueue_note_signals("p".into(), 0.9, 5, false, 2, 111, 222, 1.23, 100, true)
            .unwrap();

        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "a margin")])
            .unwrap();

        let sig = stored_signals(db_path, "p").expect("signals row live again");
        assert_eq!(sig["deleted"], json!(false));
        assert_eq!(sig["has_annotation"], json!(true));
        assert_eq!(
            sig["return_visits"],
            json!(5),
            "counters survive the resurrect"
        );
        let payload = collapsed_payload_for(db_path, "note_signals", "p").expect("queued");
        assert_eq!(
            payload["deleted"],
            json!(false),
            "what flushes is LIVE — the queued tombstone was dropped, not collapsed over"
        );
        assert_eq!(
            Store::open(db_path)
                .unwrap()
                .outbox_items()
                .unwrap()
                .iter()
                .filter(|(_, t, _, _, _)| t == "note_signals")
                .count(),
            1,
            "the queued tombstone was DROPPED (one live item), not merely outweighed by collapse"
        );
    }

    /// Post-condition oracle for a SUCCESSFUL replace: the parent's live handwritten edge set is
    /// EXACTLY `expected`'s (link, child) pairs; every expected child is a live handwritten note
    /// whose text decrypts to the input, filed under the parent's live book; and what would FLUSH
    /// for each expected id is LIVE (the sticky-delete check) with the edge pointing parent→child.
    fn assert_margins_converged(
        engine: &SyncEngine,
        db_path: &str,
        parent_id: &str,
        book_id: Option<&str>,
        expected: &[(&str, &str, &str)],
    ) {
        let live_hw: std::collections::BTreeSet<(String, String)> = engine
            .note_links_for_note(parent_id.into())
            .unwrap()
            .into_iter()
            .filter(|e| {
                e.from_note_id == parent_id
                    && e.relation_type
                        .as_deref()
                        .unwrap_or("handwritten_annotation")
                        == "handwritten_annotation"
            })
            .map(|e| (e.id, e.to_note_id))
            .collect();
        let want: std::collections::BTreeSet<(String, String)> = expected
            .iter()
            .map(|(child, link, _)| (link.to_string(), child.to_string()))
            .collect();
        assert_eq!(
            live_hw, want,
            "live handwritten edge set == the new set, exactly"
        );

        for (child_id, link_id, text) in expected {
            let child = engine
                .get_note((*child_id).into())
                .unwrap()
                .unwrap_or_else(|| panic!("{child_id}: expected child live"));
            assert_eq!(child.source.as_deref(), Some("handwritten"), "{child_id}");
            assert_eq!(
                child.text.as_deref(),
                Some(*text),
                "{child_id}: sealed text round-trips"
            );
            assert_eq!(
                child.book_id.as_deref(),
                book_id,
                "{child_id}: parent's live book"
            );

            let note = collapsed_payload_for(db_path, "notes", child_id)
                .unwrap_or_else(|| panic!("{child_id}: create queued"));
            assert_eq!(
                note["deleted"],
                json!(false),
                "{child_id}: flushes live — never a sticky delete"
            );
            let edge = collapsed_payload_for(db_path, "note_links", link_id)
                .unwrap_or_else(|| panic!("{link_id}: edge queued"));
            assert_eq!(
                edge["deleted"],
                json!(false),
                "{link_id}: edge flushes live"
            );
            assert_eq!(edge["from_note_id"], json!(parent_id), "{link_id}");
            assert_eq!(
                edge["to_note_id"],
                json!(child_id),
                "{link_id}: parent→child"
            );
        }
    }

    #[test]
    fn replace_handwritten_annotations_reuse_state_grid_rejects_or_converges() {
        // Rounds 2–8, mechanized: reuse legality is a function of (stored-row state × edge topology
        // × queue state), and every regression this op has had was one unenumerated cell of that
        // space. Each reachable cell must either REJECT before staging or CONVERGE per the oracle —
        // no third outcome. When the space grows (a new relation semantic, a new reconcile
        // producer), extend the grid FIRST, then the op.
        enum Expect {
            Converges(&'static [(&'static str, &'static str, &'static str)]),
            Rejects(&'static str),
        }
        use Expect::*;
        type Setup = fn(&SyncEngine, &str);
        let cells: Vec<(&str, Setup, Vec<MarginChild>, Expect)> = vec![
            // ── reused CHILD id "x" (fresh link "eF" unless the cell is about the pair) ──
            (
                "child absent — fresh mint",
                |_, _| {},
                vec![margin("x", "eF", "m")],
                Converges(&[("x", "eF", "m")]),
            ),
            (
                "child own live margin, same link — idempotent retry",
                |e, _| {
                    e.replace_handwritten_annotations("p".into(), vec![margin("x", "eL", "m")])
                        .unwrap();
                },
                vec![margin("x", "eL", "m")],
                Converges(&[("x", "eL", "m")]),
            ),
            (
                "child own live margin, fresh link — link rotation retires the old edge",
                |e, _| {
                    e.replace_handwritten_annotations("p".into(), vec![margin("x", "eL", "m")])
                        .unwrap();
                },
                vec![margin("x", "eF", "m2")],
                Converges(&[("x", "eF", "m2")]),
            ),
            (
                "child own tombstoned margin, paired tombstoned link — restore drops queued tombstones",
                |e, _| {
                    e.replace_handwritten_annotations("p".into(), vec![margin("x", "eL", "m")])
                        .unwrap();
                    e.replace_handwritten_annotations("p".into(), vec![margin("y", "eY", "mid")])
                        .unwrap();
                },
                vec![margin("x", "eL", "m3")],
                Converges(&[("x", "eL", "m3")]),
            ),
            (
                "child own tombstoned margin, FRESH link — unprovable ownership rejects",
                |e, _| {
                    e.replace_handwritten_annotations("p".into(), vec![margin("x", "eL", "m")])
                        .unwrap();
                    e.replace_handwritten_annotations("p".into(), vec![margin("y", "eY", "mid")])
                        .unwrap();
                },
                vec![margin("x", "eF", "m")],
                Rejects("another parent's margin"),
            ),
            (
                "child shared with a second parent's live handwritten edge",
                |e, _| {
                    e.replace_handwritten_annotations("p".into(), vec![margin("x", "eL", "m")])
                        .unwrap();
                    e.enqueue_note_link("e2".into(), "p2".into(), "x".into(), None, 5, false)
                        .unwrap();
                },
                vec![margin("x", "eL", "m")],
                Rejects("referenced by another live edge"),
            ),
            (
                "child carries an inbound generic edge",
                |e, _| {
                    e.replace_handwritten_annotations("p".into(), vec![margin("x", "eL", "m")])
                        .unwrap();
                    e.enqueue_note_link(
                        "eg".into(),
                        "r".into(),
                        "x".into(),
                        Some("related".into()),
                        5,
                        false,
                    )
                    .unwrap();
                },
                vec![margin("x", "eL", "m")],
                Rejects("referenced by another live edge"),
            ),
            (
                "child carries an outbound generic edge",
                |e, _| {
                    e.replace_handwritten_annotations("p".into(), vec![margin("x", "eL", "m")])
                        .unwrap();
                    e.enqueue_note_link(
                        "eg".into(),
                        "x".into(),
                        "r".into(),
                        Some("related".into()),
                        5,
                        false,
                    )
                    .unwrap();
                },
                vec![margin("x", "eL", "m")],
                Rejects("referenced by another live edge"),
            ),
            (
                "child carries this parent's OWN generic edge",
                |e, _| {
                    e.replace_handwritten_annotations("p".into(), vec![margin("x", "eL", "m")])
                        .unwrap();
                    e.enqueue_note_link(
                        "eg".into(),
                        "p".into(),
                        "x".into(),
                        Some("related".into()),
                        5,
                        false,
                    )
                    .unwrap();
                },
                vec![margin("x", "eL", "m")],
                Rejects("referenced by another live edge"),
            ),
            (
                "child id on a live regular note",
                |e, _| {
                    e.enqueue_note(note_upsert("x", "a passage")).unwrap();
                },
                vec![margin("x", "eF", "m")],
                Rejects("non-margin note"),
            ),
            (
                "child id on a tombstoned regular note",
                |e, _| {
                    e.enqueue_note(NoteUpsert {
                        deleted: true,
                        ..note_upsert("x", "a passage")
                    })
                    .unwrap();
                },
                vec![margin("x", "eF", "m")],
                Rejects("non-margin note"),
            ),
            (
                "child id on another parent's margin",
                |e, _| {
                    e.enqueue_note(parent_with_book("p2", "b2")).unwrap();
                    e.replace_handwritten_annotations(
                        "p2".into(),
                        vec![margin("x", "eX", "theirs")],
                    )
                    .unwrap();
                },
                vec![margin("x", "eF", "m")],
                Rejects("another parent's margin"),
            ),
            (
                "child id is the parent",
                |_, _| {},
                vec![margin("p", "eF", "m")],
                Rejects("the parent id"),
            ),
            // ── reused LINK id "y" (fresh child "cF" unless the cell is about the pair) ──
            (
                "link absent — fresh mint",
                |_, _| {},
                vec![margin("cF", "y", "m")],
                Converges(&[("cF", "y", "m")]),
            ),
            (
                "link own live margin edge, new child — repoint retires the old child",
                |e, _| {
                    e.replace_handwritten_annotations("p".into(), vec![margin("old", "y", "m")])
                        .unwrap();
                },
                vec![margin("cF", "y", "m2")],
                Converges(&[("cF", "y", "m2")]),
            ),
            (
                "link own tombstoned margin edge, new child — restore-repoint",
                |e, _| {
                    e.replace_handwritten_annotations("p".into(), vec![margin("old", "y", "m")])
                        .unwrap();
                    e.replace_handwritten_annotations("p".into(), vec![margin("mid", "eM", "m2")])
                        .unwrap();
                },
                vec![margin("cF", "y", "m3")],
                Converges(&[("cF", "y", "m3")]),
            ),
            (
                "link id on this parent's generic edge",
                |e, _| {
                    e.enqueue_note_link(
                        "y".into(),
                        "p".into(),
                        "r".into(),
                        Some("related".into()),
                        5,
                        false,
                    )
                    .unwrap();
                },
                vec![margin("cF", "y", "m")],
                Rejects("non-margin edge"),
            ),
            (
                "link id on another parent's edge",
                |e, _| {
                    e.enqueue_note_link("y".into(), "p2".into(), "z".into(), None, 5, false)
                        .unwrap();
                },
                vec![margin("cF", "y", "m")],
                Rejects("non-margin edge"),
            ),
            (
                "link id is the parent",
                |_, _| {},
                vec![margin("cF", "p", "m")],
                Rejects("the parent id"),
            ),
            // ── DANGLING states: live edges whose child has NO local notes row. Pull skips a
            // tombstone for a row this device never had while edges apply independently (no local
            // FK), so these are reachable on any fresh device — the round-8b bypass class. ──
            (
                "no notes row, own live margin edge only — row-less restore",
                |e, _| {
                    e.enqueue_note_link("eL".into(), "p".into(), "x".into(), None, 5, false)
                        .unwrap();
                },
                vec![margin("x", "eL", "m")],
                Converges(&[("x", "eL", "m")]),
            ),
            (
                "no notes row, own edge + a second parent's live edge",
                |e, _| {
                    e.enqueue_note_link("eL".into(), "p".into(), "x".into(), None, 5, false)
                        .unwrap();
                    e.enqueue_note_link("e2".into(), "p2".into(), "x".into(), None, 5, false)
                        .unwrap();
                },
                vec![margin("x", "eL", "m")],
                Rejects("referenced by another live edge"),
            ),
            (
                "no notes row, a foreign live edge only",
                |e, _| {
                    e.enqueue_note_link("e2".into(), "p2".into(), "x".into(), None, 5, false)
                        .unwrap();
                },
                vec![margin("x", "eF", "m")],
                Rejects("referenced by another live edge"),
            ),
            // ── Partial-skew + duplicate-edge states (converge; pinned so they stay that way) ──
            (
                "note tombstoned but own edge still live — paired reuse resurrects (restore)",
                |e, db| {
                    e.replace_handwritten_annotations("p".into(), vec![margin("x", "eL", "m")])
                        .unwrap();
                    // Tombstone JUST the note (partial-pull skew): the edge stays live.
                    let tomb = json!({
                        "id": "x",
                        "source": "handwritten",
                        "deleted": true,
                        "created_at": 1,
                        "updated_at": 1
                    });
                    Store::open(db)
                        .unwrap()
                        .apply_row("notes", tomb.as_object().unwrap())
                        .unwrap();
                },
                vec![margin("x", "eL", "m2")],
                Converges(&[("x", "eL", "m2")]),
            ),
            (
                "note live but own edge tombstoned — paired reuse relinks",
                |e, _| {
                    e.replace_handwritten_annotations("p".into(), vec![margin("x", "eL", "m")])
                        .unwrap();
                    e.enqueue_note_link("eL".into(), "p".into(), "x".into(), None, 5, true)
                        .unwrap();
                },
                vec![margin("x", "eL", "m2")],
                Converges(&[("x", "eL", "m2")]),
            ),
            (
                "two live own margin edges to one child — reuse one, the duplicate is cleaned",
                |e, _| {
                    e.replace_handwritten_annotations("p".into(), vec![margin("x", "eL", "m")])
                        .unwrap();
                    e.enqueue_note_link("eL2".into(), "p".into(), "x".into(), None, 5, false)
                        .unwrap();
                },
                vec![margin("x", "eL", "m2")],
                Converges(&[("x", "eL", "m2")]),
            ),
            (
                "reused link whose stored row is a p→p self-edge — repaired, parent kept",
                |e, _| {
                    e.enqueue_note_link("y".into(), "p".into(), "p".into(), None, 5, false)
                        .unwrap();
                },
                vec![margin("cF", "y", "m")],
                Converges(&[("cF", "y", "m")]),
            ),
            (
                "two reused pairs crossed — edges swap to the requested pairing",
                |e, _| {
                    e.replace_handwritten_annotations(
                        "p".into(),
                        vec![margin("a", "eA", "m"), margin("b", "eB", "m")],
                    )
                    .unwrap();
                },
                vec![margin("a", "eB", "ra"), margin("b", "eA", "rb")],
                Converges(&[("a", "eB", "ra"), ("b", "eA", "rb")]),
            ),
        ];

        for (name, setup, children, expect) in cells {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("t.sqlite");
            let db_path = db.to_str().unwrap();
            let engine = engine_at(db_path);
            engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
            setup(&engine, db_path);
            match expect {
                Converges(want) => {
                    engine
                        .replace_handwritten_annotations("p".into(), children)
                        .unwrap_or_else(|e| panic!("{name}: expected Ok, got {e:?}"));
                    assert_margins_converged(&engine, db_path, "p", Some("b1"), want);
                }
                Rejects(sub) => {
                    // A reject must stage NOTHING — drain the setup's writes first so an empty
                    // outbox after the failed call proves it. (For the dangling no-row cells this
                    // is the no-resurrection guarantee: not one byte queued for the fleet.)
                    drain_outbox(db_path);
                    let err = engine
                        .replace_handwritten_annotations("p".into(), children)
                        .unwrap_err();
                    let SyncError::Store(msg) = &err else {
                        panic!("{name}: wrong error kind {err:?}")
                    };
                    assert!(msg.contains(sub), "{name}: `{msg}` should contain `{sub}`");
                    assert!(
                        Store::open(db_path)
                            .unwrap()
                            .outbox_items()
                            .unwrap()
                            .is_empty(),
                        "{name}: rejected call staged something"
                    );
                }
            }
        }
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
