//! Read/query surface (SUR-744) — the DTOs the native list/search screens consume, and the
//! row → DTO mapping that decrypts note text **in core** on the way out.
//!
//! The `SyncEngine` methods that export these (`list_books`, `get_note`, `search`, …) are thin
//! wrappers in `sync/mod.rs`; the shape and the crypto boundary live here. Two invariants the
//! crypto-reviewer gate protects:
//!
//! 1. **Ciphertext never crosses the FFI for display.** `NoteRecord.text` is plaintext, produced
//!    by `Vault::decrypt_note`. A `notes.text` that structurally looks encrypted (`is_encrypted`)
//!    is decrypted; on failure the row surfaces as `text: None, decrypt_failed: true` and is
//!    dropped from the search index — it never fails the whole page (AC #2/#3, mirrors the PWA's
//!    `decryptError` skip).
//! 2. **Plaintext never reaches disk.** Decryption happens per-read into the returned DTO / the
//!    in-memory `SearchDoc`; nothing is written back to the store (ADR 0003 preserved).

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::note_encryption::is_encrypted;
use crate::search::{SearchDoc, SearchDocKind};
use crate::store::Store;
use crate::vault::Vault;

/// The PWA Home's rolling "this week" window, verbatim (`App.jsx`: `Date.now() - 7*24*60*60*1000`).
/// Epoch milliseconds — the unit `created_at` is stored in.
const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// A book for the Library / Sources grid: the descriptor column set (minus `deleted`, which is
/// always `0` for a returned row) plus `note_count` — live notes filed under this book, for the
/// grid's count badge.
#[derive(Debug, Clone, uniffi::Record)]
pub struct BookRecord {
    pub id: String,
    pub title: Option<String>,
    pub author: Option<String>,
    pub isbn: Option<String>,
    pub cover_url: Option<String>,
    pub cover_source: Option<String>,
    pub cover_resolved_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub note_count: u32,
}

