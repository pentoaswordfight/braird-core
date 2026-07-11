//! Post-pull reconciliation (SUR-820, extended by SUR-828). The passes that run after a successful
//! [`super::pull`], promoting into the core the post-sync behaviors the PWA's `fetchAllCloud`
//! orchestration runs in `src/hooks/useAuth.js` (steps 2b/2c/2d) plus its book-cover resolution —
//! excluded from the core at SUR-659, briefly re-homed to Android at SUR-768, and promoted here
//! because they mutate synced data every client reads (the SUR-812 lesson: state logic reinvented
//! per host goes wrong) and would otherwise need a whole-corpus scan over the paged FFI app-side.
//! Image download/cache (the PWA's step 3) stays host-side (SUR-768/SUR-821) — out of scope here.
//!
//! 1. **`reconcile_books`** (`useAuth.js` step 2b) — a live note's `book_id` referencing a book
//!    absent from the local store is backfilled by fetching it from the server. This is a pure
//!    read-repair of data that already exists server-side (mirrors the oracle's
//!    `mergeCloudRecords`) — no local mutation is created, so nothing is staged to the outbox.
//! 2. **`reconcile_stranded_notes`** (`useAuth.js` step 2c, `rehomeStrandedNotes` in `db.js`) — a
//!    live note pointing at a book that IS present locally but soft-deleted (an offline
//!    book-merge survivor a device didn't itself perform) is repointed to the merge survivor if
//!    the local `mergedBookIds` map (persisted in `meta`, mirroring the PWA's device-local merge
//!    map) knows one, else detached (`book_id` → null). Only a real rehome-to-survivor is a
//!    genuine mutation other clients must learn about (staged via [`super::mod::stage_local_write`]
//!    equivalent below); a map-less detach stays local-only, exactly mirroring the oracle's
//!    documented LWW-safety rule (`useAuth.js`: "letting it win the LWW race would overwrite the
//!    survivor truth a map-holding device is converging toward").
//! 3. **`reconcile_dropped_tags`** (`useAuth.js` step 2d, `preserveDroppedTagsAsCustom` in
//!    `db.js`) — GENERALIZED past the oracle's static 26-name `DROPPED_LEAVES` set (founder
//!    decision, SUR-820 decomposition): any live note tag that matches neither the vendored
//!    canon (`GREAT_IDEAS`, `vendored/canon/great-ideas.json`) nor an existing local custom idea
//!    (case-insensitive) becomes a custom idea, so a tag orphaned by a FUTURE canon revision is
//!    caught the same way, not just the historical v14 swap. The id format
//!    (`cidea_sur597_{userId}_{slug}`) is kept byte-identical to the oracle's
//!    `preservedCustomIdeaId` for every orphaned tag (not just the 26 classical names), so a
//!    core-created row converges with one the PWA already created for the same user+tag.
//! 4. **`reconcile_covers`** (SUR-828; `resolveCover` in `surfc/src/lib/coverResolver.js`) —
//!    a coverless book gets its cover resolved via Open Library (ISBN → a deterministic
//!    `covers.openlibrary.org` URL by pure construction, no egress; no-ISBN → the Search API for a
//!    `cover_i`/healed ISBN), persisting `cover_url` + `cover_source` + `cover_resolved_at` through
//!    the outbox. **⚠ The core's first non-Supabase egress** — gated by the SUR-492
//!    `openlibrary_egress` kill-switch (read through the Supabase client), paced (≤10 searches per
//!    pass), and fail-soft (a miss stamps `cover_resolved_at` so it never re-queries UNTIL the book
//!    is edited — a metadata change bumps `updated_at` past the stamp and re-opens it, mirroring
//!    the PWA's edit-time re-resolution; an outage leaves it unstamped to retry).
//!
//! **Error handling (deliberately asymmetric, mirroring the oracle):** the oracle does NOT wrap
//! steps 2b/2c in a try/catch (an error there aborts the whole `fetchAllCloud` call), but DOES
//! wrap step 2d ("Best-effort: a failure must never block the sync"). [`reconcile`] mirrors that
//! shape internally — a `reconcile_dropped_tags` or `reconcile_covers` failure is caught and logged
//! here, never propagated. Whatever `reconcile` itself returns is, in turn, treated as best-effort by ITS
//! callers ([`super::SyncEngine::pull`], [`super::pull_then_flush`]) — a reconciliation hiccup
//! (e.g. a network blip fetching a missing book) must never discard an otherwise-successful
//! pull, so those call sites log-and-zero rather than fail the whole `pull()`/`sync()`. This is a
//! deliberate strengthening past the oracle's stricter (non-try-caught) 2b/2c behavior: the
//! ticket's own framing ("idempotent"; "so hosts can't forget it") is a reliability guarantee,
//! not a fragility one — flagged for `sync-reviewer` to confirm.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::{json, Map, Value};

use super::epoch_ms;
use super::http::{CoverEgress, PostgrestSink};
use super::outbox::resolve_book_id;
use crate::store::Store;

/// The vendored canon (SUR-820 Canon-102 awareness) — baked into the binary at compile time
/// (the vendored file does not exist on a host's filesystem at runtime). Drift-guarded against
/// `surfc/main` by `.github/workflows/canon-drift.yml` / `scripts/extract-great-ideas.mjs`.
const GREAT_IDEAS_JSON: &str = include_str!("../../vendored/canon/great-ideas.json");

/// Counts from one reconciliation pass — the internal (`usize`) shape; [`super::ReconcileSummary`]
/// is its `u32` FFI mirror, following the same `PullResult`/`PullSummary` split as [`super::pull`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileResult {
    pub books_backfilled: usize,
    pub notes_rehomed: usize,
    pub notes_detached: usize,
    pub ideas_created: usize,
    pub covers_resolved: usize,
}

/// The `app_config` key (SUR-492, migration 0038) whose `{"enabled": bool}` value is the global
/// Open Library egress kill-switch. GLOBAL, service-role-write / client-read.
const OPENLIBRARY_EGRESS_KEY: &str = "openlibrary_egress";

/// Max Search-API resolutions per pass (polite-use pacing, founder decision SUR-828). ISBN books
/// resolve by pure URL construction (no egress) and DON'T count against this — only the no-ISBN
/// Search-API path does; the rest wait for the next pull.
const COVER_SEARCH_BUDGET_PER_PASS: usize = 10;

/// Open Library cover-image base, mirroring the PWA's `coverResolver.js` (`COVERS_BASE` + `SIZE`).
const COVERS_BASE: &str = "https://covers.openlibrary.org/b";

