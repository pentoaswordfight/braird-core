//! Flush orchestration (SUR-724 / SUR-659b; fanned out to all eight synced tables in SUR-726).
//! Mirrors surfc's `flushOutbox`, extended with the founder-decided seal-at-write + offline-merge
//! remap model, and hardened with a dependency-ordered dispatch the PWA doesn't need:
//!
//!   1. read the queued writes + the persisted `bookIdRemap` (from `meta`);
//!   2. collapse (LWW per field, sticky delete, note book_id repointed via the remap);
//!   3. dispatch each synced table in TOPOLOGICAL order ([`synced_table_names`]) — every FK parent
//!      before its children — recording per-table the record ids that failed or were held back;
//!   4. a row whose FK points at a failed/held parent this run stays queued (no server FK
//!      violation); on success a book records temp→server in the persisted remap, saved after the
//!      books pass and before any child table needs it;
//!   5. a collapsed group missing any of its table's [`required_insert_columns`] uses targeted
//!      PATCH, because PostgREST upsert checks the NOT-NULL insert shape before conflict update; a
//!      group carrying the full shape still upserts (so an offline create can still insert);
//!   6. clear only the succeeded outbox ids; failed/held groups stay queued for the next flush.
//!
//! One ordered pass replaces SUR-724's hard-coded books-then-notes loops. It also closes a latent
//! bug that pre-dated the fan-out: the old flush dispatched ONLY `books`/`notes` groups, so a queued
//! row in any of the other six tables was neither sent nor failed and wedged the outbox forever. The
//! PWA has no such ordering (it dispatches in collapse order and lets a server FK violation
//! fail-and-retry); the topo order + hold-back is a documented core hardening, same class as the
//! books-before-notes guard and the batch-sticky delete.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use super::http::PostgrestSink;
use super::outbox::{collapse, resolve_book_id, Collapsed, OutboxItem};
use crate::store::{synced_table_names, Store};

/// `meta` key holding the offline-merge temp→server book-id map (JSON object). Persisted so a
/// remap survives a process restart between the book flush and a later note flush.
const BOOK_ID_REMAP_KEY: &str = "bookIdRemap";

/// The pk each table upserts on (the PostgREST `on_conflict` target) — also the pk column the flush
/// reads a group's record id from. All eight synced tables key on `id` except `note_signals`
/// (keyed on `note_id`).
fn on_conflict_for(table: &str) -> &'static str {
    match table {
        "note_signals" => "note_id",
        _ => "id",
    }
}

/// The columns a table's INSERT shape requires, so a collapsed group missing ANY of them cannot
/// upsert: PostgREST validates the insert candidate BEFORE its `on_conflict` UPDATE, so a sparse
/// payload is rejected outright instead of falling through to an update that would have preserved
/// the stored value. Such a group dispatches as a targeted PATCH against the existing row.
///
/// These are each synced table's `not null` columns (surfc migration 0001 onward) minus `user_id`,
/// which [`upsert_group`] injects from the JWT on every row, and minus columns whose server default
/// makes absence harmless. `created_at` is `not null` with no default on every table except
/// `custom_ideas`; `title`/`name` likewise. `notes.text` does carry `default ''` but stays listed:
/// SUR-724 established empirically that a plaintext-free notes payload must never upsert, and this
/// list must not quietly weaken that.
///
/// SUR-1009: `books` was the gap. `books.title` is `not null` with NO default, so every sparse book
/// patch — a cover resolution, a merge tombstone — was rejected on every flush, forever. Worse, the
/// [`fk_deps`] hold-back then kept every note pointing at that book queued too, so a dedup's note
/// tombstones never reached the server and other clients kept showing the duplicates.
fn required_insert_columns(table: &str) -> &'static [&'static str] {
    match table {
        "books" => &["title", "created_at"],
        "notes" => &["text", "created_at"],
        "custom_ideas" => &["name"],
        "lenses" => &["name", "created_at"],
        "collections" => &["name", "created_at"],
        "collection_memberships" => &["note_id", "collection_id", "created_at"],
        "note_links" => &["from_note_id", "to_note_id", "created_at"],
        "note_signals" => &["created_at"],
        _ => &[],
    }
}

