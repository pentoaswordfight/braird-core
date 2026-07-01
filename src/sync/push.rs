//! Flush orchestration (SUR-724 / SUR-659b). Mirrors surfc's `flushOutbox`, extended with
//! the founder-decided seal-at-write + offline-merge-remap model:
//!
//!   1. read the queued writes + the persisted `bookIdRemap` (from `meta`);
//!   2. collapse (LWW per field, sticky delete, note book_id repointed via the remap);
//!   3. upsert BOOKS first — on success, record temp→server in the persisted remap;
//!   4. upsert NOTES — book_id already repointed at collapse; a note whose parent BOOK
//!      flush failed stays queued (never dispatched with a temp id → no server FK violation);
//!   5. clear only the succeeded outbox ids; failed groups stay queued for the next flush.
//!
//! This slice proves the model on `books` + `notes` only (the two tables with the parent/child
//! + encryption edges). The other six synced tables land in SUR-659c/d behind the same flush.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use super::http::PostgrestSink;
use super::outbox::{collapse, resolve_book_id, Collapsed, OutboxItem};
use crate::store::Store;

/// `meta` key holding the offline-merge temp→server book-id map (JSON object). Persisted so a
/// remap survives a process restart between the book flush and a later note flush.
const BOOK_ID_REMAP_KEY: &str = "bookIdRemap";

/// The pk each table upserts on (the PostgREST `on_conflict` target). books/notes = `id`.
fn on_conflict_for(table: &str) -> &'static str {
    match table {
        // note_signals is out of this slice, but keep the one non-`id` pk honest for SUR-659c/d.
        "note_signals" => "note_id",
        _ => "id",
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
    // Books whose flush FAILED — their child notes must NOT dispatch with a temp book_id.
    let mut failed_books: BTreeSet<String> = BTreeSet::new();

    // ── Books first ──────────────────────────────────────────────────────────
    for group in collapsed.iter().filter(|g| g.table == "books") {
        let book_id = group
            .payload
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match upsert_group(sink, group, user_id).await {
            Ok(()) => {
                result.ok.extend(&group.ids);
                // A book that flushed under a temp id and carries its server id maps temp→server.
                // The PWA gets its server id from the merge (SUR-463); here a book keeps its own
                // id (offline books are created with their final id), so a remap entry is only
                // recorded when a `server_id` hint is present — future-proofing the plumbing
                // without inventing a remap the current write path doesn't produce.
                if let Some(server_id) = group.payload.get("server_id").and_then(|v| v.as_str()) {
                    if server_id != book_id {
                        remap.insert(book_id.clone(), server_id.to_string());
                    }
                }
            }
            Err(_) => {
                result.failed.extend(&group.ids);
                failed_books.insert(book_id);
            }
        }
    }

    // Persist the (possibly-extended) remap before notes, so a crash mid-flush doesn't lose it.
    persist_remap(store, &remap)?;

    // ── Notes second ─────────────────────────────────────────────────────────
    for group in collapsed.iter().filter(|g| g.table == "notes") {
        // book_id was repointed at collapse; re-resolve against the just-extended remap so a
        // book merged THIS flush is picked up too.
        let book_id = group
            .payload
            .get("book_id")
            .and_then(|v| v.as_str())
            .map(|b| resolve_book_id(b, &remap));

        // Guard: a note whose parent book's flush failed this run stays queued — dispatching it
        // now would hit the server with a temp/absent book_id → FK violation.
        if let Some(ref b) = book_id {
            if failed_books.contains(b) {
                result.failed.extend(&group.ids);
                continue;
            }
        }

        // Repoint the payload's book_id to the resolved value before dispatch.
        let mut group = group.clone();
        if let Some(b) = book_id {
            group.payload.insert("book_id".into(), Value::String(b));
        }

        match upsert_group(sink, &group, user_id).await {
            Ok(()) => result.ok.extend(&group.ids),
            Err(_) => result.failed.extend(&group.ids),
        }
    }

    // Clear only the succeeded ids; failed groups stay queued.
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

    /// In-memory sink: records the table of every upsert (in call order); can fail one table.
    struct VecSink {
        calls: RefCell<Vec<String>>,
        fail_table: Option<String>,
    }

    impl PostgrestSink for VecSink {
        async fn upsert(
            &self,
            table: &str,
            _on_conflict: &str,
            _rows: &Value,
        ) -> Result<(), String> {
            self.calls.borrow_mut().push(table.to_string());
            match &self.fail_table {
                Some(t) if t == table => Err(format!("{table} sink error")),
                _ => Ok(()),
            }
        }

        // The push tests never pull; the flush path only drives `upsert`.
        async fn fetch_since(&self, _table: &str, _cursor: i64) -> Result<Vec<Value>, String> {
            Ok(Vec::new())
        }
    }

    fn sink(fail_table: Option<&str>) -> VecSink {
        VecSink {
            calls: RefCell::new(Vec::new()),
            fail_table: fail_table.map(String::from),
        }
    }

    #[test]
    fn flush_upserts_books_before_notes() {
        let store = Store::open_in_memory().unwrap();
        // Enqueue the note first; flush must still dispatch the book before the note.
        store
            .enqueue(
                "notes",
                "n1",
                r#"{"id":"n1","book_id":"b1","text":"enc:v2:x"}"#,
                100,
            )
            .unwrap();
        store
            .enqueue("books", "b1", r#"{"id":"b1","title":"T"}"#, 90)
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
    fn note_held_back_when_parent_book_flush_fails() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue("books", "b1", r#"{"id":"b1","title":"T"}"#, 90)
            .unwrap();
        store
            .enqueue(
                "notes",
                "n1",
                r#"{"id":"n1","book_id":"b1","text":"enc:v2:x"}"#,
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
}