/// `meta` key holding the device-local offline-merge survivor map (loser→survivor book id, JSON
/// object) — the core mirror of the PWA's `db.meta.get('mergedBookIds')`. **Write-less by design
/// (SUR-820 founder decision):** no host feature populates this yet (braird-core has no "merge
/// duplicate books" UI), so the map is always `{}` outside a test that seeds it directly via
/// `store.meta_set`. Distinct from `bookIdRemap` ([`super::push`]) — that one is the
/// temp-id→server-id map for offline-created books, a different concept entirely.
const MERGED_BOOK_IDS_KEY: &str = "mergedBookIds";

/// Run the full post-pull reconciliation pass. Order: books-backfill first (so a book fetched
/// this pass is visible to the stranded-notes check that follows), then stranded-notes, then
/// dropped-tags, then cover-resolution (independent of the others). `user_id` is the token's `sub`
/// — needed only for the dropped-tag pass's user-scoped custom-idea id.
pub async fn reconcile<S: PostgrestSink + CoverEgress>(
    store: &Store,
    sink: &S,
    user_id: &str,
) -> Result<ReconcileResult, String> {
    let books_backfilled = reconcile_books(store, sink).await?;
    let (notes_rehomed, notes_detached) = reconcile_stranded_notes(store)?;
    // Best-effort (mirrors the oracle's explicit try/catch around `preserveDroppedTagsAsCustom`):
    // a failure here must never block the rest of reconciliation or the pull it follows.
    let ideas_created = reconcile_dropped_tags(store, user_id).unwrap_or_else(|e| {
        eprintln!("reconcile: dropped-tag pass failed (non-fatal, retries next pull): {e}");
        0
    });
    // Best-effort, same posture: an Open Library outage (or a kill-switch read blip) must never
    // fail the pull it follows — cover resolution simply retries next pull.
    let covers_resolved = reconcile_covers(store, sink).await.unwrap_or_else(|e| {
        eprintln!("reconcile: cover-resolution pass failed (non-fatal, retries next pull): {e}");
        0
    });
    Ok(ReconcileResult {
        books_backfilled,
        notes_rehomed,
        notes_detached,
        ideas_created,
        covers_resolved,
    })
}

/// Step 2b — backfill a book referenced by a live note but absent from the local store, by
/// batch-fetching the distinct missing ids from the server. Pure read-repair (mirrors
/// `mergeCloudRecords`): the fetched rows are applied directly, never staged to the outbox — they
/// are not a new local fact, just a read-gap fill of data the server already has.
async fn reconcile_books<S: PostgrestSink>(store: &Store, sink: &S) -> Result<usize, String> {
    let notes = store
        .list_live("notes", None, -1, 0)
        .map_err(|e| format!("list notes: {e}"))?;

    // Per-row isolation (mirrors `pull.rs`'s per-table isolation / `push.rs`'s per-group
    // isolation): one unreadable local row must not abort the whole scan, so it's logged and
    // skipped rather than propagated via `?`. Only a genuinely whole-batch failure (the fetch
    // itself) aborts the function — there's nothing left to iterate over if it does.
    let mut missing_ids = BTreeSet::new();
    for row in &notes {
        let Some(book_id) = row.get("book_id").and_then(Value::as_str) else {
            continue;
        };
        match store.get_row("books", book_id) {
            Ok(None) => {
                missing_ids.insert(book_id.to_string());
            }
            Ok(Some(_)) => {}
            Err(e) => eprintln!("reconcile_books: get book {book_id} failed, skipping: {e}"),
        }
    }
    if missing_ids.is_empty() {
        return Ok(0);
    }

    let ids: Vec<String> = missing_ids.into_iter().collect();
    let fetched = sink.fetch_by_ids("books", &ids).await?;
    let mut backfilled = 0;
    for row in &fetched {
        let Some(obj) = row.as_object() else { continue };
        match store.apply_row("books", obj) {
            Ok(()) => backfilled += 1,
            Err(e) => eprintln!("reconcile_books: apply backfilled book failed, skipping: {e}"),
        }
    }
    Ok(backfilled)
}

/// Step 2c — repair a live note pointing at a book that's present locally but soft-deleted (an
/// offline book-merge this device didn't itself perform). Resolves via the local
/// `mergedBookIds` survivor map (reusing [`resolve_book_id`]'s hop-capped walk — the same shape
/// `push.rs` already uses for the unrelated `bookIdRemap`), repointing to a known survivor and
/// pushing that correction, or detaching to `null` locally-only when no survivor is known.
/// Returns `(rehomed, detached)`.
fn reconcile_stranded_notes(store: &Store) -> Result<(usize, usize), String> {
    let merged_book_ids = load_merged_book_ids(store)?;
    let notes = store
        .list_live("notes", None, -1, 0)
        .map_err(|e| format!("list notes: {e}"))?;

    // Per-note isolation (see the matching comment in `reconcile_books`): a failure repairing
    // one stranded note must not abort the pass for every other note.
    let mut rehomed = 0;
    let mut detached = 0;
    for row in &notes {
        let Some(book_id) = row.get("book_id").and_then(Value::as_str) else {
            continue; // no book reference — nothing to strand
        };
        let book = match store.get_row("books", book_id) {
            Ok(Some(b)) => b,
            Ok(None) => continue, // absent locally — reconcile_books' problem, not this pass's
            Err(e) => {
                eprintln!("reconcile_stranded_notes: get book {book_id} failed, skipping: {e}");
                continue;
            }
        };
        let book_is_deleted = matches!(book.get("deleted"), Some(Value::Bool(true)));
        if !book_is_deleted {
            continue; // live book — nothing stranded
        }

        let note_id = row
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let survivor = resolve_book_id(book_id, &merged_book_ids);
        if survivor != book_id {
            // A known survivor — a genuine mutation the fleet must converge on, so it goes
            // through the normal write path (bump updated_at, enter the outbox).
            let mut patch = Map::new();
            patch.insert("id".into(), json!(note_id));
            patch.insert("book_id".into(), json!(survivor));
            // SUR-638 (mirrored): book_id changed → content_tag is stale; null it for
            // re-derive on the note's next edit (the same stale-tag edge `enqueue_note`
            // already documents for the temp-id remap case).
            patch.insert("content_tag".into(), Value::Null);
            patch.insert("updated_at".into(), json!(epoch_ms()));
            match store.stage_local_write("notes", &note_id, patch, epoch_ms()) {
                Ok(()) => rehomed += 1,
                Err(e) => eprintln!(
                    "reconcile_stranded_notes: stage rehomed note {note_id} failed, skipping: {e}"
                ),
            }
        } else {
            // No known survivor — detach locally only, NEVER pushed. Mirrors the oracle's
            // explicit LWW-safety rule exactly: propagating a map-less detach could overwrite
            // the survivor truth a map-holding device is still converging toward.
            let mut detached_row = row.clone();
            detached_row.insert("book_id".into(), Value::Null);
            detached_row.insert("content_tag".into(), Value::Null);
            match store.apply_row("notes", &detached_row) {
                Ok(()) => detached += 1,
                Err(e) => eprintln!(
                    "reconcile_stranded_notes: detach note {note_id} failed, skipping: {e}"
                ),
            }
        }
    }
    Ok((rehomed, detached))
}