/// The foreign keys each table's rows carry as `(column, parent table)` — mirroring the surfc
/// server FKs (migrations 0001/0034/0043/0047). The flush holds a child back when its parent
/// failed/was held this run, so no dispatch hits a server FK violation. Tables with no FK (books,
/// custom_ideas, lenses, collections) return an empty slice.
fn fk_deps(table: &str) -> &'static [(&'static str, &'static str)] {
    match table {
        "notes" => &[("book_id", "books")],
        "note_links" => &[("from_note_id", "notes"), ("to_note_id", "notes")],
        "collection_memberships" => &[("note_id", "notes"), ("collection_id", "collections")],
        "note_signals" => &[("note_id", "notes")],
        _ => &[],
    }
}

/// Outcome of one flush: the outbox ids that were cleared, and the ones still queued.
#[derive(Debug, Default)]
pub struct FlushResult {
    pub ok: Vec<i64>,
    pub failed: Vec<i64>,
}

/// Run one flush. `runtime` block_on-drives the async upserts from the SyncEngine's sync FFI
/// method; this fn is itself async so the engine owns the `block_on` (keeping the runtime in
/// one place). `client` carries the access token set by `set_access_token`.
pub async fn flush<S: PostgrestSink>(
    store: &Store,
    sink: &S,
    user_id: &str,
) -> Result<FlushResult, String> {
    // Load the queued writes + the persisted remap.
    let raw = store
        .outbox_items()
        .map_err(|e| format!("read outbox: {e}"))?;
    let mut remap = load_remap(store)?;

    let items: Vec<OutboxItem> = raw
        .into_iter()
        .filter_map(|(id, table_name, record_id, payload_json, created_at)| {
            // A payload that won't parse is corrupt; skip it rather than fail the whole flush
            // (it can never succeed and would wedge every subsequent flush). ponytail: drop-and-log
            // is the lazy-correct move — one poison row shouldn't strand the queue.
            serde_json::from_str::<Value>(&payload_json)
                .ok()
                .map(|payload| OutboxItem {
                    id,
                    table_name,
                    record_id,
                    payload,
                    created_at,
                })
        })
        .collect();

    let collapsed = collapse(items, &remap);

    let mut result = FlushResult::default();
    // Per-table record ids that FAILED or were HELD BACK this run. A child whose FK points at one
    // must not dispatch (its parent isn't on the server), so holds are transitive across the topo
    // order: a failed book holds its notes, which in turn holds a note_link to those notes.
    let mut failed: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();

    // ONE pass in dependency (topological) order so every FK parent flushes before its children.
    for table in synced_table_names() {
        for group in collapsed.iter().filter(|g| g.table == table) {
            let mut group = group.clone();

            // Repoint a note's book_id onto the offline-merge survivor. Collapse already resolved it
            // against the remap as it stood; re-resolve here to pick up a book merged EARLIER in THIS
            // flush (its temp→server entry was added during the books pass). No-op for the tables
            // that carry no book_id column.
            if let Some(b) = group.payload.get("book_id").and_then(|v| v.as_str()) {
                let resolved = resolve_book_id(b, &remap);
                group
                    .payload
                    .insert("book_id".into(), Value::String(resolved));
            }

            // The record's own pk value — `on_conflict_for` is the pk column (`id`, or `note_id` for
            // note_signals). Recorded in `failed` if this group can't flush, so its children hold.
            let record_id = group
                .payload
                .get(on_conflict_for(table))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Hold back if any FK this row carries points at a parent that failed/held this run —
            // dispatching would hit a server FK violation (the parent row isn't there yet).
            let blocked = fk_deps(table).iter().any(|&(fk_col, parent)| {
                group
                    .payload
                    .get(fk_col)
                    .and_then(|v| v.as_str())
                    .is_some_and(|val| failed.get(parent).is_some_and(|s| s.contains(val)))
            });
            if blocked {
                result.failed.extend(&group.ids);
                failed.entry(table).or_default().insert(record_id);
                continue;
            }

            // A group that cannot satisfy the table's INSERT shape patches the existing row instead
            // of upserting (see [`required_insert_columns`]). Was `notes`-only, which left every
            // sparse `books` patch permanently rejected and its notes held behind it (SUR-1009).
            let sparse = required_insert_columns(table)
                .iter()
                .any(|column| !group.payload.contains_key(*column));
            let write_result = if sparse {
                patch_group(sink, &group, on_conflict_for(table), &record_id).await
            } else {
                upsert_group(sink, &group, user_id).await
            };
            match write_result {
                Ok(()) => {
                    result.ok.extend(&group.ids);
                    // A book that flushed under a temp id and carries its server id maps temp→server
                    // (SUR-463; only books produce a server_id hint — offline books otherwise keep
                    // their own final id, so a remap entry is recorded only when the hint is present).
                    if table == "books" {
                        if let Some(server_id) =
                            group.payload.get("server_id").and_then(|v| v.as_str())
                        {
                            if server_id != record_id {
                                remap.insert(record_id.clone(), server_id.to_string());
                            }
                        }
                    }
                }
                Err(_) => {
                    result.failed.extend(&group.ids);
                    failed.entry(table).or_default().insert(record_id);
                }
            }
        }

        // Persist the (possibly-extended) remap right after the books pass — BEFORE any child table —
        // so a crash mid-flush can't lose a temp→server mapping a queued note still needs.
        if table == "books" {
            persist_remap(store, &remap)?;
        }
    }

    // Clear only the succeeded ids; failed/held groups stay queued for the next flush.
    store
        .clear_outbox(&result.ok)
        .map_err(|e| format!("clear outbox: {e}"))?;
    Ok(result)
}

