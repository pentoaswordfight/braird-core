//! Incremental pull (SUR-725 / SUR-659c). Mirrors surfc's `fetchSince` + `mergeCloudRecords`:
//! per synced table, fetch the rows changed since a per-table cursor, merge **last-write-wins by
//! `updated_at`** into the local store, apply **tombstones** (incoming `deleted:1` is written but
//! a soft-deleted row is never *resurrected*), and advance the per-table cursor.
//!
//! Two invariants the gate (sync-reviewer + crypto-reviewer) turns on:
//!   - **Ciphertext stays at rest.** A pulled note's `text` is the enc:v2 ciphertext and is stored
//!     VERBATIM — never decrypted here (the inverse of push's seal-at-write). The host decrypts on
//!     demand via `Vault::decrypt_note`. Writing plaintext to SQLite would defeat E2EE.
//!   - **The cursor is the puller's own `now()`**, captured BEFORE the fetch and persisted only
//!     after the table's merge succeeds (mirrors the JS `Date.now()` checkpoint). NOT
//!     `max(updated_at)`: `updated_at` is client-authored (surfc migrations 0001…), so a batch max
//!     inherits every writer's clock skew and could skip a slower-clocked device's later write
//!     forever. The puller's own clock as the low watermark + the inclusive `>=` fetch re-pull the
//!     boundary window idempotently under LWW.
//!
//! Source of truth: surfc `src/supabase.js` (`fetchSince`) + `src/db.js` (`mergeCloudRecords`).

use serde_json::Value;

use super::http::PostgrestSink;
use crate::store::{table_schema, Store};

/// Counts across a whole pull (one or more tables). `failed_tables` are the tables whose fetch or
/// merge errored — their cursor is left unadvanced so the window re-pulls next time (per-table
/// failure isolation, the SUR-659 per-table-cursor rationale).
#[derive(Debug, Default)]
pub struct PullResult {
    pub pulled: usize,
    pub merged: usize,
    pub skipped_tombstones: usize,
    pub failed_tables: Vec<String>,
}

#[derive(Debug, Default)]
struct TableStats {
    pulled: usize,
    merged: usize,
    skipped_tombstones: usize,
}

/// Pull `tables` incrementally. `now_ms` is the puller's wall-clock (epoch ms) captured by the
/// caller BEFORE any fetch — the value each succeeding table's cursor advances to (mirrors the JS
/// single pre-fetch `nextCheckpoint`; capturing once, earlier, is a strictly-safer low watermark
/// than per-table). A table that fails is isolated: its cursor stays put and other tables proceed.
pub async fn pull<S: PostgrestSink>(
    store: &Store,
    sink: &S,
    tables: &[&str],
    now_ms: i64,
) -> Result<PullResult, String> {
    let mut result = PullResult::default();
    for &table in tables {
        match pull_table(store, sink, table, now_ms).await {
            Ok(stats) => {
                result.pulled += stats.pulled;
                result.merged += stats.merged;
                result.skipped_tombstones += stats.skipped_tombstones;
            }
            Err(e) => {
                // ponytail: log the dropped table — a silent failure would read as "nothing to
                // pull". The cursor stays put (retry next pull); the caller decides if
                // all-tables-failed is a hard error (see the FFI `pull`).
                eprintln!("pull: table {table} failed (cursor unadvanced): {e}");
                result.failed_tables.push(table.to_string());
            }
        }
    }
    Ok(result)
}

