//! Post-pull reconciliation (SUR-820, extended by SUR-835 + SUR-828). The passes that run after a
//! successful [`super::pull`], promoting into the core the post-sync behaviors the PWA's
//! `fetchAllCloud` orchestration runs in `src/hooks/useAuth.js` (steps 2b/2c/2d) plus its
//! content-tag dedup and book-cover resolution —
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
//!    book-merge survivor a device didn't itself perform) is repointed to the merge survivor,
//!    resolved from TWO sources per hop (SUR-1005): the loser row's synced `merged_into` pointer
//!    first (the fleet-wide record — so a device that never received the merge map still
//!    converges), then the local `mergedBookIds` map (persisted in `meta`, mirroring the PWA's
//!    device-local merge map) as fallback; else detached (`book_id` → null). A survivor that
//!    resolves onto a still-deleted book (merge cycle / plain-deleted chain end) also detaches —
//!    the liveness guard, mirroring surfc#362. Only a real rehome-to-survivor is a genuine
//!    mutation other clients must learn about (staged via [`super::mod::stage_local_write`]
//!    equivalent below); a survivor-less detach stays local-only, exactly mirroring the oracle's
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
//! 4. **`reconcile_heal_content_tags`** (SUR-884; the self-heal half of `reconcileContentTags`)
//!    then **`reconcile_content_dupes`** (SUR-835; `reconcileContentTags` + `mergeNotes` in
//!    `db.js`). Self-heal first re-derives a null/empty `content_tag` from the note's decrypted
//!    text (so a note whose tag was nulled by pass 2 above is re-tagged before dedup, not left for
//!    its next edit) — the tag is written LOCAL-ONLY (no `updated_at` bump, mirroring the oracle),
//!    never propagated. Then dedup collapses live notes sharing a `content_tag` (the SUR-638
//!    per-user HMAC content fingerprint) into one survivor, picked deterministically (most tags,
//!    then earliest `created_at`, then lowest `id`) so two devices reconciling independently
//!    converge on the SAME keeper. The losers' tags, image, `note_links` edges and
//!    `collection_memberships` are merged onto the survivor and the losers soft-deleted — all
//!    through the outbox (LWW-safe).
//! 5. **`reconcile_note_signals`** (SUR-976; NO oracle counterpart — `note_signals` collection is
//!    core-only, SUR-966) — a live `note_signals` row whose LOCAL `notes` row is tombstoned is
//!    retired with a full-shape tombstone through the outbox. Closes the cross-device
//!    orphaned-signals leak whole-row LWW makes inherent (a not-yet-pulled device's later signal
//!    wins the cloud row back from the deleting device's tombstone), plus every other door into
//!    the same state: same-cycle content-dedup merge losers (hence its slot right after pass 4),
//!    retired margin children, pre-SUR-975 crash strands, and imported orphans. A signals row
//!    with NO local notes row is deliberately left alone — the pull tombstone-skip makes absent
//!    ambiguous (never-synced vs deleted-elsewhere). It self-resolves once the note arrives — or
//!    persists as an INERT live row if no tombstone-holding device survives to retire it (the
//!    tombstone-skip never delivers the note tombstone to a device without the note): accepted —
//!    it cannot grow (`record_note_signal` refuses an absent note) and no read surface renders it.
//! 6. **`reconcile_covers`** (SUR-828; `resolveCover` in `surfc/src/lib/coverResolver.js`) —
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
//! shape internally — a `reconcile_dropped_tags`, `reconcile_content_dupes`,
//! `reconcile_note_signals`, or `reconcile_covers`
//! failure is caught and logged here, never propagated. Whatever `reconcile` itself returns is, in
//! turn, treated as best-effort by ITS
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
use super::read::decrypt_note_text;
use super::SyncEngine;
use crate::store::Store;
use crate::vault::Vault;

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
    pub dupes_collapsed: usize,
    pub signals_retired: usize,
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
/// dropped-tags, then content-tag self-heal, then content-dedup, then signals-retire, then
/// cover-resolution (independent of the others). Self-heal runs AFTER stranded-notes (which nulls
/// a rehomed note's now-stale `content_tag`) and immediately BEFORE content-dedup, so a note that
/// lost its tag this pass is re-tagged in time to be clustered this same pass instead of waiting
/// for its next edit. Signals-retire runs AFTER content-dedup so a merge loser tombstoned this
/// same cycle has its signals row retired now, not next pull.
/// `user_id` is the token's `sub` — needed only for the dropped-tag pass's user-scoped custom-idea
/// id. `vault` decrypts each tagless note to re-derive its tag (SUR-884) — the ONLY pass that needs
/// keys; every other pass works on stored fields alone.
pub async fn reconcile<S: PostgrestSink + CoverEgress>(
    store: &Store,
    sink: &S,
    user_id: &str,
    vault: &Vault,
) -> Result<ReconcileResult, String> {
    let books_backfilled = reconcile_books(store, sink).await?;
    let (notes_rehomed, notes_detached) = reconcile_stranded_notes(store)?;
    // Best-effort (mirrors the oracle's explicit try/catch around `preserveDroppedTagsAsCustom`):
    // a failure here must never block the rest of reconciliation or the pull it follows.
    let ideas_created = reconcile_dropped_tags(store, user_id).unwrap_or_else(|e| {
        eprintln!("reconcile: dropped-tag pass failed (non-fatal, retries next pull): {e}");
        0
    });
    // Best-effort, same posture: re-derive missing content_tags so the dedup pass below can see
    // them. A heal hiccup must never fail the pull — it retries next pull (idempotent). The count
    // is logged, not surfaced across the FFI (no `ReconcileSummary` field), to keep the binding
    // frozen — this whole ticket ships as a core-pin bump with no host change.
    match reconcile_heal_content_tags(store, vault) {
        Ok(0) => {}
        Ok(healed) => {
            eprintln!("reconcile: content-tag self-heal re-derived {healed} missing tag(s)")
        }
        Err(e) => {
            eprintln!(
                "reconcile: content-tag self-heal pass failed (non-fatal, retries next pull): {e}"
            )
        }
    }
    // Best-effort, same posture as the dropped-tag pass: a content-dedup hiccup must never fail
    // the pull it follows — it simply retries next pull (the pass is idempotent).
    let dupes_collapsed = reconcile_content_dupes(store).unwrap_or_else(|e| {
        eprintln!("reconcile: content-dedup pass failed (non-fatal, retries next pull): {e}");
        0
    });
    // Best-effort, same posture (SUR-976). Runs immediately AFTER content-dedup ON PURPOSE: the
    // dedup's merge tombstones loser notes in this same cycle, and this pass retires their
    // signals rows without waiting a full extra pull. It has no oracle counterpart (note_signals
    // is core-only), so nothing dictates fatality — and it's a pure hygiene sweep nothing later
    // in this function reads.
    let signals_retired = reconcile_note_signals(store).unwrap_or_else(|e| {
        eprintln!("reconcile: signals-retire pass failed (non-fatal, retries next pull): {e}");
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
        dupes_collapsed,
        signals_retired,
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
    let fetched = sink.fetch_by_ids("books", "id", &ids).await?;
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

/// SUR-1005 — resolve a stranded note's target book. From its (soft-deleted) book, follow
/// the synced `merged_into` pointer on the stored row, falling back to the device-local
/// map at each hop — `merged_into` wins (it's the fleet-wide record; the map fills
/// local-only gaps). Hop-capped like [`resolve_book_id`] (cycle-safe; same cap of 20).
/// Behavior-equivalent to the PWA's fold-`merged_into`-over-map + walk (surfc PR #362) —
/// implemented as a stored-row walk so no full deleted-books scan is needed.
///
/// Returns `(terminal_id, safe_to_rehome)`. Safe = the terminal row is live locally, or
/// absent (not yet pulled — the real target; it materializes on the next pull). A terminal
/// row that is itself soft-deleted — a merge CYCLE the hop cap bailed out of (concurrent
/// A→B and B→A merges are individually-valid LWW writes), or a chain terminus later
/// removed by a plain book delete (no `merged_into`) — is NOT safe: parking a note there,
/// and then PUSHING that book_id fleet-wide, would defeat the no-dangling invariant, so
/// the caller detaches instead (the PWA's liveness guard, mirrored). A mid-walk read
/// error is `Err` — the caller skips the note this pass (per-note isolation) rather than
/// guessing.
fn resolve_survivor(
    store: &Store,
    start_id: &str,
    start_row: &Map<String, Value>,
    map: &BTreeMap<String, String>,
) -> Result<(String, bool), String> {
    let mut id = start_id.to_string();
    let mut row: Option<Map<String, Value>> = Some(start_row.clone());
    for _ in 0..20 {
        let next: Option<String> = row
            .as_ref()
            .and_then(|r| r.get("merged_into"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty()) // mirror the PWA's falsy-skip: '' is absent
            .map(str::to_string)
            .or_else(|| map.get(&id).cloned());
        match next {
            Some(n) if n != id => {
                row = store
                    .get_row("books", &n)
                    .map_err(|e| format!("walk merged_into {id}→{n}: {e}"))?;
                id = n;
            }
            _ => break,
        }
    }
    let safe = match &row {
        Some(r) => !matches!(r.get("deleted"), Some(Value::Bool(true))),
        None => true, // not yet pulled — the real target, materializes next pull
    };
    Ok((id, safe))
}

/// Step 2c — repair a live note pointing at a book that's present locally but soft-deleted (an
/// offline book-merge this device didn't itself perform). Resolves the survivor from TWO
/// sources via [`resolve_survivor`]: the pulled loser row's synced `merged_into` pointer
/// (SUR-1005 — so a device that never received the merge's device-local map still converges
/// the straggler; the always-to-survivor convergence SUR-916 wanted) and the local
/// `mergedBookIds` map (the fast path, and the only source for pre-`merged_into` merges).
/// A safe survivor is a genuine mutation — staged through the outbox so the fleet
/// converges; no survivor (or a survivor that resolved onto a still-deleted book — the
/// liveness guard) detaches to `null` locally-only. Returns `(rehomed, detached)`.
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
        let (survivor, survivor_safe) =
            match resolve_survivor(store, book_id, &book, &merged_book_ids) {
                Ok(resolved) => resolved,
                Err(e) => {
                    eprintln!("reconcile_stranded_notes: note {note_id}: {e}, skipping");
                    continue; // left stranded this pass — retried next reconcile
                }
            };
        if survivor != book_id && survivor_safe {
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
            // No known survivor — or one that resolved onto a still-deleted book (the
            // liveness guard) — detach locally only, NEVER pushed. Mirrors the oracle's
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

/// Persist the device-local `mergedBookIds` survivor map (the write side [`merge_books`] adds and
/// [`unmerge_books`] prunes; `reconcile_stranded_notes` reads it). `BTreeMap` so the serialized
/// JSON is key-ordered and stable across writes.
fn save_merged_book_ids(store: &Store, map: &BTreeMap<String, String>) -> Result<(), String> {
    let json = serde_json::to_string(map).map_err(|e| format!("serialize merged book ids: {e}"))?;
    store
        .meta_set(MERGED_BOOK_IDS_KEY, &json)
        .map_err(|e| format!("write merged book ids: {e}"))
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

/// Step 2e-pre (SUR-884) — content-tag SELF-HEAL, the second half of the PWA's
/// `reconcileContentTags` (`surfc/src/db.js`). For every LIVE note with a null/empty `content_tag`
/// but decryptable text, re-derive the tag (`Vault::content_tag` = the SUR-638 per-user HMAC over
/// `normalize(plaintext)` + `book_id`) and persist it, so the [`reconcile_content_dupes`] pass that
/// follows — which keys on the STORED tag and never decrypts — can cluster it. Without this, a note
/// whose tag was nulled by [`reconcile_stranded_notes`] (a rehome/detach makes the old tag stale,
/// since `book_id` is HMAC input) stays tagless and un-clustered on native until its next user edit
/// re-seals it; the PWA heals it at load, so this closes the parity gap.
///
/// **Local-only, never propagated — matches the oracle byte-for-byte and is the safe choice.** The
/// PWA persists the healed tag with NO `updatedAt` bump (`db.notes.update(id, { contentTag })`), so
/// it never enters the sync/LWW path; each device re-derives the SAME tag independently (the
/// derivation is deterministic in MK + plaintext + book_id). We mirror that with [`Store::apply_row`]
/// — the same local-only primitive the map-less detach in [`reconcile_stranded_notes`] uses — NOT
/// `stage_local_write`. Propagating instead would be actively wrong: `notes` is **whole-row LWW**
/// (`store.rs`), so a heal write bumping `updated_at` would let this tag-only version WIN THE WHOLE
/// ROW and clobber a concurrent field edit another device hasn't pushed yet. Convergence still holds:
/// the dedup pass's loser soft-delete DOES propagate (SUR-835, LWW-safe), and two devices that
/// re-derive identical tags pick the same survivor. Cost: the tagless note is re-derived every pull
/// (its stored/server tag stays null), which is cheap — one decrypt + one HMAC per null note, and
/// null tags are rare (only rehome/detach produce them).
///
/// **Decrypt-failure gate (mirrors the oracle's `decryptError` skip).** Plaintext is read through
/// [`decrypt_note_text`] — the exact gate the display path uses — so a note that fails to decrypt
/// (foreign/corrupt ciphertext, wrong key) is left tagless, never fingerprinted from unreadable
/// bytes. A note with genuinely no text (an ABSENT `text` column → `None`) is skipped too. This
/// matches the oracle's `n.text == null` guard EXACTLY, including the empty-string case: `"" == null`
/// is false in JS, so the PWA fingerprints empty text, and so do we (`decrypt_note_text` yields
/// `Some("")`, which is tagged). Two empty-text notes in one book therefore share a tag and the
/// dedup pass collapses them — the same effect `enqueue_note` already produces by tagging empty text
/// at write (pre-existing SUR-835); whether dedup should exclude empty/image-only notes is a SUR-835
/// question, not this pass's. Per-note isolation: one unreadable/unwritable row is logged and
/// skipped, never aborting the pass. Idempotent: a second pass finds every note already tagged and
/// heals nothing.
///
/// **Detach-window convergence (self-correcting).** [`reconcile_stranded_notes`]'s map-less DETACH
/// arm is local-only (`book_id` → null, never pushed), so a device without the merge map briefly
/// heals a note to `content_tag(text, None)` while a map-holding device heals it to
/// `content_tag(text, survivor)` and may collapse it. This is transient, not a lost write: the
/// collapse's loser soft-delete propagates as an `id`-keyed, idempotent tombstone (so both devices
/// converge on the same deleted row), and the rehomed `book_id=survivor` propagates too, so the
/// lagging device re-derives the identical tag on a later pull. Steady state is identical on every
/// device (SUR-820 invariant).
///
/// This is the ONE reconcile pass that holds keys. That's a deliberate, bounded crossing of the
/// otherwise key-less sync layer (ADR 0003), and it follows the exact precedent the SUR-744 read
/// surface already set — `sync::read` takes `&Vault` to decrypt on the way out. Plaintext here is
/// transient (never persisted; only the opaque HMAC tag is written), same invariant as the read
/// path. Flagged for `crypto-reviewer`.
fn reconcile_heal_content_tags(store: &Store, vault: &Vault) -> Result<usize, String> {
    let notes = store
        .list_live("notes", None, -1, 0)
        .map_err(|e| format!("list notes: {e}"))?;

    let mut healed = 0;
    for row in &notes {
        // Only tagless notes need healing — a present, non-empty tag is left untouched (and makes
        // the pass idempotent). Same emptiness test as `reconcile_content_dupes`' grouping.
        let has_tag = row
            .get("content_tag")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.is_empty());
        if has_tag {
            continue;
        }

        let note_id = row
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // Re-derive from plaintext, gated exactly like the display path.
        let (text, decrypt_failed) = decrypt_note_text(row, &note_id, vault);
        if decrypt_failed {
            continue; // never fingerprint unreadable ciphertext (oracle's decryptError skip)
        }
        let Some(plaintext) = text else {
            continue; // no text — nothing to fingerprint (oracle's `n.text == null` skip)
        };
        let book_id = row
            .get("book_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let tag = vault.content_tag(plaintext, book_id);

        // Persist LOCAL-ONLY: clone the stored row, set the tag, leave `updated_at` as-is, write
        // via `apply_row` (no outbox, no LWW bump — see the doc comment on why propagation is wrong).
        let mut patched = row.clone();
        patched.insert("content_tag".into(), json!(tag));
        match store.apply_row("notes", &patched) {
            Ok(()) => healed += 1,
            Err(e) => {
                eprintln!("reconcile_heal_content_tags: apply healed tag for note {note_id} failed, skipping: {e}")
            }
        }
    }
    Ok(healed)
}

/// Step 2e (SUR-835) — retroactive content-tag dedup. Collapse live notes that share a
/// `content_tag` (the per-user HMAC content fingerprint, SUR-638) into one survivor, mirroring the
/// PWA's user-driven `mergeNotes` (`surfc/src/db.js`): union the losers' tags onto the survivor,
/// adopt a loser's image only when the survivor has none, re-point the losers' `note_links` edges
/// and `collection_memberships` onto the survivor (dedup/tombstone self-loops + duplicate edges),
/// then soft-delete the losers. Every mutation is staged through the outbox (LWW-safe) so the
/// collapse converges across the fleet. Returns the number of losers collapsed.
///
/// **Survivor selection is a convergence contract:** two devices reconciling independently MUST
/// pick the same survivor, or each soft-deletes the other's. Ported from
/// `pickContentDuplicateSurvivor` (most tags wins, then earliest `created_at`) with an explicit
/// final `id` tiebreak so the order is TOTAL and device-independent — the oracle leans on JS
/// stable sort over the client's load order, which two native devices can't be assumed to share.
/// The only case this is stricter than the oracle is a measure-zero exact tie (equal tag-count AND
/// equal `created_at`); flagged for `sync-reviewer`.
///
/// Dedup keys on the STORED `content_tag` alone — the core never decrypts note text here (a
/// deliberate, safe divergence from the oracle's detect path, which reads text only to *recompute*
/// a missing tag; the core leaves a tagless note untouched rather than recompute it).
///
/// **Accepted residual risk (flagged for `sync-reviewer`):** because this runs pre-decrypt on
/// stored rows, it has no equivalent to the oracle's `decryptError` gate (`reconcileContentTags`
/// operates on already-decrypted notes and excludes decrypt-failures from clustering). A row only
/// ever *has* a `content_tag` because it was encryptable at write time, so the only path to
/// "tagged but currently undecryptable" is post-write corruption (bit-rot, a key-version bug). If
/// such a corrupted note shares a `content_tag` with a healthy one, the two are BY DEFINITION the
/// same content (the tag is `HMAC(normText, bookId)`), so collapsing them loses nothing — EXCEPT
/// the narrow case where the corrupted note has more tags and is thus picked as survivor, keeping
/// the unreadable copy over the readable one. This requires post-write corruption AND a surviving
/// tag AND the corrupted row winning the survivor sort — accepted as sufficiently rare; the core
/// can't cheaply detect decrypt-failure in the sync layer (no vault/keys here). Revisit if a
/// decrypt-health signal ever reaches this layer (prefer a decryptable note as survivor).
fn reconcile_content_dupes(store: &Store) -> Result<usize, String> {
    let notes = store
        .list_live("notes", None, -1, 0)
        .map_err(|e| format!("list notes: {e}"))?;

    // Group live notes by their stored content_tag. A note with no content_tag can't be
    // fingerprint-matched without decrypting + re-deriving (out of scope) — skip it.
    let mut by_tag: BTreeMap<String, Vec<Map<String, Value>>> = BTreeMap::new();
    for row in &notes {
        match row.get("content_tag").and_then(Value::as_str) {
            Some(tag) if !tag.is_empty() => {
                by_tag.entry(tag.to_string()).or_default().push(row.clone())
            }
            _ => {}
        }
    }

    let mut collapsed = 0;
    for (_tag, mut cluster) in by_tag {
        if cluster.len() < 2 {
            continue;
        }
        // Total, device-independent survivor order (see the doc comment): most tags, then earliest
        // created_at, then lowest id. `sort_by` is stable, but the id tiebreak makes it moot.
        cluster.sort_by(|a, b| {
            let a_tags = a.get("tags").and_then(Value::as_array).map_or(0, Vec::len);
            let b_tags = b.get("tags").and_then(Value::as_array).map_or(0, Vec::len);
            b_tags
                .cmp(&a_tags)
                .then_with(|| row_i64(a, "created_at").cmp(&row_i64(b, "created_at")))
                .then_with(|| row_str(a, "id").cmp(row_str(b, "id")))
        });
        let survivor = cluster[0].clone();
        // Per-cluster isolation (see `reconcile_books`): one failed merge must not abort the pass.
        match merge_into_survivor(store, &survivor, &cluster[1..]) {
            Ok(n) => collapsed += n,
            Err(e) => eprintln!(
                "reconcile_content_dupes: merge into survivor failed, skipping cluster: {e}"
            ),
        }
    }
    Ok(collapsed)
}

fn row_str<'a>(row: &'a Map<String, Value>, key: &str) -> &'a str {
    row.get(key).and_then(Value::as_str).unwrap_or("")
}

fn row_i64(row: &Map<String, Value>, key: &str) -> i64 {
    row.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Collapse `losers` into `survivor`, mirroring the PWA's `mergeNotes` (`surfc/src/db.js`): union
/// tags, adopt an image only if the survivor lacks one, re-point edges + memberships, soft-delete
/// the losers. Returns the number of losers soft-deleted.
///
/// **Ordering is a safety invariant.** The child-row re-points (`note_links`, `collection_memberships`)
/// run BEFORE the loser soft-deletes, and a stage failure in either helper is PROPAGATED (via `?`) —
/// so a loser is only ever tombstoned once ALL of its edges/memberships have been successfully
/// re-pointed onto the survivor. The oracle gets this for free from a single Dexie transaction; the
/// core can't span one SQLite transaction across these separate outbox writes, so instead it
/// fail-fasts: on a transient stage failure the whole collapse is abandoned for this pass (the
/// cluster's losers stay live) and re-attempted next pull, idempotently. This closes the window
/// where a live edge could be left pointing at a tombstoned note — which no later pass would fix,
/// since clusters are only built from LIVE notes.
fn merge_into_survivor(
    store: &Store,
    survivor: &Map<String, Value>,
    losers: &[Map<String, Value>],
) -> Result<usize, String> {
    let now = epoch_ms();
    let sid = row_str(survivor, "id").to_string();
    let loser_ids: BTreeSet<String> = losers
        .iter()
        .map(|l| row_str(l, "id").to_string())
        .collect();

    // ── Union tags: survivor's first (order stable), then losers', dedup preserving first seen. ──
    let mut tags: Vec<Value> = survivor
        .get("tags")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut seen_tags: HashSet<String> = tags
        .iter()
        .filter_map(|t| t.as_str().map(str::to_string))
        .collect();
    for l in losers {
        if let Some(arr) = l.get("tags").and_then(Value::as_array) {
            for t in arr.iter().filter_map(Value::as_str) {
                if seen_tags.insert(t.to_string()) {
                    tags.push(Value::String(t.to_string()));
                }
            }
        }
    }

    let mut survivor_patch = Map::new();
    survivor_patch.insert("id".into(), json!(sid));
    survivor_patch.insert("tags".into(), Value::Array(tags));
    // Adopt a loser's image only when the survivor has NEITHER (`image_path` null/absent). The core
    // has no `imageDataUrl` local-copy field the PWA also considers — only the synced `image_path`.
    let survivor_has_image = survivor.get("image_path").is_some_and(|v| !v.is_null());
    if !survivor_has_image {
        if let Some(donor) = losers
            .iter()
            .find(|l| l.get("image_path").is_some_and(|v| !v.is_null()))
        {
            survivor_patch.insert("image_path".into(), donor["image_path"].clone());
        }
    }
    survivor_patch.insert("updated_at".into(), json!(now));
    store
        .stage_local_write("notes", &sid, survivor_patch, now)
        .map_err(|e| format!("stage survivor {sid}: {e}"))?;

    repoint_note_links(store, &sid, &loser_ids, now)?;
    repoint_memberships(store, &sid, &loser_ids, now)?;

    // ── Soft-delete the losers (tombstone: only `deleted`/`updated_at` change; the partial patch
    // leaves every other column intact). Per-loser isolation is SAFE here — unlike the repoints
    // above: every edge/membership has already been moved onto the (live) survivor, so a loser
    // whose delete is deferred strands nothing; it just stays a live duplicate and is re-collapsed
    // on the next pass. ──
    let mut collapsed = 0;
    for lid in &loser_ids {
        let mut patch = Map::new();
        patch.insert("id".into(), json!(lid));
        patch.insert("deleted".into(), json!(true));
        patch.insert("updated_at".into(), json!(now));
        match store.stage_local_write("notes", lid, patch, now) {
            Ok(()) => collapsed += 1,
            Err(e) => {
                eprintln!("reconcile_content_dupes: soft-delete loser {lid} failed, skipping: {e}")
            }
        }
    }
    Ok(collapsed)
}

/// Re-point every live `note_links` edge that touches a loser onto the survivor, dropping
/// self-loops and duplicates. Mirrors the `mergeNotes` edge block: `seen` is seeded with edges that
/// DON'T touch a loser so a re-pointed edge dedups against one the survivor already owns.
///
/// **Determinism (convergence):** when two losers in one cluster each link to the same external
/// note with the same `relation_type`, both re-point to the SAME key and one must be kept, the
/// other dropped — and two devices must agree on WHICH, or the fleet's per-row LWW can settle on a
/// duplicate (both live) or a lost edge (both deleted). The oracle keeps the first in Dexie
/// primary-key order (`id` ascending); `list_live` returns `created_at DESC, id DESC`, a different
/// order — so we re-sort by `id` ascending here to keep the SAME edge the PWA (and any other native
/// device) keeps. Same fix philosophy as the survivor pick's total `id` tiebreak.
fn repoint_note_links(
    store: &Store,
    sid: &str,
    loser_ids: &BTreeSet<String>,
    now: i64,
) -> Result<(), String> {
    let mut edges = store
        .list_live("note_links", None, -1, 0)
        .map_err(|e| format!("list note_links: {e}"))?;
    edges.sort_by(|a, b| row_str(a, "id").cmp(row_str(b, "id")));
    let edge_key = |from: &str, to: &str, rel: &str| format!("{from}|{to}|{rel}");

    let mut seen: HashSet<String> = HashSet::new();
    for e in &edges {
        let (from, to) = (row_str(e, "from_note_id"), row_str(e, "to_note_id"));
        if !loser_ids.contains(from) && !loser_ids.contains(to) {
            seen.insert(edge_key(from, to, row_str(e, "relation_type")));
        }
    }
    for e in &edges {
        let (from0, to0) = (row_str(e, "from_note_id"), row_str(e, "to_note_id"));
        if !loser_ids.contains(from0) && !loser_ids.contains(to0) {
            continue;
        }
        let from = if loser_ids.contains(from0) {
            sid
        } else {
            from0
        };
        let to = if loser_ids.contains(to0) { sid } else { to0 };
        let rel = row_str(e, "relation_type");
        let key = edge_key(from, to, rel);
        let eid = row_str(e, "id").to_string();
        let mut patch = Map::new();
        patch.insert("id".into(), json!(eid));
        // Full NOT-NULL shape on every staged row (SUR-954): `note_links` has no sparse-payload
        // PATCH flush fallback (`push.rs` patches `notes` only), so a `{id, deleted}` tombstone or a
        // `created_at`-less repoint 23502s on the PostgREST upsert once this edge's create has
        // already flushed — wedging the outbox forever. Mirror `replace_handwritten_annotations`'
        // edge-tombstone shape: `relation_type` + `created_at` preserved from the stored row, fresh
        // `updated_at`. `created_at` is already in hand from `list_live` — no extra read.
        patch.insert("relation_type".into(), json!(rel));
        patch.insert(
            "created_at".into(),
            e.get("created_at").cloned().unwrap_or_else(|| json!(now)),
        );
        patch.insert("updated_at".into(), json!(now));
        if from == to || seen.contains(&key) {
            // Self-loop (a loser linked to the survivor) or a duplicate of an existing edge → drop.
            // Keep the stored from/to (the row's real identity), as the SUR-952 tombstone does.
            patch.insert("from_note_id".into(), json!(from0));
            patch.insert("to_note_id".into(), json!(to0));
            patch.insert("deleted".into(), json!(true));
        } else {
            seen.insert(key);
            patch.insert("from_note_id".into(), json!(from));
            patch.insert("to_note_id".into(), json!(to));
        }
        // Propagate (do NOT swallow): the caller must not soft-delete a loser whose edge failed to
        // re-point, or the edge would be stranded live against a tombstoned note (see
        // `merge_into_survivor`'s ordering invariant).
        store
            .stage_local_write("note_links", &eid, patch, now)
            .map_err(|e| format!("repoint edge {eid}: {e}"))?;
    }
    Ok(())
}

/// Re-point every live `collection_membership` off a loser onto the survivor. A membership id is
/// the deterministic `{collection_id}:{note_id}`, so the loser's row can't be mutated in place;
/// instead tombstone it and ensure the survivor has exactly one LIVE membership in that collection
/// (reactivate its own deterministic row, preserving `created_at`, else create it). Mirrors the
/// `mergeNotes` membership block.
fn repoint_memberships(
    store: &Store,
    sid: &str,
    loser_ids: &BTreeSet<String>,
    now: i64,
) -> Result<(), String> {
    let memberships = store
        .list_live("collection_memberships", None, -1, 0)
        .map_err(|e| format!("list collection_memberships: {e}"))?;
    let mut survivor_collections: HashSet<String> = memberships
        .iter()
        .filter(|m| row_str(m, "note_id") == sid)
        .map(|m| row_str(m, "collection_id").to_string())
        .collect();

    for m in &memberships {
        if !loser_ids.contains(row_str(m, "note_id")) {
            continue;
        }
        let mid = row_str(m, "id").to_string();
        let cid = row_str(m, "collection_id").to_string();
        // Tombstone the loser's membership — its un-filing must propagate.
        let mut tomb = Map::new();
        tomb.insert("id".into(), json!(mid));
        tomb.insert("deleted".into(), json!(true));
        tomb.insert("updated_at".into(), json!(now));
        // Propagate (see `merge_into_survivor`'s ordering invariant): a loser must not be
        // soft-deleted while its collection membership is still un-tombstoned.
        store
            .stage_local_write("collection_memberships", &mid, tomb, now)
            .map_err(|e| format!("tombstone membership {mid}: {e}"))?;
        if !survivor_collections.insert(cid.clone()) {
            continue; // survivor already filed (live) in this collection — nothing to add.
        }
        let smid = format!("{cid}:{sid}");
        let existing_created = store
            .get_row("collection_memberships", &smid)
            .map_err(|e| format!("get membership {smid}: {e}"))?
            .and_then(|r| r.get("created_at").and_then(Value::as_i64));
        let mut rec = Map::new();
        rec.insert("id".into(), json!(smid));
        rec.insert("note_id".into(), json!(sid));
        rec.insert("collection_id".into(), json!(cid));
        rec.insert("created_at".into(), json!(existing_created.unwrap_or(now)));
        rec.insert("updated_at".into(), json!(now));
        rec.insert("deleted".into(), json!(false));
        store
            .stage_local_write("collection_memberships", &smid, rec, now)
            .map_err(|e| format!("survivor membership {smid}: {e}"))?;
    }
    Ok(())
}

/// Step 2f (SUR-976) — retire the `note_signals` row of any note whose LOCAL row is tombstoned.
/// Closes the cross-device half of the orphaned-signals leak that `record_note_signal`'s
/// visibility guard (same-device, SUR-966) and `enqueue_note`'s atomic delete (same-device,
/// SUR-975) cannot: under whole-row LWW a not-yet-pulled device's later signal legitimately wins
/// the cloud row back from the deleting device's tombstone (the server's `t01_lww_guard` accepts
/// equal-or-newer), and nothing else ever retires it. The same sweep catches every other door —
/// same-cycle content-dedup merge losers (hence this pass's slot right after it), retired margin
/// children, pre-SUR-975 crash strands, and imported orphans.
///
/// LOCAL-ONLY RULE (founder decision): a signals row whose `notes` row is ABSENT is left alone —
/// the pull tombstone-skip makes absent genuinely ambiguous ("never synced down" vs "deleted
/// elsewhere"), and retiring a not-yet-pulled live note's signals would destroy real evidence.
/// Such rows wait until the note itself arrives (live: row is legitimate; tombstoned: retired
/// next pass) — or persist as an INERT live row when no tombstone-holding device survives to
/// retire them (the tombstone-skip never delivers the note tombstone here): an accepted residual,
/// harmless because the row cannot grow and nothing renders it.
///
/// Each retirement stages the full-shape tombstone ([`SyncEngine::build_signals_tombstone`] —
/// counters preserved verbatim, no counter-folding for merge losers, founder decision) through
/// [`Store::stage_local_write`], so it PROPAGATES via the outbox and the same
/// `pull_then_flush` that ran this pass converges the cloud row too. The WRITE side is per-row
/// isolated (a build/stage failure on one row is logged and must not block the rest — the
/// `merge_into_survivor` posture); a scan-level READ failure aborts the pass, which is itself
/// best-effort and retries next pull. Idempotent by scan construction (a retired row is no
/// longer live, so a second pass sees nothing and stages nothing).
fn reconcile_note_signals(store: &Store) -> Result<usize, String> {
    let live_signals = store
        .list_live("note_signals", None, -1, 0)
        .map_err(|e| e.to_string())?;
    let now = super::epoch_ms();
    let mut retired = 0usize;
    for sig in live_signals {
        let Some(note_id) = sig.get("note_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(note) = store.get_row("notes", note_id).map_err(|e| e.to_string())? else {
            continue; // absent note — ambiguous, the local-only rule leaves it (see doc above)
        };
        if !matches!(note.get("deleted"), Some(Value::Bool(true))) {
            continue; // live note — its signals row is legitimate evidence
        }
        // The tombstoned notes row is in hand, so the (rarely-needed) birth-prior fallback derives
        // from its real `source` — never the unknown-source 0.5 (the lookup deliberately reads a
        // dead row's columns, same as an ordinary delete).
        let note_source = note.get("source").and_then(Value::as_str);
        match SyncEngine::build_signals_tombstone(store, note_id, note_source, now) {
            // None can't occur off a `list_live` scan under the engine's held store lock (the row
            // was live moments ago) — but the guard costs nothing and stays correct if it ever does.
            Ok(None) => {}
            Ok(Some(tomb)) => match store.stage_local_write("note_signals", note_id, tomb, now) {
                Ok(()) => retired += 1,
                Err(e) => eprintln!(
                    "reconcile: signals-retire stage for {note_id} failed (non-fatal, retries next pull): {e}"
                ),
            },
            Err(e) => eprintln!(
                "reconcile: signals-retire build for {note_id} failed (non-fatal, retries next pull): {e}"
            ),
        }
    }
    Ok(retired)
}

/// Step 2g (SUR-828) — resolve Open Library book covers for coverless books (SUR-198 parity for
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

// ── SUR-915: duplicate-resolution merge verbs ────────────────────────────────
// The host-invoked merge contract both native consumers (SUR-863 iOS / SUR-877 Android) build
// against. `merge_books` + `unmerge_books` are the byte-mirror of the PWA's `mergeBooks` /
// `unmergeBooks` (`surfc/src/db.js`); `merge_content_duplicates` is a checked, explicit-survivor
// wrapper over the existing `merge_into_survivor` (the automatic-dedup content merge). All three
// are KEY-FREE store-level patches — no vault, no re-seal — so a moved note's `content_tag` is
// nulled for the existing self-heal to re-derive, never recomputed here.

/// One note's pre-merge home, for [`unmerge_books`] — mirrors a PWA `undo.reassignments` entry
/// (`{noteId, fromBookId}`). `prior_book_id` is nullable to round-trip the column faithfully,
/// though a rehomed note always had a (loser) book.
#[derive(Debug, Clone, uniffi::Record)]
pub struct NoteBookAssignment {
    pub note_id: String,
    pub prior_book_id: Option<String>,
}

/// The ephemeral undo token [`merge_books`] returns and [`unmerge_books`] consumes — the exact
/// inverse state the PWA captures in `mergeBooks`' `undo` object. The host holds it for its
/// 10-second undo window; core does NOT persist it, so an app restart mid-window forfeits undo (the
/// timer is host UX — core guarantees only the operation).
#[derive(Debug, Clone, uniffi::Record)]
pub struct BookMergeUndo {
    pub survivor_id: String,
    pub loser_ids: Vec<String>,
    pub survivor_prior_created_at: Option<i64>,
    pub reassignments: Vec<NoteBookAssignment>,
}

/// Filter+dedupe a loser-id list, dropping empties and the survivor (mirrors the oracle's
/// `.filter(id => id && id !== survivorId)`), preserving first-seen order.
fn clean_loser_ids(survivor_id: &str, loser_ids: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    loser_ids
        .iter()
        .filter(|id| !id.is_empty() && id.as_str() != survivor_id && seen.insert((*id).clone()))
        .cloned()
        .collect()
}

/// Merge duplicate source BOOKS into `survivor_id` (SUR-915) — the byte-mirror of the PWA's
/// `mergeBooks`. Rehomes every live note off each loser onto the survivor (narrow `book_id` +
/// `content_tag=null` patch, so decrypt-failed notes rehome too — no re-seal), keeps the earliest
/// `created_at` on the survivor, tombstones the losers, and records the loser→survivor redirects in
/// the device-local `mergedBookIds` map so the fleet + decrypt-failed stragglers converge via
/// [`reconcile_stranded_notes`] on their next pull. Returns the [`BookMergeUndo`] snapshot.
///
/// **Known residual (SUR-916, matches the PWA's own deferral).** The redirect map is device-local,
/// so a note that lives on a device which never received the map AND that this device never saw
/// (created offline elsewhere, or not-yet-pulled here at merge time) can't be resolved by that
/// device: it pulls the loser tombstone with an empty map and `reconcile_stranded_notes` DETACHES
/// it (`book_id=null`) rather than rehoming it to the survivor. Full always-to-survivor convergence
/// (a synced redirect or a server-side rehome) is tracked in SUR-916 — the native equivalent of the
/// PWA's deferred server-side merge; native ships at parity with the web here, not behind it.
///
/// **Replay-safe, ordered for crash-safety.** The core can't span one SQLite transaction across the
/// separate outbox writes the oracle does in one Dexie transaction, so it ORDERS the writes and
/// fail-fasts (like [`merge_into_survivor`]):
/// 1. redirects are recorded FIRST — an interrupted merge still converges (a stranded note resolves
///    to the survivor through the map even if the tombstone never landed), and the insert is
///    idempotent (preserves existing entries);
/// 2. notes are rehomed next, each capturing its prior book into the undo token;
/// 3. losers are tombstoned LAST — only after every rehome staged (a stage failure propagates via
///    `?`, so no loser is ever tombstoned with a note still stranded on it).
///
/// A re-run of a completed merge is a no-op: a tombstoned loser contributes no LIVE notes to rehome
/// and its redirect is already present.
pub fn merge_books(
    store: &Store,
    survivor_id: &str,
    loser_ids: &[String],
) -> Result<BookMergeUndo, String> {
    let now = epoch_ms();
    if survivor_id.is_empty() {
        return Err("merge_books: empty survivor id".into());
    }
    let losers = clean_loser_ids(survivor_id, loser_ids);

    let mut undo = BookMergeUndo {
        survivor_id: survivor_id.to_string(),
        loser_ids: Vec::new(),
        survivor_prior_created_at: None,
        reassignments: Vec::new(),
    };
    if losers.is_empty() {
        return Ok(undo); // nothing to merge (mirrors the oracle's early return)
    }

    // Survivor must exist and be live — it's the merge target.
    let survivor = match store
        .get_row("books", survivor_id)
        .map_err(|e| format!("merge_books: get survivor {survivor_id}: {e}"))?
    {
        Some(b) if !matches!(b.get("deleted"), Some(Value::Bool(true))) => b,
        _ => {
            return Err(format!(
                "merge_books: survivor {survivor_id} not found or deleted"
            ))
        }
    };

    // Reject a redirect cycle: if the survivor already resolves (transitively) to one of the
    // losers, merging would make the map point in a loop and strand every note in it.
    let mut map = load_merged_book_ids(store)?;
    for lid in &losers {
        if resolve_book_id(survivor_id, &map) == *lid {
            return Err(format!(
                "merge_books: redirect cycle — survivor {survivor_id} already resolves to loser {lid}"
            ));
        }
    }

    let survivor_created = row_i64(&survivor, "created_at");
    undo.survivor_prior_created_at = Some(survivor_created);
    let mut earliest = survivor_created;

    // Snapshot the redirect map BEFORE this invocation writes it: a loser already pointing at this
    // survivor here means a PRIOR (crashed) attempt already recorded it — so its notes may already
    // have been rehomed. Used below to detect a resumed partial merge.
    let prior_map = map.clone();

    // ── 1. Record redirects FIRST (idempotent, preserves existing). ──
    for lid in &losers {
        map.insert(lid.clone(), survivor_id.to_string());
    }
    save_merged_book_ids(store, &map)?;

    // ── 2. Rehome each LIVE loser's notes; capture undo; track earliest created_at. Two kinds of
    // loser are completed but deliberately kept OUT of the undo token, so `unmerge_books` on the
    // returned token can never half-undo:
    //   • missing / already soft-deleted (a completed-merge retry) — skipped entirely; its redirect
    //     stays recorded (step 1) so a later pull still converges;
    //   • a RESUMED merge — a live loser with ANY prior mapping (to this survivor OR a different one
    //     a crashed earlier attempt chose), meaning some of its notes may already have moved
    //     elsewhere (we can't tell which, or where). We complete it — rehome whatever notes are still
    //     live, then tombstone — but leave it un-undoable: the full set of reassignments is
    //     unrecoverable, so an undo could only move back the suffix THIS retry saw and would strand
    //     the earlier-moved notes.
    // Every loser we tombstone goes in `to_tombstone`; only faithfully-undoable ones (and their note
    // reassignments) go in `undo.loser_ids` / `undo.reassignments`. ──
    // Each entry carries the loser's stored row — the tombstone stages its full shape
    // (see the SUR-1005 wire-shape comment at step 3).
    let mut to_tombstone: Vec<(String, Map<String, Value>)> = Vec::new();
    for lid in &losers {
        let loser = match store
            .get_row("books", lid)
            .map_err(|e| format!("merge_books: get loser {lid}: {e}"))?
        {
            Some(b) if !matches!(b.get("deleted"), Some(Value::Bool(true))) => b,
            _ => continue, // missing or already-deleted — nothing for this invocation to merge
        };
        earliest = earliest.min(row_i64(&loser, "created_at"));

        // ANY prior mapping for this loser marks a resumed merge — a crashed earlier attempt (to
        // THIS survivor or a different one) may have already moved some of its notes. Complete it,
        // but it can't be undone: the full reassignment set isn't knowable here.
        let resumed = prior_map.contains_key(lid);

        let notes = store
            .list_live("notes", Some(("book_id", lid)), -1, 0)
            .map_err(|e| format!("merge_books: list notes for {lid}: {e}"))?;
        for note in &notes {
            let note_id = row_str(note, "id").to_string();
            let mut patch = Map::new();
            patch.insert("id".into(), json!(note_id));
            patch.insert("book_id".into(), json!(survivor_id));
            patch.insert("content_tag".into(), Value::Null); // SUR-638: stale on book change
            patch.insert("updated_at".into(), json!(now));
            store
                .stage_local_write("notes", &note_id, patch, now)
                .map_err(|e| format!("merge_books: rehome note {note_id}: {e}"))?;
            if !resumed {
                undo.reassignments.push(NoteBookAssignment {
                    note_id,
                    prior_book_id: Some(lid.clone()),
                });
            }
        }
        to_tombstone.push((lid.clone(), loser));
        if !resumed {
            undo.loser_ids.push(lid.clone());
        }
    }

    // Nothing live was merged this invocation (all losers missing/already-deleted): skip the
    // survivor bump + tombstone loop and return the (empty) undo token, which `unmerge_books` treats
    // as a no-op. The redirect map was still (idempotently) recorded above.
    if to_tombstone.is_empty() {
        return Ok(undo);
    }

    // ── 3. Survivor keeps the earliest created_at across the cluster (mirrors the oracle, which
    // always writes it — LWW-safe even when unchanged). ──
    //
    // SUR-1005 — every books patch below stages the STORED row's full shape with the changes
    // overlaid, not a sparse patch. Two reasons, both oracle contracts: (a) the PWA's
    // `upsertBook` always sends the full record, so sparse core payloads were a wire
    // divergence; (b) `books.title`/`created_at` are NOT NULL without defaults, and a
    // PostgREST upsert NOT-NULL-checks its INSERT candidate before conflict resolution — a
    // sparse patch 23502-wedges the outbox the moment the book's own full-shape create is no
    // longer queued in front of it (any PULLED book — the SUR-954 note_links class exactly).
    // Wire-payload-only: `stage_local_write` already merges partials onto the stored row
    // locally, so carrying the stored columns changes nothing local.
    let mut sp = survivor.clone();
    sp.insert("created_at".into(), json!(earliest));
    sp.insert("updated_at".into(), json!(now));
    store
        .stage_local_write("books", survivor_id, sp, now)
        .map_err(|e| format!("merge_books: stage survivor created_at: {e}"))?;

    // ── 4. Tombstone every merged loser (undoable or resumed) — only now that every rehome staged. ──
    for (lid, loser_row) in &to_tombstone {
        let mut patch = loser_row.clone(); // full stored shape (see the step-3 wire-shape comment)
        patch.insert("deleted".into(), json!(true));
        // SUR-1005 — the synced loser→survivor pointer (SUR-916 Option 1): rides the
        // tombstone fleet-wide so a device without this merge's local map still converges
        // stragglers via `reconcile_stranded_notes` (and the PWA via rehomeStrandedNotes).
        patch.insert("merged_into".into(), json!(survivor_id));
        patch.insert("updated_at".into(), json!(now));
        store
            .stage_local_write("books", lid, patch, now)
            .map_err(|e| format!("merge_books: tombstone loser {lid}: {e}"))?;
    }

    Ok(undo)
}

/// Reverse a [`merge_books`] within the host's undo window — the inverse of the PWA's
/// `unmergeBooks`. Narrow restores only: each reassignment's note returns to its `prior_book_id`
/// (`content_tag` nulled to re-derive), each loser book is un-tombstoned, the survivor's prior
/// `created_at` is restored, and ONLY the `mergedBookIds` entries still pointing at THIS merge's
/// survivor are removed (a later merge into the same survivor keeps its own entries). Idempotent.
pub fn unmerge_books(store: &Store, undo: &BookMergeUndo) -> Result<(), String> {
    if undo.loser_ids.is_empty() {
        return Ok(());
    }
    let now = epoch_ms();

    for r in &undo.reassignments {
        let mut patch = Map::new();
        patch.insert("id".into(), json!(r.note_id));
        patch.insert(
            "book_id".into(),
            r.prior_book_id.clone().map_or(Value::Null, Value::String),
        );
        patch.insert("content_tag".into(), Value::Null);
        patch.insert("updated_at".into(), json!(now));
        store
            .stage_local_write("notes", &r.note_id, patch, now)
            .map_err(|e| format!("unmerge_books: restore note {}: {e}", r.note_id))?;
    }

    for lid in &undo.loser_ids {
        // Resurrect the loser ATOMICALLY: the outbox collapse makes `deleted` sticky ("delete wins"
        // — SUR-724), so a resurrection staged behind an un-flushed merge tombstone would flush as
        // `deleted:true`. `stage_local_write_resurrecting` drops the pending tombstone and stages the
        // `deleted:false` write in ONE transaction — a crash can't leave the row soft-deleted with
        // the tombstone gone but the resurrection unqueued (the ephemeral undo token can't retry).
        // Full stored shape (the merge_books step-3 wire-shape contract) — the loser row
        // always exists locally (this device merged it); a missing row falls back to the
        // sparse patch rather than failing the undo.
        let mut patch = store
            .get_row("books", lid)
            .map_err(|e| format!("unmerge_books: get loser {lid}: {e}"))?
            .unwrap_or_else(|| {
                let mut p = Map::new();
                p.insert("id".into(), json!(lid));
                p
            });
        patch.insert("deleted".into(), json!(false));
        // SUR-1005 — clear the synced pointer so the undo propagates fleet-wide: a device
        // that already pulled `merged_into` stops rehoming stragglers onto the survivor.
        patch.insert("merged_into".into(), Value::Null);
        patch.insert("updated_at".into(), json!(now));
        store
            .stage_local_write_resurrecting("books", lid, patch, now)
            .map_err(|e| format!("unmerge_books: resurrect loser {lid}: {e}"))?;
    }

    // Load the redirect map once — used to recompute the survivor's created_at over the STILL-merged
    // cluster and then to prune this merge's entries.
    let mut map = load_merged_book_ids(store)?;

    // Restore the survivor's created_at to the EARLIEST of its pre-merge value and any loser STILL
    // merged into it. A LATER merge into the same survivor may have lowered it further; restoring
    // unconditionally to this token's captured value would clobber that, leaving the survivor no
    // longer holding the current cluster's earliest. "Still merged" = redirects pointing at this
    // survivor that this undo is NOT removing. (Undoing every same-survivor merge strictly out of
    // order can still leave an intermediate value — an accepted 10s-window edge; the common LIFO and
    // the older-while-newer-stands cases both resolve correctly.)
    if let Some(prev) = undo.survivor_prior_created_at {
        let mut earliest = prev;
        for (lid, sid) in &map {
            if sid == &undo.survivor_id && !undo.loser_ids.contains(lid) {
                if let Some(b) = store
                    .get_row("books", lid)
                    .map_err(|e| format!("unmerge_books: get still-merged loser {lid}: {e}"))?
                {
                    earliest = earliest.min(row_i64(&b, "created_at"));
                }
            }
        }
        // Full stored shape (the merge_books step-3 wire-shape contract).
        let mut sp = store
            .get_row("books", &undo.survivor_id)
            .map_err(|e| format!("unmerge_books: get survivor {}: {e}", undo.survivor_id))?
            .unwrap_or_else(|| {
                let mut p = Map::new();
                p.insert("id".into(), json!(undo.survivor_id));
                p
            });
        sp.insert("created_at".into(), json!(earliest));
        sp.insert("updated_at".into(), json!(now));
        store
            .stage_local_write("books", &undo.survivor_id, sp, now)
            .map_err(|e| format!("unmerge_books: restore survivor created_at: {e}"))?;
    }

    // Prune only redirects still pointing at THIS survivor.
    for lid in &undo.loser_ids {
        if map.get(lid).map(String::as_str) == Some(undo.survivor_id.as_str()) {
            map.remove(lid);
        }
    }
    save_merged_book_ids(store, &map)?;

    Ok(())
}

/// The manual/user-selected content merge (SUR-915): collapse the `loser_ids` note duplicates into
/// `survivor_id` via [`merge_into_survivor`], with the survivor chosen by the HOST rather than the
/// dedup pass's deterministic pick. A checked wrapper — it loads the LIVE rows and validates them.
///
/// `allow_cross_cluster` gates the host's two detection modes (SUR-877): the EXACT path (`false`)
/// requires every selected note to share one non-empty `content_tag` — the same invariant the
/// automatic [`reconcile_content_dupes`] relies on; the FUZZY path (`true`, a 0.92 title-similarity
/// match the host surfaced) crosses `content_tag` clusters by definition, so the cluster check is
/// skipped. Returns the number of losers collapsed.
///
/// Inherits [`merge_into_survivor`]'s best-effort atomicity: it can't span one SQLite transaction
/// across the separate outbox writes, so a mid-merge stage failure leaves the survivor patched but a
/// loser un-tombstoned. That's convergent, not corrupt — the automatic dedup pass re-collapses the
/// still-live cluster on the next pull. The host treats a returned error as "retry / saved locally,
/// sync pending" (the SUR-915 sync-integration contract).
pub fn merge_content_duplicates(
    store: &Store,
    survivor_id: &str,
    loser_ids: &[String],
    allow_cross_cluster: bool,
) -> Result<usize, String> {
    if survivor_id.is_empty() {
        return Err("merge_content_duplicates: empty survivor id".into());
    }
    let loser_ids = clean_loser_ids(survivor_id, loser_ids);
    if loser_ids.is_empty() {
        return Ok(0);
    }

    let survivor = match store
        .get_row("notes", survivor_id)
        .map_err(|e| format!("merge_content_duplicates: get survivor {survivor_id}: {e}"))?
    {
        Some(n) if !matches!(n.get("deleted"), Some(Value::Bool(true))) => n,
        _ => {
            return Err(format!(
                "merge_content_duplicates: survivor note {survivor_id} not found or deleted"
            ))
        }
    };

    let mut losers = Vec::with_capacity(loser_ids.len());
    for lid in &loser_ids {
        match store
            .get_row("notes", lid)
            .map_err(|e| format!("merge_content_duplicates: get loser {lid}: {e}"))?
        {
            Some(n) if !matches!(n.get("deleted"), Some(Value::Bool(true))) => losers.push(n),
            _ => {
                return Err(format!(
                    "merge_content_duplicates: loser note {lid} not found or deleted"
                ))
            }
        }
    }

    if !allow_cross_cluster {
        // EXACT path: every selected note must share ONE non-empty content_tag (the fuzzy path sets
        // allow_cross_cluster and skips this — a fuzzy match legitimately spans clusters).
        let tag = row_str(&survivor, "content_tag");
        if tag.is_empty() {
            return Err(format!(
                "merge_content_duplicates: survivor note {survivor_id} has no content_tag \
                 (an exact merge requires a shared cluster; set allow_cross_cluster for a fuzzy merge)"
            ));
        }
        for l in &losers {
            if row_str(l, "content_tag") != tag {
                return Err(format!(
                    "merge_content_duplicates: loser note {} is not in the survivor's content_tag \
                     cluster (set allow_cross_cluster for a fuzzy merge)",
                    row_str(l, "id")
                ));
            }
        }
    }

    merge_into_survivor(store, &survivor, &losers)
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
        async fn fetch_by_ids(
            &self,
            table: &str,
            primary_key: &str,
            ids: &[String],
        ) -> Result<Vec<Value>, String> {
            assert_eq!(primary_key, "id");
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

        // Self-heal is a no-op here: the fixtures' `text` is fake ciphertext ("enc:v2:x") that
        // can't decrypt, so every tagless note is skipped and no tag is re-derived.
        let vault = Vault::generate();
        let first = block(reconcile(&store, &sink, "user-1", &vault)).unwrap();
        assert_eq!(
            first,
            ReconcileResult {
                books_backfilled: 1, // only "missing-book" was actually absent
                notes_rehomed: 1,
                notes_detached: 0,
                ideas_created: 1,
                dupes_collapsed: 0, // the fixtures carry no content_tag, so nothing to dedup
                signals_retired: 0, // no note_signals rows seeded — nothing to retire
                covers_resolved: 0, // kill-switch off — no cover work this pass
            }
        );

        let second = block(reconcile(&store, &sink, "user-1", &vault)).unwrap();
        assert_eq!(
            second,
            ReconcileResult::default(),
            "a second pass over an already-reconciled store changes nothing"
        );
    }

    // ── reconcile_content_dupes (SUR-835) ────────────────────────────────────

    /// A note carrying a `content_tag` (+ optional `image_path`) for the dedup tests.
    fn cnote(id: &str, tag: &str, tags: &[&str], created_at: i64, image: Option<&str>) -> Value {
        json!({
            "id": id, "book_id": null, "text": "enc:v2:x", "tags": tags,
            "content_tag": tag, "image_path": image,
            "created_at": created_at, "updated_at": created_at, "deleted": false
        })
    }

    fn put(store: &Store, table: &str, row: &Value) {
        store.apply_row(table, row.as_object().unwrap()).unwrap();
    }

    fn is_deleted(store: &Store, table: &str, id: &str) -> bool {
        matches!(
            store.get_row(table, id).unwrap().unwrap().get("deleted"),
            Some(Value::Bool(true))
        )
    }

    fn live_ids(store: &Store, table: &str) -> Vec<String> {
        store
            .list_live(table, None, -1, 0)
            .unwrap()
            .iter()
            .map(|r| row_str(r, "id").to_string())
            .collect()
    }

    #[test]
    fn collapses_a_pair_keeping_the_note_with_more_tags() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &cnote("n1", "T", &["a"], 1, None));
        put(&store, "notes", &cnote("n2", "T", &["a", "b"], 2, None)); // more tags → survivor

        let collapsed = reconcile_content_dupes(&store).unwrap();

        assert_eq!(collapsed, 1);
        assert_eq!(live_ids(&store, "notes"), vec!["n2"]);
        assert!(is_deleted(&store, "notes", "n1"));
    }

    #[test]
    fn tie_on_tag_count_breaks_to_earliest_created_at() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &cnote("late", "T", &["x"], 5, None));
        put(&store, "notes", &cnote("early", "T", &["x"], 3, None)); // earliest → survivor
        put(&store, "notes", &cnote("mid", "T", &["x"], 4, None));

        assert_eq!(reconcile_content_dupes(&store).unwrap(), 2);
        assert_eq!(live_ids(&store, "notes"), vec!["early"]);
    }

    #[test]
    fn full_tie_breaks_to_lowest_id_for_cross_device_convergence() {
        let store = Store::open_in_memory().unwrap();
        // Same tag-count AND same created_at: only the id tiebreak decides — deterministically.
        put(&store, "notes", &cnote("bbb", "T", &["x"], 5, None));
        put(&store, "notes", &cnote("aaa", "T", &["x"], 5, None)); // lowest id → survivor

        assert_eq!(reconcile_content_dupes(&store).unwrap(), 1);
        assert_eq!(live_ids(&store, "notes"), vec!["aaa"]);
    }

    #[test]
    fn two_devices_in_different_insert_order_converge_on_the_same_survivor() {
        let mk = |order: [&str; 3]| {
            let store = Store::open_in_memory().unwrap();
            let rows = std::collections::HashMap::from([
                ("p", cnote("p", "T", &["a", "b"], 2, None)),
                ("q", cnote("q", "T", &["a"], 1, None)),
                ("r", cnote("r", "T", &["a"], 3, None)),
            ]);
            for id in order {
                put(&store, "notes", &rows[id]);
            }
            reconcile_content_dupes(&store).unwrap();
            live_ids(&store, "notes")
        };
        // "p" has the most tags → survivor, regardless of the order rows landed locally.
        assert_eq!(mk(["p", "q", "r"]), vec!["p"]);
        assert_eq!(mk(["r", "q", "p"]), vec!["p"]);
    }

    #[test]
    fn survivor_gets_the_union_of_all_tags_order_preserved() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &cnote("n1", "T", &["a", "b"], 1, None)); // tie count, earliest → survivor
        put(&store, "notes", &cnote("n2", "T", &["b", "c"], 2, None));

        reconcile_content_dupes(&store).unwrap();

        let tags = store.get_row("notes", "n1").unwrap().unwrap()["tags"].clone();
        assert_eq!(
            tags,
            json!(["a", "b", "c"]),
            "survivor's order first, losers' new tags appended"
        );
    }

    #[test]
    fn survivor_adopts_a_losers_image_only_when_it_has_none() {
        // Survivor lacks an image → adopts the loser's.
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &cnote("s", "T", &["a", "b"], 1, None));
        put(&store, "notes", &cnote("l", "T", &["a"], 2, Some("img-l")));
        reconcile_content_dupes(&store).unwrap();
        assert_eq!(
            store.get_row("notes", "s").unwrap().unwrap()["image_path"],
            json!("img-l")
        );

        // Survivor already has an image → keeps its own.
        let store = Store::open_in_memory().unwrap();
        put(
            &store,
            "notes",
            &cnote("s", "T", &["a", "b"], 1, Some("img-s")),
        );
        put(&store, "notes", &cnote("l", "T", &["a"], 2, Some("img-l")));
        reconcile_content_dupes(&store).unwrap();
        assert_eq!(
            store.get_row("notes", "s").unwrap().unwrap()["image_path"],
            json!("img-s")
        );
    }

    fn edge(id: &str, from: &str, to: &str, rel: &str) -> Value {
        json!({
            "id": id, "from_note_id": from, "to_note_id": to, "relation_type": rel,
            "created_at": 1, "updated_at": 1, "deleted": false
        })
    }

    /// The staged outbox PAYLOAD (not the local row) for a `note_links` id. The local row stays
    /// complete through a sparse `stage_local_write` merge — the SUR-954 defect is visible ONLY in
    /// the payload the flush pushes, so a row-state assertion can't see it.
    fn staged_note_link(store: &Store, id: &str) -> Map<String, Value> {
        store
            .outbox_items()
            .unwrap()
            .into_iter()
            .filter(|r| r.1 == "note_links")
            .map(|r| serde_json::from_str::<Map<String, Value>>(&r.3).unwrap())
            .find(|p| p.get("id") == Some(&json!(id)))
            .unwrap_or_else(|| panic!("no staged note_links payload for {id}"))
    }

    #[test]
    fn note_links_repoint_to_survivor_dropping_self_loops_and_duplicates() {
        let store = Store::open_in_memory().unwrap();
        // S survives, L is the loser (more tags on S). X is an unrelated note.
        put(&store, "notes", &cnote("S", "T", &["a", "b"], 1, None));
        put(&store, "notes", &cnote("L", "T", &["a"], 2, None));
        put(
            &store,
            "notes",
            &json!({ "id": "X", "text": "enc:v2:x", "tags": [],
            "created_at": 1, "updated_at": 1, "deleted": false }),
        );
        put(&store, "note_links", &edge("e1", "X", "L", "ref")); // → repoint to X→S
        put(&store, "note_links", &edge("e2", "L", "S", "ref")); // → self-loop S→S, dropped
        put(&store, "note_links", &edge("e3", "X", "S", "dup")); // pre-existing
        put(&store, "note_links", &edge("e4", "X", "L", "dup")); // → dup of e3, dropped

        reconcile_content_dupes(&store).unwrap();

        let e1 = store.get_row("note_links", "e1").unwrap().unwrap();
        assert_eq!(e1["from_note_id"], json!("X"));
        assert_eq!(e1["to_note_id"], json!("S"), "e1 repointed L→S");
        assert!(!is_deleted(&store, "note_links", "e1"));
        assert!(is_deleted(&store, "note_links", "e2"), "self-loop dropped");
        assert!(
            is_deleted(&store, "note_links", "e4"),
            "duplicate edge dropped"
        );
        assert!(
            !is_deleted(&store, "note_links", "e3"),
            "pre-existing edge untouched"
        );
        assert_eq!(
            store.get_row("note_links", "e3").unwrap().unwrap()["updated_at"],
            json!(1),
            "an untouched edge keeps its updated_at (never staged)"
        );
    }

    #[test]
    fn two_losers_sharing_a_duplicate_edge_collapse_to_one_deterministically() {
        let store = Store::open_in_memory().unwrap();
        // S survives (most tags); L1, L2 are both losers in the same content-tag cluster.
        put(&store, "notes", &cnote("S", "T", &["a", "b", "c"], 1, None));
        put(&store, "notes", &cnote("L1", "T", &["a"], 2, None));
        put(&store, "notes", &cnote("L2", "T", &["a"], 3, None));
        put(
            &store,
            "notes",
            &json!({ "id": "X", "text": "enc:v2:x", "tags": [],
            "created_at": 1, "updated_at": 1, "deleted": false }),
        );
        // Both losers link to X with the same relation → both re-point to X→S "ref": one must be
        // kept, one dropped. Insert e2 first so list_live's created_at/id-DESC order would surface
        // it before e1 — proving the id-ascending re-sort (not raw scan order) decides the keeper.
        put(&store, "note_links", &edge("e2", "X", "L2", "ref"));
        put(&store, "note_links", &edge("e1", "X", "L1", "ref"));

        reconcile_content_dupes(&store).unwrap();

        // Exactly one live X→S "ref" edge survives, and it's the lowest-id one (e1) — the SAME edge
        // the PWA (Dexie id-asc) keeps, so two devices converge instead of leaving a dup or a gap.
        assert_eq!(live_ids(&store, "note_links"), vec!["e1"]);
        assert_eq!(
            store.get_row("note_links", "e1").unwrap().unwrap()["to_note_id"],
            json!("S")
        );
        assert!(is_deleted(&store, "note_links", "e2"));
    }

    #[test]
    fn repointed_and_tombstoned_edges_stage_the_full_not_null_shape() {
        // SUR-954: `repoint_note_links` must stage the server's NOT-NULL columns
        // (from/to/relation_type/created_at) on BOTH the repoint and the tombstone. `note_links`
        // has no sparse-PATCH flush fallback (`push.rs` patches `notes` only), so once an edge's
        // create has flushed, a sparse merge payload stands alone as a fresh INSERT candidate and
        // 23502s on every flush — wedging the outbox. Seeding via `apply_row` enqueues nothing, so
        // the merge payload IS alone here: the exact post-flush wire condition.
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &cnote("S", "T", &["a", "b"], 1, None));
        put(&store, "notes", &cnote("L", "T", &["a"], 2, None));
        put(
            &store,
            "notes",
            &json!({ "id": "X", "text": "enc:v2:x", "tags": [],
            "created_at": 1, "updated_at": 1, "deleted": false }),
        );
        put(&store, "note_links", &edge("e1", "X", "L", "ref")); // → repoint to X→S (live)
        put(&store, "note_links", &edge("e2", "L", "S", "ref")); // → self-loop S→S, tombstoned

        reconcile_content_dupes(&store).unwrap();

        // Every staged edge — repoint AND tombstone — carries the full NOT-NULL shape.
        for id in ["e1", "e2"] {
            let p = staged_note_link(&store, id);
            for col in [
                "id",
                "from_note_id",
                "to_note_id",
                "relation_type",
                "created_at",
                "updated_at",
            ] {
                assert!(
                    p.contains_key(col),
                    "{id} outbox payload missing NOT-NULL column `{col}`: {p:?}"
                );
            }
            assert!(!p["from_note_id"].is_null() && !p["to_note_id"].is_null());
            assert_eq!(
                p["created_at"],
                json!(1),
                "{id} preserves the STORED created_at, not a fresh stamp"
            );
            assert_eq!(p["relation_type"], json!("ref"));
        }

        // The repoint carries the redirected target and stays live (no sticky tombstone).
        let e1 = staged_note_link(&store, "e1");
        assert_eq!(e1["from_note_id"], json!("X"));
        assert_eq!(e1["to_note_id"], json!("S"), "e1 repointed L→S");
        assert_ne!(e1.get("deleted"), Some(&json!(true)));
        // The tombstone keeps the stored identity (the SUR-952 `{...l, deleted: 1}` convention).
        let e2 = staged_note_link(&store, "e2");
        assert_eq!(e2["deleted"], json!(true));
        assert_eq!(e2["from_note_id"], json!("L"));
        assert_eq!(e2["to_note_id"], json!("S"));
    }

    fn membership(id: &str, note: &str, collection: &str, deleted: bool, created_at: i64) -> Value {
        json!({
            "id": id, "note_id": note, "collection_id": collection,
            "created_at": created_at, "updated_at": created_at, "deleted": deleted
        })
    }

    #[test]
    fn memberships_repoint_to_survivor_dedup_and_reactivate() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &cnote("S", "T", &["a", "b"], 1, None));
        put(&store, "notes", &cnote("L", "T", &["a"], 2, None));
        // L in c1 (survivor absent → survivor row created, reactivating a tombstone w/ its createdAt).
        put(
            &store,
            "collection_memberships",
            &membership("c1:L", "L", "c1", false, 10),
        );
        put(
            &store,
            "collection_memberships",
            &membership("c1:S", "S", "c1", true, 100),
        ); // tombstoned
           // L in c2 where S is already live → tombstone L's, no new survivor row (dedup).
        put(
            &store,
            "collection_memberships",
            &membership("c2:L", "L", "c2", false, 20),
        );
        put(
            &store,
            "collection_memberships",
            &membership("c2:S", "S", "c2", false, 30),
        );

        reconcile_content_dupes(&store).unwrap();

        assert!(
            is_deleted(&store, "collection_memberships", "c1:L"),
            "loser's c1 membership tombstoned"
        );
        let c1s = store
            .get_row("collection_memberships", "c1:S")
            .unwrap()
            .unwrap();
        assert!(
            !matches!(c1s.get("deleted"), Some(Value::Bool(true))),
            "survivor reactivated in c1"
        );
        assert_eq!(
            c1s["created_at"],
            json!(100),
            "reactivation preserves the original filing time"
        );
        assert!(
            is_deleted(&store, "collection_memberships", "c2:L"),
            "loser's c2 membership tombstoned"
        );
        assert!(
            !is_deleted(&store, "collection_memberships", "c2:S"),
            "survivor already in c2 — untouched"
        );
        assert_eq!(
            store
                .get_row("collection_memberships", "c2:S")
                .unwrap()
                .unwrap()["updated_at"],
            json!(30),
            "no redundant survivor write for a collection it's already in"
        );
    }

    #[test]
    fn a_second_pass_is_a_no_op() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &cnote("n1", "T", &["a"], 1, None));
        put(&store, "notes", &cnote("n2", "T", &["a", "b"], 2, None));

        assert_eq!(reconcile_content_dupes(&store).unwrap(), 1);
        assert_eq!(
            reconcile_content_dupes(&store).unwrap(),
            0,
            "only the survivor is live with tag T — nothing left to collapse"
        );
    }

    // ── reconcile_heal_content_tags (SUR-884) ────────────────────────────────

    /// A live note whose `text` is REAL ciphertext sealed by `vault` (so heal can decrypt it),
    /// with the `content_tag` left absent (null) — the tagless state heal is meant to repair.
    fn tagless_note(vault: &Vault, id: &str, book_id: Option<&str>, plaintext: &str) -> Value {
        json!({
            "id": id,
            "book_id": book_id,
            "text": vault.encrypt_note(Some(id.to_string()), plaintext.to_string()),
            "tags": [],
            "created_at": 1,
            "updated_at": 1,
            "deleted": false,
        })
    }

    fn stored_tag(store: &Store, id: &str) -> Option<String> {
        store
            .get_row("notes", id)
            .unwrap()
            .unwrap()
            .get("content_tag")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    #[test]
    fn heals_a_tagless_note_by_re_deriving_from_decrypted_text() {
        // The core AC: a live note with a null content_tag + decryptable text gets its tag
        // re-derived WITHOUT a user edit, and the value is exactly what enqueue_note would have
        // sealed in (self-consistent with the same vault's derivation).
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        put(
            &store,
            "notes",
            &tagless_note(&vault, "n1", Some("book-1"), "the unexamined life"),
        );

        let healed = reconcile_heal_content_tags(&store, &vault).unwrap();

        assert_eq!(healed, 1);
        let expected = vault.content_tag("the unexamined life".into(), Some("book-1".into()));
        assert_eq!(stored_tag(&store, "n1").as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn heal_then_dedup_collapses_a_pair_a_stranded_null_created() {
        // End-to-end intent: reconcile_stranded_notes nulls a rehomed note's tag; heal re-derives
        // it; dedup then collapses it against its identical twin — all in one reconcile pass. Here
        // we simulate the post-stranded state: two notes with the SAME plaintext + book, one still
        // tagged (the twin), one tag-nulled. After heal they share a tag and dedup collapses them.
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        let tag = vault.content_tag("same passage".into(), Some("b1".into()));

        // The twin keeps its tag and 2 tags (so it's the deterministic survivor).
        let mut twin = tagless_note(&vault, "keep", Some("b1"), "same passage");
        twin["content_tag"] = json!(tag);
        twin["tags"] = json!(["a", "b"]);
        put(&store, "notes", &twin);
        // The rehome-nulled duplicate: same content, no tag yet.
        put(
            &store,
            "notes",
            &tagless_note(&vault, "dupe", Some("b1"), "same passage"),
        );

        assert_eq!(reconcile_heal_content_tags(&store, &vault).unwrap(), 1);
        assert_eq!(stored_tag(&store, "dupe").as_deref(), Some(tag.as_str()));
        // Now dedup sees two notes sharing `tag` and collapses the duplicate into the survivor.
        assert_eq!(reconcile_content_dupes(&store).unwrap(), 1);
        assert_eq!(live_ids(&store, "notes"), vec!["keep"]);
        assert!(is_deleted(&store, "notes", "dupe"));
    }

    #[test]
    fn skips_a_note_whose_ciphertext_cannot_be_decrypted() {
        // Mirror the oracle's decryptError gate: a note sealed under a DIFFERENT vault can't be
        // decrypted, so it's left tagless — never fingerprinted from unreadable bytes.
        let store = Store::open_in_memory().unwrap();
        let mine = Vault::generate();
        let foreign = Vault::generate();
        put(
            &store,
            "notes",
            &tagless_note(&foreign, "n1", Some("b1"), "not mine to read"),
        );

        let healed = reconcile_heal_content_tags(&store, &mine).unwrap();

        assert_eq!(
            healed, 0,
            "an undecryptable note is skipped, not fingerprinted"
        );
        assert_eq!(stored_tag(&store, "n1"), None, "tag stays null");
    }

    #[test]
    fn leaves_an_already_tagged_note_untouched() {
        // A present, non-empty tag is never recomputed — that's what makes the pass idempotent and
        // keeps it off notes whose tag is already correct.
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        put(&store, "notes", &cnote("n1", "PRESET", &["a"], 1, None));

        assert_eq!(reconcile_heal_content_tags(&store, &vault).unwrap(), 0);
        assert_eq!(stored_tag(&store, "n1").as_deref(), Some("PRESET"));
    }

    #[test]
    fn heal_write_is_local_only_and_does_not_bump_updated_at() {
        // The convergence-critical invariant: the healed tag is written via apply_row, NOT the
        // outbox — so it never enters the LWW/sync path (notes are whole-row LWW; a bumped
        // updated_at would clobber a concurrent edit). Assert nothing is queued and updated_at is
        // preserved.
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        put(
            &store,
            "notes",
            &tagless_note(&vault, "n1", Some("b1"), "local only"),
        );
        let before = store.outbox_items().unwrap().len();

        assert_eq!(reconcile_heal_content_tags(&store, &vault).unwrap(), 1);

        assert_eq!(
            store.outbox_items().unwrap().len(),
            before,
            "healed tag must NOT be staged to the outbox (local-only, never propagated)"
        );
        let updated_at = store
            .get_row("notes", "n1")
            .unwrap()
            .unwrap()
            .get("updated_at")
            .and_then(Value::as_i64);
        assert_eq!(
            updated_at,
            Some(1),
            "updated_at must be preserved, not bumped"
        );
    }

    #[test]
    fn is_idempotent_a_second_pass_heals_nothing() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        put(
            &store,
            "notes",
            &tagless_note(&vault, "n1", None, "idempotent"),
        );

        assert_eq!(reconcile_heal_content_tags(&store, &vault).unwrap(), 1);
        assert_eq!(
            reconcile_heal_content_tags(&store, &vault).unwrap(),
            0,
            "the note is already tagged — a second pass is a no-op"
        );
    }

    #[test]
    fn empty_text_is_tagged_and_collapses_like_the_oracle() {
        // Pins the empty-text behavior (sync-reviewer ask). The oracle's guard is `n.text == null`,
        // and `"" == null` is FALSE in JS, so the PWA fingerprints empty text too — we match it:
        // `decrypt_note_text` yields `Some("")`, which is tagged (only an ABSENT text column,
        // `None`, is skipped). Two empty-text notes in the same book therefore share a tag and the
        // dedup pass collapses them — this is the same behavior `enqueue_note` already produces by
        // tagging empty text at write time (pre-existing SUR-835), not new to self-heal. Whether
        // content-dedup SHOULD exclude empty/image-only notes is a SUR-835 question, out of scope
        // here; this test just locks the current, oracle-matching behavior so it can't drift silently.
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        put(&store, "notes", &tagless_note(&vault, "e1", Some("b1"), ""));
        put(&store, "notes", &tagless_note(&vault, "e2", Some("b1"), ""));

        assert_eq!(reconcile_heal_content_tags(&store, &vault).unwrap(), 2);
        let expected = vault.content_tag(String::new(), Some("b1".into()));
        assert_eq!(stored_tag(&store, "e1").as_deref(), Some(expected.as_str()));
        assert_eq!(stored_tag(&store, "e2").as_deref(), Some(expected.as_str()));
        // ...and dedup then collapses the pair (both empty → same tag), like the oracle clusters them.
        assert_eq!(reconcile_content_dupes(&store).unwrap(), 1);
    }

    /// The AC's byte-parity assertion: heal end-to-end reproduces the PWA's SUR-638 known-answer
    /// vector (MK = 0x11*32, text "hello world", bookId "book-1" → a663…bb05). Gated on the
    /// `test-seams` fixed-MK constructor (same seam `tests/parity.rs` uses); the derivation itself
    /// is locked to this vector there too, so this proves the HEAL PATH feeds it correctly.
    #[cfg(feature = "test-seams")]
    #[test]
    fn healed_tag_byte_matches_the_pwa_sur638_vector() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::__with_raw_mk_hex(&"11".repeat(32)).unwrap();
        put(
            &store,
            "notes",
            &tagless_note(&vault, "n1", Some("book-1"), "hello world"),
        );

        assert_eq!(reconcile_heal_content_tags(&store, &vault).unwrap(), 1);
        assert_eq!(
            stored_tag(&store, "n1").as_deref(),
            Some("a6632b65607c8efb959f50d9767e862fcc231fc7cb64b4519abe393a96ccbb05"),
        );
    }

    // ── reconcile_covers (SUR-828) ───────────────────────────────────────────

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
    fn notes_without_a_content_tag_are_never_matched() {
        let store = Store::open_in_memory().unwrap();
        // Two notes, no content_tag at all — not fingerprint-matchable, must be left alone.
        put(&store, "notes", &note("n1", None, &[], 1));
        put(&store, "notes", &note("n2", None, &[], 2));
        // And an empty-string tag is treated as absent.
        put(&store, "notes", &cnote("n3", "", &[], 3, None));
        put(&store, "notes", &cnote("n4", "", &[], 4, None));

        assert_eq!(reconcile_content_dupes(&store).unwrap(), 0);
        assert_eq!(live_ids(&store, "notes").len(), 4, "nothing collapsed");
    }

    #[test]
    fn a_singleton_content_tag_is_left_untouched() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &cnote("only", "T", &["a"], 1, None));
        assert_eq!(reconcile_content_dupes(&store).unwrap(), 0);
        assert_eq!(live_ids(&store, "notes"), vec!["only"]);
    }

    #[test]
    fn an_outage_never_fails_the_overall_reconcile_pass() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &cbook("b1", "Flaky", None));
        let sink = StubSink::new().with_cover("Flaky", StubCover::Outage);

        // reconcile() wraps cover-resolution best-effort — an outage yields covers_resolved: 0,
        // never an Err.
        let r = block(reconcile(&store, &sink, "user-1", &Vault::generate())).unwrap();
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

    // ── SUR-915: merge_books / unmerge_books / merge_content_duplicates ─────

    fn book_at(id: &str, created_at: i64) -> Value {
        json!({ "id": id, "title": "T", "created_at": created_at, "updated_at": created_at, "deleted": false })
    }

    fn note_ct(id: &str, book_id: Option<&str>, content_tag: &str, tags: &[&str]) -> Value {
        json!({ "id": id, "book_id": book_id, "text": "enc:v2:x", "tags": tags,
                "content_tag": content_tag, "created_at": 1, "updated_at": 1, "deleted": false })
    }

    fn book_id_of(store: &Store, note_id: &str) -> Option<String> {
        store
            .get_row("notes", note_id)
            .unwrap()
            .unwrap()
            .get("book_id")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    #[test]
    fn merge_books_rehomes_notes_records_map_keeps_earliest_and_captures_undo() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 100));
        put(&store, "books", &book_at("l1", 50));
        put(&store, "notes", &note("n1", Some("l1"), &["a"], 1));
        put(&store, "notes", &note("n2", Some("l1"), &["b"], 2));
        put(&store, "notes", &note("keep", Some("s"), &["c"], 3));

        let undo = merge_books(&store, "s", &["l1".into()]).unwrap();

        // notes rehomed onto the survivor, content_tag nulled for re-derive.
        assert_eq!(book_id_of(&store, "n1").as_deref(), Some("s"));
        assert_eq!(book_id_of(&store, "n2").as_deref(), Some("s"));
        assert!(store.get_row("notes", "n1").unwrap().unwrap()["content_tag"].is_null());
        // loser tombstoned; survivor inherits the earliest created_at across the cluster.
        assert!(is_deleted(&store, "books", "l1"));
        assert_eq!(
            store.get_row("books", "s").unwrap().unwrap()["created_at"].as_i64(),
            Some(50)
        );
        // redirect recorded.
        assert_eq!(
            load_merged_book_ids(&store)
                .unwrap()
                .get("l1")
                .map(String::as_str),
            Some("s")
        );
        // undo token captures the inverse state.
        assert_eq!(undo.survivor_id, "s");
        assert_eq!(undo.loser_ids, vec!["l1"]);
        assert_eq!(undo.survivor_prior_created_at, Some(100));
        let mut moved: Vec<_> = undo
            .reassignments
            .iter()
            .map(|r| r.note_id.as_str())
            .collect();
        moved.sort();
        assert_eq!(moved, vec!["n1", "n2"]);
        assert!(undo
            .reassignments
            .iter()
            .all(|r| r.prior_book_id.as_deref() == Some("l1")));
    }

    #[test]
    fn merge_books_validates_dedupes_and_rejects_cycle() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 1));
        put(&store, "books", &book_at("l1", 1));

        assert!(merge_books(&store, "", &["l1".into()]).is_err()); // empty survivor
                                                                   // empty / self-only losers → no-op (survivor filtered out).
        assert!(merge_books(&store, "s", &["s".into(), "".into()])
            .unwrap()
            .loser_ids
            .is_empty());
        // missing survivor row.
        assert!(merge_books(&store, "ghost", &["l1".into()]).is_err());

        // cycle: the map already resolves s → l1, so merging l1 into s would loop.
        save_merged_book_ids(&store, &BTreeMap::from([("s".into(), "l1".into())])).unwrap();
        assert!(merge_books(&store, "s", &["l1".into()]).is_err());
    }

    #[test]
    fn merge_books_preserves_existing_redirects_and_dedupes_losers() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 1));
        put(&store, "books", &book_at("l1", 1));
        save_merged_book_ids(&store, &BTreeMap::from([("x".into(), "y".into())])).unwrap();

        // duplicate loser id collapses to one entry; existing x→y untouched.
        merge_books(&store, "s", &["l1".into(), "l1".into()]).unwrap();
        let map = load_merged_book_ids(&store).unwrap();
        assert_eq!(map.get("x").map(String::as_str), Some("y"));
        assert_eq!(map.get("l1").map(String::as_str), Some("s"));
    }

    #[test]
    fn merge_books_completed_rerun_is_a_noop() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 100));
        put(&store, "books", &book_at("l1", 50));
        put(&store, "notes", &note("n1", Some("l1"), &["a"], 1));

        let first = merge_books(&store, "s", &["l1".into()]).unwrap();
        assert_eq!(first.reassignments.len(), 1);

        // Second run: l1 is already tombstoned — it's skipped entirely, so the retry token is EMPTY
        // (no losers, no reassignments), not a token that would resurrect an empty duplicate on undo.
        let second = merge_books(&store, "s", &["l1".into()]).unwrap();
        assert!(
            second.loser_ids.is_empty(),
            "an already-merged loser is not re-recorded"
        );
        assert!(
            second.reassignments.is_empty(),
            "no notes to rehome on a completed re-run"
        );
        assert_eq!(book_id_of(&store, "n1").as_deref(), Some("s"));
        assert!(is_deleted(&store, "books", "l1"));
    }

    #[test]
    fn unmerge_of_a_completed_rerun_token_does_not_resurrect_a_duplicate() {
        // The founder's case: merge, then a completed-merge RETRY, then undo the RETRY's token. The
        // retry merged nothing live, so its token is empty and undo is a no-op — the loser must NOT
        // come back as an empty live duplicate, and the survivor keeps the merged created_at.
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 100));
        put(&store, "books", &book_at("l1", 50));
        put(&store, "notes", &note("n1", Some("l1"), &["a"], 1));

        merge_books(&store, "s", &["l1".into()]).unwrap();
        let retry = merge_books(&store, "s", &["l1".into()]).unwrap();

        unmerge_books(&store, &retry).unwrap();

        assert!(
            is_deleted(&store, "books", "l1"),
            "loser stays merged away — no duplicate resurrected"
        );
        assert_eq!(
            book_id_of(&store, "n1").as_deref(),
            Some("s"),
            "note stays on the survivor"
        );
        assert_eq!(
            store.get_row("books", "s").unwrap().unwrap()["created_at"].as_i64(),
            Some(50),
            "survivor keeps the merged created_at"
        );
    }

    #[test]
    fn resumed_partial_merge_completes_but_is_not_undoable() {
        // A prior attempt recorded the map and rehomed SOME of l1's notes (n1→s), then crashed with
        // others (n2) STILL under l1. The retry must complete the merge (rehome n2, tombstone l1) but
        // return a NON-undoable token — it can't reconstruct that n1 also came from l1, so offering
        // undo would move only n2 back and strand n1 on the survivor (the half-undo #7 guards).
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 100));
        put(&store, "books", &book_at("l1", 50));
        put(&store, "notes", &note("n1", Some("s"), &["a"], 1)); // already rehomed by the crash
        put(&store, "notes", &note("n2", Some("l1"), &["b"], 2)); // NOT yet rehomed
        save_merged_book_ids(&store, &BTreeMap::from([("l1".into(), "s".into())])).unwrap();
        assert!(
            !is_deleted(&store, "books", "l1"),
            "precondition: loser still live"
        );

        let undo = merge_books(&store, "s", &["l1".into()]).unwrap();
        assert!(
            is_deleted(&store, "books", "l1"),
            "retry completes the tombstone"
        );
        assert_eq!(
            book_id_of(&store, "n2").as_deref(),
            Some("s"),
            "remaining note rehomed"
        );
        assert!(
            undo.loser_ids.is_empty(),
            "a resumed merge (any prior progress) is not undoable"
        );
        assert!(undo.reassignments.is_empty());

        // Undo is a no-op — l1 stays merged away, BOTH notes stay on s (no half-undo).
        unmerge_books(&store, &undo).unwrap();
        assert!(is_deleted(&store, "books", "l1"));
        assert_eq!(book_id_of(&store, "n1").as_deref(), Some("s"));
        assert_eq!(book_id_of(&store, "n2").as_deref(), Some("s"));
    }

    #[test]
    fn resumed_merge_into_a_different_survivor_is_also_not_undoable() {
        // A crashed attempt mapped l1→old and moved n1 to `old`. The user retries into a DIFFERENT
        // survivor `new`, with n2 still under l1. The retry completes (n2→new, tombstone l1) but must
        // be non-undoable: `resumed` keyed on the survivor id alone would miss this (l1 maps to `old`,
        // not `new`) and undo would restore only n2, stranding n1 under `old`.
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("old", 10));
        put(&store, "books", &book_at("new", 20));
        put(&store, "books", &book_at("l1", 50));
        put(&store, "notes", &note("n1", Some("old"), &["a"], 1)); // moved by the crashed attempt
        put(&store, "notes", &note("n2", Some("l1"), &["b"], 2)); // still on the loser
        save_merged_book_ids(&store, &BTreeMap::from([("l1".into(), "old".into())])).unwrap();

        let undo = merge_books(&store, "new", &["l1".into()]).unwrap();
        assert!(
            is_deleted(&store, "books", "l1"),
            "retry completes the tombstone"
        );
        assert_eq!(
            book_id_of(&store, "n2").as_deref(),
            Some("new"),
            "remaining note rehomed to the new survivor"
        );
        assert!(
            undo.loser_ids.is_empty(),
            "any prior mapping (even to a different survivor) → not undoable"
        );
        assert!(undo.reassignments.is_empty());
    }

    #[test]
    fn unmerge_of_an_older_merge_keeps_a_later_merges_earliest_created_at() {
        // s=100; merge l1(50) → s=50; merge l2(20) → s=20. Undo the FIRST merge (l1) while l2 stays
        // merged: the survivor must keep 20 (the still-merged cluster's earliest), not snap to 100.
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 100));
        put(&store, "books", &book_at("l1", 50));
        put(&store, "books", &book_at("l2", 20));
        put(&store, "notes", &note("n1", Some("l1"), &["a"], 1));
        put(&store, "notes", &note("n2", Some("l2"), &["b"], 2));

        let undo1 = merge_books(&store, "s", &["l1".into()]).unwrap();
        merge_books(&store, "s", &["l2".into()]).unwrap();
        assert_eq!(
            store.get_row("books", "s").unwrap().unwrap()["created_at"].as_i64(),
            Some(20)
        );

        unmerge_books(&store, &undo1).unwrap();

        assert_eq!(
            store.get_row("books", "s").unwrap().unwrap()["created_at"].as_i64(),
            Some(20),
            "undoing the older merge must not clobber the later merge's earliest"
        );
        assert_eq!(
            book_id_of(&store, "n1").as_deref(),
            Some("l1"),
            "l1's note restored"
        );
        assert_eq!(
            book_id_of(&store, "n2").as_deref(),
            Some("s"),
            "l2's note stays merged"
        );
        assert!(!is_deleted(&store, "books", "l1"));
        assert!(is_deleted(&store, "books", "l2"), "l2 stays merged");
    }

    #[test]
    fn merge_books_rehomes_decrypt_failed_and_stranded_notes_converge_via_map() {
        // A live note whose text can't decrypt is still rehomed (merge is store-level, no keys),
        // and a SECOND device that only has the map (not the local rehome) converges the same note
        // through reconcile_stranded_notes.
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 10));
        put(&store, "books", &book_at("l1", 5));
        // foreign/undecryptable ciphertext — merge_books never touches it.
        put(
            &store,
            "notes",
            &json!({ "id": "bad", "book_id": "l1", "text": "enc:v2:FOREIGN",
            "tags": [], "created_at": 1, "updated_at": 1, "deleted": false }),
        );
        merge_books(&store, "s", &["l1".into()]).unwrap();
        assert_eq!(book_id_of(&store, "bad").as_deref(), Some("s"));

        // Device B: note still on the (now soft-deleted) loser, map copied over → converges on pull.
        let dev_b = Store::open_in_memory().unwrap();
        put(&dev_b, "books", &book_at("s", 5));
        put(
            &dev_b,
            "books",
            &json!({ "id": "l1", "title": "T", "created_at": 5, "updated_at": 9, "deleted": true }),
        );
        put(&dev_b, "notes", &note("bad", Some("l1"), &[], 1));
        save_merged_book_ids(&dev_b, &BTreeMap::from([("l1".into(), "s".into())])).unwrap();
        let (rehomed, _) = reconcile_stranded_notes(&dev_b).unwrap();
        assert_eq!(rehomed, 1);
        assert_eq!(book_id_of(&dev_b, "bad").as_deref(), Some("s"));
    }

    // ── SUR-1005: synced merged_into consumption (SUR-916 Option 1 part B) ──

    /// The collapsed outbox payload for one record — the row as it would flush.
    fn collapsed_payload(
        store: &Store,
        table: &str,
        record_id: &str,
    ) -> Option<Map<String, Value>> {
        let items: Vec<crate::sync::outbox::OutboxItem> = store
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
        crate::sync::outbox::collapse(items, &BTreeMap::new())
            .into_iter()
            .find(|g| {
                g.table == table && g.payload.get("id").and_then(Value::as_str) == Some(record_id)
            })
            .map(|g| g.payload)
    }

    fn deleted_book_mi(id: &str, merged_into: Option<&str>) -> Value {
        json!({ "id": id, "title": "T", "merged_into": merged_into,
                "created_at": 5, "updated_at": 9, "deleted": true })
    }

    #[test]
    fn stranded_note_converges_via_pulled_merged_into_with_no_local_map() {
        // Device B never received the merge map — the pulled loser's synced merged_into is
        // the ONLY survivor source. The whole point of SUR-916 Option 1: rehome, don't detach.
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 5));
        put(&store, "books", &deleted_book_mi("l1", Some("s")));
        put(
            &store,
            "notes",
            &note_ct("n1", Some("l1"), "STALE-TAG", &[]),
        );

        let (rehomed, detached) = reconcile_stranded_notes(&store).unwrap();

        assert_eq!((rehomed, detached), (1, 0));
        assert_eq!(book_id_of(&store, "n1").as_deref(), Some("s"));
        let row = store.get_row("notes", "n1").unwrap().unwrap();
        assert_eq!(
            row.get("content_tag"),
            Some(&Value::Null),
            "content_tag is book-id-keyed (HMAC input) — must null for re-derive"
        );
        // A genuine mutation: staged through the outbox so the fleet converges.
        let wire = collapsed_payload(&store, "notes", "n1").expect("rehome is pushed");
        assert_eq!(wire.get("book_id").and_then(Value::as_str), Some("s"));
    }

    #[test]
    fn stranded_note_chain_resolves_transitively_via_merged_into() {
        // A→B→C recorded purely in synced pointers (two merges on other devices) → terminal C.
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("c", 5));
        put(&store, "books", &deleted_book_mi("b", Some("c")));
        put(&store, "books", &deleted_book_mi("a", Some("b")));
        put(&store, "notes", &note("n1", Some("a"), &[], 1));

        let (rehomed, _) = reconcile_stranded_notes(&store).unwrap();

        assert_eq!(rehomed, 1);
        assert_eq!(book_id_of(&store, "n1").as_deref(), Some("c"));
    }

    #[test]
    fn stranded_note_chain_alternates_merged_into_and_the_local_map() {
        // Hop 1 from the synced pointer, hop 2 from the device-local map — the two sources
        // compose (the PWA folds merged_into over the map; the walk is behavior-equivalent).
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("c", 5));
        put(&store, "books", &deleted_book_mi("b", None)); // no pointer — map's turn
        put(&store, "books", &deleted_book_mi("a", Some("b")));
        put(&store, "notes", &note("n1", Some("a"), &[], 1));
        save_merged_book_ids(&store, &BTreeMap::from([("b".into(), "c".into())])).unwrap();

        let (rehomed, _) = reconcile_stranded_notes(&store).unwrap();

        assert_eq!(rehomed, 1);
        assert_eq!(book_id_of(&store, "n1").as_deref(), Some("c"));
    }

    #[test]
    fn merged_into_wins_over_a_conflicting_local_map_entry() {
        // The synced pointer is the fleet-wide record; a stale local map entry must lose.
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 5));
        put(&store, "books", &book_at("x", 6));
        put(&store, "books", &deleted_book_mi("l1", Some("s")));
        put(&store, "notes", &note("n1", Some("l1"), &[], 1));
        save_merged_book_ids(&store, &BTreeMap::from([("l1".into(), "x".into())])).unwrap();

        reconcile_stranded_notes(&store).unwrap();

        assert_eq!(book_id_of(&store, "n1").as_deref(), Some("s"));
    }

    #[test]
    fn merge_cycle_detaches_instead_of_parking_on_a_ghost() {
        // Concurrent opposite-direction merges (A→B on one device, B→A on another) are each
        // individually-valid LWW writes, so the fleet can hold both pointers. The hop cap
        // bails out ON a soft-deleted book — the liveness guard must detach, never park the
        // note on a ghost (and never push that ghost fleet-wide). Mirrors surfc PR #362.
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &deleted_book_mi("a", Some("b")));
        put(&store, "books", &deleted_book_mi("b", Some("a")));
        put(&store, "notes", &note("n1", Some("a"), &[], 1));

        let (rehomed, detached) = reconcile_stranded_notes(&store).unwrap();

        assert_eq!((rehomed, detached), (0, 1));
        assert_eq!(book_id_of(&store, "n1"), None, "detached, not parked");
        assert!(
            collapsed_payload(&store, "notes", "n1").is_none(),
            "detach is local-only — never pushed"
        );
    }

    #[test]
    fn plain_deleted_chain_terminus_detaches() {
        // A→B but B was later deleted outright (deleteBook — no merged_into). The walk ends
        // on a soft-deleted row with no onward pointer → liveness guard → detach.
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &deleted_book_mi("b", None));
        put(&store, "books", &deleted_book_mi("a", Some("b")));
        put(&store, "notes", &note("n1", Some("a"), &[], 1));

        let (rehomed, detached) = reconcile_stranded_notes(&store).unwrap();

        assert_eq!((rehomed, detached), (0, 1));
        assert_eq!(book_id_of(&store, "n1"), None);
    }

    #[test]
    fn absent_survivor_is_rehomed_and_materializes_next_pull() {
        // The pointer names a survivor this device hasn't pulled yet. It's the real target:
        // rehome onto it (staged), and the row itself arrives on the next pull — same
        // absent-is-live rule as the PWA's guard.
        let store = Store::open_in_memory().unwrap();
        put(
            &store,
            "books",
            &deleted_book_mi("l1", Some("not-yet-pulled")),
        );
        put(&store, "notes", &note("n1", Some("l1"), &[], 1));

        let (rehomed, _) = reconcile_stranded_notes(&store).unwrap();

        assert_eq!(rehomed, 1);
        assert_eq!(book_id_of(&store, "n1").as_deref(), Some("not-yet-pulled"));
        assert!(collapsed_payload(&store, "notes", "n1").is_some(), "pushed");
    }

    #[test]
    fn merge_books_stamps_merged_into_on_the_local_row_and_the_wire() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 100));
        put(&store, "books", &book_at("l1", 50));

        merge_books(&store, "s", &["l1".into()]).unwrap();

        let row = store.get_row("books", "l1").unwrap().unwrap();
        assert_eq!(row.get("merged_into"), Some(&Value::String("s".into())));
        let wire = collapsed_payload(&store, "books", "l1").expect("tombstone queued");
        assert_eq!(
            wire.get("merged_into").and_then(Value::as_str),
            Some("s"),
            "the pointer rides the tombstone fleet-wide"
        );
    }

    #[test]
    fn unmerge_nulls_merged_into_locally_and_on_the_wire_and_stops_rehoming() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 100));
        put(&store, "books", &book_at("l1", 50));
        put(&store, "notes", &note("n1", Some("l1"), &[], 1));

        let undo = merge_books(&store, "s", &["l1".into()]).unwrap();
        unmerge_books(&store, &undo).unwrap();

        let row = store.get_row("books", "l1").unwrap().unwrap();
        assert_eq!(row.get("merged_into"), Some(&Value::Null));
        let wire = collapsed_payload(&store, "books", "l1").expect("resurrection queued");
        assert_eq!(
            wire.get("merged_into"),
            Some(&Value::Null),
            "the cleared pointer propagates so the fleet stops rehoming"
        );

        // Device B pulls the resurrected loser: live book → nothing stranded → no rehome.
        let dev_b = Store::open_in_memory().unwrap();
        put(&dev_b, "books", &book_at("s", 100));
        put(
            &dev_b,
            "books",
            &json!({ "id": "l1", "title": "T", "merged_into": null,
                     "created_at": 50, "updated_at": 999, "deleted": false }),
        );
        put(&dev_b, "notes", &note("n1", Some("l1"), &[], 1));
        let (rehomed, detached) = reconcile_stranded_notes(&dev_b).unwrap();
        assert_eq!((rehomed, detached), (0, 0), "undo stops the convergence");
    }

    #[test]
    fn books_wire_payloads_carry_the_full_not_null_shape() {
        // PULLED books (create not queued in front — put() applies without enqueueing): the
        // staged patches are each row's ONLY outbox entries, so the collapsed payload is the
        // exact upsert INSERT candidate. title + created_at are NOT NULL without defaults
        // server-side — a sparse candidate 23502-wedges the outbox (the SUR-954 note_links
        // class) — and the PWA's upsertBook always sends the full record.
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 100));
        put(&store, "books", &book_at("l1", 50));

        let undo = merge_books(&store, "s", &["l1".into()]).unwrap();

        let tomb = collapsed_payload(&store, "books", "l1").expect("tombstone queued");
        assert_eq!(tomb.get("title").and_then(Value::as_str), Some("T"));
        assert_eq!(tomb.get("created_at").and_then(Value::as_i64), Some(50));
        assert_eq!(tomb.get("merged_into").and_then(Value::as_str), Some("s"));
        assert!(matches!(tomb.get("deleted"), Some(Value::Bool(true))));
        let sp = collapsed_payload(&store, "books", "s").expect("survivor bump queued");
        assert_eq!(sp.get("title").and_then(Value::as_str), Some("T"));
        assert_eq!(
            sp.get("created_at").and_then(Value::as_i64),
            Some(50),
            "survivor carries the cluster's earliest"
        );

        unmerge_books(&store, &undo).unwrap();
        let res = collapsed_payload(&store, "books", "l1").expect("resurrection queued");
        assert_eq!(res.get("title").and_then(Value::as_str), Some("T"));
        assert_eq!(res.get("created_at").and_then(Value::as_i64), Some(50));
        assert!(matches!(res.get("deleted"), Some(Value::Bool(false))));
        assert_eq!(res.get("merged_into"), Some(&Value::Null));
        let sr = collapsed_payload(&store, "books", "s").expect("survivor restore queued");
        assert_eq!(sr.get("title").and_then(Value::as_str), Some("T"));
        assert_eq!(
            sr.get("created_at").and_then(Value::as_i64),
            Some(100),
            "undo restores the survivor's prior created_at"
        );
    }

    #[test]
    fn tombstone_skip_then_backfill_then_rehome_converges_in_two_steps() {
        // The ticket's pull-ordering caveat: pull skips a tombstone with no local row, but
        // reconcile_books backfills the loser (fetch_by_ids returns owner tombstones) WITH
        // its merged_into — so the stranded pass right after it rehomes in the same
        // reconcile(). This is the map-less device's end-to-end convergence shape.
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 5));
        put(&store, "notes", &note("n1", Some("l1"), &[], 1)); // l1 has NO local row
        let sink = StubSink::new().with(
            "books",
            vec![json!({ "id": "l1", "title": "T", "merged_into": "s",
                         "created_at": 5, "updated_at": 9, "deleted": true })],
        );

        let backfilled = block(reconcile_books(&store, &sink)).unwrap();
        assert_eq!(backfilled, 1, "loser tombstone materialized locally");
        let l1 = store.get_row("books", "l1").unwrap().unwrap();
        assert_eq!(l1.get("merged_into"), Some(&Value::String("s".into())));

        let (rehomed, _) = reconcile_stranded_notes(&store).unwrap();
        assert_eq!(rehomed, 1);
        assert_eq!(book_id_of(&store, "n1").as_deref(), Some("s"));
    }

    #[test]
    fn unmerge_books_restores_state_and_prunes_only_matching_redirects() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 100));
        put(&store, "books", &book_at("l1", 50));
        put(&store, "notes", &note("n1", Some("l1"), &["a"], 1));
        // an UNRELATED redirect into a different survivor must survive the undo.
        save_merged_book_ids(&store, &BTreeMap::from([("other".into(), "z".into())])).unwrap();

        let undo = merge_books(&store, "s", &["l1".into()]).unwrap();
        unmerge_books(&store, &undo).unwrap();

        assert_eq!(
            book_id_of(&store, "n1").as_deref(),
            Some("l1"),
            "note restored to its loser"
        );
        assert!(!is_deleted(&store, "books", "l1"), "loser un-tombstoned");
        assert_eq!(
            store.get_row("books", "s").unwrap().unwrap()["created_at"].as_i64(),
            Some(100),
            "survivor created_at restored"
        );
        let map = load_merged_book_ids(&store).unwrap();
        assert!(!map.contains_key("l1"), "this merge's redirect pruned");
        assert_eq!(
            map.get("other").map(String::as_str),
            Some("z"),
            "unrelated redirect kept"
        );
    }

    /// The `deleted` field a flush would push for `book_id`, collapsing the CURRENT outbox exactly
    /// as `flush` does (`Some(true)` = tombstone on the wire, `Some(false)` = live, `None` = no
    /// queued write for that book).
    fn collapsed_book_deleted(store: &Store, book_id: &str) -> Option<bool> {
        let items: Vec<crate::sync::outbox::OutboxItem> = store
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
        crate::sync::outbox::collapse(items, &BTreeMap::new())
            .into_iter()
            .find(|g| {
                g.table == "books" && g.payload.get("id").and_then(Value::as_str) == Some(book_id)
            })
            .map(|g| matches!(g.payload.get("deleted"), Some(Value::Bool(true))))
    }

    #[test]
    fn unmerge_before_flush_resurrects_loser_on_the_wire_not_a_sticky_tombstone() {
        // Merge then undo BEFORE any flush. The outbox collapse makes `deleted` sticky, so without
        // neutralizing the queued tombstone the loser would flush as deleted:true and the undo would
        // never reach the server — this asserts the flush now pushes it LIVE.
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 100));
        put(&store, "books", &book_at("l1", 50));
        put(&store, "notes", &note("n1", Some("l1"), &["a"], 1));

        let undo = merge_books(&store, "s", &["l1".into()]).unwrap();
        assert_eq!(
            collapsed_book_deleted(&store, "l1"),
            Some(true),
            "merge queues the tombstone"
        );

        unmerge_books(&store, &undo).unwrap();
        assert_eq!(
            collapsed_book_deleted(&store, "l1"),
            Some(false),
            "undo-before-flush must resurrect the loser on the wire, not a sticky tombstone"
        );
    }

    #[test]
    fn unmerge_books_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "books", &book_at("s", 100));
        put(&store, "books", &book_at("l1", 50));
        put(&store, "notes", &note("n1", Some("l1"), &["a"], 1));

        let undo = merge_books(&store, "s", &["l1".into()]).unwrap();
        unmerge_books(&store, &undo).unwrap();
        unmerge_books(&store, &undo).unwrap(); // second call: no panic, state unchanged.

        assert_eq!(book_id_of(&store, "n1").as_deref(), Some("l1"));
        assert!(!is_deleted(&store, "books", "l1"));
    }

    #[test]
    fn merge_content_duplicates_collapses_into_explicit_survivor() {
        let store = Store::open_in_memory().unwrap();
        // Three notes in ONE content_tag cluster; the host picks the middle one as survivor
        // (NOT the deterministic most-tags/earliest pick the auto-dedup would choose).
        put(
            &store,
            "notes",
            &note_ct("rich", None, "TAG", &["a", "b", "c"]),
        );
        put(&store, "notes", &note_ct("pick", None, "TAG", &["b"]));
        put(&store, "notes", &note_ct("dup", None, "TAG", &["d"]));

        let collapsed =
            merge_content_duplicates(&store, "pick", &["rich".into(), "dup".into()], false)
                .unwrap();

        assert_eq!(collapsed, 2);
        assert!(
            !is_deleted(&store, "notes", "pick"),
            "host-picked survivor kept"
        );
        assert!(is_deleted(&store, "notes", "rich"));
        assert!(is_deleted(&store, "notes", "dup"));
        // survivor unions all cluster tags (survivor-first order).
        let tags: Vec<String> = store.get_row("notes", "pick").unwrap().unwrap()["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(tags, vec!["b", "a", "c", "d"]);
    }

    #[test]
    fn merge_content_duplicates_exact_rejects_cross_cluster_but_fuzzy_allows() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &note_ct("s", None, "TAG_A", &["a"]));
        put(&store, "notes", &note_ct("l", None, "TAG_B", &["b"])); // different cluster

        // exact path refuses the cross-cluster loser.
        assert!(merge_content_duplicates(&store, "s", &["l".into()], false).is_err());
        assert!(
            !is_deleted(&store, "notes", "l"),
            "rejected merge changed nothing"
        );

        // fuzzy path (allow_cross_cluster) collapses across clusters.
        let collapsed = merge_content_duplicates(&store, "s", &["l".into()], true).unwrap();
        assert_eq!(collapsed, 1);
        assert!(is_deleted(&store, "notes", "l"));
    }

    #[test]
    fn merge_content_duplicates_validates_inputs() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &note_ct("s", None, "TAG", &["a"]));
        put(&store, "notes", &note_ct("live", None, "TAG", &["b"]));

        assert!(merge_content_duplicates(&store, "", &["live".into()], false).is_err()); // empty survivor
        assert_eq!(
            merge_content_duplicates(&store, "s", &[], false).unwrap(),
            0
        ); // no losers
        assert!(merge_content_duplicates(&store, "s", &["ghost".into()], false).is_err()); // missing loser
                                                                                           // survivor with no content_tag can't anchor an exact cluster.
        put(&store, "notes", &note("untagged", None, &["x"], 1));
        assert!(merge_content_duplicates(&store, "untagged", &["live".into()], false).is_err());
    }

    // ── reconcile_note_signals (SUR-976) ─────────────────────────────────────

    fn signals(note_id: &str, deleted: bool) -> Value {
        json!({
            "note_id": note_id, "source_prior": 0.7, "return_visits": 3, "has_annotation": true,
            "stitch_spawns": 1, "exposure_recency_at": 100, "engagement_recency_at": 200,
            "importance": 0.9, "created_at": 1, "updated_at": 1, "deleted": deleted
        })
    }

    fn tombstoned_note(id: &str) -> Value {
        json!({ "id": id, "text": "enc:v2:x", "tags": [], "source": "handwritten",
                "created_at": 1, "updated_at": 2, "deleted": true })
    }

    #[test]
    fn retires_signals_for_a_note_whose_local_row_exists_and_is_deleted() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &tombstoned_note("n"));
        put(&store, "note_signals", &signals("n", false));

        assert_eq!(reconcile_note_signals(&store).unwrap(), 1);
        assert!(is_deleted(&store, "note_signals", "n"));
    }

    #[test]
    fn leaves_signals_alone_for_a_live_note() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &note("n", None, &[], 1));
        put(&store, "note_signals", &signals("n", false));
        let before = store.get_row("note_signals", "n").unwrap().unwrap();

        assert_eq!(reconcile_note_signals(&store).unwrap(), 0);
        assert_eq!(
            store.get_row("note_signals", "n").unwrap().unwrap(),
            before,
            "a live note's signals row is legitimate evidence — untouched"
        );
        assert!(store.outbox_items().unwrap().is_empty());
    }

    #[test]
    fn leaves_signals_alone_when_no_local_notes_row_exists() {
        // The founder's local-only rule: absent is ambiguous (the pull tombstone-skip makes
        // "never synced down" and "deleted elsewhere" indistinguishable), so the pass must not
        // guess — retiring a not-yet-pulled live note's signals would destroy real evidence.
        let store = Store::open_in_memory().unwrap();
        put(&store, "note_signals", &signals("ghost", false));
        let before = store.get_row("note_signals", "ghost").unwrap().unwrap();

        assert_eq!(reconcile_note_signals(&store).unwrap(), 0);
        assert_eq!(
            store.get_row("note_signals", "ghost").unwrap().unwrap(),
            before
        );
        assert!(store.outbox_items().unwrap().is_empty());
    }

    #[test]
    fn is_idempotent_on_a_second_run() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &tombstoned_note("n"));
        put(&store, "note_signals", &signals("n", false));

        assert_eq!(reconcile_note_signals(&store).unwrap(), 1);
        let tomb = store.get_row("note_signals", "n").unwrap().unwrap();
        let queued = store.outbox_items().unwrap().len();

        assert_eq!(reconcile_note_signals(&store).unwrap(), 0);
        assert_eq!(
            store.get_row("note_signals", "n").unwrap().unwrap(),
            tomb,
            "second run stages nothing — no updated_at churn"
        );
        assert_eq!(
            store.outbox_items().unwrap().len(),
            queued,
            "no new outbox row"
        );
    }

    #[test]
    fn preserves_earned_counters_onto_the_tombstone() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &tombstoned_note("n"));
        put(&store, "note_signals", &signals("n", false));

        reconcile_note_signals(&store).unwrap();
        let tomb = store.get_row("note_signals", "n").unwrap().unwrap();
        assert_eq!(tomb["return_visits"], json!(3));
        assert_eq!(tomb["stitch_spawns"], json!(1));
        assert_eq!(tomb["has_annotation"], json!(true));
        assert_eq!(
            tomb["source_prior"],
            json!(0.7),
            "the STORED prior is carried verbatim, never re-derived from the note's source \
             (which would give 0.9 here) — SUR-956"
        );
    }

    #[test]
    fn propagates_via_the_outbox_as_a_full_shape_row() {
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &tombstoned_note("n"));
        put(&store, "note_signals", &signals("n", false));

        reconcile_note_signals(&store).unwrap();
        let items = store.outbox_items().unwrap();
        assert_eq!(items.len(), 1);
        let (_, table, record_id, payload_json, _) = &items[0];
        assert_eq!(table, "note_signals");
        assert_eq!(
            record_id.as_deref(),
            Some("n"),
            "keyed by note_id (the collapse key)"
        );
        let payload: Value = serde_json::from_str(payload_json).unwrap();
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
                payload.get(col).is_some(),
                "full-shape tombstone (no sparse-PATCH fallback for note_signals): `{col}` present"
            );
        }
        assert_eq!(payload["deleted"], json!(true));
    }

    #[test]
    fn retirement_stamp_is_monotone_over_a_future_stamped_row() {
        // The t01_lww_guard silent-cancel hole (sync-reviewer, SUR-976): a pulled foreign row can
        // carry a stamp AHEAD of this device's clock (skew), and a retirement stamped older would
        // be silently cancelled server-side while the outbox clears and the local row tombstones —
        // a retry-immune divergence. The tombstone must therefore be stamped strictly after the
        // row it retires, regardless of the local clock.
        let store = Store::open_in_memory().unwrap();
        let future = 9_999_999_999_999_i64; // far ahead of any test-run wall clock
        put(&store, "notes", &tombstoned_note("n"));
        let mut row = signals("n", false);
        row["updated_at"] = json!(future);
        put(&store, "note_signals", &row);

        assert_eq!(reconcile_note_signals(&store).unwrap(), 1);
        let tomb = store.get_row("note_signals", "n").unwrap().unwrap();
        assert_eq!(tomb["deleted"], json!(true));
        assert_eq!(
            tomb["updated_at"],
            json!(future + 1),
            "clamped to strictly-after the stored stamp — the server's LWW guard cannot cancel it"
        );
    }

    #[test]
    fn merge_loser_signals_retire_in_the_same_reconcile_cycle() {
        // Pins the pass-ordering decision: content-dedup tombstones the loser, then the
        // signals-retire pass running right after it catches the loser's row THIS cycle. The
        // loser's counters are discarded, not folded into the survivor (founder decision).
        let store = Store::open_in_memory().unwrap();
        put(&store, "notes", &note_ct("keep", None, "TAG", &["a", "b"]));
        put(&store, "notes", &note_ct("lose", None, "TAG", &["a"]));
        put(&store, "note_signals", &signals("lose", false));
        let survivor_absent_before = store.get_row("note_signals", "keep").unwrap().is_none();

        assert_eq!(reconcile_content_dupes(&store).unwrap(), 1);
        assert!(is_deleted(&store, "notes", "lose"));
        assert_eq!(reconcile_note_signals(&store).unwrap(), 1);

        assert!(is_deleted(&store, "note_signals", "lose"));
        assert!(
            survivor_absent_before && store.get_row("note_signals", "keep").unwrap().is_none(),
            "no counter-folding: the survivor gains no signals row from the loser's"
        );
    }
}