/// Upsert one collapsed group: stamp `user_id`, wrap in a single-element array, POST.
async fn upsert_group<S: PostgrestSink>(
    sink: &S,
    group: &Collapsed,
    user_id: &str,
) -> Result<(), String> {
    let mut row = group.payload.clone();
    // `user_id` is auth-injected here (from the JWT sub), never stored in the outbox — exactly
    // as the PWA injects the auth user id at write.
    row.insert("user_id".into(), json!(user_id));
    let body = Value::Array(vec![Value::Object(row)]);
    sink.upsert(&group.table, on_conflict_for(&group.table), &body)
        .await
}

/// Patch one existing row without constructing an INSERT candidate. This is required for a
/// plaintext-free notes partial: `notes.text` is NOT NULL, so PostgREST upsert rejects a sparse
/// payload before its conflict UPDATE can preserve the existing ciphertext.
async fn patch_group<S: PostgrestSink>(
    sink: &S,
    group: &Collapsed,
    primary_key: &str,
    record_id: &str,
) -> Result<(), String> {
    let mut row = group.payload.clone();
    row.remove(primary_key);
    sink.patch(&group.table, primary_key, record_id, &Value::Object(row))
        .await
}

fn load_remap(store: &Store) -> Result<BTreeMap<String, String>, String> {
    match store
        .meta_get(BOOK_ID_REMAP_KEY)
        .map_err(|e| format!("read remap: {e}"))?
    {
        Some(json) => serde_json::from_str(&json).map_err(|e| format!("parse remap: {e}")),
        None => Ok(BTreeMap::new()),
    }
}

