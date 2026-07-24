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

use crate::embeddings::{
    self, EmbedSummary, Embedder, EmbedderError, RegisterEmbedderSummary, SemanticHit,
};
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
    /// An embedding method was called before [`SyncEngine::register_embedder`] (SUR-997).
    #[error("no embedder registered")]
    EmbedderNotRegistered,
    /// An embedding operation failed (SUR-997). The message is always core-authored — a
    /// host's own error detail never transits through core ([`EmbedderError`] is fieldless).
    #[error("embed error: {0}")]
    Embed(String),
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
/// created for a note tag orphaned from the current canon, duplicate notes collapsed by shared
/// `content_tag` (SUR-835), `note_signals` rows retired for locally-tombstoned notes (SUR-976),
/// and book covers resolved via Open Library for natively-created books (SUR-828). Nested onto
/// [`PullSummary`] (not flattened) — a pull-mechanics count (`pulled`/`merged`) and a
/// reconciliation-outcome count are different concerns. A reconciliation failure never fails the
/// `pull`/`sync` it's attached to (best-effort — see [`reconcile`]); this summary is all-zero in
/// that case.
#[derive(Debug, Default, uniffi::Record)]
pub struct ReconcileSummary {
    pub books_backfilled: u32,
    pub notes_rehomed: u32,
    pub notes_detached: u32,
    pub ideas_created: u32,
    pub dupes_collapsed: u32,
    /// `note_signals` rows retired for a locally-tombstoned note (SUR-976).
    pub signals_retired: u32,
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
            signals_retired: r.signals_retired as u32,
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

/// The kind of behavioural signal a host records for a note (SUR-966), mirroring surfc
/// `applyNoteSignal`. Collection lives HERE (not host-side) because `note_signals` is a
/// whole-row LWW table with no FFI read-back — a host can't increment a counter it can't read
/// without clobbering another device's earned counters. Only the mutation math differs per kind;
/// `record_note_signal` owns it so the scoring constants stay in one place (the SUR-475
/// calibration harness retunes them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NoteSignalKind {
    /// The note passed the reader's eyes — bumps `exposure_recency_at`, throttled (see
    /// [`SIGNAL_THROTTLE_MS`]): a scroll-list re-seeing the same note all afternoon must not
    /// restage the identical "recently seen" fact fifty times.
    Exposure,
    /// A deliberate act on the note (a reflective interaction) — bumps `engagement_recency_at`.
    /// NOT throttled: engagement fires only on rare, intentional acts, so every call is genuine
    /// evidence ranking should trust.
    Engagement,
    /// The reader returned to re-read the note — `return_visits += 1` and bumps
    /// `exposure_recency_at`. Deliberately NOT engagement (the PWA: "re-reading isn't reflection").
    ReturnVisit,
}

/// Exposure-write dedup window (SUR-966; founder-approved 1 hour). Exposure stores a *timestamp*,
/// not a count, so a note re-seen inside this window would restage the identical "recently seen"
/// fact — every scroll-pass another outbox row saying exactly what the first said. Skip the write
/// when `now - exposure_recency_at` is within it. This does NOT change what the reader sees or the
/// ranking (the value being written is already "recently"). It is deliberately NOT `scoring.js`'s
/// `EXPOSURE_COOLDOWN_MS` (7 days — the *selector's* don't-show-again rule): different job, different
/// number, do not reuse. Applies to [`NoteSignalKind::Exposure`] only — engagement and return-visit
/// fire on deliberate, inherently-rare acts and are never throttled (LRN-20260610-001: a named
/// tunable, never inlined at the call site).
const SIGNAL_THROTTLE_MS: i64 = 60 * 60 * 1000;

/// The mutable counter state of a `note_signals` row, read from the stored row (or birth defaults)
/// and mutated by a signal write before it is recomputed + staged (SUR-966). `source_prior` and
/// `created_at` are effectively immutable per note but carried so the whole-row stage preserves
/// them. Equality drives the change-detection no-op: a live row a mutation leaves untouched stages
/// nothing.
///
/// `importance` is INSIDE change-detection (SUR-977) with an asymmetric contract: `before` reads
/// the STORED value verbatim (the pre-image invariant — nothing derived may enter it; a LIVE
/// row's non-numeric stored value reads as NaN so it can never no-op and must heal), `after` is
/// recomputed from the post-mutation fields, and mutation closures never touch it. So a stored
/// value that disagrees with the formula for its own row IS a diff, and the next signal — even a
/// throttled no-op one — stages the correction. Comparison is [`signals_agree`], NOT derived
/// `PartialEq` — deliberately not derivable, so an exact f64 compare can't sneak back in.
#[derive(Clone)]
struct SignalState {
    source_prior: f64,
    return_visits: i64,
    has_annotation: bool,
    stitch_spawns: i64,
    exposure_recency_at: i64,
    engagement_recency_at: i64,
    importance: f64,
}

/// `importance` disagreements below this stage nothing. `compute_importance` runs `.exp()` — a
/// system-libm transcendental with NO cross-implementation bit-determinism — so an exact compare
/// would let two devices on divergent libms (bionic vs Apple vs glibc, or vs the PWA's
/// `Math.exp`) ping-pong one-ULP "corrections" forever, each a fresh whole-row LWW write
/// (SUR-977 sync-reviewer). A sub-epsilon lie is harmless to ranking; a real one (the blind-FFI
/// class) is orders of magnitude larger.
const IMPORTANCE_EPSILON: f64 = 1e-9;

/// The change-detection compare (SUR-977): exact on every counter/stamp/prior, epsilon-tolerant
/// on the derived `importance` only. NaN-safe by construction — a NaN `importance` (a live row's
/// laundered non-numeric stored value) fails the epsilon test against everything, so the row
/// always stages its heal.
fn signals_agree(a: &SignalState, b: &SignalState) -> bool {
    a.source_prior == b.source_prior
        && a.return_visits == b.return_visits
        && a.has_annotation == b.has_annotation
        && a.stitch_spawns == b.stitch_spawns
        && a.exposure_recency_at == b.exposure_recency_at
        && a.engagement_recency_at == b.engagement_recency_at
        && (a.importance - b.importance).abs() <= IMPORTANCE_EPSILON
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
    /// The host-registered embedder (SUR-997), with its descriptor-derived identity cached
    /// at registration so no later call re-crosses the FFI for it. `None` until
    /// [`SyncEngine::register_embedder`]; every embedding method requires it.
    embedder: Mutex<Option<RegisteredEmbedder>>,
}

/// A registered embedder plus its cached identity. `descriptor()` is a foreign call — it
/// runs ONCE at registration (validated there), never under a lock afterwards.
#[derive(Clone)]
struct RegisteredEmbedder {
    embedder: Arc<dyn Embedder>,
    corpus_key: String,
    dims: u32,
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
            embedder: Mutex::new(None),
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
    /// ATOMIC SIGNALS RETIRE (SUR-975): a `deleted: true` write — either path — also stages the
    /// note's `note_signals` tombstone (the same full-shape tombstone
    /// [`SyncEngine::soft_delete_signals_for_note`] stages) in the SAME transaction, so a note
    /// tombstone can never commit with its signals tombstone unqueued — the two-separate-calls
    /// crash window this closes. Already-tombstoned signals stage nothing (no `updated_at`
    /// churn), so a host still calling `soft_delete_signals_for_note` afterwards double-stages
    /// nothing. Live writes (`deleted: false`) never touch `note_signals` here.
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
                if deleted {
                    // SUR-975: the note tombstone and its signals tombstone commit together.
                    // The full write always has a real `source` in hand (explicit or the
                    // "manual" default) — pass it so a tombstone born here seeds the true
                    // prior, never the unknown-source fallback.
                    let store = lock!(self.store);
                    let note_source = row
                        .get("source")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let mut writes = vec![("notes", id.clone(), row)];
                    if let Some(tomb) =
                        Self::build_signals_tombstone(&store, &id, note_source.as_deref(), now)?
                    {
                        writes.push(("note_signals", id.clone(), tomb));
                    }
                    // SUR-959: retire this note's handwritten child-edges + recompute each affected
                    // parent's has_annotation, in the SAME batch (reads the still-live edges before
                    // the note tombstone commits).
                    writes.extend(self.stage_annotation_cascade_on_delete(&store, &id, now)?);
                    return store.stage_local_writes(writes, now).map_err(store_err);
                }
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
                self.stage_existing_live_note_patch(&id, row, deleted, now)
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
    /// - The parent's `note_signals` row rides the SAME batch (SUR-956/SUR-966; the PWA fires
    ///   `refreshAnnotationSignal` on every margin save, and importance scoring weights the flag at
    ///   0.3): the stored signals row — or a birth-defaults row with the prior derived from the
    ///   parent's `source` — is re-staged WHOLE with `has_annotation: true`, an
    ///   `engagement_recency_at` bump (SUR-966 §2: "Add the margins" is a single, always-deliberate
    ///   act → a genuine engagement signal), and `importance` recomputed, preserving earned
    ///   behavioural counters verbatim. The engagement bump fires on EVERY save, including on a note
    ///   already flagged — the shared [`SyncEngine::stage_signal_write`] helper's change-detection
    ///   still no-ops only when truly nothing moved. Dropping the flag to false (a margins-delete
    ///   recompute) is deliberately NOT here — this op never ends with zero margins (SUR-959).
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

        // SUR-956/SUR-966: the op ends with ≥1 live margin, so the parent's signals row rides the
        // SAME batch — `has_annotation: true` AND an `engagement_recency_at` bump. Read-merge-stage
        // IN CORE via the shared helper: `enqueue_note_signals` is a blind whole-row LWW write a
        // host must never point at this (it would clobber earned counters), and the FFI has no
        // signals read. "Add the margins" has a single, always-deliberate caller, so it is a genuine
        // engagement signal (SUR-966 §2/§5 — the ONLY engagement signal that can safely live in
        // core). Firing engagement UNCONDITIONALLY here (not gated on the old `has_annotation`
        // no-op) is deliberate: a note already carrying an annotation must STILL get its engagement
        // bump on every re-save, else already-annotated notes would never record margin engagement.
        // The helper's change-detection still no-ops when truly nothing moved. Engagement is never
        // throttled. The recompute-to-FALSE half lives in the future margins-delete path (SUR-959).
        if let Some(sig) = self.stage_signal_write(
            &store,
            &parent_id,
            parent.get("source").and_then(Value::as_str),
            now,
            |s| {
                s.has_annotation = true;
                s.engagement_recency_at = now;
            },
        )? {
            writes.push(("note_signals", parent_id.clone(), sig));
        }

