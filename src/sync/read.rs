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

use std::collections::HashSet;

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

pub fn counts(store: &Store) -> rusqlite::Result<StoreCounts> {
    Ok(StoreCounts {
        books: store.count_live("books", None)? as u32,
        notes: store.count_live("notes", None)? as u32,
        custom_ideas: store.count_live("custom_ideas", None)? as u32,
        active_ideas: active_ideas(store)?,
    })
}

/// Distinct idea tags across all live notes — the PWA Home's `activeIdeasCount`. The oracle is
/// `ideaCountsFor(notes)` (a `{tag: count}` tally) filtered to `count > 0`; since a key is only
/// ever created by an increment, that filter is a no-op, so the result is exactly the count of
/// distinct tag names appearing on ≥1 live note. Tags are a plaintext `Json` column, so this scans
/// without touching the `Vault`. ponytail: full `tags` scan; a tag index only if note counts ever
/// make it matter.
fn active_ideas(store: &Store) -> rusqlite::Result<u32> {
    let mut seen: HashSet<String> = HashSet::new();
    for row in store.list_live("notes", None, -1, 0)? {
        seen.extend(string_array_field(&row, "tags"));
    }
    Ok(seen.len() as u32)
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
}