fn load_merged_book_ids(store: &Store) -> Result<BTreeMap<String, String>, String> {
    match store
        .meta_get(MERGED_BOOK_IDS_KEY)
        .map_err(|e| format!("read merged book ids: {e}"))?
    {
        Some(json) => {
            serde_json::from_str(&json).map_err(|e| format!("parse merged book ids: {e}"))
        }
        None => Ok(BTreeMap::new()),
    }
}

/// Step 2d, generalized (founder decision, SUR-820 decomposition) — any live note tag that
/// matches neither the vendored canon nor an existing local custom idea (case-insensitive)
/// becomes a custom idea. Idempotent: a name already present (as canon, an existing custom idea,
/// or one created earlier in this same pass) is skipped.
fn reconcile_dropped_tags(store: &Store, user_id: &str) -> Result<usize, String> {
    let canon = great_ideas_lowercase();
    let mut known_names: HashSet<String> = store
        .list_live("custom_ideas", None, -1, 0)
        .map_err(|e| format!("list custom ideas: {e}"))?
        .iter()
        .filter_map(|r| r.get("name").and_then(Value::as_str).map(str::to_lowercase))
        .collect();

    let notes = store
        .list_live("notes", None, -1, 0)
        .map_err(|e| format!("list notes: {e}"))?;

    let mut created = 0;
    for row in &notes {
        let Some(tags) = row.get("tags").and_then(Value::as_array) else {
            continue;
        };
        for tag in tags {
            let Some(name) = tag.as_str() else { continue };
            let lower = name.to_lowercase();
            if canon.contains(&lower) || known_names.contains(&lower) {
                continue;
            }

            let id = preserved_custom_idea_id(user_id, name);
            // Defensive: the deterministic id may already exist under a name variant that
            // differs only by case from what `known_names` collected (e.g. a row written before
            // this pass with different casing) — never double-create the same id. Per-tag
            // isolation (see `reconcile_books`): one unreadable/unwriteable row must not abort
            // the pass for every other tag.
            let already_present = match store.get_row("custom_ideas", &id) {
                Ok(row) => row.is_some(),
                Err(e) => {
                    eprintln!("reconcile_dropped_tags: get custom idea {id} failed, skipping: {e}");
                    continue;
                }
            };
            if already_present {
                known_names.insert(lower);
                continue;
            }

            let now = epoch_ms();
            let mut idea = Map::new();
            idea.insert("id".into(), json!(id));
            idea.insert("name".into(), json!(name));
            idea.insert("description".into(), json!(""));
            idea.insert("created_at".into(), json!(now));
            idea.insert("updated_at".into(), json!(now));
            idea.insert("deleted".into(), json!(false));
            match store.stage_local_write("custom_ideas", &id, idea, now) {
                Ok(()) => {
                    known_names.insert(lower); // avoid a second create for the same name this pass
                    created += 1;
                }
                Err(e) => eprintln!(
                    "reconcile_dropped_tags: stage custom idea {id} failed, skipping: {e}"
                ),
            }
        }
    }
    Ok(created)
}

fn great_ideas_lowercase() -> HashSet<String> {
    let names: Vec<String> = serde_json::from_str(GREAT_IDEAS_JSON)
        .expect("vendored/canon/great-ideas.json must be a JSON array of strings");
    names.into_iter().map(|s| s.to_lowercase()).collect()
}

/// Byte-identical mirror of surfc's `preservedCustomIdeaId(userId, name)`
/// (`src/ideaNormalize.js`): lowercase, collapse every run of non-`[a-z0-9]` characters to one
/// `_`, trim leading/trailing `_`. Kept as the id format for EVERY orphaned tag (not just the 26
/// classical `DROPPED_LEAVES` names the oracle's static set covers) — a core-created row then
/// converges with one the PWA already created for the same user+tag, and stays consistent should
/// the user later sign into the PWA.
fn preserved_custom_idea_id(user_id: &str, name: &str) -> String {
    let slug = name
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    format!("cidea_sur597_{user_id}_{slug}")
}

