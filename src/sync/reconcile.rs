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
//! 4. **`reconcile_content_dupes`** (SUR-835; `reconcileContentTags` + `mergeNotes` in `db.js`) —
//!    live notes sharing a `content_tag` (the SUR-638 per-user HMAC content fingerprint) are
//!    collapsed into one survivor, picked deterministically (most tags, then earliest `created_at`,
//!    then lowest `id`) so two devices reconciling independently converge on the SAME keeper. The
//!    losers' tags, image, `note_links` edges and `collection_memberships` are merged onto the
//!    survivor and the losers soft-deleted — all through the outbox (LWW-safe).
//! 5. **`reconcile_covers`** (SUR-828; `resolveCover` in `surfc/src/lib/coverResolver.js`) —
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
//! shape internally — a `reconcile_dropped_tags`, `reconcile_content_dupes`, or `reconcile_covers`
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
    pub dupes_collapsed: usize,
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
/// dropped-tags, then content-dedup, then cover-resolution (independent of the others).
/// Content-dedup runs after stranded-notes because that pass nulls a rehomed note's now-stale
/// `content_tag`, and a null-tagged note is (correctly) skipped by dedup. `user_id` is the token's
/// `sub` — needed only for the dropped-tag pass's user-scoped custom-idea id.
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
    // Best-effort, same posture as the dropped-tag pass: a content-dedup hiccup must never fail
    // the pull it follows — it simply retries next pull (the pass is idempotent).
    let dupes_collapsed = reconcile_content_dupes(store).unwrap_or_else(|e| {
        eprintln!("reconcile: content-dedup pass failed (non-fatal, retries next pull): {e}");
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
        let key = edge_key(from, to, row_str(e, "relation_type"));
        let eid = row_str(e, "id").to_string();
        let mut patch = Map::new();
        patch.insert("id".into(), json!(eid));
        if from == to || seen.contains(&key) {
            // Self-loop (a loser linked to the survivor) or a duplicate of an existing edge → drop.
            patch.insert("deleted".into(), json!(true));
        } else {
            seen.insert(key);
            patch.insert("from_note_id".into(), json!(from));
            patch.insert("to_note_id".into(), json!(to));
        }
        patch.insert("updated_at".into(), json!(now));
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
                dupes_collapsed: 0, // the fixtures carry no content_tag, so nothing to dedup
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