/// A note for the Commonplace list / NoteForm. `text` is **plaintext** (decrypted in core), or
/// `None` when the row failed to decrypt (`decrypt_failed = true`) or genuinely has no text.
/// `source_meta_json` mirrors the write-side `…Json` convention — the `source_meta` object
/// re-serialized to a JSON string, since UniFFI has no jsonb type.
#[derive(Debug, Clone, uniffi::Record)]
pub struct NoteRecord {
    pub id: String,
    pub book_id: Option<String>,
    pub text: Option<String>,
    pub decrypt_failed: bool,
    pub page: Option<String>,
    pub tags: Vec<String>,
    pub image_path: Option<String>,
    pub ink_crop_path: Option<String>,
    pub source: Option<String>,
    pub source_id: Option<String>,
    pub source_meta_json: Option<String>,
    pub chapter: Option<String>,
    pub content_tag: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A custom idea for the AddIdeaSheet "Your Ideas" section.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CustomIdeaRecord {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Live (non-deleted) row totals for stat / empty-state surfaces.
#[derive(Debug, Clone, uniffi::Record)]
pub struct StoreCounts {
    pub books: u32,
    pub notes: u32,
    pub custom_ideas: u32,
    /// Distinct idea **tags** present on ≥1 live note — the PWA Home's `activeIdeasCount`
    /// (SUR-806). Deliberately **not** `custom_ideas` (raw idea rows): canon and custom tags both
    /// count, and a tag on no live note doesn't count. Tags are plaintext, so this never decrypts.
    pub active_ideas: u32,
}

/// One row of the per-idea tally (SUR-858): a distinct idea tag and the number of live notes that
/// carry it. The Commonplace tree / Lexicon overlays these onto the client-generated **canon
/// structure** (which stays a host constant), so only tags actually present on ≥1 live note appear
/// here (`count ≥ 1`). Tags are plaintext, so building this never decrypts.
#[derive(Debug, Clone, uniffi::Record)]
pub struct IdeaCount {
    pub idea: String,
    pub count: u32,
}

/// A collection for the Lexicon list (SUR-858). A **bare** descriptor row — no membership count
/// (the consuming screen doesn't render one yet; add it only when it does). No crypto: every column
/// is plaintext metadata.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CollectionRecord {
    pub id: String,
    pub name: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A lens — one authored saved-query — for the Lexicon list (SUR-858). `leaf_ids` is the query's
/// leaf set (SUR-737 whole-row LWW: a lens is ONE authored query, so no leaf union). `combinator` /
/// `threshold` are the query's combine rule; both are always written by `enqueue_lens` (defaults
/// `AND` / `100`) but read defensively as `Option`. No crypto: plaintext metadata.
#[derive(Debug, Clone, uniffi::Record)]
pub struct LensRecord {
    pub id: String,
    pub name: Option<String>,
    pub leaf_ids: Vec<String>,
    pub combinator: Option<String>,
    pub threshold: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One live `note_links` edge (SUR-923) — the parent↔margin relation for the note action sheets.
/// `from_note_id` is the parent (the printed/typed source note), `to_note_id` the margin child —
/// exactly the PWA's row shape (`saveNoteLink`, `db.js`). `relation_type` is always written by
/// `enqueue_note_link` (defaults `handwritten_annotation`, the only value in existence) but read
/// defensively as `Option`, like [`LensRecord`]'s `combinator`. No crypto: link rows are plaintext
/// metadata — the note text they relate stays in `notes`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct NoteLinkRecord {
    pub id: String,
    pub from_note_id: String,
    pub to_note_id: String,
    pub relation_type: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One row of the per-collection live-note tally (SUR-923) — the Lexicon Collections tab's
/// "N notes" subtitles, shaped like [`IdeaCount`]. Founder decision (2026-07-17): a membership
/// counts only when its note row is **present and live** — a deliberate divergence from the PWA's
/// `noteCountByCollection` (a raw live-membership tally, no notes join), matching the read-time
/// scope resolver `notesInCollection` and this core's [`idea_counts`] instead, so the subtitle
/// agrees with what the collection-scoped note list shows. Only `count ≥ 1` rows appear (hosts
/// default a missing collection to 0). No decryption: ids are plaintext.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CollectionNoteCount {
    pub collection_id: String,
    pub count: u32,
}

// ── store reads → DTOs ───────────────────────────────────────────────────────

/// Library grid page: books newest-first, each with its live note count. N+1 counts, one per
/// book on the page. ponytail: fine at page scale (a page is dozens of books); a single
/// `GROUP BY` join only if a page ever grows large enough to matter.
pub fn list_books(store: &Store, limit: i64, offset: i64) -> rusqlite::Result<Vec<BookRecord>> {
    let mut out = Vec::new();
    for row in store.list_live("books", None, limit, offset)? {
        out.push(book_record(store, &row)?);
    }
    Ok(out)
}

pub fn get_book(store: &Store, id: &str) -> rusqlite::Result<Option<BookRecord>> {
    match store.get_row("books", id)? {
        Some(row) if !is_deleted(&row) => Ok(Some(book_record(store, &row)?)),
        _ => Ok(None),
    }
}

/// Notes newest-first — `book_id = None` is the Commonplace flat list (all notes), `Some` filters
/// to one book. Each row's text is decrypted here.
pub fn list_notes(
    store: &Store,
    vault: &Vault,
    book_id: Option<&str>,
    limit: i64,
    offset: i64,
) -> rusqlite::Result<Vec<NoteRecord>> {
    let filter = book_id.map(|b| ("book_id", b));
    Ok(store
        .list_live("notes", filter, limit, offset)?
        .iter()
        .map(|row| note_record(row, vault))
        .collect())
}

pub fn get_note(store: &Store, vault: &Vault, id: &str) -> rusqlite::Result<Option<NoteRecord>> {
    match store.get_row("notes", id)? {
        Some(row) if !is_deleted(&row) => Ok(Some(note_record(&row, vault))),
        _ => Ok(None),
    }
}

/// Live notes carrying `idea` as an idea tag, newest-first, decrypted in core (SUR-858) — the
/// Commonplace idea filter / IdeaDetail / RelatedNotes. `idea` is the raw tag string as stored in
/// `notes.tags` (== a [`CustomIdeaRecord`]'s `name`, == an [`IdeaCount`] key); the match is
/// **exact**, so a tag the client got from [`idea_counts`] round-trips straight back here without
/// any tag↔id resolution (the internal `cidea_…` id is never in `tags`, and this read never sees
/// it). `tags` is a JSON array, so the store's scalar filter can't push this into SQL —
/// **scan-then-filter**, filtering on the plaintext `tags` column BEFORE decrypting so only the
/// page's notes pay the decrypt cost. ponytail: full scan; a tag index only if note counts ever
/// make it matter.
pub fn notes_by_idea(
    store: &Store,
    vault: &Vault,
    idea: &str,
    limit: i64,
    offset: i64,
) -> rusqlite::Result<Vec<NoteRecord>> {
    Ok(store
        .list_live("notes", None, -1, 0)?
        .into_iter()
        .filter(|row| string_array_field(row, "tags").iter().any(|t| t == idea))
        .skip(offset.max(0) as usize)
        .take(page_take(limit))
        .map(|row| note_record(&row, vault))
        .collect())
}

/// Live notes with NO idea tags, newest-first, decrypted in core (SUR-858) — BulkDiscovery's work
/// queue. Same scan-then-filter / window-before-decrypt shape as [`notes_by_idea`]. ponytail: full
/// scan; a "has no tags" index only if it ever matters.
pub fn untagged_notes(
    store: &Store,
    vault: &Vault,
    limit: i64,
    offset: i64,
) -> rusqlite::Result<Vec<NoteRecord>> {
    Ok(store
        .list_live("notes", None, -1, 0)?
        .into_iter()
        .filter(|row| string_array_field(row, "tags").is_empty())
        .skip(offset.max(0) as usize)
        .take(page_take(limit))
        .map(|row| note_record(&row, vault))
        .collect())
}

/// Count of the whole [`untagged_notes`] set (SUR-858) — BulkDiscovery's queue badge. Tags are
/// plaintext, so this counts without decrypting (and ignores pagination — it's the full queue size).
pub fn untagged_notes_count(store: &Store) -> rusqlite::Result<u32> {
    Ok(store
        .list_live("notes", None, -1, 0)?
        .iter()
        .filter(|row| string_array_field(row, "tags").is_empty())
        .count() as u32)
}

pub fn list_custom_ideas(
    store: &Store,
    limit: i64,
    offset: i64,
) -> rusqlite::Result<Vec<CustomIdeaRecord>> {
    Ok(store
        .list_live("custom_ideas", None, limit, offset)?
        .iter()
        .map(custom_idea_record)
        .collect())
}

/// Collections for the Lexicon list (SUR-858), newest-first — the `collections` store's first read
/// path (it had a write path since SUR-726, no read). Scalar metadata, no crypto, so this is a
/// straight `list_live` map like [`list_custom_ideas`].
pub fn list_collections(
    store: &Store,
    limit: i64,
    offset: i64,
) -> rusqlite::Result<Vec<CollectionRecord>> {
    Ok(store
        .list_live("collections", None, limit, offset)?
        .iter()
        .map(collection_record)
        .collect())
}

/// Lenses (authored saved-queries) for the Lexicon list (SUR-858), newest-first — the `lenses`
/// store's first read path. Plaintext metadata, no crypto.
pub fn list_lenses(store: &Store, limit: i64, offset: i64) -> rusqlite::Result<Vec<LensRecord>> {
    Ok(store
        .list_live("lenses", None, limit, offset)?
        .iter()
        .map(lens_record)
        .collect())
}

pub fn counts(store: &Store) -> rusqlite::Result<StoreCounts> {
    Ok(StoreCounts {
        books: store.count_live("books", None)? as u32,
        notes: store.count_live("notes", None)? as u32,
        custom_ideas: store.count_live("custom_ideas", None)? as u32,
        active_ideas: active_ideas(store)?,
    })
}

/// The `{tag: live-note count}` tally over every live note — the PWA's `ideaCountsFor(notes)`
/// (`src/lib/scope.js`), byte-for-byte: iterate each note's `tags` and increment, **no within-note
/// dedup** (a note tagged `["logic","logic"]` contributes 2 to `logic`). The single scan behind both
/// [`active_ideas`] (its key count) and [`idea_counts`] (the tally itself). Tags are a plaintext
/// `Json` column, so this never touches the `Vault`. ponytail: full `tags` scan; a tag index only if
/// note counts ever make it matter.
fn tag_tally(store: &Store) -> rusqlite::Result<HashMap<String, u32>> {
    let mut tally: HashMap<String, u32> = HashMap::new();
    for row in store.list_live("notes", None, -1, 0)? {
        for tag in string_array_field(&row, "tags") {
            *tally.entry(tag).or_insert(0) += 1;
        }
    }
    Ok(tally)
}

/// Distinct idea tags across all live notes — the PWA Home's `activeIdeasCount`, which is
/// `Object.entries(ideaCounts).filter(([, c]) => c > 0).length`. A tally key exists only via an
/// increment, so that filter is a no-op: the answer is exactly the number of distinct tag names on
/// ≥1 live note — the key count of [`tag_tally`].
fn active_ideas(store: &Store) -> rusqlite::Result<u32> {
    Ok(tag_tally(store)?.len() as u32)
}

/// Per-idea live-note counts (SUR-858) — the PWA's `ideaCountsFor` tally as a list, sorted by idea
/// name **ascending** for a stable order across reads. Present-tags-only (every entry has
/// `count ≥ 1`): a canon idea on no live note is absent, because the client overlays these onto its
/// own generated canon structure. No decryption (tags are plaintext).
pub fn idea_counts(store: &Store) -> rusqlite::Result<Vec<IdeaCount>> {
    let mut out: Vec<IdeaCount> = tag_tally(store)?
        .into_iter()
        .map(|(idea, count)| IdeaCount { idea, count })
        .collect();
    out.sort_by(|a, b| a.idea.cmp(&b.idea));
    Ok(out)
}

// ── relation reads (SUR-923) ─────────────────────────────────────────────────
// Extension #3 of the read surface: the membership + note-link relations, both directions.
// No decryption anywhere — no note text is involved in any of these.

/// Ids of the live collections whose membership row pairs with `note_id` (SUR-923) — the
/// AddToCollectionSheet's `memberIds`, mirroring the PWA derivation exactly (`new Set(
/// collectionMemberships.filter(m => !m.deleted && m.noteId === noteId).map(m => m.collectionId))`,
/// `NoteActionOverlay.jsx`): live **membership** rows only — no collection-liveness check and no
/// notes join (the sheet filters its rendered rows to live collections itself). Deduped like the
/// oracle's `Set`, for the same reason [`note_ids_for_collection`] dedups: a foreign row under a
/// rogue random pk can pair the same (collection, note) twice, and a host rendering these as chips
/// would show the collection twice until that row is tombstoned. Store scan order (membership
/// `created_at` DESC). No pagination: a note belongs to a handful of collections.
pub fn collection_ids_for_note(store: &Store, note_id: &str) -> rusqlite::Result<Vec<String>> {
    let mut seen = HashSet::new();
    Ok(store
        .list_live("collection_memberships", Some(("note_id", note_id)), -1, 0)?
        .iter()
        .filter_map(|row| string_field(row, "collection_id"))
        .filter(|id| seen.insert(id.clone()))
        .collect())
}

/// Live `note_links` edges touching `note_id` on either end (SUR-923) — one hop, both directions,
/// the PWA's cascade query (`where('fromNoteId').equals(noteId).or('toNoteId').equals(noteId)`,
/// `db.js`). The host filters by direction ("children of this parent" = rows where the note is
/// `from`; "parent of this child" = rows where it is `to`) and by `relation_type`, as every PWA
/// read does. Scan-then-filter like [`notes_by_idea`] (the store's scalar filter is a single
/// equality — no OR); store scan order (`created_at` DESC — PWA display sorts ascending
/// host-side, and hosts have the timestamps). No pagination: per-note links are small.
pub fn note_links_for_note(store: &Store, note_id: &str) -> rusqlite::Result<Vec<NoteLinkRecord>> {
    Ok(store
        .list_live("note_links", None, -1, 0)?
        .iter()
        .filter(|row| {
            string_field(row, "from_note_id").as_deref() == Some(note_id)
                || string_field(row, "to_note_id").as_deref() == Some(note_id)
        })
        .map(note_link_record)
        .collect())
}

/// Live member note ids of one collection (SUR-923) — the PWA's `memberNoteIds`
/// (`lib/collections.js`): live **membership** rows only, deduped like the oracle's `Set`.
/// Deliberately **no notes join** — the host-side collection-delete cascade consumes this and
/// must see every live membership, including one whose note is already soft-deleted, to
/// tombstone them all (`useCollections.removeCollection`); the collection-scoped note *list*
/// re-checks note liveness host-side, as the PWA's `notesInCollection` does. The deterministic
/// `membership_id` pk makes a live duplicate pair impossible via `enqueue_*`; the dedup guards
/// against a foreign row under a rogue random pk. Store scan order (`created_at` DESC).
pub fn note_ids_for_collection(
    store: &Store,
    collection_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut seen = HashSet::new();
    Ok(store
        .list_live(
            "collection_memberships",
            Some(("collection_id", collection_id)),
            -1,
            0,
        )?
        .iter()
        .filter_map(|row| string_field(row, "note_id"))
        .filter(|id| seen.insert(id.clone()))
        .collect())
}

/// Per-collection live-note counts (SUR-923) — the Lexicon Collections tab subtitles, shaped like
/// [`idea_counts`]: one pass over live memberships, sorted by `collection_id` **ascending**, only
/// `count ≥ 1` rows. Per the founder decision recorded on [`CollectionNoteCount`], a membership
/// counts only when its note is present and live, and a distinct (collection, note) pair counts
/// once — so the subtitle always equals the length of the host's liveness-filtered
/// [`note_ids_for_collection`] list. No collection-liveness join (matches both PWA counts — a
/// deleted collection's tally simply never renders). No decryption.
pub fn collection_note_counts(store: &Store) -> rusqlite::Result<Vec<CollectionNoteCount>> {
    let live_notes: HashSet<String> = store
        .list_live("notes", None, -1, 0)?
        .iter()
        .filter_map(|row| string_field(row, "id"))
        .collect();
    let mut seen_pairs = HashSet::new();
    let mut tally: HashMap<String, u32> = HashMap::new();
    for row in store.list_live("collection_memberships", None, -1, 0)? {
        let (Some(cid), Some(nid)) = (
            string_field(&row, "collection_id"),
            string_field(&row, "note_id"),
        ) else {
            continue;
        };
        if !live_notes.contains(&nid) || !seen_pairs.insert((cid.clone(), nid)) {
            continue;
        }
        *tally.entry(cid).or_insert(0) += 1;
    }
    let mut out: Vec<CollectionNoteCount> = tally
        .into_iter()
        .map(|(collection_id, count)| CollectionNoteCount {
            collection_id,
            count,
        })
        .collect();
    out.sort_by(|a, b| a.collection_id.cmp(&b.collection_id));
    Ok(out)
}

/// The PWA Home "this week" set (`App.jsx` `useMemo`): live notes created within the last
/// [`WEEK_MS`] whose **decrypted** text is non-empty, newest-first. Both `notes_this_week` (its
/// size) and `recent_note` (a pick from it) derive from this one set — exactly as the PWA computes
/// both in a single memo. `now_ms` is the host's `Date.now()` (this core has no read-side clock),
/// so the window is a pure function of its inputs and the store — deterministic at the boundary.
///
/// ponytail: window-filter on `created_at` BEFORE decrypting, so a 7-day count never pays to
/// decrypt the whole archive — only the notes actually in the window.
fn fresh_notes(store: &Store, vault: &Vault, now_ms: i64) -> rusqlite::Result<Vec<NoteRecord>> {
    let cutoff = now_ms - WEEK_MS;
    let mut out = Vec::new();
    for row in store.list_live("notes", None, -1, 0)? {
        if int_field(&row, "created_at") < cutoff {
            continue; // outside the rolling window — skip before paying the decrypt cost
        }
        let rec = note_record(&row, vault);
        // Mirror the oracle's filter: `!decryptError && (text || '').trim()`.
        if rec.decrypt_failed || rec.text.as_deref().unwrap_or("").trim().is_empty() {
            continue;
        }
        out.push(rec);
    }
    Ok(out)
}

/// Count of the [`fresh_notes`] set — the PWA Home's `notesThisWeek` (`fresh.length`), byte-matched
/// (SUR-806 AC): a rolling 168h window on `created_at` (inclusive lower bound), decrypt-in-core,
/// with empty/whitespace text and decrypt failures excluded.
pub fn notes_this_week(store: &Store, vault: &Vault, now_ms: i64) -> rusqlite::Result<u32> {
    Ok(fresh_notes(store, vault, now_ms)?.len() as u32)
}

/// A pseudo-random note from the same [`fresh_notes`] set — the Home "Recently surfaced" card
/// (`fresh[floor(random()*len)]`) — or `None` when nothing is fresh. `seed` is the host's random
/// draw: the core has no read-path RNG and stays deterministic for a fixture, and the host re-rolls
/// `seed` to re-surface a note, exactly as the PWA re-runs the memo on a `notes` change.
pub fn recent_note(
    store: &Store,
    vault: &Vault,
    now_ms: i64,
    seed: u64,
) -> rusqlite::Result<Option<NoteRecord>> {
    let fresh = fresh_notes(store, vault, now_ms)?;
    if fresh.is_empty() {
        return Ok(None);
    }
    Ok(Some(fresh[(seed % fresh.len() as u64) as usize].clone()))
}

/// Build the search corpus: every live note (decrypted) + every live custom idea, mirroring the
/// PWA's `toDocuments` skip rules — drop decrypt failures, empty text, and empty ideas. No
/// pagination (`limit < 0`): search indexes the whole archive.
pub fn build_search_docs(store: &Store, vault: &Vault) -> rusqlite::Result<Vec<SearchDoc>> {
    let mut docs = Vec::new();

    for row in store.list_live("notes", None, -1, 0)? {
        let id = string_field(&row, "id").unwrap_or_default();
        let (text, failed) = decrypt_note_text(&row, &id, vault);
        if failed {
            continue; // a decrypt failure never enters the index (AC #3)
        }
        let content = text.unwrap_or_default().trim().to_string();
        if content.is_empty() {
            continue;
        }
        docs.push(SearchDoc {
            kind: SearchDocKind::Note,
            ref_id: id,
            title: String::new(), // notes have no title field (PWA hardcodes '')
            content,
        });
    }

    for row in store.list_live("custom_ideas", None, -1, 0)? {
        let id = string_field(&row, "id").unwrap_or_default();
        let title = string_field(&row, "name")
            .unwrap_or_default()
            .trim()
            .to_string();
        let content = string_field(&row, "description")
            .unwrap_or_default()
            .trim()
            .to_string();
        if title.is_empty() && content.is_empty() {
            continue;
        }
        docs.push(SearchDoc {
            kind: SearchDocKind::Idea,
            ref_id: id,
            title,
            content,
        });
    }

    Ok(docs)
}

// ── row → DTO helpers ────────────────────────────────────────────────────────

fn book_record(store: &Store, row: &Map<String, Value>) -> rusqlite::Result<BookRecord> {
    let id = string_field(row, "id").unwrap_or_default();
    let note_count = store.count_live("notes", Some(("book_id", &id)))? as u32;
    Ok(BookRecord {
        id,
        title: string_field(row, "title"),
        author: string_field(row, "author"),
        isbn: string_field(row, "isbn"),
        cover_url: string_field(row, "cover_url"),
        cover_source: string_field(row, "cover_source"),
        cover_resolved_at: opt_int_field(row, "cover_resolved_at"),
        created_at: int_field(row, "created_at"),
        updated_at: int_field(row, "updated_at"),
        note_count,
    })
}

fn note_record(row: &Map<String, Value>, vault: &Vault) -> NoteRecord {
    let id = string_field(row, "id").unwrap_or_default();
    let (text, decrypt_failed) = decrypt_note_text(row, &id, vault);
    NoteRecord {
        book_id: string_field(row, "book_id"),
        text,
        decrypt_failed,
        page: string_field(row, "page"),
        tags: string_array_field(row, "tags"),
        image_path: string_field(row, "image_path"),
        ink_crop_path: string_field(row, "ink_crop_path"),
        source: string_field(row, "source"),
        source_id: string_field(row, "source_id"),
        source_meta_json: json_string_field(row, "source_meta"),
        chapter: string_field(row, "chapter"),
        content_tag: string_field(row, "content_tag"),
        created_at: int_field(row, "created_at"),
        updated_at: int_field(row, "updated_at"),
        id,
    }
}

fn custom_idea_record(row: &Map<String, Value>) -> CustomIdeaRecord {
    CustomIdeaRecord {
        id: string_field(row, "id").unwrap_or_default(),
        name: string_field(row, "name"),
        description: string_field(row, "description"),
        created_at: int_field(row, "created_at"),
        updated_at: int_field(row, "updated_at"),
    }
}

fn collection_record(row: &Map<String, Value>) -> CollectionRecord {
    CollectionRecord {
        id: string_field(row, "id").unwrap_or_default(),
        name: string_field(row, "name"),
        created_at: int_field(row, "created_at"),
        updated_at: int_field(row, "updated_at"),
    }
}

fn note_link_record(row: &Map<String, Value>) -> NoteLinkRecord {
    NoteLinkRecord {
        id: string_field(row, "id").unwrap_or_default(),
        from_note_id: string_field(row, "from_note_id").unwrap_or_default(),
        to_note_id: string_field(row, "to_note_id").unwrap_or_default(),
        relation_type: string_field(row, "relation_type"),
        created_at: int_field(row, "created_at"),
        updated_at: int_field(row, "updated_at"),
    }
}

fn lens_record(row: &Map<String, Value>) -> LensRecord {
    LensRecord {
        id: string_field(row, "id").unwrap_or_default(),
        name: string_field(row, "name"),
        leaf_ids: string_array_field(row, "leaf_ids"),
        combinator: string_field(row, "combinator"),
        threshold: opt_int_field(row, "threshold"),
        created_at: int_field(row, "created_at"),
        updated_at: int_field(row, "updated_at"),
    }
}

/// `take(n)` count for a Rust-side page slice, mirroring [`Store::list_live`]'s convention that a
/// negative `limit` means "no limit". The FFI reads pass a `u32` (always ≥ 0); this only guards the
/// internal contract so a `-1` scans the whole filtered set instead of taking none.
fn page_take(limit: i64) -> usize {
    if limit < 0 {
        usize::MAX
    } else {
        limit as usize
    }
}

/// Decrypt a `notes.text` on the way out. `(plaintext, decrypt_failed)`:
/// - absent → `(None, false)`; empty → `(Some(""), false)` (not a failure);
/// - looks encrypted → `Vault::decrypt_note`; `Ok` → `(Some(plaintext), false)`, `Err` →
///   `(None, true)` (foreign/corrupt AAD, wrong key, malformed — never distinguished, never
///   fails the page);
/// - already plaintext (defensive; the core assumes encrypted users) → passed through.
///
/// A successfully stored ciphertext therefore never crosses the FFI in its `enc:` form (AC #2):
/// it is either decrypted plaintext or `None`.
///
/// `pub(super)` so the content-tag self-heal ([`super::reconcile::reconcile_heal_content_tags`],
/// SUR-884) re-derives a missing tag through the EXACT same decrypt gate the display path uses —
/// one source for the `decryptError` skip, so the two paths can't drift.
pub(super) fn decrypt_note_text(
    row: &Map<String, Value>,
    id: &str,
    vault: &Vault,
) -> (Option<String>, bool) {
    match string_field(row, "text") {
        None => (None, false),
        Some(t) if t.is_empty() => (Some(String::new()), false),
        Some(t) if is_encrypted(&t) => match vault.decrypt_note(Some(id.to_string()), t) {
            Ok(plaintext) => (Some(plaintext), false),
            Err(_) => (None, true),
        },
        Some(t) => (Some(t), false),
    }
}

fn is_deleted(row: &Map<String, Value>) -> bool {
    matches!(row.get("deleted"), Some(Value::Bool(true)))
}

fn string_field(row: &Map<String, Value>, key: &str) -> Option<String> {
    match row.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn int_field(row: &Map<String, Value>, key: &str) -> i64 {
    row.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn opt_int_field(row: &Map<String, Value>, key: &str) -> Option<i64> {
    row.get(key).and_then(Value::as_i64)
}

/// A `Json` column that stores an array of strings (`tags`), read back by `sql_to_json` as a
/// JSON array. Non-string / absent → empty vec.
fn string_array_field(row: &Map<String, Value>, key: &str) -> Vec<String> {
    match row.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

/// A `Json` object column (`source_meta`) re-serialized to its JSON string for the FFI, or `None`
/// when absent/null.
fn json_string_field(row: &Map<String, Value>, key: &str) -> Option<String> {
    match row.get(key) {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::vault::Vault;
    use serde_json::json;

    // A minimal notes row as `apply_row` would store it (pull sink): text is ciphertext at rest.
    fn seal(vault: &Vault, id: &str, plaintext: &str) -> String {
        vault.encrypt_note(Some(id.to_string()), plaintext.to_string())
    }

    fn note_row(
        id: &str,
        book_id: Option<&str>,
        text: &str,
        created_at: i64,
    ) -> Map<String, Value> {
        let mut r = Map::new();
        r.insert("id".into(), json!(id));
        if let Some(b) = book_id {
            r.insert("book_id".into(), json!(b));
        }
        r.insert("text".into(), json!(text));
        r.insert("tags".into(), json!(["philosophy", "ethics"]));
        r.insert("content_tag".into(), json!("deadbeef"));
        r.insert("created_at".into(), json!(created_at));
        r.insert("updated_at".into(), json!(created_at));
        r.insert("deleted".into(), json!(false));
        r
    }

    // Same row, with the `tags` array overridden (the SUR-806 active-ideas / fresh-set cases).
    fn note_row_tagged(id: &str, text: &str, created_at: i64, tags: &[&str]) -> Map<String, Value> {
        let mut r = note_row(id, None, text, created_at);
        r.insert("tags".into(), json!(tags));
        r
    }

    #[test]
    fn note_text_round_trips_to_plaintext_never_ciphertext() {
        // AC #2: a stored enc:v2 ciphertext comes back as plaintext, never the enc: sentinel.
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        let ct = seal(&vault, "n1", "the unexamined life");
        store
            .apply_row("notes", &note_row("n1", Some("b1"), &ct, 1000))
            .unwrap();

        let rec = get_note(&store, &vault, "n1").unwrap().expect("live note");
        assert_eq!(rec.text.as_deref(), Some("the unexamined life"));
        assert!(!rec.decrypt_failed);
        assert!(
            !rec.text.unwrap().starts_with("enc:v"),
            "plaintext must not carry an enc: sentinel"
        );
        assert_eq!(rec.book_id.as_deref(), Some("b1"));
        assert_eq!(rec.tags, vec!["philosophy", "ethics"]);
    }

    #[test]
    fn foreign_row_yields_decrypt_failed_without_failing_the_page() {
        // AC #3: a note sealed under a DIFFERENT vault (foreign MK/AAD) can't decrypt — the row
        // surfaces decrypt_failed=true, text=None, and the page still returns it.
        let store = Store::open_in_memory().unwrap();
        let mine = Vault::generate();
        let foreign = Vault::generate();
        let good = seal(&mine, "n1", "mine to read");
        let bad = seal(&foreign, "n2", "not mine");
        store
            .apply_row("notes", &note_row("n1", None, &good, 2000))
            .unwrap();
        store
            .apply_row("notes", &note_row("n2", None, &bad, 1000))
            .unwrap();

        let notes = list_notes(&store, &mine, None, 50, 0).unwrap();
        assert_eq!(notes.len(), 2, "one bad row must not drop the whole page");
        let n1 = notes.iter().find(|n| n.id == "n1").unwrap();
        let n2 = notes.iter().find(|n| n.id == "n2").unwrap();
        assert_eq!(n1.text.as_deref(), Some("mine to read"));
        assert!(!n1.decrypt_failed);
        assert!(n2.text.is_none());
        assert!(n2.decrypt_failed);
    }

    #[test]
    fn lists_exclude_soft_deleted_and_paginate_newest_first() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        for (i, ts) in [("n1", 100), ("n2", 300), ("n3", 200)] {
            store
                .apply_row("notes", &note_row(i, None, &seal(&vault, i, "text"), ts))
                .unwrap();
        }
        // soft-delete n2
        let mut del = note_row("n2", None, &seal(&vault, "n2", "text"), 300);
        del.insert("deleted".into(), json!(true));
        store.apply_row("notes", &del).unwrap();

        let notes = list_notes(&store, &vault, None, 50, 0).unwrap();
        let ids: Vec<&str> = notes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["n3", "n1"], "deleted excluded; created_at DESC");
    }

    #[test]
    fn book_carries_live_note_count() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        let mut book = Map::new();
        book.insert("id".into(), json!("b1"));
        book.insert("title".into(), json!("Meditations"));
        book.insert("created_at".into(), json!(1));
        book.insert("updated_at".into(), json!(1));
        book.insert("deleted".into(), json!(false));
        store.apply_row("books", &book).unwrap();
        store
            .apply_row(
                "notes",
                &note_row("n1", Some("b1"), &seal(&vault, "n1", "a"), 1),
            )
            .unwrap();
        store
            .apply_row(
                "notes",
                &note_row("n2", Some("b1"), &seal(&vault, "n2", "b"), 2),
            )
            .unwrap();
        // a deleted note under b1 must not be counted
        let mut del = note_row("n3", Some("b1"), &seal(&vault, "n3", "c"), 3);
        del.insert("deleted".into(), json!(true));
        store.apply_row("notes", &del).unwrap();

        let book = get_book(&store, "b1").unwrap().expect("live book");
        assert_eq!(book.title.as_deref(), Some("Meditations"));
        assert_eq!(book.note_count, 2);
    }

    #[test]
    fn counts_and_search_docs_skip_deleted_and_decrypt_failures() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        let foreign = Vault::generate();
        store
            .apply_row(
                "notes",
                &note_row("n1", None, &seal(&vault, "n1", "readable text"), 1),
            )
            .unwrap();
        store
            .apply_row(
                "notes",
                &note_row("n2", None, &seal(&foreign, "n2", "unreadable"), 2),
            )
            .unwrap();

        let mut idea = Map::new();
        idea.insert("id".into(), json!("i1"));
        idea.insert("name".into(), json!("Antifragility"));
        idea.insert("description".into(), json!("gains from disorder"));
        idea.insert("created_at".into(), json!(1));
        idea.insert("updated_at".into(), json!(1));
        idea.insert("deleted".into(), json!(false));
        store.apply_row("custom_ideas", &idea).unwrap();

        let c = counts(&store).unwrap();
        assert_eq!((c.notes, c.custom_ideas, c.books), (2, 1, 0));
        // active_ideas counts tags over live notes regardless of decrypt state (tags are plaintext):
        // both n1 and the decrypt-failed n2 carry the default ["philosophy", "ethics"] → 2 distinct.
        assert_eq!(c.active_ideas, 2);

        // build_search_docs drops the decrypt-failed note but keeps the readable one + the idea.
        let docs = build_search_docs(&store, &vault).unwrap();
        assert_eq!(docs.len(), 2);
        assert!(docs
            .iter()
            .any(|d| d.ref_id == "n1" && d.content.contains("readable")));
        assert!(docs
            .iter()
            .any(|d| d.ref_id == "i1" && d.title == "Antifragility"));
        assert!(
            !docs.iter().any(|d| d.ref_id == "n2"),
            "decrypt failure excluded from index"
        );
    }

    #[test]
    fn active_ideas_counts_distinct_tags_over_live_notes() {
        // The PWA's activeIdeasCount: distinct tag names on ≥1 live note. Overlap collapses,
        // within-note duplicates collapse, untagged notes contribute nothing, deleted notes drop.
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        let seal = |id: &str| seal(&vault, id, "t");
        store
            .apply_row(
                "notes",
                &note_row_tagged("n1", &seal("n1"), 1, &["philosophy", "ethics"]),
            )
            .unwrap();
        store
            .apply_row(
                "notes",
                &note_row_tagged("n2", &seal("n2"), 2, &["ethics", "stoicism"]),
            )
            .unwrap(); // "ethics" overlaps n1
        store
            .apply_row("notes", &note_row_tagged("n3", &seal("n3"), 3, &[]))
            .unwrap(); // untagged → contributes nothing
        store
            .apply_row(
                "notes",
                &note_row_tagged("n4", &seal("n4"), 4, &["logic", "logic"]),
            )
            .unwrap(); // duplicate within one note → counted once
        let mut del = note_row_tagged("n5", &seal("n5"), 5, &["ghost"]);
        del.insert("deleted".into(), json!(true));
        store.apply_row("notes", &del).unwrap(); // deleted → its tag doesn't count

        // distinct live tags: philosophy, ethics, stoicism, logic = 4 ("ghost" excluded).
        assert_eq!(counts(&store).unwrap().active_ideas, 4);
    }

    #[test]
    fn notes_this_week_matches_the_pwa_window_and_text_filter() {
        // AC: byte-match the oracle — rolling 168h on created_at (inclusive lower bound),
        // decrypt-in-core, empty/whitespace text and decrypt failures excluded.
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        let foreign = Vault::generate();
        let now = 1_700_000_000_000i64;
        let seed = |id: &str, txt: &str, ts: i64| {
            store
                .apply_row("notes", &note_row(id, None, &seal(&vault, id, txt), ts))
                .unwrap();
        };

        seed("in", "fresh", now - 1); // inside the window, has text → counts
        seed("edge", "boundary", now - WEEK_MS); // exactly at the inclusive lower bound → counts
        seed("future", "ahead", now + 5_000); // future-dated → still >= cutoff → counts (no upper bound)
        seed("old", "stale", now - WEEK_MS - 1); // one ms too old → excluded
        seed("blank", "   \n\t ", now - 2); // whitespace-only decrypted text → excluded
        store
            .apply_row("notes", &note_row("empty", None, "", now - 3))
            .unwrap(); // stored empty text → excluded
        store
            .apply_row(
                "notes",
                &note_row("foreign", None, &seal(&foreign, "foreign", "nope"), now - 4),
            )
            .unwrap(); // in-window but decrypt-fails → excluded

        assert_eq!(notes_this_week(&store, &vault, now).unwrap(), 3); // in, edge, future
    }

    #[test]
    fn recent_note_picks_deterministically_from_the_fresh_set() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        let now = 1_700_000_000_000i64;

        // empty store → None (the PWA hides the card when nothing is fresh).
        assert!(recent_note(&store, &vault, now, 0).unwrap().is_none());

        // A NEWER text-less note must never be the pick (the set is has-text only).
        store
            .apply_row("notes", &note_row("blank", None, "", now - 1))
            .unwrap();
        store
            .apply_row(
                "notes",
                &note_row("a", None, &seal(&vault, "a", "alpha"), now - 10),
            )
            .unwrap();
        store
            .apply_row(
                "notes",
                &note_row("b", None, &seal(&vault, "b", "beta"), now - 20),
            )
            .unwrap();

        // fresh = [a, b] (created_at DESC, "blank" filtered out). seed indexes deterministically.
        let pick0 = recent_note(&store, &vault, now, 0).unwrap().unwrap();
        assert_eq!(pick0.id, "a"); // 0 % 2 = 0
        assert_eq!(
            recent_note(&store, &vault, now, 1).unwrap().unwrap().id,
            "b"
        ); // 1 % 2 = 1
        assert_eq!(
            recent_note(&store, &vault, now, 2).unwrap().unwrap().id,
            "a"
        ); // wraps
           // decrypt-in-core: the pick's text is plaintext, never a ciphertext sentinel.
        assert_eq!(pick0.text.as_deref(), Some("alpha"));
        assert!(!pick0.text.unwrap().starts_with("enc:v"));
        for s in 0..6 {
            assert_ne!(
                recent_note(&store, &vault, now, s).unwrap().unwrap().id,
                "blank"
            );
        }
    }

    // ── SUR-858: organise reads ──────────────────────────────────────────────

    #[test]
    fn notes_by_idea_filters_newest_first_excludes_deleted_and_paginates() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        let seal = |id: &str| seal(&vault, id, "t");
        // three live "philosophy" notes + one other-tag + one deleted "philosophy".
        store
            .apply_row(
                "notes",
                &note_row_tagged("p1", &seal("p1"), 100, &["philosophy"]),
            )
            .unwrap();
        store
            .apply_row(
                "notes",
                &note_row_tagged("p2", &seal("p2"), 300, &["philosophy", "ethics"]),
            )
            .unwrap();
        store
            .apply_row(
                "notes",
                &note_row_tagged("p3", &seal("p3"), 200, &["philosophy"]),
            )
            .unwrap();
        store
            .apply_row(
                "notes",
                &note_row_tagged("other", &seal("other"), 400, &["ethics"]),
            )
            .unwrap();
        let mut del = note_row_tagged("pdel", &seal("pdel"), 500, &["philosophy"]);
        del.insert("deleted".into(), json!(true));
        store.apply_row("notes", &del).unwrap();

        // newest-first, deleted excluded, only the tag's notes.
        let all = notes_by_idea(&store, &vault, "philosophy", 50, 0).unwrap();
        assert_eq!(
            all.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            vec!["p2", "p3", "p1"]
        );
        // pagination: limit/offset slice the newest-first set.
        let page = notes_by_idea(&store, &vault, "philosophy", 1, 1).unwrap();
        assert_eq!(
            page.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            vec!["p3"]
        );
        // a tag no note carries → empty.
        assert!(notes_by_idea(&store, &vault, "stoicism", 50, 0)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn notes_by_idea_decrypts_in_core_without_failing_the_page() {
        // A foreign-sealed matching note surfaces decrypt_failed=true, text=None, but stays in the page.
        let store = Store::open_in_memory().unwrap();
        let mine = Vault::generate();
        let foreign = Vault::generate();
        store
            .apply_row(
                "notes",
                &note_row_tagged(
                    "good",
                    &seal(&mine, "good", "readable"),
                    200,
                    &["philosophy"],
                ),
            )
            .unwrap();
        store
            .apply_row(
                "notes",
                &note_row_tagged("bad", &seal(&foreign, "bad", "nope"), 100, &["philosophy"]),
            )
            .unwrap();

        let notes = notes_by_idea(&store, &mine, "philosophy", 50, 0).unwrap();
        assert_eq!(notes.len(), 2);
        let good = notes.iter().find(|n| n.id == "good").unwrap();
        assert_eq!(good.text.as_deref(), Some("readable"));
        assert!(!good.text.clone().unwrap().starts_with("enc:v"));
        let bad = notes.iter().find(|n| n.id == "bad").unwrap();
        assert!(bad.text.is_none());
        assert!(bad.decrypt_failed);
    }

    #[test]
    fn idea_counts_matches_the_pwa_ideacountsfor_tally() {
        // Byte-match `ideaCountsFor`: increment per tag OCCURRENCE (no within-note dedup), skip
        // untagged, drop deleted. Sorted idea-asc; keys line up with active_ideas.
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        let seal = |id: &str| seal(&vault, id, "t");
        store
            .apply_row(
                "notes",
                &note_row_tagged("n1", &seal("n1"), 1, &["philosophy", "ethics"]),
            )
            .unwrap();
        store
            .apply_row(
                "notes",
                &note_row_tagged("n2", &seal("n2"), 2, &["ethics", "stoicism"]),
            )
            .unwrap();
        store
            .apply_row("notes", &note_row_tagged("n3", &seal("n3"), 3, &[]))
            .unwrap(); // untagged → contributes nothing
        store
            .apply_row(
                "notes",
                &note_row_tagged("n4", &seal("n4"), 4, &["logic", "logic"]),
            )
            .unwrap(); // within-note dup → counts TWICE (oracle parity)
        let mut del = note_row_tagged("n5", &seal("n5"), 5, &["ghost"]);
        del.insert("deleted".into(), json!(true));
        store.apply_row("notes", &del).unwrap(); // deleted → excluded

        let counts = idea_counts(&store).unwrap();
        let as_pairs: Vec<(&str, u32)> =
            counts.iter().map(|c| (c.idea.as_str(), c.count)).collect();
        assert_eq!(
            as_pairs,
            vec![
                ("ethics", 2),
                ("logic", 2),
                ("philosophy", 1),
                ("stoicism", 1)
            ],
            "idea-asc order, per-occurrence counts, ghost excluded"
        );
        // key count == active_ideas / StoreCounts.active_ideas.
        assert_eq!(counts.len() as u32, counts_active_ideas(&store));
    }

    // active_ideas is private; reach it via the public StoreCounts for the cross-check above.
    fn counts_active_ideas(store: &Store) -> u32 {
        counts(store).unwrap().active_ideas
    }

    #[test]
    fn untagged_notes_and_count_exclude_tagged_and_deleted() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        let seal = |id: &str| seal(&vault, id, "t");
        store
            .apply_row("notes", &note_row_tagged("u1", &seal("u1"), 100, &[]))
            .unwrap();
        store
            .apply_row("notes", &note_row_tagged("u2", &seal("u2"), 300, &[]))
            .unwrap();
        store
            .apply_row(
                "notes",
                &note_row_tagged("tagged", &seal("tagged"), 200, &["philosophy"]),
            )
            .unwrap();
        let mut del = note_row_tagged("udel", &seal("udel"), 400, &[]);
        del.insert("deleted".into(), json!(true));
        store.apply_row("notes", &del).unwrap();

        // newest-first, only untagged live notes; text decrypted in core.
        let notes = untagged_notes(&store, &vault, 50, 0).unwrap();
        assert_eq!(
            notes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            vec!["u2", "u1"]
        );
        assert_eq!(notes[0].text.as_deref(), Some("t"));
        // count is the full untagged set, not a page.
        assert_eq!(untagged_notes_count(&store).unwrap(), 2);
        // pagination slices the same order.
        assert_eq!(
            untagged_notes(&store, &vault, 1, 1)
                .unwrap()
                .iter()
                .map(|n| n.id.as_str())
                .collect::<Vec<_>>(),
            vec!["u1"]
        );
    }

    #[test]
    fn list_collections_and_lenses_exclude_deleted_and_map_fields() {
        let store = Store::open_in_memory().unwrap();

        let mut c1 = Map::new();
        c1.insert("id".into(), json!("c1"));
        c1.insert("name".into(), json!("Reading list"));
        c1.insert("created_at".into(), json!(100));
        c1.insert("updated_at".into(), json!(150));
        c1.insert("deleted".into(), json!(false));
        store.apply_row("collections", &c1).unwrap();
        let mut cdel = c1.clone();
        cdel.insert("id".into(), json!("cdel"));
        cdel.insert("created_at".into(), json!(200));
        cdel.insert("deleted".into(), json!(true));
        store.apply_row("collections", &cdel).unwrap();

        let cols = list_collections(&store, 50, 0).unwrap();
        assert_eq!(
            cols.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["c1"]
        );
        assert_eq!(cols[0].name.as_deref(), Some("Reading list"));
        assert_eq!((cols[0].created_at, cols[0].updated_at), (100, 150));

        let mut l1 = Map::new();
        l1.insert("id".into(), json!("l1"));
        l1.insert("name".into(), json!("Stoic core"));
        l1.insert("leaf_ids".into(), json!(["philosophy", "ethics"]));
        l1.insert("combinator".into(), json!("OR"));
        l1.insert("threshold".into(), json!(75));
        l1.insert("created_at".into(), json!(10));
        l1.insert("updated_at".into(), json!(20));
        l1.insert("deleted".into(), json!(false));
        store.apply_row("lenses", &l1).unwrap();

        let lenses = list_lenses(&store, 50, 0).unwrap();
        assert_eq!(lenses.len(), 1);
        let l = &lenses[0];
        assert_eq!(l.name.as_deref(), Some("Stoic core"));
        assert_eq!(l.leaf_ids, vec!["philosophy", "ethics"]);
        assert_eq!(l.combinator.as_deref(), Some("OR"));
        assert_eq!(l.threshold, Some(75));

        // empty stores → empty vecs (no panic).
        let empty = Store::open_in_memory().unwrap();
        assert!(list_collections(&empty, 50, 0).unwrap().is_empty());
        assert!(list_lenses(&empty, 50, 0).unwrap().is_empty());
    }

    // ── SUR-923: relation reads ──────────────────────────────────────────────

    fn membership_row(collection_id: &str, note_id: &str, created_at: i64) -> Map<String, Value> {
        let mut r = Map::new();
        r.insert(
            "id".into(),
            json!(crate::store::membership_id(collection_id, note_id)),
        );
        r.insert("collection_id".into(), json!(collection_id));
        r.insert("note_id".into(), json!(note_id));
        r.insert("created_at".into(), json!(created_at));
        r.insert("updated_at".into(), json!(created_at));
        r.insert("deleted".into(), json!(false));
        r
    }

    fn link_row(id: &str, from: &str, to: &str, created_at: i64) -> Map<String, Value> {
        let mut r = Map::new();
        r.insert("id".into(), json!(id));
        r.insert("from_note_id".into(), json!(from));
        r.insert("to_note_id".into(), json!(to));
        r.insert("relation_type".into(), json!("handwritten_annotation"));
        r.insert("created_at".into(), json!(created_at));
        r.insert("updated_at".into(), json!(created_at));
        r.insert("deleted".into(), json!(false));
        r
    }

    #[test]
    fn collection_ids_for_note_mirrors_the_member_ids_oracle() {
        // PWA memberIds: live MEMBERSHIP rows only — a deleted membership is out, but there is
        // no collection-liveness check (a membership into a dead/never-pulled collection still
        // appears; the sheet's own live-collections filter is what hides it). Note that no
        // `collections` rows are inserted at all — the read must never look at that table.
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row("collection_memberships", &membership_row("c1", "n1", 100))
            .unwrap();
        let mut mdel = membership_row("c2", "n1", 200);
        mdel.insert("deleted".into(), json!(true));
        store.apply_row("collection_memberships", &mdel).unwrap(); // deleted membership → out
        store
            .apply_row(
                "collection_memberships",
                &membership_row("c1", "other", 300),
            )
            .unwrap(); // other note → out
        store
            .apply_row(
                "collection_memberships",
                &membership_row("cdead", "n1", 400),
            )
            .unwrap(); // collection has no row anywhere → still IN (oracle fidelity)
        let mut dup = membership_row("c1", "n1", 500);
        dup.insert("id".into(), json!("rogue-random-pk"));
        store.apply_row("collection_memberships", &dup).unwrap(); // foreign dup pair → deduped

        // store scan order: created_at DESC — the rogue dup (500) takes c1's slot, then dedup.
        assert_eq!(
            collection_ids_for_note(&store, "n1").unwrap(),
            vec!["c1", "cdead"]
        );
        assert!(collection_ids_for_note(&store, "unknown")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn note_links_for_note_returns_both_directions_live_only() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row("note_links", &link_row("e1", "parent", "n1", 100))
            .unwrap(); // n1 is the child (to-side)
        store
            .apply_row("note_links", &link_row("e2", "n1", "child", 200))
            .unwrap(); // n1 is the parent (from-side)
        store
            .apply_row("note_links", &link_row("e3", "a", "b", 300))
            .unwrap(); // unrelated → out
        let mut edel = link_row("e4", "n1", "gone", 400);
        edel.insert("deleted".into(), json!(true));
        store.apply_row("note_links", &edel).unwrap(); // deleted → out

        let links = note_links_for_note(&store, "n1").unwrap();
        assert_eq!(
            links.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
            vec!["e2", "e1"], // created_at DESC
        );
        let e1 = links.iter().find(|l| l.id == "e1").unwrap();
        assert_eq!(
            (e1.from_note_id.as_str(), e1.to_note_id.as_str()),
            ("parent", "n1")
        );
        assert_eq!(e1.relation_type.as_deref(), Some("handwritten_annotation"));
        assert_eq!((e1.created_at, e1.updated_at), (100, 100));
    }

    #[test]
    fn note_ids_for_collection_stays_join_free_for_the_cascade() {
        // The host-side collection-delete cascade tombstones every LIVE membership row —
        // including one whose note is already soft-deleted. A notes join here would hide that
        // row and the cascade would leave it live forever; the PWA's removeCollection sees it,
        // so must we. (The scoped note LIST re-checks note liveness host-side instead.)
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        store
            .apply_row(
                "notes",
                &note_row("nlive", None, &seal(&vault, "nlive", "t"), 10),
            )
            .unwrap();
        let mut ndel = note_row("ndead", None, &seal(&vault, "ndead", "t"), 20);
        ndel.insert("deleted".into(), json!(true));
        store.apply_row("notes", &ndel).unwrap();

        store
            .apply_row(
                "collection_memberships",
                &membership_row("c1", "nlive", 100),
            )
            .unwrap();
        store
            .apply_row(
                "collection_memberships",
                &membership_row("c1", "ndead", 200),
            )
            .unwrap(); // note dead, membership LIVE → in
        let mut mdel = membership_row("c1", "mgone", 300);
        mdel.insert("deleted".into(), json!(true));
        store.apply_row("collection_memberships", &mdel).unwrap(); // membership dead → out
        store
            .apply_row(
                "collection_memberships",
                &membership_row("c2", "nlive", 400),
            )
            .unwrap(); // other collection → out
        let mut dup = membership_row("c1", "nlive", 500);
        dup.insert("id".into(), json!("rogue-random-pk"));
        store.apply_row("collection_memberships", &dup).unwrap(); // foreign duplicate pair → deduped

        // created_at DESC; the rogue duplicate (500) wins the first "nlive" slot, then dedup.
        assert_eq!(
            note_ids_for_collection(&store, "c1").unwrap(),
            vec!["nlive", "ndead"]
        );
    }

    #[test]
    fn collection_note_counts_joins_live_notes_by_founder_decision() {
        // Founder decision (2026-07-17): count only memberships whose note is present AND live —
        // a recorded divergence from the PWA's raw `noteCountByCollection` tally, so the subtitle
        // agrees with the collection-scoped note list (`notesInCollection`, which joins live
        // notes). No `collections` rows are inserted: the tally never reads that table.
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        store
            .apply_row("notes", &note_row("n1", None, &seal(&vault, "n1", "t"), 10))
            .unwrap();
        store
            .apply_row("notes", &note_row("n2", None, &seal(&vault, "n2", "t"), 20))
            .unwrap();
        let mut ndel = note_row("ndead", None, &seal(&vault, "ndead", "t"), 30);
        ndel.insert("deleted".into(), json!(true));
        store.apply_row("notes", &ndel).unwrap();

        // beta: two live notes + a dead-note membership + a duplicate pair → 2.
        store
            .apply_row("collection_memberships", &membership_row("beta", "n1", 100))
            .unwrap();
        store
            .apply_row("collection_memberships", &membership_row("beta", "n2", 200))
            .unwrap();
        store
            .apply_row(
                "collection_memberships",
                &membership_row("beta", "ndead", 300),
            )
            .unwrap();
        let mut dup = membership_row("beta", "n1", 400);
        dup.insert("id".into(), json!("rogue"));
        store.apply_row("collection_memberships", &dup).unwrap();
        // alpha: one live note; a deleted membership and a never-pulled note contribute nothing.
        store
            .apply_row(
                "collection_memberships",
                &membership_row("alpha", "n1", 500),
            )
            .unwrap();
        let mut mdel = membership_row("alpha", "n2", 600);
        mdel.insert("deleted".into(), json!(true));
        store.apply_row("collection_memberships", &mdel).unwrap();
        store
            .apply_row(
                "collection_memberships",
                &membership_row("alpha", "never-pulled", 700),
            )
            .unwrap();
        // ghost: only a dead-note membership → absent entirely (count ≥ 1 rule).
        store
            .apply_row(
                "collection_memberships",
                &membership_row("ghost", "ndead", 800),
            )
            .unwrap();

        let tally = collection_note_counts(&store).unwrap();
        let as_pairs: Vec<(&str, u32)> = tally
            .iter()
            .map(|c| (c.collection_id.as_str(), c.count))
            .collect();
        assert_eq!(
            as_pairs,
            vec![("alpha", 1), ("beta", 2)],
            "collection-id asc; dead/absent notes and dead memberships excluded; pair deduped"
        );
    }
}