/// Step 2e (SUR-828) — resolve Open Library book covers for coverless books (SUR-198 parity for
/// natively-created books, which the PWA only resolves on its own create path). Mirrors the PWA's
/// `resolveCover` (`surfc/src/lib/coverResolver.js`): a book WITH an ISBN gets a deterministic
/// `covers.openlibrary.org/b/isbn/<isbn>` URL by pure construction — **no network call**; a book
/// WITHOUT an ISBN hits the Open Library Search API for a `cover_i` (else a healed ISBN, the
/// SUR-566 self-heal). Persists `cover_url` + `cover_source='openlibrary'` + `cover_resolved_at`
/// through the outbox (LWW-safe). Returns the number of books settled this pass.
///
/// **⚠ New egress boundary (the core's first non-Supabase egress).** Three guards, all mirroring
/// the PWA:
/// - **Kill-switch (SUR-492):** read the global `openlibrary_egress` `app_config` flag through the
///   existing Supabase client; if explicitly `{"enabled": false}` skip the WHOLE pass (zero egress
///   AND no new `covers.openlibrary.org` URLs — matching the PWA's top-level gate in
///   `resolveAndPersistCover`). **Fail OPEN** on a missing row / read error / malformed value.
/// - **Pacing:** at most [`COVER_SEARCH_BUDGET_PER_PASS`] Search-API calls per pass; ISBN books are
///   free (construct-only). Over budget → leave the book unstamped so it retries next pull.
/// - **Fail-soft:** a definitive miss (searched, no cover) STAMPS `cover_resolved_at` (SUR-566 — so
///   the pass never re-hammers Open Library for the same edition); a transient outage leaves it
///   UNSTAMPED to retry. Manual (`cover_source='manual'`) covers are never touched.
async fn reconcile_covers<S: PostgrestSink + CoverEgress>(
    store: &Store,
    sink: &S,
) -> Result<usize, String> {
    if !egress_enabled(sink).await {
        return Ok(0);
    }

    let books = store
        .list_live("books", None, -1, 0)
        .map_err(|e| format!("list books: {e}"))?;

    let mut resolved = 0;
    let mut search_budget = COVER_SEARCH_BUDGET_PER_PASS;
    for book in &books {
        // A manual cover is the user's own choice — never overwritten.
        if row_str(book, "cover_source") == "manual" {
            continue;
        }
        // A book that already has a cover is left as-is.
        if book.get("cover_url").is_some_and(|v| !v.is_null()) {
            continue;
        }
        // A prior attempt stamped `cover_resolved_at`. Re-resolve ONLY if the book has been edited
        // SINCE (its `updated_at` moved past the stamp) — a metadata edit that changes the
        // cover-lookup inputs (new title/author/ISBN, all of which bump `updated_at` via
        // `enqueue_book`) must retry with the new inputs, mirroring the PWA's create/EDIT
        // re-resolution. An unchanged stamped book is skipped so the pass never re-hammers Open
        // Library (SUR-566) — reconcile's own resolve writes `cover_resolved_at == updated_at`, so
        // it stays skipped until the next real edit.
        if let Some(resolved_at) = book.get("cover_resolved_at").and_then(Value::as_i64) {
            let updated_at = book.get("updated_at").and_then(Value::as_i64).unwrap_or(0);
            if resolved_at >= updated_at {
                continue;
            }
        }

        let id = row_str(book, "id").to_string();

        let outcome = if let Some(isbn) = normalize_isbn(book.get("isbn").and_then(Value::as_str)) {
            // ISBN path: construct-only, no egress — the ISBN is normalized first, exactly as the
            // PWA's `resolveCover` does (`normalizeIsbn`). A *valid* ISBN always yields a URL;
            // `?default=false` lets the host render nothing if the edition has no cover, so there
            // is no resolve-time "miss" here.
            CoverOutcome::Hit(cover_url_from_isbn(&isbn))
        } else {
            // No ISBN (or an unparseable one) → the title/author Search API. Mirror the PWA's
            // short-circuit: a titleless book is a miss with NO egress.
            let title = row_str(book, "title").trim();
            if title.is_empty() {
                CoverOutcome::Miss
            } else if search_budget == 0 {
                continue; // over budget this pass — leave UNSTAMPED so it retries next pull.
            } else {
                search_budget -= 1;
                let author = book.get("author").and_then(Value::as_str);
                match sink.search_cover(title, author).await {
                    // Healed ISBN from the search hit is normalized too (SUR-566 self-heal parity).
                    Ok(Some(hit)) => match (hit.cover_i, normalize_isbn(hit.isbn.as_deref())) {
                        (Some(cover_i), _) => CoverOutcome::Hit(cover_url_from_cover_id(cover_i)),
                        (None, Some(healed)) => CoverOutcome::Hit(cover_url_from_isbn(&healed)),
                        (None, None) => CoverOutcome::Miss, // searched, no usable cover → definitive miss
                    },
                    Ok(None) => CoverOutcome::Miss, // no docs → definitive miss
                    Err(e) => {
                        // Transient outage: never fail the pass; leave the book unstamped to retry.
                        eprintln!("reconcile_covers: Open Library search for book {id} failed, will retry: {e}");
                        CoverOutcome::Outage
                    }
                }
            }
        };

        let now = epoch_ms();
        let mut patch = Map::new();
        patch.insert("id".into(), json!(id));
        match outcome {
            CoverOutcome::Hit(url) => {
                patch.insert("cover_url".into(), json!(url));
                patch.insert("cover_source".into(), json!("openlibrary"));
                patch.insert("cover_resolved_at".into(), json!(now));
            }
            // SUR-566: stamp even on a miss so the pass never re-queries this edition; url/source
            // stay null.
            CoverOutcome::Miss => {
                patch.insert("cover_resolved_at".into(), json!(now));
            }
            CoverOutcome::Outage => continue, // don't stamp — retry next pass
        }
        patch.insert("updated_at".into(), json!(now));
        match store.stage_local_write("books", &id, patch, now) {
            Ok(()) => resolved += 1,
            Err(e) => {
                eprintln!("reconcile_covers: stage cover for book {id} failed, skipping: {e}")
            }
        }
    }
    Ok(resolved)
}