fn persist_remap(store: &Store, remap: &BTreeMap<String, String>) -> Result<(), String> {
    let json = serde_json::to_string(remap).map_err(|e| format!("serialize remap: {e}"))?;
    store
        .meta_set(BOOK_ID_REMAP_KEY, &json)
        .map_err(|e| format!("write remap: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// In-memory sink: records the table (and the on_conflict target) of every upsert, in call
    /// order; can fail one table.
    struct VecSink {
        calls: RefCell<Vec<String>>,
        conflicts: RefCell<Vec<(String, String)>>,
        patches: RefCell<Vec<(String, String, String, Value)>>,
        fail_table: Option<String>,
    }

    impl PostgrestSink for VecSink {
        async fn upsert(
            &self,
            table: &str,
            on_conflict: &str,
            _rows: &Value,
        ) -> Result<(), String> {
            self.calls.borrow_mut().push(table.to_string());
            self.conflicts
                .borrow_mut()
                .push((table.to_string(), on_conflict.to_string()));
            match &self.fail_table {
                Some(t) if t == table => Err(format!("{table} sink error")),
                _ => Ok(()),
            }
        }

        async fn patch(
            &self,
            table: &str,
            primary_key: &str,
            record_id: &str,
            row: &Value,
        ) -> Result<(), String> {
            self.patches.borrow_mut().push((
                table.to_string(),
                primary_key.to_string(),
                record_id.to_string(),
                row.clone(),
            ));
            match &self.fail_table {
                Some(t) if t == table => Err(format!("{table} sink error")),
                _ => Ok(()),
            }
        }

        // The push tests never pull; the flush path only drives `upsert`.
        async fn fetch_page(
            &self,
            _table: &str,
            _after_seq: i64,
            _limit: i64,
        ) -> Result<Vec<Value>, String> {
            Ok(Vec::new())
        }
    }

    fn sink(fail_table: Option<&str>) -> VecSink {
        VecSink {
            calls: RefCell::new(Vec::new()),
            conflicts: RefCell::new(Vec::new()),
            patches: RefCell::new(Vec::new()),
            fail_table: fail_table.map(String::from),
        }
    }

    /// Enqueue a minimal valid row for `table` keyed `record_id`. `extra` fields (e.g. FK columns)
    /// merge over the pk so a test can wire parent/child edges. Used by the SUR-726 fan-out tests.
    ///
    /// Carries every [`required_insert_columns`] entry, so the row dispatches as an UPSERT: these
    /// tests are about dispatch ORDER and FK hold-back, and a row missing its insert shape would
    /// silently take the sparse-PATCH arm instead (SUR-1009). A real create always carries the full
    /// shape — only a partial patch is sparse — so a test row must too. `extra` still wins, letting a
    /// test override an FK column.
    fn enqueue_row(store: &Store, table: &str, pk_col: &str, record_id: &str, extra: Value) {
        let mut payload = serde_json::Map::new();
        payload.insert(pk_col.to_string(), json!(record_id));
        for column in required_insert_columns(table) {
            let value = if *column == "created_at" {
                json!(1)
            } else {
                json!(format!("{column}-value"))
            };
            payload.insert((*column).to_string(), value);
        }
        if let Value::Object(fields) = extra {
            for (k, v) in fields {
                payload.insert(k, v);
            }
        }
        store
            .enqueue(table, record_id, &Value::Object(payload).to_string(), 100)
            .unwrap();
    }

    #[test]
    fn flush_upserts_books_before_notes() {
        let store = Store::open_in_memory().unwrap();
        // Enqueue the note first; flush must still dispatch the book before the note.
        store
            .enqueue(
                "notes",
                "n1",
                r#"{"id":"n1","book_id":"b1","text":"enc:v2:x","created_at":1}"#,
                100,
            )
            .unwrap();
        store
            .enqueue(
                "books",
                "b1",
                r#"{"id":"b1","title":"T","created_at":1}"#,
                90,
            )
            .unwrap();
        let s = sink(None);
        let res = block(flush(&store, &s, "user-1")).unwrap();
        assert_eq!(
            *s.calls.borrow(),
            vec!["books".to_string(), "notes".to_string()]
        );
        assert_eq!(res.ok.len(), 2);
        assert!(res.failed.is_empty());
    }

    #[test]
    fn plaintext_free_note_flush_uses_targeted_patch() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue(
                "notes",
                "n1",
                r#"{"id":"n1","tags":["after"],"updated_at":20,"deleted":false}"#,
                100,
            )
            .unwrap();

        let sink = sink(None);
        let result = block(flush(&store, &sink, "user-1")).unwrap();

        assert!(
            sink.calls.borrow().is_empty(),
            "sparse note must not upsert"
        );
        let patches = sink.patches.borrow();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].0, "notes");
        assert_eq!(patches[0].1, "id");
        assert_eq!(patches[0].2, "n1");
        assert_eq!(patches[0].3["tags"], json!(["after"]));
        assert!(patches[0].3.get("id").is_none());
        assert!(patches[0].3.get("user_id").is_none());
        assert!(patches[0].3.get("text").is_none());
        assert!(patches[0].3.get("content_tag").is_none());
        assert_eq!(result.ok.len(), 1);
        assert!(result.failed.is_empty());
    }

    #[test]
    fn failed_plaintext_free_note_patch_stays_queued() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue(
                "notes",
                "n1",
                r#"{"id":"n1","tags":["after"],"updated_at":20,"deleted":false}"#,
                100,
            )
            .unwrap();

        let sink = sink(Some("notes"));
        let result = block(flush(&store, &sink, "user-1")).unwrap();

        assert!(result.ok.is_empty());
        assert_eq!(result.failed.len(), 1);
        assert_eq!(
            store.outbox_items().unwrap().len(),
            1,
            "an unconfirmed server patch must remain retryable"
        );
    }

    /// SUR-1009: a sparse BOOK patch must not upsert. `books.title` is `not null` with **no
    /// default** (surfc 0001), so PostgREST validates the insert shape and rejects the row before
    /// its conflict UPDATE could preserve the stored title — the same failure the notes arm already
    /// avoids, on a table that never got the treatment. A cover-resolution patch or a
    /// `deleted:true` tombstone carries no title, so it was rejected on every flush forever.
    #[test]
    fn partial_book_flush_uses_targeted_patch() {
        let store = Store::open_in_memory().unwrap();
        // Verbatim the shape found wedged on the founder's device: a cover-resolution patch.
        store
            .enqueue(
                "books",
                "b1",
                r#"{"id":"b1","cover_url":"https://example.test/c.jpg","cover_source":"openlibrary","updated_at":20}"#,
                100,
            )
            .unwrap();

        let sink = sink(None);
        let result = block(flush(&store, &sink, "user-1")).unwrap();

        assert!(
            sink.calls.borrow().is_empty(),
            "a book patch with no title must not upsert"
        );
        let patches = sink.patches.borrow();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].0, "books");
        assert_eq!(patches[0].1, "id");
        assert_eq!(patches[0].2, "b1");
        assert_eq!(
            patches[0].3["cover_url"],
            json!("https://example.test/c.jpg")
        );
        assert!(
            patches[0].3.get("id").is_none(),
            "pk is the filter, not the body"
        );
        assert!(
            patches[0].3.get("user_id").is_none(),
            "patch must not inject user_id"
        );
        assert_eq!(result.ok.len(), 1);
        assert!(result.failed.is_empty());
    }

    /// The counterpart guard: a book group carrying the full insert shape still UPSERTS, so an
    /// offline-created book can still be inserted (a PATCH would match no row and never create it).
    #[test]
    fn full_book_flush_still_upserts() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue(
                "books",
                "b1",
                r#"{"id":"b1","title":"Thinking in Systems","created_at":10,"updated_at":20,"deleted":false}"#,
                100,
            )
            .unwrap();

        let sink = sink(None);
        let result = block(flush(&store, &sink, "user-1")).unwrap();

        assert_eq!(sink.calls.borrow().as_slice(), ["books"]);
        assert!(
            sink.patches.borrow().is_empty(),
            "a complete book row must insert, not patch"
        );
        assert_eq!(result.ok.len(), 1);
    }

    /// SUR-1009's actual damage: the sparse book FAILED, and `fk_deps` then held every note whose
    /// `book_id` pointed at it — so six note tombstones from a dedup sat queued for days while the
    /// other client kept showing the duplicates. Dispatching the book as a PATCH unblocks them.
    #[test]
    fn partial_book_does_not_wedge_its_notes() {
        let store = Store::open_in_memory().unwrap();
        // The book: a merged-away tombstone, no title (device shape).
        store
            .enqueue(
                "books",
                "b1",
                r#"{"id":"b1","deleted":true,"updated_at":20}"#,
                100,
            )
            .unwrap();
        // The note: re-homed onto b1, then tombstoned — collapse makes `deleted` sticky.
        store
            .enqueue(
                "notes",
                "n1",
                r#"{"id":"n1","book_id":"b1","updated_at":21,"deleted":false}"#,
                101,
            )
            .unwrap();
        store
            .enqueue(
                "notes",
                "n1",
                r#"{"id":"n1","updated_at":22,"deleted":true}"#,
                102,
            )
            .unwrap();

        let sink = sink(None);
        let result = block(flush(&store, &sink, "user-1")).unwrap();

        assert_eq!(result.failed.len(), 0, "nothing may be held back");
        assert_eq!(
            result.ok.len(),
            3,
            "the book patch and both note rows clear"
        );
        assert!(
            store.outbox_items().unwrap().is_empty(),
            "the wedge is gone — the outbox drains"
        );
        let patches = sink.patches.borrow();
        assert_eq!(patches.len(), 2, "both the book and the sparse note patch");
        assert_eq!(patches[0].0, "books", "parent dispatches before its child");
        assert_eq!(patches[1].0, "notes");
        assert_eq!(
            patches[1].3["deleted"],
            json!(true),
            "sticky delete survives collapse"
        );
    }

    #[test]
    fn unflushed_note_create_then_patch_still_upserts_the_collapsed_full_row() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue(
                "notes",
                "n1",
                r#"{"id":"n1","text":"enc:v2:create","content_tag":"tag","created_at":10,"tags":["before"],"updated_at":10,"deleted":false}"#,
                100,
            )
            .unwrap();
        store
            .enqueue(
                "notes",
                "n1",
                r#"{"id":"n1","tags":["after"],"updated_at":20,"deleted":false}"#,
                200,
            )
            .unwrap();

        let sink = sink(None);
        let result = block(flush(&store, &sink, "user-1")).unwrap();

        assert_eq!(*sink.calls.borrow(), vec!["notes".to_string()]);
        assert!(sink.patches.borrow().is_empty());
        assert_eq!(result.ok.len(), 2);
        assert!(result.failed.is_empty());
    }

    #[test]
    fn note_held_back_when_parent_book_flush_fails() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue(
                "books",
                "b1",
                r#"{"id":"b1","title":"T","created_at":1}"#,
                90,
            )
            .unwrap();
        store
            .enqueue(
                "notes",
                "n1",
                r#"{"id":"n1","book_id":"b1","text":"enc:v2:x","created_at":1}"#,
                100,
            )
            .unwrap();
        let s = sink(Some("books"));
        let res = block(flush(&store, &s, "user-1")).unwrap();
        // The book upsert failed → the note must NOT be dispatched (no server FK violation).
        assert_eq!(*s.calls.borrow(), vec!["books".to_string()]);
        assert!(res.ok.is_empty());
        assert_eq!(
            res.failed.len(),
            2,
            "book + held-back note both stay queued"
        );
    }

    #[test]
    fn remap_persisted_to_meta_on_server_id_hint() {
        let store = Store::open_in_memory().unwrap();
        // A book carrying a server_id hint records temp→server in meta on a successful flush.
        store
            .enqueue(
                "books",
                "temp1",
                r#"{"id":"temp1","title":"T","server_id":"srv-1"}"#,
                90,
            )
            .unwrap();
        block(flush(&store, &sink(None), "user-1")).unwrap();
        let remap = store.meta_get("bookIdRemap").unwrap().unwrap();
        assert!(
            remap.contains("temp1") && remap.contains("srv-1"),
            "remap persisted to meta: {remap}"
        );
    }

    // ── SUR-726 fan-out: topo-ordered dispatch across all eight tables ─────────

    #[test]
    fn flush_dispatches_all_eight_tables_in_topological_order() {
        // Enqueue one row per synced table in REVERSE topo order; the flush must still dispatch them
        // parents-first (the schema order). No FK parent fails, so nothing is held.
        let store = Store::open_in_memory().unwrap();
        enqueue_row(&store, "note_signals", "note_id", "n1", json!({})); // no separate id column
        enqueue_row(&store, "collection_memberships", "id", "c1:n1", json!({}));
        enqueue_row(&store, "collections", "id", "c1", json!({}));
        enqueue_row(&store, "lenses", "id", "l1", json!({}));
        enqueue_row(&store, "note_links", "id", "nl1", json!({}));
        enqueue_row(&store, "custom_ideas", "id", "ci1", json!({}));
        enqueue_row(&store, "notes", "id", "n1", json!({ "text": "enc:v2:x" }));
        enqueue_row(&store, "books", "id", "b1", json!({}));

        let s = sink(None);
        let res = block(flush(&store, &s, "user-1")).unwrap();

        assert_eq!(
            *s.calls.borrow(),
            synced_table_names(),
            "dispatch order is the topological (schema) order regardless of enqueue order"
        );
        assert_eq!(res.ok.len(), 8, "every table flushed");
        assert!(res.failed.is_empty());
    }

    #[test]
    fn silent_wedge_regression_all_six_new_tables_dispatch() {
        // Pre-726 the flush dispatched ONLY books/notes groups — a queued row in any other table was
        // never sent and never failed, wedging the outbox forever. Each of the six new tables must
        // now dispatch and clear. (None carry a failing FK parent here, so none are held.)
        let store = Store::open_in_memory().unwrap();
        enqueue_row(&store, "custom_ideas", "id", "ci1", json!({}));
        enqueue_row(&store, "note_links", "id", "nl1", json!({}));
        enqueue_row(&store, "lenses", "id", "l1", json!({}));
        enqueue_row(&store, "collections", "id", "c1", json!({}));
        enqueue_row(&store, "collection_memberships", "id", "c1:n1", json!({}));
        enqueue_row(&store, "note_signals", "note_id", "n1", json!({}));

        let s = sink(None);
        let res = block(flush(&store, &s, "user-1")).unwrap();

        let dispatched = s.calls.borrow().clone();
        for t in [
            "custom_ideas",
            "note_links",
            "lenses",
            "collections",
            "collection_memberships",
            "note_signals",
        ] {
            assert!(dispatched.contains(&t.to_string()), "{t} must dispatch");
        }
        assert_eq!(res.ok.len(), 6, "all six cleared — none wedged");
        assert!(store.outbox_items().unwrap().is_empty());
    }

    #[test]
    fn note_signals_upserts_on_the_note_id_conflict_target() {
        // note_signals has no `id` column — its upsert must conflict on `note_id`, every other table
        // on `id`. (The pk column also drives the flush's record-id read.)
        let store = Store::open_in_memory().unwrap();
        enqueue_row(&store, "note_signals", "note_id", "n1", json!({}));
        enqueue_row(&store, "collections", "id", "c1", json!({}));
        let s = sink(None);
        block(flush(&store, &s, "user-1")).unwrap();

        let conflicts = s.conflicts.borrow().clone();
        assert!(conflicts.contains(&("note_signals".into(), "note_id".into())));
        assert!(conflicts.contains(&("collections".into(), "id".into())));
    }

    #[test]
    fn membership_held_when_parent_collection_flush_fails() {
        // A membership whose parent collection failed this run must stay queued — dispatching it
        // would hit a server FK violation.
        let store = Store::open_in_memory().unwrap();
        enqueue_row(&store, "collections", "id", "c1", json!({}));
        enqueue_row(
            &store,
            "collection_memberships",
            "id",
            "c1:n1",
            json!({ "note_id": "n1", "collection_id": "c1" }),
        );
        let s = sink(Some("collections"));
        let res = block(flush(&store, &s, "user-1")).unwrap();

        assert_eq!(
            *s.calls.borrow(),
            vec!["collections".to_string()],
            "the membership is never dispatched"
        );
        assert_eq!(
            res.failed.len(),
            2,
            "collection + held membership stay queued"
        );
        assert!(res.ok.is_empty());
    }

    #[test]
    fn hold_back_is_transitive_book_to_note_to_note_link() {
        // A failed book holds its note (book_id FK); the held note in turn holds a note_link that
        // references it (from/to_note_id FK) — the transitive chain across the topo order.
        let store = Store::open_in_memory().unwrap();
        enqueue_row(&store, "books", "id", "b1", json!({}));
        enqueue_row(
            &store,
            "notes",
            "id",
            "n1",
            json!({ "book_id": "b1", "text": "enc:v2:x" }),
        );
        enqueue_row(
            &store,
            "note_links",
            "id",
            "nl1",
            json!({ "from_note_id": "n1", "to_note_id": "n1" }),
        );
        let s = sink(Some("books"));
        let res = block(flush(&store, &s, "user-1")).unwrap();

        assert_eq!(
            *s.calls.borrow(),
            vec!["books".to_string()],
            "only the book was dispatched; note and note_link were held transitively"
        );
        assert_eq!(
            res.failed.len(),
            3,
            "book + note + note_link all stay queued"
        );
        assert!(res.ok.is_empty());
    }
}