        store.stage_local_writes(writes, now).map_err(store_err)?;
        Ok(children.len() as u32)
    }

    /// Record a behavioural signal for a note (SUR-966), mirroring surfc `applyNoteSignal`. Owns the
    /// per-kind mutation IN CORE — the FFI has no `note_signals` read-back, so a host cannot safely
    /// increment a counter it cannot read (it would clobber another device's earned counters over
    /// whole-row LWW). Reads the stored row (or births defaults with `source_prior` from the note's
    /// `source`), applies the mutation for `kind`, recomputes `importance`, and whole-row stages it
    /// (`deleted: false`, so a queued tombstone is dropped — the resurrect rule):
    ///  - [`NoteSignalKind::ReturnVisit`] → `return_visits += 1`, `exposure_recency_at = now`
    ///    (deliberately NOT engagement — "re-reading isn't reflection").
    ///  - [`NoteSignalKind::Exposure`] → `exposure_recency_at = now`, throttled by
    ///    [`SIGNAL_THROTTLE_MS`]. An absent/epoch stamp (a first-ever signal of another kind birthed
    ///    the row) counts as "never exposed", so the first real Exposure always writes — no unsigned
    ///    underflow, no NULL short-circuit suppressing it.
    ///  - [`NoteSignalKind::Engagement`] → `engagement_recency_at = now`. NOT throttled: engagement
    ///    fires only on rare, deliberate acts, so throttling would drop real evidence.
    ///
    /// Returns `true` if a row was staged, `false` on a change-detection no-op / throttled write
    /// (nothing staged, no `updated_at` bump) — or when the note is NOT LOCALLY VISIBLE, i.e. its
    /// row is absent or tombstoned. Only a note this device can actually see earns a signal:
    ///  - deleted → staging would take the resurrect path and drop the queued signals tombstone,
    ///    leaking live metadata for a dead note;
    ///  - absent → there is no `source` to derive `source_prior` from, so the row would be born at
    ///    the unknown-source fallback and pinned there (nothing re-derives a stored prior — SUR-956,
    ///    v0.9.1), permanently under-scoring the note in [`compute_importance`].
    ///
    /// A signal for a note the host cannot render is near-unreachable in practice, and signals are
    /// cheap and repeat, so dropping one racing an unsynced note costs nothing next to storing a
    /// wrong prior forever.
    pub fn record_note_signal(
        &self,
        note_id: String,
        kind: NoteSignalKind,
    ) -> Result<bool, SyncError> {
        let store = lock!(self.store);
        let now = epoch_ms();
        let note = store.get_row("notes", &note_id).map_err(store_err)?;
        // ONLY A LOCALLY-VISIBLE NOTE EARNS A SIGNAL. Two different leaks close on this one guard:
        //
        //  - DELETED: a late callback landing after the host's delete would take the resurrect path
        //    (`was_live == false` always stages) and `stage_local_writes` would drop the queued
        //    tombstone — live signal metadata for a dead note.
        //  - ABSENT: with no note we cannot know its `source`, so the row would be born at the
        //    unknown-source fallback. Nothing re-derives a stored prior (SUR-956's "stored prior
        //    kept, not re-derived", v0.9.1), so a `handwritten` (0.9), `share` (0.75) or `manual`
        //    (0.7) note would sit at 0.5 forever, under-scored by `compute_importance`.
        //
        // The ABSENT arm buys "we never guess a prior we could have known", NOT "the fallback is
        // unreachable". `notes.source` is nullable, so a perfectly VISIBLE sourceless note derives
        // 0.5 and pins there — correctly: there is nothing better to derive from, and the PWA
        // behaves the same. (Locally it cannot happen: `enqueue_note`'s create path defaults source
        // to "manual". Such a row arrives by PULL or IMPORT, landing verbatim.)
        //
        // Refusing the absent case is what lets the prior stay a plain read. The alternative — birth
        // at a sentinel and heal later — cannot work: a stored 0.5 is genuinely ambiguous (a
        // `readwise` note derives 0.5, and an import or the blind `enqueue_note_signals` FFI can
        // write a real 0.5 onto a note whose source derives higher), so healing on the value would
        // overwrite legitimate priors and break the very invariant it was protecting.
        //
        // Same-device only, and deliberately so: it does not close the leak fleet-wide. A device
        // that has not yet pulled a tombstone still holds a LIVE local row whose signal wins on
        // whole-row LWW. Retiring those needs a post-pull signals reconciliation pass — SUR-976.
        let Some(note) = note.filter(|r| !matches!(r.get("deleted"), Some(Value::Bool(true))))
        else {
            return Ok(false);
        };
        let note_source = note
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string);
        let write =
            self.stage_signal_write(&store, &note_id, note_source.as_deref(), now, |s| {
                match kind {
                    NoteSignalKind::Exposure => {
                        // Throttle: skip when exposed within the window. Treat an absent/epoch stamp as
                        // "never exposed" (cold start) so the first real Exposure always writes.
                        let never_exposed = s.exposure_recency_at <= 0;
                        if never_exposed
                            || now.saturating_sub(s.exposure_recency_at) >= SIGNAL_THROTTLE_MS
                        {
                            s.exposure_recency_at = now;
                        }
                    }
                    NoteSignalKind::Engagement => s.engagement_recency_at = now,
                    NoteSignalKind::ReturnVisit => {
                        s.return_visits += 1;
                        s.exposure_recency_at = now;
                    }
                }
            })?;
        match write {
            Some(sig) => {
                store
                    .stage_local_writes(vec![("note_signals", note_id.clone(), sig)], now)
                    .map_err(store_err)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Tombstone a note's `note_signals` row on note delete (SUR-966), mirroring the surfc oracle.
    /// ALWAYS stages a tombstone even when this device has NO local signals row (a birth row is
    /// local-only, and another device may hold a live cloud row from its own bump), so the delete
    /// tears that cross-device row down instead of leaking it as orphaned metadata. A repeat call on
    /// an already-tombstoned row is a no-op (no `updated_at` churn).
    ///
    /// Since SUR-975 an ordinary note delete no longer needs this call: `enqueue_note` with
    /// `deleted: true` stages the same tombstone (built by [`SyncEngine::build_signals_tombstone`])
    /// in the note tombstone's own transaction. This stays exported for the cases `enqueue_note`
    /// cannot cover — retiring signals for a note with no LIVE local row, i.e. ABSENT (the
    /// cross-device rule) or already TOMBSTONED (e.g. a pre-SUR-975 device crashed inside the old
    /// two-call window and pushed a note tombstone with its signals row still live; the delete-
    /// patch path refuses a dead target). For the tombstoned case the post-pull reconciler
    /// (`reconcile_note_signals`, SUR-976) also retires such orphans systematically on the next
    /// pull — this call remains the ON-DEMAND repair for a host that wants it fixed sooner — and
    /// it stays the no-op-safe second half of the legacy two-call sequence.
    ///
    /// Staged through the plain `stage_local_writes` path, which stages a `deleted: true` write
    /// unconditionally (no existing-live precondition), so the no-local-row tombstone is never
    /// silently dropped.
    pub fn soft_delete_signals_for_note(&self, note_id: String) -> Result<(), SyncError> {
        let store = lock!(self.store);
        let now = epoch_ms();
        let note_source = store
            .get_row("notes", &note_id)
            .map_err(store_err)?
            .and_then(|r| r.get("source").and_then(Value::as_str).map(str::to_string));
        match Self::build_signals_tombstone(&store, &note_id, note_source.as_deref(), now)? {
            Some(tomb) => store
                .stage_local_writes(vec![("note_signals", note_id, tomb)], now)
                .map_err(store_err),
            None => Ok(()),
        }
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
    ///
    /// `source_prior` and `importance` must be FINITE (SUR-977): `json!` cannot represent a
    /// non-finite f64, so NaN/±inf would be silently laundered to a stored JSON null — which a
    /// later signal must then heal (importance) or derive around (prior). Rejecting at this trust
    /// boundary keeps the stored row numeric, the same posture the import path already takes.
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
        if !source_prior.is_finite() || !importance.is_finite() {
            return Err(SyncError::Store(
                "source_prior and importance must be finite numbers".into(),
            ));
        }
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
    /// Read-merge-stage a `note_signals` row (SUR-966; extracted verbatim from SUR-956's inline
    /// margins block so `replace_handwritten_annotations` and `record_note_signal` share ONE
    /// read-merge-stage — the FFI has no signals read-back, so this math cannot live host-side
    /// without clobbering earned counters). Reads the stored row (or births defaults with
    /// `source_prior` derived from `note_source`), applies `mutate` to the counters, recomputes
    /// `importance`, and returns the WHOLE row to stage (`deleted: false`) — or `None` when nothing
    /// moved on a LIVE row (the PWA `applyNoteSignal` change-detection no-op: no `updated_at` bump,
    /// no outbox churn). A tombstoned or absent row always stages (birth/resurrect): the caller's
    /// live `deleted: false` write drops any queued tombstone (the [`Store::stage_local_writes`]
    /// resurrect rule). The caller pushes the returned map into its own `stage_local_writes` batch,
    /// so the margins op keeps staging the signals row in the SAME transaction as its children.
    fn stage_signal_write(
        &self,
        store: &Store,
        note_id: &str,
        note_source: Option<&str>,
        now: i64,
        mutate: impl FnOnce(&mut SignalState),
    ) -> Result<Option<Map<String, Value>>, SyncError> {
        let existing = store.get_row("note_signals", note_id).map_err(store_err)?;
        let was_live = existing
            .as_ref()
            .is_some_and(|s| !matches!(s.get("deleted"), Some(Value::Bool(true))));
        let row = existing.unwrap_or_default();
        let int_or =
            |field: &str, default: i64| row.get(field).and_then(Value::as_i64).unwrap_or(default);
        // Stored columns verbatim, or birth defaults (`freshNoteSignals`): prior from the note's
        // `source`, zeroed counters. `created_at` is preserved (or born now) around the mutation.
        //
        // `source_prior` is a PLAIN READ: stored verbatim, or derived at birth from the note's
        // `source`. Nothing re-derives it afterwards — SUR-956's "stored prior kept, not
        // re-derived" invariant (v0.9.1). Both callers refuse a note they cannot see
        // (`record_note_signal`'s visibility guard, `replace_handwritten_annotations`' live-parent
        // requirement), so a row is never born from a source we could have read but didn't. Do NOT
        // relax either guard without revisiting this line.
        //
        // That is NOT a claim the fallback is unreachable — two paths still reach it legitimately,
        // and both pin 0.5 permanently because nothing re-derives:
        //  - a VISIBLE note with a null `source` (the column is nullable) — correct, there is
        //    nothing better to derive;
        //  - a tombstone staged by `soft_delete_signals_for_note` when this device holds no local
        //    note row: it must stage regardless (the cross-device rule), so it fabricates the
        //    fallback — and that stored value is the seed a later resurrect reads verbatim here.
        //
        // `before` MIRRORS THE STORED ROW, verbatim. Nothing derived belongs here: the no-op check
        // below compares against it to decide whether the persisted row already says what we are
        // about to write, so anything computed into `before` is a change that silently cannot be
        // detected. `importance`'s two fallbacks keep that spirit exactly (SUR-977 sync-reviewer):
        //  - absent/tombstoned row → birth value; `!was_live` always stages, never compared.
        //  - LIVE row with a non-numeric stored importance (a pre-guard blind-FFI write laundered
        //    to JSON null — `json!` cannot represent a non-finite f64) → NaN, which equals
        //    NOTHING, so the compare below can never no-op and the row heals to the recomputed
        //    finite value. A derived stand-in here would equal `after` by construction and shield
        //    the null forever.
        let stored_source_prior = row
            .get("source_prior")
            .and_then(Value::as_f64)
            .unwrap_or_else(|| source_prior(note_source));
        let return_visits = int_or("return_visits", 0);
        let has_annotation = matches!(row.get("has_annotation"), Some(Value::Bool(true)));
        let stitch_spawns = int_or("stitch_spawns", 0);
        let stored_importance = row.get("importance").and_then(Value::as_f64);
        let before = SignalState {
            source_prior: stored_source_prior,
            return_visits,
            has_annotation,
            stitch_spawns,
            exposure_recency_at: int_or("exposure_recency_at", 0),
            engagement_recency_at: int_or("engagement_recency_at", 0),
            importance: stored_importance.unwrap_or(if was_live {
                f64::NAN // force the heal — see above
            } else {
                compute_importance(
                    stored_source_prior,
                    return_visits,
                    has_annotation,
                    stitch_spawns,
                )
            }),
        };
        let created_at = int_or("created_at", now);
        let mut after = before.clone();
        mutate(&mut after);
        // `importance` is recomputed on the AFTER side only (SUR-977): derived values belong
        // there, never in the pre-image. A stored value disagreeing with the formula for its own
        // row therefore differs from `after` even when the mutation changed nothing — the
        // correction stages. A consistent row recomputes (near-)identically and still no-ops.
        after.importance = compute_importance(
            after.source_prior,
            after.return_visits,
            after.has_annotation,
            after.stitch_spawns,
        );
        // Change-detection no-op: a LIVE row the mutation left byte-identical stages nothing (an
        // Exposure inside the throttle window, a repeat of an already-set flag). A tombstoned or
        // absent row (`!was_live`) always stages — the resurrect/birth path.
        //
        // ACCEPTED (SUR-977, under SUR-737's ratified lossiness): a formerly write-silent
        // throttled signal on a LYING row now pushes a whole corrected row with a fresh
        // `updated_at`, which can beat another device's just-pushed earned counter under
        // whole-row LWW — at most once per lying row per device, then the row is consistent and
        // the path is write-silent again.
        if was_live && signals_agree(&after, &before) {
            return Ok(None);
        }
        let mut sig = Map::new();
        sig.insert("note_id".into(), json!(note_id));
        sig.insert("source_prior".into(), json!(after.source_prior));
        sig.insert("return_visits".into(), json!(after.return_visits));
        sig.insert("has_annotation".into(), json!(after.has_annotation));
        sig.insert("stitch_spawns".into(), json!(after.stitch_spawns));
        sig.insert(
            "exposure_recency_at".into(),
            json!(after.exposure_recency_at),
        );
        sig.insert(
            "engagement_recency_at".into(),
            json!(after.engagement_recency_at),
        );
        sig.insert("importance".into(), json!(after.importance));
        sig.insert("created_at".into(), json!(created_at));
        sig.insert("updated_at".into(), json!(now));
        sig.insert("deleted".into(), json!(false));
        Ok(Some(sig))
    }

    /// Build the full-shape `note_signals` tombstone for `note_id` — build-don't-write, the
    /// tombstone twin of [`SyncEngine::stage_signal_write`]: the caller stages the returned row in
    /// its own batch. Returns `None` when the row is already tombstoned locally (repeat delete —
    /// stage nothing, no `updated_at` churn). Shared by `soft_delete_signals_for_note` and
    /// `enqueue_note`'s `deleted: true` paths (SUR-975).
    ///
    /// `note_source` is a plain pass-through like `stage_signal_write`'s: used only to seed
    /// `source_prior` when there is no stored row, never to re-derive a stored prior (SUR-956).
    /// Callers supply the freshest source they hold — the full-write delete passes the source it
    /// is staging right now; the patch/standalone paths read the stored note's.
    ///
    /// Full shape — or birth defaults when absent — NOT a bare `{note_id, deleted}`:
    /// `note_signals` has no sparse-PATCH flush fallback (only `notes` does — `push.rs`), so a
    /// minimal payload would risk a NOT-NULL upsert reject that wedges the outbox (the SUR-942
    /// note_links lesson).
    // An associated fn, not a method: it never touches `self` (the caller supplies the already-
    // locked `&Store`), and the SUR-976 reconcile pass — which runs under the engine's held store
    // lock with no `&self` in reach — calls it as `SyncEngine::build_signals_tombstone`.
    pub(crate) fn build_signals_tombstone(
        store: &Store,
        note_id: &str,
        note_source: Option<&str>,
        now: i64,
    ) -> Result<Option<Map<String, Value>>, SyncError> {
        let existing = store.get_row("note_signals", note_id).map_err(store_err)?;
        if existing
            .as_ref()
            .is_some_and(|s| matches!(s.get("deleted"), Some(Value::Bool(true))))
        {
            return Ok(None); // already tombstoned locally — don't churn the outbox / bump updated_at
        }
        let row = existing.unwrap_or_default();
        let int_or =
            |field: &str, default: i64| row.get(field).and_then(Value::as_i64).unwrap_or(default);
        // ACCEPTED: with no local signals row AND no source to derive from (no local note row on
        // the standalone path), this fabricates the unknown-source fallback — and that value is
        // not inert. The tombstone IS a local row, so a later resurrect (`stage_signal_write`'s
        // `!was_live` path) reads this prior VERBATIM; nothing re-derives it. A `handwritten` note
        // that gets retired that way and later arrives live therefore resurrects pinned at 0.5,
        // under-scored in `compute_importance` and pushed fleet-wide over whole-row LWW.
        //
        // Not fixable by refusing, the way `record_note_signal` refuses: the cross-device contract
        // REQUIRES staging a tombstone even with no local row (another device may hold a live
        // cloud row from its own bump), and the flush shape has no room to omit the column.
        // Reaching it is narrow — an ordinary delete reads the note's `source` fine (the callers'
        // lookups deliberately omit the `deleted` filter), and `enqueue_note`'s full-write delete
        // always has a real source in hand. Tracked as part of SUR-976. Pinned by
        // `soft_delete_with_no_note_row_seeds_the_fallback_prior_for_a_later_resurrect`.
        let prior = row
            .get("source_prior")
            .and_then(Value::as_f64)
            .unwrap_or_else(|| source_prior(note_source));
        let return_visits = int_or("return_visits", 0);
        let stitch_spawns = int_or("stitch_spawns", 0);
        let has_annotation = matches!(row.get("has_annotation"), Some(Value::Bool(true)));
        let mut tomb = Map::new();
        tomb.insert("note_id".into(), json!(note_id));
        tomb.insert("source_prior".into(), json!(prior));
        tomb.insert("return_visits".into(), json!(return_visits));
        tomb.insert("has_annotation".into(), json!(has_annotation));
        tomb.insert("stitch_spawns".into(), json!(stitch_spawns));
        tomb.insert(
            "exposure_recency_at".into(),
            json!(int_or("exposure_recency_at", 0)),
        );
        tomb.insert(
            "engagement_recency_at".into(),
            json!(int_or("engagement_recency_at", 0)),
        );
        tomb.insert(
            "importance".into(),
            json!(compute_importance(
                prior,
                return_visits,
                has_annotation,
                stitch_spawns
            )),
        );
        tomb.insert("created_at".into(), json!(int_or("created_at", now)));
        // MONOTONE OVER THE ROW IT RETIRES (SUR-976 sync-reviewer): the server's `t01_lww_guard`
        // SILENTLY cancels a strictly-older write (statement still 2xx, no change_seq bump), and
        // the flush would clear the outbox row as if it landed — with this device already locally
        // tombstoned, nothing would ever retry: a retry-immune divergence. Reachable whenever this
        // device's clock trails the stamp on a just-pulled foreign row (the reconcile retirement
        // races a stamp only seconds old; the delete paths share the smaller human-scale window).
        // Clamping to strictly-after the stored stamp restores the 0050 §4.1 monotonicity
        // assumption for every signals-tombstone path.
        tomb.insert(
            "updated_at".into(),
            json!(now.max(int_or("updated_at", 0) + 1)),
        );
        tomb.insert("deleted".into(), json!(true));
        Ok(Some(tomb))
    }

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

    /// Stage a plaintext-free note patch behind the existing-live precondition. When the patch is
    /// a DELETE (`retire_signals`), the note's `note_signals` tombstone rides the same transaction
    /// as an extra write (SUR-975) — so a failed precondition stages neither, and a committed note
    /// tombstone always has its signals tombstone queued.
    fn stage_existing_live_note_patch(
        &self,
        record_id: &str,
        row: Map<String, Value>,
        retire_signals: bool,
        now: i64,
    ) -> Result<(), SyncError> {
        // `now` is the caller's stamp — the same one already written into the patch row's
        // `updated_at` — so the note row, the signals tombstone, and both outbox rows carry one
        // timestamp, exactly like the full-write delete arm.
        let store = lock!(self.store);
        let mut extra_writes = Vec::new();
        if retire_signals {
            // Prefer a source the patch itself carries; else the stored note's (the read
            // deliberately ignores its `deleted` flag — a re-delete still reads it fine, though
            // the precondition below then rejects the patch before anything stages).
            let note_source = match row.get("source").and_then(Value::as_str) {
                Some(source) => Some(source.to_string()),
                None => store
                    .get_row("notes", record_id)
                    .map_err(store_err)?
                    .and_then(|r| r.get("source").and_then(Value::as_str).map(str::to_string)),
            };
            if let Some(tomb) =
                Self::build_signals_tombstone(&store, record_id, note_source.as_deref(), now)?
            {
                extra_writes.push(("note_signals", record_id.to_string(), tomb));
            }
            // SUR-959: retire this note's handwritten child-edges + recompute each affected parent's
            // has_annotation, riding the SAME existing-live precondition — a re-delete that fails
            // the precondition stages neither the note patch nor this cascade.
            extra_writes.extend(self.stage_annotation_cascade_on_delete(&store, record_id, now)?);
        }
        store
            .stage_local_write_existing_live("notes", record_id, row, extra_writes, now)
            .map_err(|error| match error {
                StageExistingWriteError::TargetMissing => SyncError::PatchTargetMissing,
                StageExistingWriteError::Sql(error) => SyncError::Store(error.to_string()),
            })
    }

    /// SUR-959 — the margins-delete cascade for a note being tombstoned: retire the
    /// `handwritten_annotation` edges for which this note is the CHILD (`to_note_id`), and recompute
    /// each affected parent's `has_annotation` from its SURVIVING live handwritten edges. A parent
    /// that just lost its last margin child drops the 0.3 annotation weight and recomputes
    /// `importance`, instead of crediting a stale signal fleet-wide. Build-don't-write: returns rows
    /// the caller appends to its delete batch, so the note + its signals tombstone + these edge
    /// tombstones + the parent recomputes all commit in ONE transaction (the SUR-975 atomicity,
    /// extended).
    ///
    /// Mirrors the PWA `deleteNote` (`useNoteActions.js` / `db.js`): `softDeleteNoteLinksForNote`'s
    /// child leg + a `refreshAnnotationSignal` per affected parent — count the parent's live
    /// `from_note_id == parent` handwritten edges → `has_annotation`, and SKIP creating a row for a
    /// parent that has neither a signals row nor a surviving edge (`!existing && !hasLive`).
    ///
    /// Handwritten-only, child-leg only (founder scope 2026-07-22): a deleted PARENT's outgoing
    /// edges and any non-`handwritten_annotation` edge belong to the broader note-delete edge
    /// cascade (SUR-84 parity), tracked separately — this path never retires them.
    // `(table, id, row)` is the batch shape `stage_local_writes` consumes verbatim (store.rs); the
    // `Result<Vec<…>>` wrapper is what trips type_complexity, not the tuple the codebase uses raw.
    #[allow(clippy::type_complexity)]
    fn stage_annotation_cascade_on_delete(
        &self,
        store: &Store,
        deleted_note_id: &str,
        now: i64,
    ) -> Result<Vec<(&'static str, String, Map<String, Value>)>, SyncError> {
        const HANDWRITTEN: &str = "handwritten_annotation";
        // An absent `relation_type` defaults to `handwritten_annotation` everywhere else in the
        // margins code (`enqueue_note_link`, the replace path's reuse check `.is_none_or(...)`) —
        // treat None the same here, in BOTH the retire filter and the surviving-edge scan, or a
        // default-handwritten edge with a NULL column is left live and the recompute is skipped.
        let is_handwritten = |rt: &Option<String>| rt.as_deref().is_none_or(|r| r == HANDWRITTEN);
        let mut writes: Vec<(&'static str, String, Map<String, Value>)> = Vec::new();

        // Edges where the deleted note is the handwritten CHILD → retire; their parents may recompute.
        let retiring: Vec<NoteLinkRecord> = read::note_links_for_note(store, deleted_note_id)
            .map_err(store_err)?
            .into_iter()
            .filter(|e| e.to_note_id == deleted_note_id && is_handwritten(&e.relation_type))
            .collect();
        if retiring.is_empty() {
            return Ok(writes);
        }
        let retiring_ids: std::collections::HashSet<&str> =
            retiring.iter().map(|e| e.id.as_str()).collect();

        // Tombstone each retiring edge with the full NOT-NULL shape (SUR-942/SUR-952 — `note_links`
        // has no sparse-PATCH flush fallback, so a bare `{id, deleted}` would 23502 and wedge the
        // outbox). `created_at` is the stored edge's, preserved.
        for edge in &retiring {
            let mut tomb = Map::new();
            tomb.insert("id".into(), json!(edge.id));
            tomb.insert("from_note_id".into(), json!(edge.from_note_id));
            tomb.insert("to_note_id".into(), json!(edge.to_note_id));
            tomb.insert(
                "relation_type".into(),
                json!(edge
                    .relation_type
                    .clone()
                    .unwrap_or_else(|| HANDWRITTEN.into())),
            );
            tomb.insert("created_at".into(), json!(edge.created_at));
            // MONOTONE over the row it retires (SUR-976, as `build_signals_tombstone`): the server's
            // `t01_lww_guard` SILENTLY cancels a strictly-older write and the flush clears the outbox
            // as if it landed — with the edge already locally tombstoned nothing retries, a
            // retry-immune divergence that leaves the edge live fleet-wide. Reachable when this
            // device's clock trails a just-pulled foreign edge's stamp.
            tomb.insert("updated_at".into(), json!(now.max(edge.updated_at + 1)));
            tomb.insert("deleted".into(), json!(true));
            writes.push(("note_links", edge.id.clone(), tomb));
        }

        // Recompute `has_annotation` once per distinct affected parent.
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for edge in &retiring {
            let parent_id = edge.from_note_id.as_str();
            if parent_id == deleted_note_id || !seen.insert(parent_id) {
                continue; // a degenerate p→p self-edge parent IS the note we're deleting; else once
            }
            // Only a LIVE parent needs has_annotation tracked. If the parent note is missing or
            // already tombstoned, its signals must STAY retired: `stage_signal_write` treats a
            // tombstoned row as a birth/resurrect and would stage `deleted: false`, re-creating live
            // signal metadata for a dead note. This child-leg-only cascade leaves a deleted parent's
            // outgoing edge LIVE (SUR-84 scope), so deleting a margin child AFTER its parent still
            // reaches it here — the PWA never does (its full edge cascade tombstoned that edge with
            // the parent, so `refreshAnnotationSignal` is never called for a dead parent).
            let parent_note = store.get_row("notes", parent_id).map_err(store_err)?;
            let parent_live = parent_note
                .as_ref()
                .is_some_and(|r| !matches!(r.get("deleted"), Some(Value::Bool(true))));
            if !parent_live {
                continue;
            }
            // Any SURVIVING live handwritten edge from this parent? Exclude the ids we're retiring —
            // they are not committed yet, so `note_links_for_note` still returns them live.
            let has_live = read::note_links_for_note(store, parent_id)
                .map_err(store_err)?
                .iter()
                .any(|l| {
                    l.from_note_id == parent_id
                        && is_handwritten(&l.relation_type)
                        && !retiring_ids.contains(l.id.as_str())
                });
            // Read the stored parent signals row once — for the skip-create guard AND the stamp clamp.
            let parent_sig = store
                .get_row("note_signals", parent_id)
                .map_err(store_err)?;
            // Skip-create (PWA `refreshAnnotationSignal`): no signals row AND no live edge → nothing
            // to track (an absent row already reads `has_annotation = false`).
            if !has_live && parent_sig.is_none() {
                continue;
            }
            let parent_source = parent_note
                .and_then(|r| r.get("source").and_then(Value::as_str).map(str::to_string));
            if let Some(mut sig) =
                self.stage_signal_write(store, parent_id, parent_source.as_deref(), now, |s| {
                    s.has_annotation = has_live;
                })?
            {
                // MONOTONE over the stored parent signals row (SUR-976, as `build_signals_tombstone`).
                // `stage_signal_write` stamps `updated_at = now`; a recompute-to-false is a
                // convergence-required reconcile, not a best-effort behavioural bump, so if this
                // device's clock trails the pulled parent stamp an unclamped `now` is SILENTLY
                // cancelled by the server's t01 LWW guard and never retried (the outbox clears) —
                // leaving the cloud parent at `has_annotation: true`. Clamp strictly above the stored.
                let stored_updated = parent_sig
                    .as_ref()
                    .and_then(|r| r.get("updated_at").and_then(Value::as_i64))
                    .unwrap_or(0);
                sig.insert("updated_at".into(), json!(now.max(stored_updated + 1)));
                writes.push(("note_signals", parent_id.to_string(), sig));
            }
        }
        Ok(writes)
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

// ── the embedding surface (SUR-997, ADR 0006) ────────────────────────────────
// The five host-facing methods over the sealed vector store + the derived embed queue.
// LOCK DISCIPLINE (load-bearing): no mutex is EVER held across a host `Embedder` call —
// `std::sync::Mutex` is not reentrant, so a host that calls back into the engine
// mid-embed (progress reads are natural there) would deadlock. Every foreign call sits
// between lock scopes, and at most one note's plaintext is live at a time.

impl SyncEngine {
    /// The registration snapshot, cloned out so the embedder mutex is released before any
    /// use. `Err(EmbedderNotRegistered)` before [`SyncEngine::register_embedder`].
    fn registered_embedder(&self) -> Result<RegisteredEmbedder, SyncError> {
        lock!(self.embedder)
            .clone()
            .ok_or(SyncError::EmbedderNotRegistered)
    }

    /// Brute-force cosine top-k over the sealed live corpus (SUR-529: no ANN below ~100k
    /// docs). Vectors are opened with the vault (AAD = their note id) OUTSIDE the store
    /// lock; a blob that fails to open or decode is hard-deleted so the pending derivation
    /// re-embeds that note (self-heal) — it is never surfaced and never wedges.
    fn scan_corpus(
        &self,
        reg: &RegisteredEmbedder,
        probe: &[f32],
        limit: u32,
        exclude: Option<&str>,
    ) -> Result<Vec<SemanticHit>, SyncError> {
        let rows = lock!(self.store)
            .live_embeddings(&reg.corpus_key)
            .map_err(store_err)?;
        let mut hits = Vec::with_capacity(rows.len());
        let mut corrupt: Vec<String> = Vec::new();
        for (note_id, sealed) in rows {
            if exclude == Some(note_id.as_str()) {
                continue;
            }
            match self
                .vault
                .open_bytes(sealed, embeddings::embed_aad(&note_id))
                .and_then(|bytes| embeddings::from_le_bytes(&bytes, reg.dims))
            {
                Ok(vector) => hits.push(SemanticHit {
                    score: embeddings::dot(probe, &vector),
                    note_id,
                }),
                Err(_) => corrupt.push(note_id),
            }
        }
        if !corrupt.is_empty() {
            let store = lock!(self.store);
            for id in &corrupt {
                store.delete_embedding(id).map_err(store_err)?;
            }
        }
        Ok(embeddings::top_k(hits, limit as usize))
    }
}

#[uniffi::export]
impl SyncEngine {
    /// Register the host's embedder (SUR-997 item 1) and reconcile the corpus to its
    /// identity: vectors keyed to a DIFFERENT corpus key are hard-deleted here (their notes
    /// re-enter the derived queue), which is the whole model-upgrade path — the rebuild is
    /// progressive, not a blackout, because the scan filters on the current key and
    /// re-embedded notes return immediately.
    ///
    /// Core refuses a mismatched embedder in three concrete ways rather than a model
    /// allowlist: the descriptor is validated here (non-empty `|`-free identity, dims
    /// 1..=4096); every embed's returned length is checked against the declared dims; and a
    /// corpus-key change invalidates + re-queues. See [`RegisterEmbedderSummary`] for which
    /// returned signal drives which notification UI.
    pub fn register_embedder(
        &self,
        embedder: Arc<dyn Embedder>,
    ) -> Result<RegisterEmbedderSummary, SyncError> {
        // The ONE descriptor() foreign call — before any lock, cached for the registration's
        // lifetime.
        let descriptor = embedder.descriptor();
        if descriptor.model_id.is_empty()
            || descriptor.model_id.contains('|')
            || descriptor.quantization.contains('|')
        {
            return Err(SyncError::Embed(
                "embedder descriptor needs a non-empty, '|'-free identity".into(),
            ));
        }
        if descriptor.dims == 0 || descriptor.dims > 4096 {
            return Err(SyncError::Embed(
                "embedder descriptor dims out of range (1..=4096)".into(),
            ));
        }
        let corpus_key = embeddings::corpus_key(&descriptor);
        *lock!(self.embedder) = Some(RegisteredEmbedder {
            embedder,
            corpus_key: corpus_key.clone(),
            dims: descriptor.dims,
        });
        let store = lock!(self.store);
        let invalidated = store
            .delete_embeddings_not_matching(&corpus_key)
            .map_err(store_err)?;
        let pending = store
            .pending_embedding_count(&corpus_key)
            .map_err(store_err)?;
        Ok(RegisterEmbedderSummary {
            corpus_changed: invalidated > 0,
            invalidated: invalidated as u32,
            pending: pending.max(0) as u32,
        })
    }

    /// The derived embed queue's current size — the host's durable rebuild/progress signal
    /// (survives a process restart, unlike [`RegisterEmbedderSummary::corpus_changed`]; drive
    /// any persistent "search index is rebuilding" UI off this). Zero = corpus current.
    pub fn pending_embed_count(&self) -> Result<u32, SyncError> {
        let reg = self.registered_embedder()?;
        let pending = lock!(self.store)
            .pending_embedding_count(&reg.corpus_key)
            .map_err(store_err)?;
        Ok(pending.max(0) as u32)
    }

    /// Drain up to `max_items` of the derived embed queue (SUR-997 item 5): per note —
    /// read + decrypt in core, hand the PLAINTEXT to the host embedder (the one place note
    /// text leaves core other than display DTOs; no lock held across the call), validate
    /// length + normalize, seal with the vault key (AAD = `emb:{note id}`, domain-separated
    /// from enc:v2), and store — via a write-if-current store check, so a note edited or
    /// deleted during the ~0.8 s embed re-queues instead of storing a stale vector.
    ///
    /// Hosts own the schedule (WorkManager / BGProcessingTask): call in chunks, stop
    /// between calls. One failed embed never halts the pass (counted `failed`, still
    /// queued); [`EmbedderError::Unavailable`] aborts the whole pass. A note with empty text
    /// or undecryptable ciphertext writes a NULL-vector skip marker (mirrors ADR 0005
    /// dropping decrypt failures from the lexical index) so the queue actually drains; its
    /// next edit re-queues it. Orphan vectors are swept once per pass. Re-registering a
    /// different embedder mid-pass is benign: a stale-key row written by the old pass is
    /// invisible to the scan and re-embedded via the derived queue.
    pub fn embed_pending(&self, max_items: u32) -> Result<EmbedSummary, SyncError> {
        let reg = self.registered_embedder()?;
        let ids = {
            let store = lock!(self.store);
            store.sweep_orphan_embeddings().map_err(store_err)?;
            store
                .pending_embeddings(&reg.corpus_key, i64::from(max_items))
                .map_err(store_err)?
        };
        let (mut attempted, mut embedded, mut skipped, mut failed) = (0u32, 0u32, 0u32, 0u32);
        'pass: for id in ids {
            attempted += 1;
            // One lock acquisition for the read leg: the row (ciphertext) + its token.
            let (row, token) = {
                let store = lock!(self.store);
                (
                    store.get_row("notes", &id).map_err(store_err)?,
                    store.note_content_token(&id).map_err(store_err)?,
                )
            };
            let (Some(row), Some(token)) = (row, token) else {
                skipped += 1; // vanished/tombstoned since the queue was derived
                continue;
            };
            // Decrypt in core (vault only — store lock already released).
            let (text, decrypt_failed) = read::decrypt_note_text(&row, &id, &self.vault);
            let content = if decrypt_failed {
                None
            } else {
                text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
            };
            let Some(content) = content else {
                // Skip marker: NULL vector at the current key + token, so the queue drains
                // instead of re-attempting this note every pass forever.
                lock!(self.store)
                    .upsert_embedding_if_current(&id, &reg.corpus_key, &token, None, epoch_ms())
                    .map_err(store_err)?;
                skipped += 1;
                continue;
            };
            // The host callback — NO locks held (see the section header).
            let vector = match reg.embedder.embed_document(content) {
                Ok(v) => v,
                Err(EmbedderError::Runtime) => {
                    failed += 1;
                    continue;
                }
                Err(EmbedderError::Unavailable) => {
                    failed += 1;
                    break 'pass; // the runtime is gone; the host re-drains later
                }
            };
            if vector.len() != reg.dims as usize {
                failed += 1; // dims contract violated — never store a mis-sized vector
                continue;
            }
            let Some(unit) = embeddings::normalize(vector) else {
                failed += 1; // zero/NaN/Inf output — unusable
                continue;
            };
            let sealed = self
                .vault
                .seal_bytes(embeddings::to_le_bytes(&unit), embeddings::embed_aad(&id));
            let wrote = lock!(self.store)
                .upsert_embedding_if_current(
                    &id,
                    &reg.corpus_key,
                    &token,
                    Some(&sealed),
                    epoch_ms(),
                )
                .map_err(store_err)?;
            if wrote {
                embedded += 1;
            } else {
                skipped += 1; // edited/deleted mid-embed — re-queues with its new token
            }
        }
        let pending = lock!(self.store)
            .pending_embedding_count(&reg.corpus_key)
            .map_err(store_err)?;
        Ok(EmbedSummary {
            attempted,
            embedded,
            skipped,
            failed,
            pending: pending.max(0) as u32,
        })
    }

    /// Semantic search (SUR-997 item 4 → SUR-157): embed the query via the host embedder
    /// (its query prompt template), then brute-force cosine top-k over the sealed live
    /// corpus, decrypting vectors only in core. Mid-rebuild this returns the already
    /// re-embedded notes — partial, not empty ([`SyncEngine::pending_embed_count`] reports
    /// the gap). Empty/whitespace queries return `[]` without an embed (the lexical
    /// engine's "no search-everything surprise", and a query embed costs ~0.8 s on CPU).
    pub fn semantic_search(
        &self,
        query: String,
        limit: u32,
    ) -> Result<Vec<SemanticHit>, SyncError> {
        let reg = self.registered_embedder()?;
        if query.trim().is_empty() || limit == 0 {
            return Ok(vec![]);
        }
        // Host callback with no lock held.
        let vector = match reg.embedder.embed_query(query) {
            Ok(v) => v,
            Err(e) => return Err(SyncError::Embed(e.to_string())), // core-authored text only
        };
        if vector.len() != reg.dims as usize {
            return Err(SyncError::Embed(
                "embedder returned a query vector of the wrong dimension".into(),
            ));
        }
        let probe = embeddings::normalize(vector).ok_or_else(|| {
            SyncError::Embed("embedder returned a zero or non-finite query vector".into())
        })?;
        self.scan_corpus(&reg, &probe, limit, None)
    }

    /// Notes semantically nearest to `note_id` (SUR-997 item 4 → SUR-647/SUR-996's
    /// embedding upgrades), probing with its STORED vector — no embed call, so this is
    /// cheap. The probe note is excluded from its own results. `[]` when the note has no
    /// current-key vector yet (not embedded, skip marker, or stale key) — indistinguishable
    /// here from "no neighbours", by design: the host's rebuild signal is
    /// [`SyncEngine::pending_embed_count`], not this.
    pub fn similar_notes(
        &self,
        note_id: String,
        limit: u32,
    ) -> Result<Vec<SemanticHit>, SyncError> {
        let reg = self.registered_embedder()?;
        if limit == 0 {
            return Ok(vec![]);
        }
        let sealed = lock!(self.store)
            .sealed_embedding(&note_id, &reg.corpus_key)
            .map_err(store_err)?;
        let Some(sealed) = sealed else {
            return Ok(vec![]);
        };
        let probe = match self
            .vault
            .open_bytes(sealed, embeddings::embed_aad(&note_id))
            .and_then(|bytes| embeddings::from_le_bytes(&bytes, reg.dims))
        {
            Ok(v) => v,
            Err(_) => {
                // A corrupt probe: drop it so the derived queue re-embeds the note, and
                // report no neighbours for now (same self-heal as the scan's).
                lock!(self.store)
                    .delete_embedding(&note_id)
                    .map_err(store_err)?;
                return Ok(vec![]);
            }
        };
        self.scan_corpus(&reg, &probe, limit, Some(&note_id))
    }
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
        // SUR-975: the delete-patch stages TWO rows — the note tombstone plus the signals
        // tombstone riding the same transaction.
        let items = store.outbox_items().unwrap();
        assert_eq!(items.len(), 2, "note tombstone + signals tombstone");
        let note_payload_json = &items
            .iter()
            .find(|(_, t, _, _, _)| t == "notes")
            .expect("notes outbox row")
            .3;
        let payload: Value = serde_json::from_str(note_payload_json).unwrap();
        assert_eq!(payload["deleted"], json!(true));
        assert!(payload.get("text").is_none());
        assert!(payload.get("content_tag").is_none());
        assert_eq!(
            store.get_row("note_signals", "n1").unwrap().unwrap()["deleted"],
            json!(true),
            "the signals tombstone landed with the note tombstone"
        );
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

    // ── SUR-959 — margins-delete recomputes the parent's has_annotation ──────────

    #[test]
    fn deleting_the_last_margin_child_tombstones_the_edge_and_drops_parent_has_annotation() {
        // The patch-delete route (plaintext-free `deleted: true`, the host's soft-delete). After a
        // create→flush, the delete batch stands alone: the child's handwritten edge tombstone must
        // carry the full NOT-NULL shape, AND the parent — now with no live margin — recomputes
        // has_annotation to false with importance re-derived. All in ONE batch.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "a margin")])
            .unwrap();
        drain_outbox(db_path); // creates flushed; the delete batch queues its tombstones ALONE

        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_patch("c1")
            })
            .unwrap();

        // Edge e1 (p→c1) tombstoned with the full NOT-NULL shape (no sparse-PATCH fallback).
        let e1 = collapsed_payload_for(db_path, "note_links", "e1").expect("edge tombstone queued");
        assert_eq!(e1["deleted"], json!(true));
        for col in [
            "from_note_id",
            "to_note_id",
            "relation_type",
            "created_at",
            "updated_at",
        ] {
            assert!(
                e1.contains_key(col),
                "edge tombstone missing `{col}`: {e1:?}"
            );
        }
        assert_eq!(e1["from_note_id"], json!("p"));
        assert_eq!(e1["to_note_id"], json!("c1"));
        // Parent p recomputed to has_annotation:false (importance a finite number, re-derived).
        let sig = collapsed_payload_for(db_path, "note_signals", "p")
            .expect("parent signal recompute queued");
        assert_eq!(sig["has_annotation"], json!(false));
        assert_eq!(sig["deleted"], json!(false));
        assert!(sig["importance"].as_f64().is_some_and(f64::is_finite));
        // The child note itself is tombstoned in the same batch.
        assert_eq!(
            collapsed_payload_for(db_path, "notes", "c1").expect("child tombstone queued")
                ["deleted"],
            json!(true)
        );
    }

    #[test]
    fn full_write_delete_route_also_recomputes_parent_has_annotation() {
        // The full-write delete arm (plaintext present, `deleted: true`, SUR-975) must cascade too.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "a margin")])
            .unwrap();
        drain_outbox(db_path);

        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_upsert("c1", "the child text")
            })
            .unwrap();

        assert_eq!(
            collapsed_payload_for(db_path, "note_links", "e1").expect("edge tombstone queued")
                ["deleted"],
            json!(true)
        );
        assert_eq!(
            collapsed_payload_for(db_path, "note_signals", "p").expect("parent recompute queued")
                ["has_annotation"],
            json!(false)
        );
    }

    #[test]
    fn deleting_one_of_two_margins_keeps_has_annotation_and_stages_no_signal() {
        // A surviving live margin means the flag stays true → the change-detection no-op stages NO
        // note_signals write for the parent (only the deleted child's edge + note tombstones move).
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations(
                "p".into(),
                vec![margin("c1", "e1", "first"), margin("c2", "e2", "second")],
            )
            .unwrap();
        drain_outbox(db_path);

        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_patch("c1")
            })
            .unwrap();

        assert_eq!(
            collapsed_payload_for(db_path, "note_links", "e1").expect("e1 tombstone queued")
                ["deleted"],
            json!(true)
        );
        assert!(
            collapsed_payload_for(db_path, "note_signals", "p").is_none(),
            "c2 still live → has_annotation unchanged → no signal write"
        );
    }

    #[test]
    fn margin_delete_without_a_parent_signal_row_creates_none() {
        // Skip-create parity (PWA `refreshAnnotationSignal`: `!existing && !hasLive` → no row). A
        // LIVE parent that never got a signals row (edge added directly, not via the margins op)
        // must not birth one on delete.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(note_upsert("p", "the parent")).unwrap();
        engine.enqueue_note(note_upsert("c", "the child")).unwrap();
        engine
            .enqueue_note_link(
                "e1".into(),
                "p".into(),
                "c".into(),
                Some("handwritten_annotation".into()),
                0,
                false,
            )
            .unwrap();
        drain_outbox(db_path);

        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_patch("c")
            })
            .unwrap();

        assert_eq!(
            collapsed_payload_for(db_path, "note_links", "e1").expect("edge tombstone queued")
                ["deleted"],
            json!(true)
        );
        assert!(
            collapsed_payload_for(db_path, "note_signals", "p").is_none(),
            "live parent, no prior signal row + no surviving edge → skip-create, not a birthed row"
        );
    }

    #[test]
    fn deleting_a_margin_child_after_its_parent_does_not_resurrect_the_parents_signals() {
        // Review regression (PR #68): child-leg-only cascade leaves a deleted parent's outgoing edge
        // LIVE, so deleting the child LATER still reaches the parent recompute. The parent note and
        // its signals are already tombstoned — the recompute must NOT resurrect them (`deleted:false`)
        // and re-create live signal metadata for a dead note. Covers BOTH branches: the surviving
        // edge here (c2/e2 kept live by the child-leg scope) means has_live=true, which the
        // skip-create guard alone would not catch — only the parent-liveness guard does.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations(
                "p".into(),
                vec![margin("c1", "e1", "first"), margin("c2", "e2", "second")],
            )
            .unwrap();
        // Delete the PARENT first: p note + p signals tombstoned; e1/e2 stay live (child-leg scope).
        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_patch("p")
            })
            .unwrap();
        drain_outbox(db_path);

        // Now delete a (still-live) margin child.
        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_patch("c1")
            })
            .unwrap();

        // The edge is tombstoned (child-leg), but the deleted parent's signals stay retired.
        assert_eq!(
            collapsed_payload_for(db_path, "note_links", "e1").expect("edge tombstone queued")
                ["deleted"],
            json!(true)
        );
        assert!(
            collapsed_payload_for(db_path, "note_signals", "p").is_none(),
            "parent p is deleted — its signals must NOT be resurrected to deleted:false"
        );
        // And the local row stays a tombstone (never flipped live).
        let p_sig = Store::open(db_path)
            .unwrap()
            .get_row("note_signals", "p")
            .unwrap();
        assert_eq!(
            p_sig.and_then(|r| r.get("deleted").and_then(Value::as_bool)),
            Some(true),
            "parent p's local signals row stays tombstoned"
        );
    }

    #[test]
    fn edge_tombstone_stamp_is_clamped_above_a_future_foreign_updated_at() {
        // Review regression (PR #68): a margin edge pulled from a device with a faster clock carries
        // an `updated_at` ahead of ours. The tombstone must be stamped STRICTLY after it, or the
        // server's t01 LWW guard silently drops the delete and the edge stays live fleet-wide.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "a margin")])
            .unwrap();
        // Simulate the pulled foreign stamp: overwrite e1's updated_at far into the future.
        let future = 9_999_999_999_999i64;
        {
            let store = Store::open(db_path).unwrap();
            let mut e1row = store.get_row("note_links", "e1").unwrap().unwrap();
            e1row.insert("updated_at".into(), json!(future));
            store.apply_row("note_links", &e1row).unwrap();
        }
        drain_outbox(db_path);

        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_patch("c1")
            })
            .unwrap();

        let e1 = collapsed_payload_for(db_path, "note_links", "e1").expect("edge tombstone queued");
        assert_eq!(e1["deleted"], json!(true));
        assert_eq!(
            e1["updated_at"],
            json!(future + 1),
            "edge tombstone clamped strictly above the stored foreign stamp"
        );
    }

    #[test]
    fn margin_edge_with_null_relation_type_is_retired_as_handwritten() {
        // Review regression (PR #68): an absent relation_type defaults to handwritten_annotation in
        // the rest of the margins code, so a NULL-relation margin edge whose child is deleted must be
        // retired (not left live) and count toward the parent recompute.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(note_upsert("p", "the parent")).unwrap();
        engine.enqueue_note(note_upsert("c", "the child")).unwrap();
        {
            let store = Store::open(db_path).unwrap();
            store
                .apply_row(
                    "note_links",
                    json!({ "id": "e1", "from_note_id": "p", "to_note_id": "c",
                    "relation_type": null, "created_at": 1, "updated_at": 1, "deleted": false })
                    .as_object()
                    .unwrap(),
                )
                .unwrap();
        }
        drain_outbox(db_path);

        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_patch("c")
            })
            .unwrap();

        let e1 = collapsed_payload_for(db_path, "note_links", "e1")
            .expect("NULL-relation edge retired as handwritten");
        assert_eq!(e1["deleted"], json!(true));
    }

    #[test]
    fn parent_recompute_stamp_is_clamped_above_a_future_foreign_signal_updated_at() {
        // Review regression (PR #68): the parent note_signals row was pulled from a faster-clock
        // device, so its updated_at leads ours. The recompute-to-false must be stamped STRICTLY after
        // it, or the server's t01 LWW guard silently drops it and the cloud parent stays true.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "a margin")])
            .unwrap();
        // Simulate a pulled foreign parent-signal stamp far ahead of this device's clock.
        let future = 9_999_999_999_999i64;
        {
            let store = Store::open(db_path).unwrap();
            let mut sig = store
                .get_row("note_signals", "p")
                .unwrap()
                .expect("parent signal row from replace");
            sig.insert("updated_at".into(), json!(future));
            store.apply_row("note_signals", &sig).unwrap();
        }
        drain_outbox(db_path);

        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_patch("c1")
            })
            .unwrap();

        let sig =
            collapsed_payload_for(db_path, "note_signals", "p").expect("parent recompute queued");
        assert_eq!(sig["has_annotation"], json!(false));
        assert_eq!(
            sig["updated_at"],
            json!(future + 1),
            "recompute clamped strictly above the stored foreign stamp"
        );
    }

    #[test]
    fn direct_edge_remove_does_not_recompute_has_annotation_pwa_parity() {
        // Parity boundary (PR #68 review): a standalone edge remove via enqueue_note_link(deleted:true)
        // does NOT recompute the parent — matching the PWA, whose deleteNoteLink is unused and unwired
        // to refreshAnnotationSignal (has_annotation is reconciled only via replace + note-delete).
        // Locks the boundary so firing the recompute here later is a deliberate, PWA-coordinated call.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "a margin")])
            .unwrap();
        drain_outbox(db_path);

        // Remove the edge directly; the child note c1 stays live, so no note-delete cascade runs.
        engine
            .enqueue_note_link(
                "e1".into(),
                "p".into(),
                "c1".into(),
                Some("handwritten_annotation".into()),
                0,
                true,
            )
            .unwrap();

        assert_eq!(
            collapsed_payload_for(db_path, "note_links", "e1").expect("edge tombstone queued")
                ["deleted"],
            json!(true)
        );
        assert!(
            collapsed_payload_for(db_path, "note_signals", "p").is_none(),
            "direct edge remove does not recompute has_annotation (PWA parity — deleteNoteLink unwired)"
        );
    }

    #[test]
    fn deleting_the_parent_leaves_its_outgoing_edge_live_scope_boundary() {
        // Founder scope 2026-07-22: handwritten-only, CHILD-leg only. Deleting the PARENT retires no
        // edge here — its outgoing p→c edge is the broader note-delete edge cascade (SUR-84 parity),
        // tracked separately. This test locks that boundary so a future SUR-84 change is deliberate.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "a margin")])
            .unwrap();
        drain_outbox(db_path);

        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_patch("p")
            })
            .unwrap();

        // The parent→child edge is NOT retired by this path (child-leg only).
        assert!(
            collapsed_payload_for(db_path, "note_links", "e1").is_none(),
            "deleting the parent does not tombstone its outgoing edge (SUR-84 scope)"
        );
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
        assert!(
            sig["engagement_recency_at"].as_i64().unwrap() > 0,
            "SUR-966: the margins op is a deliberate act — it bumps engagement_recency_at to now"
        );
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
        assert!(
            sig["engagement_recency_at"].as_i64().unwrap() > 222,
            "SUR-966: the margins op bumps engagement past the seeded stamp (earned, not clobbered elsewhere)"
        );
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
    fn replace_handwritten_annotations_already_flagged_still_bumps_engagement() {
        // SUR-966 regression (the widened guard): the SUR-956 no-op skipped an already-`has_annotation`
        // live row ENTIRELY. Adding engagement inside that skip would mean an already-annotated note
        // NEVER records margin engagement — every re-save is a genuine deliberate act. So the margins
        // op must STILL stage an `engagement_recency_at` bump on an already-flagged row, preserving the
        // earned counters, and enqueue the signals outbox row (engagement is never throttled).
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(parent_with_book("p", "b1")).unwrap();
        engine
            .enqueue_note_signals("p".into(), 0.9, 5, true, 2, 111, 222, 1.23, 100, false)
            .unwrap();
        drain_outbox(db_path);

        engine
            .replace_handwritten_annotations("p".into(), vec![margin("c1", "e1", "a margin")])
            .unwrap();

        let after = stored_signals(db_path, "p").expect("signals row live");
        assert_eq!(after["has_annotation"], json!(true), "flag stays true");
        assert!(
            after["engagement_recency_at"].as_i64().unwrap() > 222,
            "already-annotated note STILL gets its engagement bump (widened guard)"
        );
        assert_eq!(after["return_visits"], json!(5), "earned counter preserved");
        assert_eq!(after["stitch_spawns"], json!(2), "earned counter preserved");
        assert_eq!(
            after["exposure_recency_at"],
            json!(111),
            "exposure untouched"
        );
        assert_eq!(after["created_at"], json!(100), "created_at preserved");
        assert_eq!(
            Store::open(db_path)
                .unwrap()
                .outbox_items()
                .unwrap()
                .iter()
                .filter(|(_, t, _, _, _)| t == "note_signals")
                .count(),
            1,
            "the engagement bump IS enqueued (one signals outbox row)"
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

    // ─────────────────────────── SUR-966: record_note_signal ───────────────────────────

    fn note_with_source(id: &str, source: &str) -> NoteUpsert {
        NoteUpsert {
            source: Some(source.into()),
            ..note_upsert(id, "a note")
        }
    }

    #[test]
    fn record_note_signal_return_visit_increments_and_bumps_exposure_only() {
        // ReturnVisit → return_visits += 1, exposure_recency_at = now, engagement UNTOUCHED
        // (the PWA: "re-reading isn't reflection"). Earned counters on an unrelated field survive.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine
            .enqueue_note(note_with_source("n", "manual"))
            .unwrap();
        engine
            .enqueue_note_signals("n".into(), 0.7, 3, false, 2, 0, 0, 0.0, 50, false)
            .unwrap();

        assert!(engine
            .record_note_signal("n".into(), NoteSignalKind::ReturnVisit)
            .unwrap());

        let sig = stored_signals(db_path, "n").unwrap();
        assert_eq!(sig["return_visits"], json!(4), "return_visits incremented");
        assert!(
            sig["exposure_recency_at"].as_i64().unwrap() > 0,
            "exposure bumped to now"
        );
        assert_eq!(
            sig["engagement_recency_at"],
            json!(0),
            "engagement left untouched"
        );
        assert_eq!(sig["stitch_spawns"], json!(2), "earned counter preserved");
        assert_eq!(sig["created_at"], json!(50), "created_at preserved");
        assert!(
            (sig["importance"].as_f64().unwrap() - compute_importance(0.7, 4, false, 2)).abs()
                < 1e-12,
            "importance recomputed over the new counters"
        );
    }

    #[test]
    fn record_note_signal_births_a_full_row_from_the_note_source() {
        // A first signal on a note with no signals row stages a WHOLE row: source_prior derived from
        // the note's `source`, every column populated (no server-default holes), importance matching
        // compute_importance. Outbox row keyed by note_id, no `id` key.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(note_with_source("n", "share")).unwrap();

        assert!(engine
            .record_note_signal("n".into(), NoteSignalKind::Engagement)
            .unwrap());

        let sig = stored_signals(db_path, "n").expect("birth row staged");
        assert_eq!(
            sig["source_prior"],
            json!(0.75),
            "prior from source `share`"
        );
        assert_eq!(sig["return_visits"], json!(0));
        assert_eq!(sig["has_annotation"], json!(false));
        assert_eq!(sig["stitch_spawns"], json!(0));
        assert_eq!(sig["exposure_recency_at"], json!(0));
        assert!(sig["engagement_recency_at"].as_i64().unwrap() > 0);
        assert_eq!(sig["deleted"], json!(false));
        for col in [
            "note_id",
            "source_prior",
            "return_visits",
            "has_annotation",
            "stitch_spawns",
            "exposure_recency_at",
            "engagement_recency_at",
            "importance",
            "created_at",
            "updated_at",
            "deleted",
        ] {
            assert!(sig.contains_key(col), "column `{col}` populated (no hole)");
        }
        assert!(
            (sig["importance"].as_f64().unwrap() - compute_importance(0.75, 0, false, 0)).abs()
                < 1e-12,
            "importance matches compute_importance"
        );

        // Outbox row keyed by note_id, omitting `id`.
        let items = Store::open(db_path).unwrap().outbox_items().unwrap();
        let (_, table, record_id, payload_json, _) = items
            .iter()
            .find(|(_, t, _, _, _)| t == "note_signals")
            .expect("signals outbox row queued");
        assert_eq!(table, "note_signals");
        assert_eq!(record_id.as_deref(), Some("n"), "keyed by note_id");
        let payload: Value = serde_json::from_str(payload_json).unwrap();
        assert!(payload.get("id").is_none(), "no `id` key");
        assert_eq!(payload["note_id"], json!("n"));
    }

    #[test]
    fn record_note_signal_exposure_throttled_within_window_stages_nothing() {
        // Change-detection / throttle no-op: a repeat Exposure inside SIGNAL_THROTTLE_MS stages
        // nothing, bumps no updated_at, and returns false (two calls in a test are far under 1h).
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine
            .enqueue_note(note_with_source("n", "manual"))
            .unwrap();

        assert!(engine
            .record_note_signal("n".into(), NoteSignalKind::Exposure)
            .unwrap());
        let after_first = stored_signals(db_path, "n").unwrap();
        drain_outbox(db_path);

        assert!(
            !engine
                .record_note_signal("n".into(), NoteSignalKind::Exposure)
                .unwrap(),
            "repeat Exposure within the window returns false"
        );
        assert_eq!(
            stored_signals(db_path, "n").unwrap(),
            after_first,
            "row byte-identical — no updated_at bump"
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
    fn record_note_signal_exposure_writes_on_cold_start_over_epoch_zero() {
        // Cold start: a note whose first-ever signal is Engagement births a row with
        // exposure_recency_at = 0. The next Exposure must NOT be suppressed by the epoch-0 throttle
        // comparison (no unsigned underflow, no NULL short-circuit) — absent/epoch = "never exposed".
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine
            .enqueue_note(note_with_source("n", "manual"))
            .unwrap();

        assert!(engine
            .record_note_signal("n".into(), NoteSignalKind::Engagement)
            .unwrap());
        assert_eq!(
            stored_signals(db_path, "n").unwrap()["exposure_recency_at"],
            json!(0),
            "birthed with an epoch-0 exposure stamp"
        );

        assert!(
            engine
                .record_note_signal("n".into(), NoteSignalKind::Exposure)
                .unwrap(),
            "cold-start Exposure writes over the epoch-0 stamp"
        );
        assert!(
            stored_signals(db_path, "n").unwrap()["exposure_recency_at"]
                .as_i64()
                .unwrap()
                > 0
        );
    }

    #[test]
    fn record_note_signal_resurrects_after_a_soft_delete_dropping_the_tombstone() {
        // Tombstone-BEFORE-live ordering (distinct from live-write-after-tombstone): soft_delete
        // stages a tombstone with no local row, THEN record_note_signal for the same id must drop
        // the queued tombstone (resurrect rule) and stage a live row that flushes LIVE.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine
            .enqueue_note(note_with_source("n", "manual"))
            .unwrap();

        engine.soft_delete_signals_for_note("n".into()).unwrap();
        assert_eq!(
            stored_signals(db_path, "n").unwrap()["deleted"],
            json!(true),
            "tombstoned first"
        );

        assert!(engine
            .record_note_signal("n".into(), NoteSignalKind::ReturnVisit)
            .unwrap());
        let sig = stored_signals(db_path, "n").unwrap();
        assert_eq!(sig["deleted"], json!(false), "resurrected live");
        assert_eq!(sig["return_visits"], json!(1));
        let payload =
            collapsed_payload_for(db_path, "note_signals", "n").expect("queued signals row");
        assert_eq!(
            payload["deleted"],
            json!(false),
            "what flushes is LIVE — the queued tombstone was dropped"
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
            "the queued tombstone was DROPPED (one live item)"
        );
    }

    #[test]
    fn record_note_signal_on_a_deleted_note_is_a_no_op_and_keeps_the_tombstone_queued() {
        // The delete/callback race: the host deletes the note and tombstones its signals row, THEN
        // a late exposure/engagement callback fires. Without the deleted-note guard this takes the
        // resurrect path (a tombstoned row always stages) and `stage_local_writes` DROPS the queued
        // tombstone — leaving live signal metadata for a dead note, and losing the retirement.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine
            .enqueue_note(note_with_source("n", "manual"))
            .unwrap();
        engine
            .record_note_signal("n".into(), NoteSignalKind::ReturnVisit)
            .unwrap();

        // Host deletes the note, then retires its signals row.
        let mut tomb = Map::new();
        tomb.insert("id".into(), json!("n"));
        tomb.insert("deleted".into(), json!(true));
        tomb.insert("updated_at".into(), json!(epoch_ms()));
        Store::open(db_path)
            .unwrap()
            .stage_local_writes(vec![("notes", "n".into(), tomb)], epoch_ms())
            .unwrap();
        engine.soft_delete_signals_for_note("n".into()).unwrap();

        // Every kind is a no-op — none of them may resurrect the row.
        for kind in [
            NoteSignalKind::Exposure,
            NoteSignalKind::Engagement,
            NoteSignalKind::ReturnVisit,
        ] {
            assert!(
                !engine.record_note_signal("n".into(), kind).unwrap(),
                "a signal on a locally-deleted note stages nothing"
            );
        }

        assert_eq!(
            stored_signals(db_path, "n").unwrap()["deleted"],
            json!(true),
            "signals row stays tombstoned"
        );
        let payload =
            collapsed_payload_for(db_path, "note_signals", "n").expect("queued signals row");
        assert_eq!(
            payload["deleted"],
            json!(true),
            "what flushes is still the TOMBSTONE — it was not dropped by a resurrect"
        );
    }

    #[test]
    fn record_note_signal_resumes_after_the_note_is_restored_preserving_prior_and_counters() {
        // The restore interleave — the path the SUR-966 fix chain kept breaking, and the one the
        // single-device tests do not reach. Signals are tombstoned alongside a note delete, the note
        // then comes back LIVE (a pull applying a newer live cloud row from another device writes
        // over the local tombstone), and the next signal must resume: resurrect the signals row,
        // keep the stored prior and the earned counters, and flush LIVE rather than the tombstone.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine
            .enqueue_note(note_with_source("n", "handwritten"))
            .unwrap();
        engine
            .record_note_signal("n".into(), NoteSignalKind::ReturnVisit)
            .unwrap();
        engine
            .record_note_signal("n".into(), NoteSignalKind::ReturnVisit)
            .unwrap();

        // Host deletes the note and retires its signals row.
        let mut tomb = Map::new();
        tomb.insert("id".into(), json!("n"));
        tomb.insert("deleted".into(), json!(true));
        tomb.insert("updated_at".into(), json!(epoch_ms()));
        let store = Store::open(db_path).unwrap();
        store
            .stage_local_writes(vec![("notes", "n".into(), tomb)], epoch_ms())
            .unwrap();
        engine.soft_delete_signals_for_note("n".into()).unwrap();
        assert_eq!(
            stored_signals(db_path, "n").unwrap()["deleted"],
            json!(true),
            "signals tombstoned"
        );
        // While the note is dead, signals stay refused.
        assert!(!engine
            .record_note_signal("n".into(), NoteSignalKind::Engagement)
            .unwrap());

        // The note is RESTORED live (pull LWW writing a newer live row over the local tombstone).
        let mut live = Map::new();
        live.insert("id".into(), json!("n"));
        live.insert("source".into(), json!("handwritten"));
        live.insert("deleted".into(), json!(false));
        live.insert("updated_at".into(), json!(epoch_ms() + 1));
        store
            .stage_local_writes(vec![("notes", "n".into(), live)], epoch_ms())
            .unwrap();

        assert!(
            engine
                .record_note_signal("n".into(), NoteSignalKind::Engagement)
                .unwrap(),
            "a restored note earns signals again"
        );
        let sig = stored_signals(db_path, "n").unwrap();
        assert_eq!(sig["deleted"], json!(false), "signals row resurrected");
        assert_eq!(
            sig["source_prior"],
            json!(0.9),
            "stored prior survived the tombstone round-trip"
        );
        assert_eq!(
            sig["return_visits"],
            json!(2),
            "earned counters survived the tombstone round-trip"
        );
        let payload =
            collapsed_payload_for(db_path, "note_signals", "n").expect("queued signals row");
        assert_eq!(
            payload["deleted"],
            json!(false),
            "what flushes is LIVE — the queued tombstone was dropped"
        );
    }

    #[test]
    fn soft_delete_with_no_note_row_seeds_the_fallback_prior_for_a_later_resurrect() {
        // ACCEPTED BEHAVIOUR, pinned so it is a decision and not a surprise. Retiring signals for a
        // note this device holds NO row for must still stage a tombstone (the cross-device rule), so
        // it fabricates the unknown-source fallback. That tombstone is a local row, so a later
        // resurrect reads the fabricated prior VERBATIM — nothing re-derives it. A `handwritten`
        // note therefore comes back pinned at 0.5 rather than 0.9.
        //
        // Not fixable by refusing (the tombstone is required) and not by re-deriving on resurrect (a
        // stored 0.5 is ambiguous — that is exactly why the heal was abandoned). Narrow to reach: an
        // ordinary delete reads the tombstoned note's `source` fine.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);

        // No local `notes` row at all — retire signals for it anyway.
        engine.soft_delete_signals_for_note("n".into()).unwrap();
        assert_eq!(
            stored_signals(db_path, "n").unwrap()["source_prior"],
            json!(0.5),
            "the tombstone fabricates the fallback prior"
        );

        // The note now arrives as `handwritten`; the resurrect reads the fabricated prior verbatim.
        engine
            .enqueue_note(note_with_source("n", "handwritten"))
            .unwrap();
        assert!(engine
            .record_note_signal("n".into(), NoteSignalKind::ReturnVisit)
            .unwrap());
        assert_eq!(
            stored_signals(db_path, "n").unwrap()["source_prior"],
            json!(0.5),
            "resurrects at the SEEDED fallback, not the note's real 0.9 — the accepted gap"
        );
    }

    #[test]
    fn record_note_signal_keeps_a_real_stored_prior_when_the_note_is_visible() {
        // SUR-956's "stored prior kept, not re-derived" invariant (v0.9.1): a stored prior is kept
        // verbatim even when the visible note's `source` would derive a different number.
        //
        // 0.5 is asserted DELIBERATELY, and is the sharper half. A stored 0.5 is a legitimate value
        // — `readwise` derives it, and an import or the blind `enqueue_note_signals` FFI can write
        // it onto a note whose source derives higher. Any scheme that treated 0.5 as an
        // "unknown source" sentinel and re-derived it would silently overwrite a real prior here.
        for (seeded, source) in [(0.9_f64, "manual"), (0.5_f64, "handwritten")] {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("t.sqlite");
            let db_path = db.to_str().unwrap();
            let engine = engine_at(db_path);
            engine.enqueue_note(note_with_source("n", source)).unwrap();
            engine
                .enqueue_note_signals("n".into(), seeded, 0, false, 0, 0, 0, 0.0, 100, false)
                .unwrap();

            assert!(engine
                .record_note_signal("n".into(), NoteSignalKind::Engagement)
                .unwrap());
            assert_eq!(
                stored_signals(db_path, "n").unwrap()["source_prior"],
                json!(seeded),
                "a real stored prior is never re-derived (seeded {seeded} on a `{source}` note)"
            );
        }
    }

    #[test]
    fn a_throttled_signal_corrects_a_stored_importance_that_disagrees_with_the_formula() {
        // SUR-977: `importance` was staged but invisible to change-detection, so a stored value
        // disagreeing with the formula (writable via the blind `enqueue_note_signals` FFI) sat on
        // a live row forever — the SUR-956/966 `source_prior` bug class, one field over. With
        // importance inside `SignalState` (before = stored verbatim, after = recomputed), the
        // disagreement IS a diff: even a throttled Exposure — the no-op path that used to
        // preserve the lie — now stages the correction.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine
            .enqueue_note(note_with_source("n", "manual"))
            .unwrap();
        // The blind FFI writes importance: 999 against counters whose formula value differs;
        // exposure stamped fresh so the next Exposure is inside the throttle window.
        let now = epoch_ms();
        engine
            .enqueue_note_signals("n".into(), 0.7, 2, false, 0, now, 0, 999.0, 1, false)
            .unwrap();

        assert!(
            engine
                .record_note_signal("n".into(), NoteSignalKind::Exposure)
                .unwrap(),
            "a throttled Exposure on a lying row stages the correction instead of no-opping"
        );
        let row = stored_signals(db_path, "n").unwrap();
        assert_eq!(
            row["importance"],
            json!(compute_importance(0.7, 2, false, 0)),
            "corrected to the formula for the row's own stored fields"
        );
        assert_eq!(row["return_visits"], json!(2), "earned counters untouched");
        assert_eq!(
            row["source_prior"],
            json!(0.7),
            "stored prior untouched (SUR-956)"
        );

        // Once consistent, the throttle no-op is back — no churn on healthy rows.
        let before = stored_signals(db_path, "n").unwrap();
        assert!(
            !engine
                .record_note_signal("n".into(), NoteSignalKind::Exposure)
                .unwrap(),
            "consistent row: the throttled Exposure no-ops again"
        );
        assert_eq!(
            stored_signals(db_path, "n").unwrap(),
            before,
            "no updated_at churn once corrected"
        );
    }

    #[test]
    fn enqueue_note_signals_rejects_non_finite_prior_and_importance() {
        // The trust-boundary guard (SUR-977 sync-reviewer): `json!` launders a non-finite f64 to
        // JSON null, which would land a null importance/prior on the stored row. Reject instead —
        // the same posture the import path takes with `finite_number`.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);

        for (prior, importance) in [(f64::NAN, 0.5), (0.5, f64::NAN), (f64::INFINITY, 0.5)] {
            let err = engine
                .enqueue_note_signals("n".into(), prior, 0, false, 0, 0, 0, importance, 1, false)
                .unwrap_err();
            assert!(matches!(err, SyncError::Store(_)));
        }
        assert!(stored_signals(db_path, "n").is_none(), "nothing staged");
        assert!(Store::open(db_path)
            .unwrap()
            .outbox_items()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_live_rows_null_importance_is_healed_not_shielded() {
        // A pre-guard/legacy row can hold a JSON-null importance. The pre-image must NOT paper
        // over it with a derived stand-in (which would equal `after` by construction and shield
        // the null forever) — it reads NaN, which agrees with nothing, so even the throttled
        // no-op path stages the heal.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine
            .enqueue_note(note_with_source("n", "manual"))
            .unwrap();
        let now = epoch_ms();
        Store::open(db_path)
            .unwrap()
            .apply_row(
                "note_signals",
                json!({
                    "note_id": "n", "source_prior": 0.7, "return_visits": 2,
                    "has_annotation": false, "stitch_spawns": 0,
                    "exposure_recency_at": now, "engagement_recency_at": 0,
                    "importance": null, "created_at": 1, "updated_at": 1, "deleted": false
                })
                .as_object()
                .unwrap(),
            )
            .unwrap();

        assert!(
            engine
                .record_note_signal("n".into(), NoteSignalKind::Exposure)
                .unwrap(),
            "a throttled Exposure on a null-importance row stages the heal"
        );
        assert_eq!(
            stored_signals(db_path, "n").unwrap()["importance"],
            json!(compute_importance(0.7, 2, false, 0)),
            "healed to the formula"
        );
    }

    #[test]
    fn a_sub_epsilon_importance_difference_stages_nothing() {
        // The epsilon guard (SUR-977 sync-reviewer): `compute_importance` runs `.exp()`, whose
        // output is libm-specific — a one-ULP cross-platform disagreement must NOT ping-pong
        // whole-row "corrections" between devices. Sub-epsilon differences are treated as
        // agreement; only a real lie (orders of magnitude larger) stages.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine
            .enqueue_note(note_with_source("n", "manual"))
            .unwrap();
        let now = epoch_ms();
        let ulp_off = compute_importance(0.7, 2, false, 0) + 1e-12;
        engine
            .enqueue_note_signals("n".into(), 0.7, 2, false, 0, now, 0, ulp_off, 1, false)
            .unwrap();
        let before = stored_signals(db_path, "n").unwrap();

        assert!(
            !engine
                .record_note_signal("n".into(), NoteSignalKind::Exposure)
                .unwrap(),
            "a sub-epsilon disagreement is agreement — the throttled no-op holds"
        );
        assert_eq!(
            stored_signals(db_path, "n").unwrap(),
            before,
            "no churn from libm-scale drift"
        );
    }

    #[test]
    fn record_note_signal_on_an_absent_note_is_a_no_op_and_stages_nothing() {
        // ABSENT is refused for a different reason than DELETED: with no note there is no `source`,
        // so the row would be born at the unknown-source fallback (0.5) — and nothing re-derives a
        // stored prior (SUR-956), so `handwritten`/`share`/`manual` notes would sit under-scored
        // forever. Healing later cannot fix it either: a stored 0.5 is ambiguous (a `readwise` note
        // derives 0.5, and imports / the blind `enqueue_note_signals` FFI can write a real 0.5), so
        // healing on the value would overwrite legitimate priors. Refusing the birth is the fix.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);

        for kind in [
            NoteSignalKind::Exposure,
            NoteSignalKind::Engagement,
            NoteSignalKind::ReturnVisit,
        ] {
            assert!(
                !engine
                    .record_note_signal("never-synced".into(), kind)
                    .unwrap(),
                "a signal on a note this device cannot see stages nothing"
            );
        }
        assert!(
            stored_signals(db_path, "never-synced").is_none(),
            "no signals row was born at the unknown-source fallback"
        );
        assert_eq!(
            Store::open(db_path).unwrap().outbox_items().unwrap().len(),
            0,
            "nothing queued for the fleet either"
        );
    }

    #[test]
    fn record_note_signal_births_the_real_prior_once_the_note_is_visible() {
        // The other half of refusing the absent case: the signal that lands AFTER the note syncs
        // down births the row with the note's real prior, so nothing is stuck at the fallback.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);

        assert!(!engine
            .record_note_signal("n".into(), NoteSignalKind::ReturnVisit)
            .unwrap());
        engine
            .enqueue_note(note_with_source("n", "handwritten"))
            .unwrap();
        assert!(engine
            .record_note_signal("n".into(), NoteSignalKind::ReturnVisit)
            .unwrap());

        let sig = stored_signals(db_path, "n").expect("row born once the note is visible");
        assert_eq!(
            sig["source_prior"],
            json!(0.9),
            "born at the note's real prior, never the fallback"
        );

        // A VISIBLE note with a NULL `source` still derives the fallback — accepted, not a bug:
        // `notes.source` is nullable and there is nothing better to derive from. The visibility
        // guard buys "we never guess a prior we could have known", NOT "0.5 is unreachable".
        //
        // Staged directly rather than through `enqueue_note`, because the create path DEFAULTS
        // source to "manual" (the PWA's create-time default, mod.rs `enqueue_note`). A sourceless
        // note therefore only reaches the store by PULL or IMPORT, where the row lands verbatim —
        // which is what this stages.
        let mut pulled = Map::new();
        pulled.insert("id".into(), json!("sourceless"));
        pulled.insert("deleted".into(), json!(false));
        pulled.insert("updated_at".into(), json!(epoch_ms()));
        Store::open(db_path)
            .unwrap()
            .stage_local_writes(vec![("notes", "sourceless".into(), pulled)], epoch_ms())
            .unwrap();
        assert!(engine
            .record_note_signal("sourceless".into(), NoteSignalKind::ReturnVisit)
            .unwrap());
        assert_eq!(
            stored_signals(db_path, "sourceless").unwrap()["source_prior"],
            json!(0.5),
            "a sourceless note legitimately derives the fallback"
        );
        assert_eq!(
            sig["return_visits"],
            json!(1),
            "only the landed signal counts"
        );
    }

    #[test]
    fn soft_delete_signals_for_note_stages_tombstone_with_no_local_row_then_no_ops() {
        // The manifest cross-device tail: ALWAYS stage a tombstone even with no local row (another
        // device may hold a live cloud row). Full-shape (no NOT-NULL holes, no PATCH fallback for
        // note_signals). A second call is a no-op.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);

        engine.soft_delete_signals_for_note("ghost".into()).unwrap();
        let tomb = stored_signals(db_path, "ghost").expect("tombstone staged with no local row");
        assert_eq!(tomb["deleted"], json!(true));
        for col in [
            "note_id",
            "source_prior",
            "return_visits",
            "has_annotation",
            "stitch_spawns",
            "exposure_recency_at",
            "engagement_recency_at",
            "importance",
            "created_at",
            "updated_at",
            "deleted",
        ] {
            assert!(
                tomb.contains_key(col),
                "full-shape tombstone: `{col}` present"
            );
        }
        let (_, table, record_id, payload_json, _) = only_row(db_path);
        assert_eq!(table, "note_signals");
        assert_eq!(record_id.as_deref(), Some("ghost"), "keyed by note_id");
        assert!(
            serde_json::from_str::<Value>(&payload_json)
                .unwrap()
                .get("id")
                .is_none(),
            "no `id` key"
        );

        engine.soft_delete_signals_for_note("ghost".into()).unwrap();
        assert_eq!(
            stored_signals(db_path, "ghost").unwrap(),
            tomb,
            "second call is a no-op — row byte-identical, no updated_at churn"
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
            "no new outbox row on the repeat call"
        );
    }

    #[test]
    fn enqueue_note_full_write_delete_stages_note_and_signals_tombstones_in_one_batch() {
        // SUR-975: the note tombstone and its signals tombstone commit together — one transaction,
        // one outbox enqueue stamp — and the tombstone preserves the earned counters verbatim.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine
            .enqueue_note(note_with_source("n", "manual"))
            .unwrap();
        assert!(engine
            .record_note_signal("n".into(), NoteSignalKind::Engagement)
            .unwrap());
        assert!(engine
            .record_note_signal("n".into(), NoteSignalKind::ReturnVisit)
            .unwrap());

        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_with_source("n", "manual")
            })
            .unwrap();

        let store = Store::open(db_path).unwrap();
        assert_eq!(
            store.get_row("notes", "n").unwrap().unwrap()["deleted"],
            json!(true)
        );
        let tomb = stored_signals(db_path, "n").unwrap();
        assert_eq!(tomb["deleted"], json!(true));
        assert_eq!(
            tomb["return_visits"],
            json!(1),
            "earned counters survive onto the tombstone"
        );
        assert_eq!(
            tomb["source_prior"],
            json!(0.7),
            "the stored prior is carried, not re-derived"
        );
        // The two delete rows are the LAST two queued and share one enqueue stamp (one tx).
        let items = store.outbox_items().unwrap();
        let deletes: Vec<_> = items
            .iter()
            .filter(|(_, _, _, payload, _)| {
                serde_json::from_str::<Value>(payload).unwrap()["deleted"] == json!(true)
            })
            .collect();
        let tables: std::collections::BTreeSet<&str> =
            deletes.iter().map(|(_, t, _, _, _)| t.as_str()).collect();
        assert_eq!(
            tables,
            ["note_signals", "notes"].into(),
            "exactly one tombstone row per table"
        );
        assert_eq!(deletes.len(), 2);
        assert_eq!(
            deletes[0].4, deletes[1].4,
            "one enqueue stamp — both tombstones rode one transaction"
        );
    }

    #[test]
    fn enqueue_note_full_write_delete_seeds_the_signals_prior_from_the_row_in_hand() {
        // SUR-975 narrows the ACCEPTED fallback wart: the full-write delete always has a real
        // `source` in the row it is staging, so a signals tombstone born HERE seeds the true
        // prior — unlike the standalone no-note-row path, which must fabricate 0.5.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);

        // No prior note row, no prior signals row — a tombstone arriving out of nowhere.
        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_with_source("n", "handwritten")
            })
            .unwrap();

        let tomb = stored_signals(db_path, "n").expect("signals tombstone born with the delete");
        assert_eq!(tomb["deleted"], json!(true));
        assert_eq!(
            tomb["source_prior"],
            json!(0.9),
            "seeded from the write's own source, not the unknown-source fallback"
        );
    }

    #[test]
    fn enqueue_note_patch_delete_stages_note_and_signals_tombstones_in_one_batch() {
        // The plaintext-free delete (the PWA-shaped patch) folds the signals tombstone into the
        // same transaction as the existing-live-precondition write.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine.enqueue_note(note_with_source("n", "share")).unwrap();
        assert!(engine
            .record_note_signal("n".into(), NoteSignalKind::Engagement)
            .unwrap());

        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_patch("n")
            })
            .unwrap();

        let store = Store::open(db_path).unwrap();
        assert_eq!(
            store.get_row("notes", "n").unwrap().unwrap()["deleted"],
            json!(true)
        );
        let tomb = stored_signals(db_path, "n").unwrap();
        assert_eq!(tomb["deleted"], json!(true));
        assert_eq!(
            tomb["source_prior"],
            json!(0.75),
            "stored prior carried verbatim"
        );
        assert!(
            tomb["engagement_recency_at"].as_i64().unwrap() > 0,
            "earned recency survives onto the tombstone"
        );
        assert_eq!(
            store
                .outbox_items()
                .unwrap()
                .iter()
                .filter(|(_, t, _, _, _)| t == "note_signals")
                .count(),
            2,
            "the live engagement row + the delete's tombstone row"
        );
    }

    #[test]
    fn enqueue_note_patch_delete_of_a_dead_target_stages_no_signals_tombstone_either() {
        // The existing-live precondition guards the WHOLE batch: when the note patch is rejected,
        // the signals tombstone that would have ridden along must not stage either.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);

        // A live signals row with NO note row (arrived by pull/import) — the sharp case: a
        // rejected delete-patch must not retire it as a side effect.
        engine
            .enqueue_note_signals("orphan".into(), 0.9, 2, false, 0, 5, 5, 0.9, 1, false)
            .unwrap();
        let live_before = stored_signals(db_path, "orphan").unwrap();
        let outbox_before = Store::open(db_path).unwrap().outbox_items().unwrap().len();

        let err = engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_patch("orphan")
            })
            .unwrap_err();
        assert!(matches!(err, SyncError::PatchTargetMissing));
        assert_eq!(
            stored_signals(db_path, "orphan").unwrap(),
            live_before,
            "the signals row is untouched when the note precondition fails"
        );
        assert_eq!(
            Store::open(db_path).unwrap().outbox_items().unwrap().len(),
            outbox_before,
            "nothing new queued"
        );
    }

    #[test]
    fn enqueue_note_re_delete_stages_no_second_signals_tombstone() {
        // Idempotence via the full-write branch (a patch re-delete is rejected by the
        // precondition): the second delete re-stages the note row but the already-tombstoned
        // signals row stages nothing — byte-identical, no new outbox row.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        let delete = || NoteUpsert {
            deleted: true,
            ..note_with_source("n", "manual")
        };
        engine.enqueue_note(delete()).unwrap();
        let tomb = stored_signals(db_path, "n").unwrap();

        engine.enqueue_note(delete()).unwrap();
        assert_eq!(
            stored_signals(db_path, "n").unwrap(),
            tomb,
            "re-delete leaves the signals tombstone byte-identical"
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
            "no second note_signals outbox row"
        );
    }

    #[test]
    fn enqueue_note_live_writes_never_touch_note_signals() {
        // The SUR-975 fold is delete-only: live full writes and live patches stage exactly the
        // notes row, as before.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);

        engine.enqueue_note(note_upsert("n", "a note")).unwrap();
        engine
            .enqueue_note(NoteUpsert {
                tags: vec!["t".into()],
                ..note_patch("n")
            })
            .unwrap();

        assert!(
            stored_signals(db_path, "n").is_none(),
            "no signals row born"
        );
        let store = Store::open(db_path).unwrap();
        assert!(
            store
                .outbox_items()
                .unwrap()
                .iter()
                .all(|(_, t, _, _, _)| t == "notes"),
            "only notes rows queued"
        );
    }

    #[test]
    fn enqueue_note_delete_then_record_note_signal_still_refuses() {
        // The SUR-966 visibility guard composes with the SUR-975 fold: after the atomic delete a
        // late signal callback is refused and the signals tombstone stays a tombstone.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let db_path = db.to_str().unwrap();
        let engine = engine_at(db_path);
        engine
            .enqueue_note(note_with_source("n", "manual"))
            .unwrap();
        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_with_source("n", "manual")
            })
            .unwrap();

        assert!(
            !engine
                .record_note_signal("n".into(), NoteSignalKind::Engagement)
                .unwrap(),
            "a signal on a deleted note stages nothing"
        );
        assert_eq!(
            stored_signals(db_path, "n").unwrap()["deleted"],
            json!(true),
            "the tombstone was not resurrected"
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

    // ── SUR-997: the embedding surface ───────────────────────────────────────

    use crate::embeddings::EmbedderDescriptor;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A deterministic Rust-side [`Embedder`]: a byte histogram over `DIMS` buckets. Same
    /// text → same vector (cosine 1.0); disjoint letter sets → orthogonal vectors. Counts
    /// its document-embed calls so tests can assert what the pipeline actually invoked.
    struct HistogramEmbedder {
        model_id: String,
        embed_calls: AtomicU32,
    }

    const DIMS: u32 = 8;

    fn histogram(text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; DIMS as usize];
        for b in text.bytes() {
            v[b as usize % DIMS as usize] += 1.0;
        }
        v
    }

    impl HistogramEmbedder {
        fn new(model_id: &str) -> Arc<Self> {
            Arc::new(Self {
                model_id: model_id.into(),
                embed_calls: AtomicU32::new(0),
            })
        }
    }

    impl Embedder for HistogramEmbedder {
        fn descriptor(&self) -> EmbedderDescriptor {
            EmbedderDescriptor {
                model_id: self.model_id.clone(),
                dims: DIMS,
                quantization: "test".into(),
            }
        }
        fn embed_document(&self, text: String) -> Result<Vec<f32>, EmbedderError> {
            self.embed_calls.fetch_add(1, Ordering::SeqCst);
            Ok(histogram(&text))
        }
        fn embed_query(&self, text: String) -> Result<Vec<f32>, EmbedderError> {
            Ok(histogram(&text))
        }
    }

    fn embed_engine(dir: &tempfile::TempDir, name: &str) -> (Arc<SyncEngine>, String) {
        let db = dir.path().join(format!("{name}.sqlite"));
        let db_path = db.to_str().unwrap().to_string();
        let engine = SyncEngine::open(
            db_path.clone(),
            "https://x.supabase.co".into(),
            "anon".into(),
            Vault::generate(),
        )
        .unwrap();
        (engine, db_path)
    }

    #[test]
    fn every_embedding_method_requires_a_registered_embedder() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = embed_engine(&dir, "unregistered");
        assert!(matches!(
            engine.pending_embed_count().unwrap_err(),
            SyncError::EmbedderNotRegistered
        ));
        assert!(matches!(
            engine.embed_pending(10).unwrap_err(),
            SyncError::EmbedderNotRegistered
        ));
        assert!(matches!(
            engine.semantic_search("q".into(), 5).unwrap_err(),
            SyncError::EmbedderNotRegistered
        ));
        assert!(matches!(
            engine.similar_notes("n1".into(), 5).unwrap_err(),
            SyncError::EmbedderNotRegistered
        ));
    }

    #[test]
    fn register_embedder_validates_the_descriptor() {
        struct BadEmbedder(EmbedderDescriptor);
        impl Embedder for BadEmbedder {
            fn descriptor(&self) -> EmbedderDescriptor {
                self.0.clone()
            }
            fn embed_document(&self, _t: String) -> Result<Vec<f32>, EmbedderError> {
                Err(EmbedderError::Runtime)
            }
            fn embed_query(&self, _t: String) -> Result<Vec<f32>, EmbedderError> {
                Err(EmbedderError::Runtime)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = embed_engine(&dir, "validate");
        let desc = |model_id: &str, dims: u32, quant: &str| EmbedderDescriptor {
            model_id: model_id.into(),
            dims,
            quantization: quant.into(),
        };
        for bad in [
            desc("", 8, "q8"),         // empty model id
            desc("m|odel", 8, "q8"),   // '|' would corrupt the corpus key
            desc("model", 8, "q|8"),   // …in either segment
            desc("model", 0, "q8"),    // zero dims
            desc("model", 5000, "q8"), // absurd dims
        ] {
            let err = engine
                .register_embedder(Arc::new(BadEmbedder(bad)))
                .unwrap_err();
            assert!(matches!(err, SyncError::Embed(_)));
        }
        // A rejected registration leaves the engine unregistered.
        assert!(matches!(
            engine.pending_embed_count().unwrap_err(),
            SyncError::EmbedderNotRegistered
        ));
    }

    #[test]
    fn embed_pending_drains_seals_and_search_finds_the_right_note() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, db_path) = embed_engine(&dir, "drain");
        // Disjoint byte sets so the histogram vectors are orthogonal.
        engine.enqueue_note(note_upsert("n-aaa", "aaaa")).unwrap();
        engine.enqueue_note(note_upsert("n-bbb", "bbbb")).unwrap();

        let embedder = HistogramEmbedder::new("fake-model");
        let reg = engine.register_embedder(embedder.clone()).unwrap();
        assert_eq!(
            (reg.corpus_changed, reg.invalidated, reg.pending),
            (false, 0, 2),
            "first registration: nothing invalidated, both notes pending"
        );
        assert_eq!(engine.pending_embed_count().unwrap(), 2);

        let progress = engine.embed_pending(10).unwrap();
        assert_eq!(
            (progress.attempted, progress.embedded, progress.pending),
            (2, 2, 0),
            "both embedded, queue drained"
        );
        assert_eq!(embedder.embed_calls.load(Ordering::SeqCst), 2);

        // The scan finds the right note, best-first, and never surfaces the other axis.
        let hits = engine.semantic_search("aaaa".into(), 10).unwrap();
        assert_eq!(hits[0].note_id, "n-aaa");
        assert!(
            (hits[0].score - 1.0).abs() < 1e-6,
            "identical text → cosine 1"
        );
        assert!(
            hits.iter().all(|h| h.note_id != "n-bbb" || h.score < 1e-6),
            "orthogonal note scores ~0"
        );
        // Empty/whitespace query → [] without an embed; limit is respected.
        assert!(engine.semantic_search("   ".into(), 10).unwrap().is_empty());
        assert_eq!(engine.semantic_search("aaaa".into(), 1).unwrap().len(), 1);

        // A second pass is a no-op — the derived queue self-reports empty.
        let again = engine.embed_pending(10).unwrap();
        assert_eq!((again.attempted, again.pending), (0, 0));
        assert_eq!(
            embedder.embed_calls.load(Ordering::SeqCst),
            2,
            "no re-embed"
        );

        // CRYPTO BOUNDARY: nothing embedding-related ever reaches the outbox (the mirror of
        // the PWA's embeddings-sync-exclusion), and neither the plaintext nor the RAW vector
        // bytes are on disk — only the sealed blob.
        let store = Store::open(&db_path).unwrap();
        for (_, table, _, _, _) in store.outbox_items().unwrap() {
            assert_ne!(table, "embeddings", "vectors must never enqueue for sync");
        }
        let expected_raw = crate::embeddings::to_le_bytes(
            &crate::embeddings::normalize(histogram("aaaa")).unwrap(),
        );
        let bytes = std::fs::read(&db_path).unwrap();
        assert!(
            !bytes
                .windows(expected_raw.len())
                .any(|w| w == expected_raw.as_slice()),
            "raw vector bytes must never be at rest — the stored blob is sealed"
        );
        let (_, _, sealed) = store.embedding_row("n-aaa").unwrap().unwrap();
        let sealed = sealed.expect("a real vector, not a marker");
        assert_eq!(sealed[0], 0x02, "the 0x02 byte-seal header");
        assert_eq!(
            engine
                .vault
                .open_bytes(sealed.clone(), crate::embeddings::embed_aad("n-aaa"))
                .unwrap(),
            expected_raw,
            "the sealed blob opens back to the unit vector under the emb:-prefixed AAD"
        );
        // Domain separation (crypto-review): the BARE note id — enc:v2's AAD under the
        // same Master Key — must NOT open a vector seal, and neither must another note's.
        assert!(
            engine
                .vault
                .open_bytes(sealed.clone(), "n-aaa".into())
                .is_err(),
            "the bare note-id AAD (the enc:v2 namespace) must fail the open"
        );
        assert!(
            engine
                .vault
                .open_bytes(sealed, crate::embeddings::embed_aad("n-bbb"))
                .is_err(),
            "a different note's emb: AAD must fail the open"
        );
    }

    #[test]
    fn similar_notes_probes_the_stored_vector_and_excludes_itself() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = embed_engine(&dir, "similar");
        engine.enqueue_note(note_upsert("n-a1", "aaaa")).unwrap();
        engine.enqueue_note(note_upsert("n-a2", "aaab")).unwrap(); // mostly-a: near n-a1
        engine.enqueue_note(note_upsert("n-c", "cccc")).unwrap(); // orthogonal
        engine
            .register_embedder(HistogramEmbedder::new("fake-model"))
            .unwrap();
        engine.embed_pending(10).unwrap();

        let hits = engine.similar_notes("n-a1".into(), 10).unwrap();
        assert!(hits.iter().all(|h| h.note_id != "n-a1"), "probe excluded");
        assert_eq!(hits[0].note_id, "n-a2", "nearest neighbour first");
        assert!(hits[0].score > 0.9);

        // A note with no vector yet (never enqueued/embedded) → [] rather than an error.
        assert!(engine.similar_notes("ghost".into(), 10).unwrap().is_empty());
        assert!(engine.similar_notes("n-a1".into(), 0).unwrap().is_empty());
    }

    #[test]
    fn empty_and_undecryptable_notes_get_skip_markers_and_the_queue_drains() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, db_path) = embed_engine(&dir, "markers");
        engine.enqueue_note(note_upsert("n-blank", "   ")).unwrap(); // whitespace-only
        engine.enqueue_note(note_upsert("n-real", "dddd")).unwrap();
        // A foreign-vault note on the same store: this vault cannot decrypt it.
        {
            let foreign = SyncEngine::open(
                db_path.clone(),
                "https://x.supabase.co".into(),
                "anon".into(),
                Vault::generate(),
            )
            .unwrap();
            foreign
                .enqueue_note(note_upsert("n-foreign", "unreadable here"))
                .unwrap();
        }

        let embedder = HistogramEmbedder::new("fake-model");
        engine.register_embedder(embedder.clone()).unwrap();
        let progress = engine.embed_pending(10).unwrap();
        assert_eq!(
            (
                progress.attempted,
                progress.embedded,
                progress.skipped,
                progress.pending
            ),
            (3, 1, 2, 0),
            "markers drain the queue instead of re-attempting forever"
        );
        assert_eq!(
            embedder.embed_calls.load(Ordering::SeqCst),
            1,
            "the embedder is never called for empty or undecryptable text"
        );
        // The markers are NULL-vector rows at the current key, invisible to the scan.
        let store = Store::open(&db_path).unwrap();
        for id in ["n-blank", "n-foreign"] {
            let (_, _, sealed) = store.embedding_row(id).unwrap().unwrap();
            assert!(sealed.is_none(), "{id} holds a skip marker");
        }
        let hits = engine.semantic_search("dddd".into(), 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].note_id, "n-real");
    }

    #[test]
    fn an_edit_re_queues_and_a_delete_hard_drops_the_vector() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, db_path) = embed_engine(&dir, "lifecycle");
        engine.enqueue_note(note_upsert("n1", "aaaa")).unwrap();
        engine.enqueue_note(note_upsert("n2", "bbbb")).unwrap();
        engine
            .register_embedder(HistogramEmbedder::new("fake-model"))
            .unwrap();
        engine.embed_pending(10).unwrap();
        assert_eq!(engine.pending_embed_count().unwrap(), 0);

        // An edit recomputes content_tag → the token moves → the note re-queues; after the
        // re-embed the search follows the NEW text.
        engine.enqueue_note(note_upsert("n1", "cccc")).unwrap();
        assert_eq!(engine.pending_embed_count().unwrap(), 1);
        engine.embed_pending(10).unwrap();
        let hits = engine.semantic_search("cccc".into(), 10).unwrap();
        assert_eq!(hits[0].note_id, "n1");

        // A local delete (the patch arm) hard-drops the vector via the apply_row hook.
        engine
            .enqueue_note(NoteUpsert {
                deleted: true,
                ..note_patch("n2")
            })
            .unwrap();
        assert!(
            Store::open(&db_path)
                .unwrap()
                .embedding_row("n2")
                .unwrap()
                .is_none(),
            "a note tombstone must not outlive-in-vector form"
        );
        // The deleted note never surfaces again (the scan is a pure top-k — OTHER live
        // notes may still return with ~0 scores; cutoffs are the consumer's business).
        assert!(engine
            .semantic_search("bbbb".into(), 10)
            .unwrap()
            .iter()
            .all(|h| h.note_id != "n2"));
    }

    #[test]
    fn a_model_upgrade_invalidates_and_rebuilds_progressively() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = embed_engine(&dir, "upgrade");
        engine.enqueue_note(note_upsert("n1", "aaaa")).unwrap();
        engine.enqueue_note(note_upsert("n2", "bbbb")).unwrap();
        engine
            .register_embedder(HistogramEmbedder::new("model-v1"))
            .unwrap();
        engine.embed_pending(10).unwrap();

        // Re-registering the SAME descriptor is a no-op: nothing invalidated, nothing pending.
        let same = engine
            .register_embedder(HistogramEmbedder::new("model-v1"))
            .unwrap();
        assert_eq!(
            (same.corpus_changed, same.invalidated, same.pending),
            (false, 0, 0)
        );

        // A different model id re-keys the corpus: both vectors drop, both notes re-queue.
        let upgraded = engine
            .register_embedder(HistogramEmbedder::new("model-v2"))
            .unwrap();
        assert_eq!(
            (
                upgraded.corpus_changed,
                upgraded.invalidated,
                upgraded.pending
            ),
            (true, 2, 2)
        );

        // Mid-rebuild: search is PARTIAL, not empty — the newest note re-embeds first and
        // returns immediately; the pending count reports the durable gap (this is the
        // restart-safe notification signal, not corpus_changed).
        engine.embed_pending(1).unwrap();
        assert_eq!(engine.pending_embed_count().unwrap(), 1);
        let visible = engine.semantic_search("bbbb".into(), 10).unwrap();
        assert_eq!(visible.len(), 1, "re-embedded note returns mid-rebuild");
        // And a FRESH registration at the new key mid-rebuild reports corpus_changed:false
        // while the count stays high — exactly the relaunch case.
        let relaunch = engine
            .register_embedder(HistogramEmbedder::new("model-v2"))
            .unwrap();
        assert_eq!(
            (
                relaunch.corpus_changed,
                relaunch.invalidated,
                relaunch.pending
            ),
            (false, 0, 1)
        );
    }

    #[test]
    fn embed_failures_never_halt_the_pass_but_unavailable_aborts_it() {
        /// Fails `embed_document` for texts containing "poison"; `Unavailable` for "gone".
        struct FlakyEmbedder;
        impl Embedder for FlakyEmbedder {
            fn descriptor(&self) -> EmbedderDescriptor {
                EmbedderDescriptor {
                    model_id: "flaky".into(),
                    dims: DIMS,
                    quantization: "test".into(),
                }
            }
            fn embed_document(&self, text: String) -> Result<Vec<f32>, EmbedderError> {
                if text.contains("poison") {
                    Err(EmbedderError::Runtime)
                } else if text.contains("gone") {
                    Err(EmbedderError::Unavailable)
                } else {
                    Ok(histogram(&text))
                }
            }
            fn embed_query(&self, text: String) -> Result<Vec<f32>, EmbedderError> {
                Ok(histogram(&text))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = embed_engine(&dir, "flaky");
        // created_at DESC ordering: stamp explicit created_at so the pass order is pinned —
        // poison first, then the good note, then "gone" LAST (created earliest).
        let stamped = |id: &str, text: &str, created_at: i64| NoteUpsert {
            created_at,
            ..note_upsert(id, text)
        };
        engine
            .enqueue_note(stamped("n-poison", "poison", 30))
            .unwrap();
        engine.enqueue_note(stamped("n-good", "aaaa", 20)).unwrap();
        engine.enqueue_note(stamped("n-gone", "gone", 10)).unwrap();
        engine.register_embedder(Arc::new(FlakyEmbedder)).unwrap();

        let progress = engine.embed_pending(10).unwrap();
        assert_eq!(
            (
                progress.attempted,
                progress.embedded,
                progress.failed,
                progress.pending
            ),
            (3, 1, 2, 2),
            "Runtime failure skips one; Unavailable aborts; both stay queued"
        );
        // The failures remain in the derived queue for the next pass — no marker written.
        assert_eq!(engine.pending_embed_count().unwrap(), 2);
    }

    #[test]
    fn a_wrong_dimension_or_degenerate_vector_is_never_stored() {
        struct WrongDims;
        impl Embedder for WrongDims {
            fn descriptor(&self) -> EmbedderDescriptor {
                EmbedderDescriptor {
                    model_id: "wrong".into(),
                    dims: DIMS,
                    quantization: "test".into(),
                }
            }
            fn embed_document(&self, text: String) -> Result<Vec<f32>, EmbedderError> {
                Ok(if text.contains("short") {
                    vec![1.0; DIMS as usize - 1] // violates the declared dims
                } else {
                    vec![0.0; DIMS as usize] // zero vector — unusable
                })
            }
            fn embed_query(&self, _t: String) -> Result<Vec<f32>, EmbedderError> {
                Ok(vec![1.0; DIMS as usize - 1])
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (engine, db_path) = embed_engine(&dir, "wrongdims");
        engine
            .enqueue_note(note_upsert("n-short", "short"))
            .unwrap();
        engine.enqueue_note(note_upsert("n-zero", "zero")).unwrap();
        engine.register_embedder(Arc::new(WrongDims)).unwrap();

        let progress = engine.embed_pending(10).unwrap();
        assert_eq!((progress.embedded, progress.failed), (0, 2));
        let store = Store::open(&db_path).unwrap();
        assert!(store.embedding_row("n-short").unwrap().is_none());
        assert!(store.embedding_row("n-zero").unwrap().is_none());
        // A wrong-dimension QUERY vector is a typed error, not a bad scan.
        assert!(matches!(
            engine.semantic_search("q".into(), 5).unwrap_err(),
            SyncError::Embed(_)
        ));
    }

    #[test]
    fn a_tampered_sealed_blob_self_heals_out_of_the_scan_and_requeues() {
        // The GCM auth-failure leg (crypto-review NIT): a tampered / bit-rotted blob must
        // never surface a hit, never wedge — it hard-deletes, the note re-queues via the
        // derived queue, and the next pass restores it.
        let dir = tempfile::tempdir().unwrap();
        let (engine, db_path) = embed_engine(&dir, "tamper");
        engine.enqueue_note(note_upsert("n1", "aaaa")).unwrap();
        engine
            .register_embedder(HistogramEmbedder::new("fake-model"))
            .unwrap();
        engine.embed_pending(10).unwrap();
        assert_eq!(engine.pending_embed_count().unwrap(), 0);

        // Flip one ciphertext byte in the stored blob.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let mut blob: Vec<u8> = conn
                .query_row(
                    "SELECT encrypted_vector FROM embeddings WHERE note_id = 'n1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let last = blob.len() - 1;
            blob[last] ^= 0x01;
            conn.execute(
                "UPDATE embeddings SET encrypted_vector = ?1 WHERE note_id = 'n1'",
                rusqlite::params![blob],
            )
            .unwrap();
        }

        assert!(
            engine
                .semantic_search("aaaa".into(), 10)
                .unwrap()
                .is_empty(),
            "a blob that fails its auth tag never surfaces"
        );
        assert!(
            Store::open(&db_path)
                .unwrap()
                .embedding_row("n1")
                .unwrap()
                .is_none(),
            "the corrupt row is hard-deleted (self-heal)"
        );
        assert_eq!(engine.pending_embed_count().unwrap(), 1, "note re-queued");
        engine.embed_pending(10).unwrap();
        assert_eq!(
            engine.semantic_search("aaaa".into(), 10).unwrap()[0].note_id,
            "n1",
            "the next pass restores the vector"
        );
    }

    #[test]
    fn an_embedder_may_reenter_the_engine_mid_embed() {
        // The lock-discipline pin: NO engine mutex is held across a host embed call, so an
        // embedder that calls back into the engine (progress reads are the natural case)
        // must complete rather than deadlock. FAILURE MODE IS A HANG, not a red assert —
        // a CI timeout on this test means a lock is being held across the callback.
        struct ReentrantEmbedder {
            engine: Mutex<Option<Arc<SyncEngine>>>,
            observed_pending: AtomicU32,
        }
        impl Embedder for ReentrantEmbedder {
            fn descriptor(&self) -> EmbedderDescriptor {
                EmbedderDescriptor {
                    model_id: "reentrant".into(),
                    dims: DIMS,
                    quantization: "test".into(),
                }
            }
            fn embed_document(&self, text: String) -> Result<Vec<f32>, EmbedderError> {
                let engine = self.engine.lock().unwrap().clone().unwrap();
                let pending = engine.pending_embed_count().unwrap(); // ← the reentrant call
                self.observed_pending.store(pending, Ordering::SeqCst);
                Ok(histogram(&text))
            }
            fn embed_query(&self, text: String) -> Result<Vec<f32>, EmbedderError> {
                Ok(histogram(&text))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = embed_engine(&dir, "reentrant");
        engine.enqueue_note(note_upsert("n1", "aaaa")).unwrap();
        let embedder = Arc::new(ReentrantEmbedder {
            engine: Mutex::new(None),
            observed_pending: AtomicU32::new(u32::MAX),
        });
        engine.register_embedder(embedder.clone()).unwrap();
        *embedder.engine.lock().unwrap() = Some(engine.clone());

        let progress = engine.embed_pending(10).unwrap();
        assert_eq!(progress.embedded, 1);
        assert_eq!(
            embedder.observed_pending.load(Ordering::SeqCst),
            1,
            "the reentrant read ran mid-pass (n1 still pending while being embedded)"
        );
    }
}