/// Read the SUR-492 `openlibrary_egress` kill-switch through the sink's Supabase client. **Fail
/// OPEN**: a missing row, a read error, or a malformed value all resolve to `true` (enabled),
/// mirroring the PWA's `isEgressEnabled` / `fetchAppConfig` — the flag GATES the feature, it does
/// not OWN it, so a transient read failure must not silently disable covers.
async fn egress_enabled<S: PostgrestSink>(sink: &S) -> bool {
    sink.fetch_app_config(OPENLIBRARY_EGRESS_KEY)
        .await
        .unwrap_or(None)
        .as_ref()
        .and_then(|v| v.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

enum CoverOutcome {
    Hit(String),
    Miss,
    Outage,
}

fn row_str<'a>(row: &'a Map<String, Value>, key: &str) -> &'a str {
    row.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Byte-mirror of the PWA's `normalizeIsbn` (`surfc/src/lib/coverResolver.js`): strip everything
/// but digits and `X`, uppercase, then accept ONLY a valid ISBN-10 (`\d{9}[\dX]`) or ISBN-13
/// (`\d{13}`) shape — anything else (a hyphenated value, a garbage string, `None`) yields `None`,
/// and the caller falls through to the title/author search exactly as the PWA does. Load-bearing
/// for URL parity: a hyphenated or malformed `isbn` column must not construct a divergent
/// `covers.openlibrary.org` URL or a bogus "resolved" book.
fn normalize_isbn(raw: Option<&str>) -> Option<String> {
    let cleaned: String = raw?
        .chars()
        .filter(|c| c.is_ascii_digit() || c.eq_ignore_ascii_case(&'X'))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let b = cleaned.as_bytes();
    // ISBN-10: 9 digits + a check char that is a digit or 'X' (already uppercased above).
    let is_isbn10 = b.len() == 10
        && b[..9].iter().all(u8::is_ascii_digit)
        && (b[9].is_ascii_digit() || b[9] == b'X');
    // ISBN-13: 13 digits, no 'X'.
    let is_isbn13 = b.len() == 13 && b.iter().all(u8::is_ascii_digit);
    (is_isbn10 || is_isbn13).then_some(cleaned)
}

/// `covers.openlibrary.org/b/isbn/<isbn>-M.jpg?default=false` — the PWA's `coverUrlFromIsbn`.
fn cover_url_from_isbn(isbn: &str) -> String {
    format!("{COVERS_BASE}/isbn/{isbn}-M.jpg?default=false")
}

/// `covers.openlibrary.org/b/id/<cover_i>-M.jpg?default=false` — the PWA's `coverUrlFromCoverId`.
fn cover_url_from_cover_id(cover_i: i64) -> String {
    format!("{COVERS_BASE}/id/{cover_i}-M.jpg?default=false")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::http::CoverSearchHit;
    use std::cell::RefCell;

    fn block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// A minimal stub sink: canned rows returned per table for `fetch_by_ids`; `upsert`/
    /// `fetch_page` are inert (reconcile never calls them). Records every `fetch_by_ids` call so
    /// tests can assert reconcile_books didn't fetch when nothing was missing.
    /// Per-title Open Library stub outcome (SUR-828): a found hit or a simulated transient outage.
    /// A title with no entry defaults to `Ok(None)` — the "no result / definitive miss" case.
    enum StubCover {
        Hit(CoverSearchHit),
        Outage,
    }

    struct StubSink {
        by_ids: std::collections::HashMap<String, Vec<Value>>,
        calls: RefCell<Vec<(String, Vec<String>)>>,
        // SUR-828 cover-resolution stubbing:
        app_config: std::collections::HashMap<String, Value>,
        cover_by_title: std::collections::HashMap<String, StubCover>,
        searches: RefCell<Vec<String>>, // titles searched — assert egress happened (or didn't)
    }
    impl StubSink {
        fn new() -> Self {
            Self {
                by_ids: std::collections::HashMap::new(),
                calls: RefCell::new(Vec::new()),
                app_config: std::collections::HashMap::new(),
                cover_by_title: std::collections::HashMap::new(),
                searches: RefCell::new(Vec::new()),
            }
        }
        fn with(mut self, table: &str, rows: Vec<Value>) -> Self {
            self.by_ids.insert(table.to_string(), rows);
            self
        }
        fn with_app_config(mut self, key: &str, value: Value) -> Self {
            self.app_config.insert(key.to_string(), value);
            self
        }
        fn with_cover(mut self, title: &str, cover: StubCover) -> Self {
            self.cover_by_title.insert(title.to_string(), cover);
            self
        }
        fn search_count(&self) -> usize {
            self.searches.borrow().len()
        }
    }
    impl PostgrestSink for StubSink {
        async fn upsert(
            &self,
            _table: &str,
            _on_conflict: &str,
            _rows: &Value,
        ) -> Result<(), String> {
            Ok(())
        }
        async fn fetch_page(
            &self,
            _table: &str,
            _after_seq: i64,
            _limit: i64,
        ) -> Result<Vec<Value>, String> {
            Ok(Vec::new())
        }
        async fn fetch_by_ids(&self, table: &str, ids: &[String]) -> Result<Vec<Value>, String> {
            self.calls
                .borrow_mut()
                .push((table.to_string(), ids.to_vec()));
            // Filter to the requested ids, like a real PostgREST `in.()` fetch would — a stub
            // that returned every canned row regardless of `ids` would hide a caller bug that
            // requests the wrong set.
            let rows = self.by_ids.get(table).cloned().unwrap_or_default();
            Ok(rows
                .into_iter()
                .filter(|r| {
                    r.get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| ids.iter().any(|i| i == id))
                })
                .collect())
        }
        async fn fetch_app_config(&self, key: &str) -> Result<Option<Value>, String> {
            Ok(self.app_config.get(key).cloned())
        }
    }
    impl CoverEgress for StubSink {
        async fn search_cover(
            &self,
            title: &str,
            _author: Option<&str>,
        ) -> Result<Option<CoverSearchHit>, String> {
            self.searches.borrow_mut().push(title.to_string());
            match self.cover_by_title.get(title) {
                Some(StubCover::Hit(h)) => Ok(Some(h.clone())),
                Some(StubCover::Outage) => Err("simulated Open Library outage".into()),
                None => Ok(None),
            }
        }
    }

    fn note(id: &str, book_id: Option<&str>, tags: &[&str], created_at: i64) -> Value {
        json!({
            "id": id, "book_id": book_id, "text": "enc:v2:x", "tags": tags,
            "created_at": created_at, "updated_at": created_at, "deleted": false
        })
    }

    fn book(id: &str, deleted: bool) -> Value {
        json!({
            "id": id, "title": "T", "created_at": 1, "updated_at": 1, "deleted": deleted
        })
    }

    // ── reconcile_books ──────────────────────────────────────────────────────

    #[test]
    fn backfills_a_missing_book_referenced_by_a_live_note() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row("notes", note("n1", Some("b1"), &[], 1).as_object().unwrap())
            .unwrap();
        let sink = StubSink::new().with("books", vec![book("b1", false)]);

        let count = block(reconcile_books(&store, &sink)).unwrap();

        assert_eq!(count, 1);
        assert!(store.get_row("books", "b1").unwrap().is_some());
        assert_eq!(sink.calls.borrow().len(), 1, "one batch fetch");
    }

    #[test]
    fn does_not_fetch_when_no_book_is_missing() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row("books", book("b1", false).as_object().unwrap())
            .unwrap();
        store
            .apply_row("notes", note("n1", Some("b1"), &[], 1).as_object().unwrap())
            .unwrap();
        let sink = StubSink::new();

        let count = block(reconcile_books(&store, &sink)).unwrap();

        assert_eq!(count, 0);
        assert!(
            sink.calls.borrow().is_empty(),
            "no network call when nothing is missing"
        );
    }

    #[test]
    fn a_book_truly_gone_server_side_leaves_the_note_untouched() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row(
                "notes",
                note("n1", Some("ghost"), &[], 1).as_object().unwrap(),
            )
            .unwrap();
        let sink = StubSink::new(); // fetch_by_ids("books", ["ghost"]) -> []

        let count = block(reconcile_books(&store, &sink)).unwrap();

        assert_eq!(count, 0);
        assert!(store.get_row("books", "ghost").unwrap().is_none());
        assert!(
            store.get_row("notes", "n1").unwrap().is_some(),
            "note untouched, not dropped"
        );
    }

    // ── reconcile_stranded_notes ─────────────────────────────────────────────

    #[test]
    fn a_note_on_a_live_book_is_untouched() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row("books", book("b1", false).as_object().unwrap())
            .unwrap();
        store
            .apply_row("notes", note("n1", Some("b1"), &[], 1).as_object().unwrap())
            .unwrap();

        let (rehomed, detached) = reconcile_stranded_notes(&store).unwrap();

        assert_eq!((rehomed, detached), (0, 0));
        assert_eq!(store.outbox_items().unwrap().len(), 0);
    }

    #[test]
    fn rehomes_to_a_known_survivor_and_pushes_the_correction() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row("books", book("loser", true).as_object().unwrap())
            .unwrap();
        store
            .apply_row("books", book("survivor", false).as_object().unwrap())
            .unwrap();
        store
            .apply_row(
                "notes",
                note("n1", Some("loser"), &[], 1).as_object().unwrap(),
            )
            .unwrap();
        store
            .meta_set("mergedBookIds", r#"{"loser":"survivor"}"#)
            .unwrap();

        let (rehomed, detached) = reconcile_stranded_notes(&store).unwrap();

        assert_eq!((rehomed, detached), (1, 0));
        let row = store.get_row("notes", "n1").unwrap().unwrap();
        assert_eq!(row["book_id"], json!("survivor"));
        assert_eq!(
            row["content_tag"],
            Value::Null,
            "stale tag nulled for re-derive"
        );
        assert_eq!(
            store.outbox_items().unwrap().len(),
            1,
            "a real rehome is pushed"
        );
    }

    #[test]
    fn detaches_locally_only_when_no_survivor_is_known() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row("books", book("loser", true).as_object().unwrap())
            .unwrap();
        store
            .apply_row(
                "notes",
                note("n1", Some("loser"), &[], 1).as_object().unwrap(),
            )
            .unwrap();
        // No mergedBookIds entry — this device never performed (or learned of) the merge.

        let (rehomed, detached) = reconcile_stranded_notes(&store).unwrap();

        assert_eq!((rehomed, detached), (0, 1));
        let row = store.get_row("notes", "n1").unwrap().unwrap();
        assert_eq!(row["book_id"], Value::Null);
        assert_eq!(
            store.outbox_items().unwrap().len(),
            0,
            "a map-less detach must NOT be pushed (oracle's LWW-safety rule)"
        );
    }

    #[test]
    fn chained_merge_resolves_straight_to_the_final_survivor() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row("books", book("a", true).as_object().unwrap())
            .unwrap();
        store
            .apply_row("books", book("c", false).as_object().unwrap())
            .unwrap();
        store
            .apply_row("notes", note("n1", Some("a"), &[], 1).as_object().unwrap())
            .unwrap();
        store
            .meta_set("mergedBookIds", r#"{"a":"b","b":"c"}"#)
            .unwrap();

        let (rehomed, _) = reconcile_stranded_notes(&store).unwrap();

        assert_eq!(rehomed, 1);
        assert_eq!(
            store.get_row("notes", "n1").unwrap().unwrap()["book_id"],
            json!("c")
        );
    }

    #[test]
    fn second_pass_over_an_already_reconciled_store_is_a_no_op() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row("books", book("loser", true).as_object().unwrap())
            .unwrap();
        store
            .apply_row(
                "notes",
                note("n1", Some("loser"), &[], 1).as_object().unwrap(),
            )
            .unwrap();

        reconcile_stranded_notes(&store).unwrap(); // first pass: detaches
        let (rehomed, detached) = reconcile_stranded_notes(&store).unwrap(); // second pass

        assert_eq!(
            (rehomed, detached),
            (0, 0),
            "book_id is already null — nothing left to strand"
        );
    }

    // ── reconcile_dropped_tags ───────────────────────────────────────────────

    #[test]
    fn an_orphaned_tag_becomes_a_custom_idea() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row(
                "notes",
                note("n1", None, &["Angel"], 1).as_object().unwrap(),
            )
            .unwrap();

        let created = reconcile_dropped_tags(&store, "user-1").unwrap();

        assert_eq!(created, 1);
        let id = preserved_custom_idea_id("user-1", "Angel");
        let idea = store.get_row("custom_ideas", &id).unwrap().unwrap();
        assert_eq!(idea["name"], json!("Angel"));
        assert_eq!(store.outbox_items().unwrap().len(), 1);
    }

    #[test]
    fn a_canon_tag_is_never_treated_as_dropped() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row(
                "notes",
                note("n1", None, &["Attention"], 1).as_object().unwrap(),
            )
            .unwrap();

        let created = reconcile_dropped_tags(&store, "user-1").unwrap();

        assert_eq!(created, 0, "Attention is current canon (GREAT_IDEAS)");
    }

    #[test]
    fn skips_a_dropped_name_already_held_as_a_custom_idea() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row(
                "custom_ideas",
                json!({
                    "id": "existing", "name": "Angel", "description": "",
                    "created_at": 1, "updated_at": 1, "deleted": false
                })
                .as_object()
                .unwrap(),
            )
            .unwrap();
        store
            .apply_row(
                "notes",
                note("n1", None, &["angel"], 1).as_object().unwrap(),
            )
            .unwrap(); // different case

        let created = reconcile_dropped_tags(&store, "user-1").unwrap();

        assert_eq!(
            created, 0,
            "case-insensitive match against the existing custom idea"
        );
    }

    #[test]
    fn is_idempotent_a_second_run_creates_nothing() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row(
                "notes",
                note("n1", None, &["Angel"], 1).as_object().unwrap(),
            )
            .unwrap();

        assert_eq!(reconcile_dropped_tags(&store, "user-1").unwrap(), 1);
        assert_eq!(
            reconcile_dropped_tags(&store, "user-1").unwrap(),
            0,
            "second run is a no-op"
        );
    }

    #[test]
    fn scopes_the_id_by_user_so_two_users_never_collide() {
        let a = preserved_custom_idea_id("user-a", "Angel");
        let b = preserved_custom_idea_id("user-b", "Angel");
        assert_ne!(a, b);
    }

    #[test]
    fn returns_zero_and_writes_nothing_when_no_dropped_tags_present() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row("notes", note("n1", None, &[], 1).as_object().unwrap())
            .unwrap();

        assert_eq!(reconcile_dropped_tags(&store, "user-1").unwrap(), 0);
        assert!(store.outbox_items().unwrap().is_empty());
    }

    #[test]
    fn preserved_custom_idea_id_matches_the_oracle_example() {
        // surfc/src/ideaNormalize.js's own doc example: preservedCustomIdeaId('user-1', 'Angel')
        // -> 'cidea_sur597_user-1_angel'.
        assert_eq!(
            preserved_custom_idea_id("user-1", "Angel"),
            "cidea_sur597_user-1_angel"
        );
    }

    #[test]
    fn preserved_custom_idea_id_collapses_punctuation_and_trims_edges() {
        assert_eq!(
            preserved_custom_idea_id("u1", "Same and Other!!"),
            "cidea_sur597_u1_same_and_other"
        );
        assert_eq!(
            preserved_custom_idea_id("u1", "  -Quantity- "),
            "cidea_sur597_u1_quantity"
        );
    }

    // ── reconcile (the full pass) ────────────────────────────────────────────

    #[test]
    fn full_pass_runs_all_three_in_order_and_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row("books", book("loser", true).as_object().unwrap())
            .unwrap();
        // "survivor" is already locally present — delivered by a normal sync pull, same as any
        // other live book; reconcile_books is only ever responsible for a genuinely-absent or
        // genuinely-deleted book, never a book the ordinary 8-table pull already delivered.
        store
            .apply_row("books", book("survivor", false).as_object().unwrap())
            .unwrap();
        store
            .meta_set("mergedBookIds", r#"{"loser":"survivor"}"#)
            .unwrap();
        store
            .apply_row(
                "notes",
                note("n1", Some("loser"), &["Angel"], 1)
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        // n2 references a book absent locally but present server-side, per the stub sink below.
        store
            .apply_row(
                "notes",
                note("n2", Some("missing-book"), &[], 2)
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        // Kill-switch OFF: this test exercises the other passes, not cover resolution — keep the
        // coverless fixtures from triggering Open Library egress.
        let sink = StubSink::new()
            .with("books", vec![book("missing-book", false)])
            .with_app_config(OPENLIBRARY_EGRESS_KEY, json!({ "enabled": false }));

        let first = block(reconcile(&store, &sink, "user-1")).unwrap();
        assert_eq!(
            first,
            ReconcileResult {
                books_backfilled: 1, // only "missing-book" was actually absent
                notes_rehomed: 1,
                notes_detached: 0,
                ideas_created: 1,
                covers_resolved: 0, // kill-switch off — no cover work this pass
            }
        );

        let second = block(reconcile(&store, &sink, "user-1")).unwrap();
        assert_eq!(
            second,
            ReconcileResult::default(),
            "a second pass over an already-reconciled store changes nothing"
        );
    }

    // ── reconcile_covers (SUR-828) ───────────────────────────────────────────

    fn put(store: &Store, table: &str, row: &Value) {
        store.apply_row(table, row.as_object().unwrap()).unwrap();
    }

    /// A coverless book (no `cover_url` / `cover_source` / `cover_resolved_at`).
    fn cbook(id: &str, title: &str, isbn: Option<&str>) -> Value {
        json!({
            "id": id, "title": title, "isbn": isbn,
            "created_at": 1, "updated_at": 1, "deleted": false
        })
    }

    fn hit(cover_i: Option<i64>, isbn: Option<&str>) -> StubCover {
        StubCover::Hit(CoverSearchHit {
            cover_i,
            isbn: isbn.map(str::to_string),
        })
    }

    fn cover_of(store: &Store, id: &str) -> (Value, Value, Value) {
        let b = store.get_row("books", id).unwrap().unwrap();
        (
            b.get("cover_url").cloned().unwrap_or(Value::Null),
            b.get("cover_source").cloned().unwrap_or(Value::Null),
            b.get("cover_resolved_at").cloned().unwrap_or(Value::Null),
        )
    }

    #[test]
    fn an_isbn_book_resolves_by_pure_construction_with_no_egress() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &cbook("b1", "Dune", Some("9780441172719")));
        let sink = StubSink::new();

        let resolved = block(reconcile_covers(&store, &sink)).unwrap();

        assert_eq!(resolved, 1);
        assert_eq!(
            sink.search_count(),
            0,
            "ISBN path never hits the Search API"
        );
        let (url, source, stamped) = cover_of(&store, "b1");
        assert_eq!(
            url,
            json!("https://covers.openlibrary.org/b/isbn/9780441172719-M.jpg?default=false")
        );
        assert_eq!(source, json!("openlibrary"));
        assert!(!stamped.is_null(), "cover_resolved_at stamped");
    }

    #[test]
    fn a_no_isbn_book_uses_the_search_cover_id() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &cbook("b1", "Dune", None));
        let sink = StubSink::new().with_cover("Dune", hit(Some(42), None));

        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 1);
        assert_eq!(sink.search_count(), 1);
        assert_eq!(
            cover_of(&store, "b1").0,
            json!("https://covers.openlibrary.org/b/id/42-M.jpg?default=false")
        );
    }

    #[test]
    fn a_no_isbn_book_falls_back_to_a_healed_isbn_when_no_cover_id() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &cbook("b1", "Dune", None));
        let sink = StubSink::new().with_cover("Dune", hit(None, Some("9780441172719")));

        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 1);
        assert_eq!(
            cover_of(&store, "b1").0,
            json!("https://covers.openlibrary.org/b/isbn/9780441172719-M.jpg?default=false")
        );
    }

    #[test]
    fn a_definitive_miss_stamps_resolved_at_and_the_second_pass_is_a_no_op() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &cbook("b1", "Ghost", None)); // no with_cover → NoDocs
        let sink = StubSink::new();

        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 1);
        let (url, source, stamped) = cover_of(&store, "b1");
        assert_eq!(url, Value::Null, "a miss leaves cover_url null");
        assert_eq!(source, Value::Null);
        assert!(!stamped.is_null(), "but stamps cover_resolved_at (SUR-566)");

        // Second pass: already stamped → skipped, no re-query (never re-hammers Open Library).
        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 0);
        assert_eq!(sink.search_count(), 1, "no second search for the same book");
    }

    #[test]
    fn a_manual_cover_is_never_touched() {
        let store = Store::open_in_memory().unwrap();
        put(
            &store,
            "books",
            &json!({ "id": "b1", "title": "Set By Hand", "cover_source": "manual",
                "created_at": 1, "updated_at": 1, "deleted": false }),
        );
        let sink = StubSink::new().with_cover("Set By Hand", hit(Some(9), None));

        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 0);
        assert_eq!(
            sink.search_count(),
            0,
            "manual rows are skipped before any egress"
        );
        assert_eq!(cover_of(&store, "b1").0, Value::Null);
    }

    #[test]
    fn an_already_resolved_book_is_skipped() {
        let store = Store::open_in_memory().unwrap();
        put(
            &store,
            "books",
            &json!({ "id": "b1", "title": "Done", "cover_url": "u", "cover_source": "openlibrary",
                "cover_resolved_at": 5, "created_at": 1, "updated_at": 1, "deleted": false }),
        );
        let sink = StubSink::new();
        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 0);
        assert_eq!(sink.search_count(), 0);
    }

    #[test]
    fn a_stamped_book_not_edited_since_stays_skipped() {
        let store = Store::open_in_memory().unwrap();
        // A prior miss stamped it; updated_at == cover_resolved_at (reconcile writes both equal).
        put(
            &store,
            "books",
            &json!({ "id": "b1", "title": "Ghost",
            "cover_resolved_at": 100, "created_at": 1, "updated_at": 100, "deleted": false }),
        );
        let sink = StubSink::new();

        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 0);
        assert_eq!(
            sink.search_count(),
            0,
            "unchanged since the stamp — no re-query (SUR-566)"
        );
    }

    #[test]
    fn a_missed_book_edited_after_the_stamp_re_resolves() {
        let store = Store::open_in_memory().unwrap();
        // Missed + stamped at t=100, then the user fixed the title (updated_at bumped to 200 by
        // enqueue_book). cover_url is still null. The later lookup inputs must be retried.
        put(
            &store,
            "books",
            &json!({ "id": "b1", "title": "Dune",
            "cover_resolved_at": 100, "created_at": 1, "updated_at": 200, "deleted": false }),
        );
        let sink = StubSink::new().with_cover("Dune", hit(Some(42), None));

        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 1);
        assert_eq!(
            sink.search_count(),
            1,
            "the edit re-opened the book for resolution"
        );
        assert_eq!(
            cover_of(&store, "b1").0,
            json!("https://covers.openlibrary.org/b/id/42-M.jpg?default=false")
        );
        // And it's idempotent again: the resolve wrote updated_at == cover_resolved_at.
        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 0);
        assert_eq!(sink.search_count(), 1);
    }

    #[test]
    fn a_covered_book_is_left_alone_even_after_an_edit() {
        let store = Store::open_in_memory().unwrap();
        // Has a cover; edited after the stamp. We leave covered books as-is (no egress/flicker on a
        // plain edit) — the reviewer's concern is coverless books, and this one has a cover.
        put(
            &store,
            "books",
            &json!({ "id": "b1", "title": "Dune", "cover_url": "u",
            "cover_source": "openlibrary", "cover_resolved_at": 100,
            "created_at": 1, "updated_at": 200, "deleted": false }),
        );
        let sink = StubSink::new().with_cover("Dune", hit(Some(42), None));

        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 0);
        assert_eq!(sink.search_count(), 0);
        assert_eq!(
            cover_of(&store, "b1").0,
            json!("u"),
            "existing cover untouched"
        );
    }

    #[test]
    fn the_kill_switch_gates_egress_in_both_states() {
        // OFF → zero egress, book left unstamped.
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &cbook("b1", "Dune", None));
        let sink = StubSink::new()
            .with_app_config(OPENLIBRARY_EGRESS_KEY, json!({ "enabled": false }))
            .with_cover("Dune", hit(Some(42), None));
        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 0);
        assert_eq!(
            sink.search_count(),
            0,
            "kill-switch off → zero Open Library egress"
        );
        assert_eq!(
            cover_of(&store, "b1").2,
            Value::Null,
            "and no stamp — retried if re-enabled"
        );

        // ON → resolves normally.
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &cbook("b1", "Dune", None));
        let sink = StubSink::new()
            .with_app_config(OPENLIBRARY_EGRESS_KEY, json!({ "enabled": true }))
            .with_cover("Dune", hit(Some(42), None));
        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 1);
        assert_eq!(sink.search_count(), 1);
    }

    #[test]
    fn a_malformed_or_missing_flag_fails_open() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &cbook("b1", "Dune", Some("9780441172719")));
        // No app_config row at all → fetch_app_config returns None → fail OPEN (egress allowed).
        let sink = StubSink::new();
        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 1);
    }

    #[test]
    fn a_hyphenated_isbn_is_normalized_before_the_url_is_built() {
        let store = Store::open_in_memory().unwrap();
        put(
            &store,
            "books",
            &cbook("b1", "Dune", Some("978-0-441-17271-9")),
        );
        let sink = StubSink::new();

        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 1);
        assert_eq!(sink.search_count(), 0, "still the construct-only ISBN path");
        assert_eq!(
            cover_of(&store, "b1").0,
            json!("https://covers.openlibrary.org/b/isbn/9780441172719-M.jpg?default=false"),
            "hyphens stripped to match the PWA's normalizeIsbn"
        );
    }

    #[test]
    fn an_unparseable_isbn_falls_through_to_the_search_path() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &cbook("b1", "Dune", Some("N/A")));
        let sink = StubSink::new().with_cover("Dune", hit(Some(42), None));

        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 1);
        assert_eq!(
            sink.search_count(),
            1,
            "a garbage ISBN is not treated as valid → search"
        );
        assert_eq!(
            cover_of(&store, "b1").0,
            json!("https://covers.openlibrary.org/b/id/42-M.jpg?default=false")
        );
    }

    #[test]
    fn a_healed_search_isbn_is_normalized_too() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &cbook("b1", "Dune", None));
        let sink = StubSink::new().with_cover("Dune", hit(None, Some("978-0-441-17271-9")));

        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 1);
        assert_eq!(
            cover_of(&store, "b1").0,
            json!("https://covers.openlibrary.org/b/isbn/9780441172719-M.jpg?default=false")
        );
    }

    #[test]
    fn a_titleless_no_isbn_book_misses_without_any_egress() {
        let store = Store::open_in_memory().unwrap();
        put(
            &store,
            "books",
            &json!({ "id": "b1", "title": "",
            "created_at": 1, "updated_at": 1, "deleted": false }),
        );
        let sink = StubSink::new();

        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 1);
        assert_eq!(
            sink.search_count(),
            0,
            "empty title short-circuits — no Open Library call"
        );
        assert!(
            !cover_of(&store, "b1").2.is_null(),
            "but the miss is stamped"
        );
    }

    #[test]
    fn normalize_isbn_matches_the_oracle_shapes() {
        assert_eq!(
            normalize_isbn(Some("978-0-441-17271-9")).as_deref(),
            Some("9780441172719")
        );
        assert_eq!(
            normalize_isbn(Some("0441172717")).as_deref(),
            Some("0441172717")
        ); // ISBN-10
        assert_eq!(
            normalize_isbn(Some("080442957x")).as_deref(),
            Some("080442957X")
        ); // X check char
        assert_eq!(normalize_isbn(Some("12345")), None); // too short
        assert_eq!(normalize_isbn(Some("N/A")), None);
        assert_eq!(normalize_isbn(Some("978044117271X")), None); // 13 digits can't carry X
        assert_eq!(normalize_isbn(None), None);
    }

    #[test]
    fn a_transient_outage_leaves_the_book_unstamped_to_retry() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &cbook("b1", "Flaky", None));
        let sink = StubSink::new().with_cover("Flaky", StubCover::Outage);

        assert_eq!(block(reconcile_covers(&store, &sink)).unwrap(), 0);
        assert_eq!(
            cover_of(&store, "b1").2,
            Value::Null,
            "an outage must NOT stamp — the book retries next pass"
        );
    }

    #[test]
    fn an_outage_never_fails_the_overall_reconcile_pass() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &cbook("b1", "Flaky", None));
        let sink = StubSink::new().with_cover("Flaky", StubCover::Outage);

        // reconcile() wraps cover-resolution best-effort — an outage yields covers_resolved: 0,
        // never an Err.
        let r = block(reconcile(&store, &sink, "user-1")).unwrap();
        assert_eq!(r.covers_resolved, 0);
    }

    #[test]
    fn the_search_budget_caps_egress_per_pass_with_the_rest_next_pull() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..(COVER_SEARCH_BUDGET_PER_PASS + 2) {
            put(
                &store,
                "books",
                &cbook(&format!("b{i:02}"), &format!("t{i:02}"), None),
            );
        }
        let sink = StubSink::new(); // all titles → NoDocs (miss)

        let first = block(reconcile_covers(&store, &sink)).unwrap();
        assert_eq!(
            first, COVER_SEARCH_BUDGET_PER_PASS,
            "capped at the per-pass budget"
        );
        assert_eq!(sink.search_count(), COVER_SEARCH_BUDGET_PER_PASS);

        // The 2 that missed the budget are still unstamped → resolved on the next pass.
        let second = block(reconcile_covers(&store, &sink)).unwrap();
        assert_eq!(second, 2);
    }
}