async fn pull_table<S: PostgrestSink>(
    store: &Store,
    sink: &S,
    table: &str,
    now_ms: i64,
) -> Result<TableStats, String> {
    let pk = table_schema(table)
        .ok_or_else(|| format!("unknown synced table: {table}"))?
        .pk[0];
    let cursor = store
        .get_sync_cursor(table)
        .map_err(|e| format!("read cursor {table}: {e}"))?
        .unwrap_or(0);

    let rows = sink.fetch_since(table, cursor).await?;

    let mut stats = TableStats::default();
    for row in &rows {
        let Some(obj) = row.as_object() else { continue };
        let Some(id) = obj.get(pk).and_then(|v| v.as_str()) else {
            continue; // a row without its pk can't be merged — skip defensively
        };
        stats.pulled += 1;

        let incoming_updated = obj.get("updated_at").and_then(Value::as_i64).unwrap_or(0);
        let incoming_deleted = obj.get("deleted").and_then(Value::as_bool).unwrap_or(false);

        // The local row's updated_at (None = no local row) is the only thing the LWW decision
        // needs; a full-row replace on a win comes from `apply_row`.
        let local_updated = store
            .get_row(table, id)
            .map_err(|e| format!("read local {table}/{id}: {e}"))?
            .map(|r| r.get("updated_at").and_then(Value::as_i64).unwrap_or(0));

        // Tombstone: a delete for a row we don't have locally is NOT resurrected — mirrors JS
        // `if (n.deleted && !local) continue`. (A delete for a row we DO have flows through LWW
        // below and writes the tombstone if it's newer.)
        if incoming_deleted && local_updated.is_none() {
            stats.skipped_tombstones += 1;
            continue;
        }

        // Last-write-wins by `updated_at`, STRICT `>` so a tie keeps the local row — mirrors JS
        // `if (!local || cloud.updated_at > local.updatedAt)`. (Distinct from the INCLUSIVE `>=`
        // fetch filter: fetch inclusive so no same-ms row is missed; merge strict so a tie doesn't
        // clobber local.)
        let should_apply = match local_updated {
            None => true,
            Some(local_ts) => incoming_updated > local_ts,
        };
        if should_apply {
            store
                .apply_row(table, obj)
                .map_err(|e| format!("apply {table}/{id}: {e}"))?;
            stats.merged += 1;
        }
    }

    // Advance only after the whole table merged (mirrors JS `saveLastSync` after a successful
    // merge). On an earlier error we returned Err above, so the cursor is untouched → re-pull.
    store
        .set_sync_cursor(table, now_ms)
        .map_err(|e| format!("advance cursor {table}: {e}"))?;

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use serde_json::json;
    use std::collections::HashMap;

    fn block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// Stub PostgREST seam: returns canned rows per table. `fail` makes `fetch_since` error for
    /// one table (the per-table failure-isolation test). Pull never upserts, so `upsert` is inert.
    struct MapSink {
        rows: HashMap<String, Vec<Value>>,
        fail: Option<String>,
    }
    impl MapSink {
        fn new() -> Self {
            Self {
                rows: HashMap::new(),
                fail: None,
            }
        }
        fn with(mut self, table: &str, rows: Vec<Value>) -> Self {
            self.rows.insert(table.to_string(), rows);
            self
        }
        fn failing(mut self, table: &str) -> Self {
            self.fail = Some(table.to_string());
            self
        }
    }
    impl PostgrestSink for MapSink {
        async fn upsert(&self, _t: &str, _c: &str, _r: &Value) -> Result<(), String> {
            Ok(())
        }
        async fn fetch_since(&self, table: &str, _cursor: i64) -> Result<Vec<Value>, String> {
            if self.fail.as_deref() == Some(table) {
                return Err(format!("{table} fetch failed"));
            }
            Ok(self.rows.get(table).cloned().unwrap_or_default())
        }
    }

    fn note(id: &str, updated_at: i64, deleted: bool, text: &str) -> Value {
        json!({ "id": id, "text": text, "content_tag": "tag", "updated_at": updated_at, "deleted": deleted })
    }

    fn apply_local(store: &Store, table: &str, row: Value) {
        store.apply_row(table, row.as_object().unwrap()).unwrap();
    }

    #[test]
    fn merges_new_rows_and_advances_each_cursor() {
        let store = Store::open_in_memory().unwrap();
        let sink = MapSink::new()
            .with(
                "books",
                vec![json!({ "id": "b1", "title": "T", "updated_at": 1000, "deleted": false })],
            )
            .with("notes", vec![note("n1", 1000, false, "enc:v2:x")]);

        let res = block(pull(&store, &sink, &["books", "notes"], 5000)).unwrap();

        assert_eq!(res.pulled, 2);
        assert_eq!(res.merged, 2);
        assert!(store.get_row("books", "b1").unwrap().is_some());
        assert!(store.get_row("notes", "n1").unwrap().is_some());
        assert_eq!(store.get_sync_cursor("books").unwrap(), Some(5000));
        assert_eq!(store.get_sync_cursor("notes").unwrap(), Some(5000));
    }

    #[test]
    fn ciphertext_is_stored_verbatim_not_decrypted() {
        let store = Store::open_in_memory().unwrap();
        let sink = MapSink::new().with("notes", vec![note("n1", 1000, false, "enc:v2:abcDEF")]);
        block(pull(&store, &sink, &["notes"], 5000)).unwrap();
        assert_eq!(
            store.get_row("notes", "n1").unwrap().unwrap()["text"],
            json!("enc:v2:abcDEF"),
            "pull must store the ciphertext verbatim (no decrypt at rest)"
        );
    }

    #[test]
    fn lww_keeps_local_when_incoming_is_older() {
        let store = Store::open_in_memory().unwrap();
        apply_local(&store, "notes", note("n1", 2000, false, "local"));
        let sink = MapSink::new().with("notes", vec![note("n1", 1000, false, "remote-old")]);
        let res = block(pull(&store, &sink, &["notes"], 9000)).unwrap();
        assert_eq!(res.merged, 0);
        assert_eq!(
            store.get_row("notes", "n1").unwrap().unwrap()["text"],
            json!("local")
        );
    }

    #[test]
    fn lww_overwrites_when_incoming_is_newer() {
        let store = Store::open_in_memory().unwrap();
        apply_local(&store, "notes", note("n1", 1000, false, "local"));
        let sink = MapSink::new().with("notes", vec![note("n1", 2000, false, "remote-new")]);
        let res = block(pull(&store, &sink, &["notes"], 9000)).unwrap();
        assert_eq!(res.merged, 1);
        assert_eq!(
            store.get_row("notes", "n1").unwrap().unwrap()["text"],
            json!("remote-new")
        );
    }

    #[test]
    fn lww_tie_keeps_local() {
        let store = Store::open_in_memory().unwrap();
        apply_local(&store, "notes", note("n1", 1000, false, "local"));
        let sink = MapSink::new().with("notes", vec![note("n1", 1000, false, "remote-tie")]);
        let res = block(pull(&store, &sink, &["notes"], 9000)).unwrap();
        assert_eq!(res.merged, 0, "a tie keeps local (strict >)");
        assert_eq!(
            store.get_row("notes", "n1").unwrap().unwrap()["text"],
            json!("local")
        );
    }

    #[test]
    fn tombstone_is_not_resurrected_when_no_local_row() {
        let store = Store::open_in_memory().unwrap();
        let sink = MapSink::new().with("notes", vec![note("n1", 2000, true, "")]);
        let res = block(pull(&store, &sink, &["notes"], 5000)).unwrap();
        assert_eq!(res.skipped_tombstones, 1);
        assert_eq!(res.merged, 0);
        assert!(
            store.get_row("notes", "n1").unwrap().is_none(),
            "a delete for a row we never had must not be inserted"
        );
    }

    #[test]
    fn tombstone_is_applied_over_an_older_local_row() {
        let store = Store::open_in_memory().unwrap();
        apply_local(&store, "notes", note("n1", 1000, false, "live"));
        let sink = MapSink::new().with("notes", vec![note("n1", 2000, true, "")]);
        let res = block(pull(&store, &sink, &["notes"], 5000)).unwrap();
        assert_eq!(res.merged, 1);
        assert_eq!(
            store.get_row("notes", "n1").unwrap().unwrap()["deleted"],
            json!(true),
            "a newer delete flips the local row to a tombstone"
        );
    }

    #[test]
    fn stale_tombstone_does_not_revive_a_newer_local_row() {
        let store = Store::open_in_memory().unwrap();
        apply_local(&store, "notes", note("n1", 3000, false, "fresh-local-edit"));
        // A delete older than the local edit must lose LWW — the local edit stays live.
        let sink = MapSink::new().with("notes", vec![note("n1", 2000, true, "")]);
        let res = block(pull(&store, &sink, &["notes"], 9000)).unwrap();
        assert_eq!(res.merged, 0);
        let row = store.get_row("notes", "n1").unwrap().unwrap();
        assert_eq!(row["deleted"], json!(false));
        assert_eq!(row["text"], json!("fresh-local-edit"));
    }

    #[test]
    fn per_table_failure_isolates_the_cursor() {
        let store = Store::open_in_memory().unwrap();
        let sink = MapSink::new()
            .with(
                "books",
                vec![json!({ "id": "b1", "title": "T", "updated_at": 1000, "deleted": false })],
            )
            .failing("notes");
        let res = block(pull(&store, &sink, &["books", "notes"], 5000)).unwrap();

        assert_eq!(res.merged, 1, "books still merged");
        assert_eq!(res.failed_tables, vec!["notes".to_string()]);
        assert_eq!(store.get_sync_cursor("books").unwrap(), Some(5000));
        assert_eq!(
            store.get_sync_cursor("notes").unwrap(),
            None,
            "failed table's cursor stays put so its window re-pulls"
        );
    }

    #[test]
    fn cursor_advances_on_an_empty_batch() {
        // An empty result is a successful pull — advance so we don't re-scan from 0 forever.
        let store = Store::open_in_memory().unwrap();
        let sink = MapSink::new().with("notes", vec![]);
        block(pull(&store, &sink, &["notes"], 5000)).unwrap();
        assert_eq!(store.get_sync_cursor("notes").unwrap(), Some(5000));
    }
}
