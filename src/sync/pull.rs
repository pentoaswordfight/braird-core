//! Incremental pull (SUR-725 / SUR-659c). Mirrors surfc's `fetchSince` + `mergeCloudRecords`:
//! per synced table, fetch the rows changed since a per-table cursor, merge **last-write-wins by
//! `updated_at`** into the local store, apply **tombstones** (incoming `deleted:1` is written but
//! a soft-deleted row is never *resurrected*), and advance the per-table cursor.
//!
//! Two invariants the gate (sync-reviewer + crypto-reviewer) turns on:
//!   - **Ciphertext stays at rest.** A pulled note's `text` is the enc:v2 ciphertext and is stored
//!     VERBATIM — never decrypted here (the inverse of push's seal-at-write). The host decrypts on
//!     demand via `Vault::decrypt_note`. Writing plaintext to SQLite would defeat E2EE.
//!   - **The cursor is the puller's own clock**, captured BEFORE the fetch and persisted only after
//!     the table's merge succeeds (mirrors the JS `Date.now()` checkpoint). NOT `max(updated_at)`:
//!     `updated_at` is client-authored (surfc migrations 0001…), so a batch max inherits every
//!     writer's clock skew and could skip a slower-clocked device's later write. The caller passes
//!     the watermark (`now_ms`); the FFI passes `now() - PULL_CURSOR_OVERLAP_MS`, a lookback that
//!     also catches a **delayed/offline flush** — a row stamped (at enqueue) before the cursor
//!     advanced but made server-visible (at flush) after. That overlap is a bounded mitigation only;
//!     any client-timestamp cursor is incomplete under long-delayed flushes, and the durable fix (a
//!     server-assigned monotonic watermark, distinct from the LWW `updated_at`) is **SUR-739**.
//!
//! Outbox rebase on an LWW win (SUR-736/738): when a pull applies a strictly-newer remote row for
//! a record that still has a queued local edit, that stale outbox entry is dropped in the SAME
//! transaction as the apply (via [`Store::apply_row_rebasing_outbox`]) — otherwise the next flush
//! would re-push the losing edit over the newer server row (a lost remote edit). Each record with a
//! dropped entry is reported as a [`SupersededEdit`] (the local edit lost LWW and was dropped;
//! full server-side conflict detection is out of scope — SUR-738 ratifies the client-side signal).
//!
//! Source of truth: surfc `src/supabase.js` (`fetchSince`) + `src/db.js` (`mergeCloudRecords`).

use serde_json::Value;

use super::http::PostgrestSink;
use super::SupersededEdit;
use crate::store::{table_schema, Store};

/// Counts across a whole pull (one or more tables). `failed_tables` are the tables whose fetch or
/// merge errored — their cursor is left unadvanced so the window re-pulls next time (per-table
/// failure isolation, the SUR-659 per-table-cursor rationale).
#[derive(Debug, Default)]
pub struct PullResult {
    pub pulled: usize,
    pub merged: usize,
    pub skipped_tombstones: usize,
    /// Local edits dropped because a strictly-newer remote row won LWW (SUR-736/738).
    pub superseded: Vec<SupersededEdit>,
    pub failed_tables: Vec<String>,
}

#[derive(Debug, Default)]
struct TableStats {
    pulled: usize,
    merged: usize,
    skipped_tombstones: usize,
    superseded: Vec<SupersededEdit>,
}

/// Pull `tables` incrementally. `now_ms` is the watermark each succeeding table's cursor advances
/// to — captured by the caller BEFORE any fetch (mirrors the JS single pre-fetch `nextCheckpoint`;
/// capturing once, earlier, is a strictly-safer low watermark than per-table). The FFI passes a
/// lookback-adjusted `now() - PULL_CURSOR_OVERLAP_MS` (SUR-739) so a delayed flush isn't skipped.
/// A table that fails is isolated: its cursor stays put and other tables proceed.
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
                result.superseded.extend(stats.superseded);
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
        //
        // WHOLE-ROW LWW, including array/composite columns (SUR-737, ratified): the winner's
        // `tags` / `source_meta` / `leaf_ids` replace the local ones wholesale — there is NO
        // element-level union (a union can't express a delete). This is table-agnostic, so it holds
        // for every table the SUR-726 fan-out adds; the convergence contract + rationale live on
        // `store::synced_schema`, and any change here is wire-visible (must move in lockstep with the
        // PWA's `mergeCloudRecords`). Ratification pin tests: `sur737_*` below.
        let should_apply = match local_updated {
            None => true,
            Some(local_ts) => incoming_updated > local_ts,
        };
        if should_apply {
            // Apply the winner AND drop any now-stale queued edit for this record atomically
            // (SUR-736) — leaving it would let the next flush re-push it over this newer row.
            let dropped = store
                .apply_row_rebasing_outbox(table, obj, incoming_updated)
                .map_err(|e| format!("apply {table}/{id}: {e}"))?;
            stats.merged += 1;
            // A dropped entry = a local edit superseded by the remote winner (SUR-738). Key it by
            // the newest discarded stamp.
            if let Some(discarded) = dropped.iter().map(|(_, ts)| *ts).max() {
                stats.superseded.push(SupersededEdit {
                    table: table.to_string(),
                    record_id: id.to_string(),
                    discarded_updated_at: discarded,
                    winning_updated_at: incoming_updated,
                });
            }
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
    use crate::sync::SupersededEdit;
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

    /// Queue a pending outbox entry for `notes/id` stamped `ts` (the offline-first shape: a local
    /// write leaves both a synced row and an outbox row). Returns the outbox id.
    fn enqueue_pending(store: &Store, id: &str, ts: i64) -> i64 {
        store
            .enqueue(
                "notes",
                id,
                &json!({ "id": id, "updated_at": ts }).to_string(),
                ts,
            )
            .unwrap()
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

    // ── SUR-736/738 outbox rebase on an LWW win + conflict signal ─────────────

    #[test]
    fn rebase_drops_stale_pending_edit_and_surfaces_conflict() {
        // (1) pending T1, incoming T2 > T1 → row = remote, outbox drained, one conflict surfaced.
        let store = Store::open_in_memory().unwrap();
        apply_local(&store, "notes", note("n1", 1000, false, "local"));
        enqueue_pending(&store, "n1", 1000);
        let sink = MapSink::new().with("notes", vec![note("n1", 2000, false, "remote")]);

        let res = block(pull(&store, &sink, &["notes"], 9000)).unwrap();

        assert_eq!(res.merged, 1);
        assert_eq!(
            res.superseded,
            vec![SupersededEdit {
                table: "notes".into(),
                record_id: "n1".into(),
                discarded_updated_at: 1000,
                winning_updated_at: 2000,
            }]
        );
        assert!(
            store.outbox_items().unwrap().is_empty(),
            "the stale queued edit is rebased away — the next flush can't re-push it (SUR-736)"
        );
        assert_eq!(
            store.get_row("notes", "n1").unwrap().unwrap()["text"],
            json!("remote")
        );
    }

    #[test]
    fn newer_local_edit_blocks_rebase_and_keeps_pending() {
        // (2) pending T3, incoming T2 < T3 → no apply, outbox intact, no conflict.
        let store = Store::open_in_memory().unwrap();
        apply_local(&store, "notes", note("n1", 3000, false, "local-newer"));
        enqueue_pending(&store, "n1", 3000);
        let sink = MapSink::new().with("notes", vec![note("n1", 2000, false, "remote-older")]);

        let res = block(pull(&store, &sink, &["notes"], 9000)).unwrap();

        assert_eq!(res.merged, 0);
        assert!(res.superseded.is_empty());
        assert_eq!(
            store.outbox_items().unwrap().len(),
            1,
            "pending edit survives"
        );
        assert_eq!(
            store.get_row("notes", "n1").unwrap().unwrap()["text"],
            json!("local-newer")
        );
    }

    #[test]
    fn tie_keeps_local_and_does_not_rebase() {
        // (3) incoming == local row ts → keep local, outbox intact (rebase never runs).
        let store = Store::open_in_memory().unwrap();
        apply_local(&store, "notes", note("n1", 2000, false, "local"));
        enqueue_pending(&store, "n1", 2000);
        let sink = MapSink::new().with("notes", vec![note("n1", 2000, false, "remote-tie")]);

        let res = block(pull(&store, &sink, &["notes"], 9000)).unwrap();

        assert_eq!(res.merged, 0);
        assert!(res.superseded.is_empty());
        assert_eq!(store.outbox_items().unwrap().len(), 1);
    }

    #[test]
    fn incoming_tombstone_rebases_a_pending_edit() {
        // (4) incoming tombstone T2 over pending edit T1 → tombstone applied, entry dropped.
        let store = Store::open_in_memory().unwrap();
        apply_local(&store, "notes", note("n1", 1000, false, "local-live"));
        enqueue_pending(&store, "n1", 1000);
        let sink = MapSink::new().with("notes", vec![note("n1", 2000, true, "")]);

        let res = block(pull(&store, &sink, &["notes"], 9000)).unwrap();

        assert_eq!(res.merged, 1);
        assert_eq!(res.superseded.len(), 1);
        assert_eq!(
            store.get_row("notes", "n1").unwrap().unwrap()["deleted"],
            json!(true)
        );
        assert!(
            store.outbox_items().unwrap().is_empty(),
            "the pending edit is dropped by the newer delete"
        );
    }

    #[test]
    fn incoming_live_edit_rebases_a_pending_delete() {
        // (5) pending delete T1 vs incoming live edit T2 → live row applied, delete dropped.
        // Symmetric with `stale_tombstone_does_not_revive_a_newer_local_row` — a newer edit beats
        // an older delete; this is LWW, not a resurrection violation (resurrection is a delete for a
        // row this device never had, guarded separately).
        let store = Store::open_in_memory().unwrap();
        apply_local(&store, "notes", note("n1", 1000, true, ""));
        enqueue_pending(&store, "n1", 1000);
        let sink = MapSink::new().with("notes", vec![note("n1", 2000, false, "remote-live")]);

        let res = block(pull(&store, &sink, &["notes"], 9000)).unwrap();

        assert_eq!(res.merged, 1);
        assert_eq!(res.superseded.len(), 1);
        let row = store.get_row("notes", "n1").unwrap().unwrap();
        assert_eq!(
            row["deleted"],
            json!(false),
            "newer live edit beats older delete"
        );
        assert_eq!(row["text"], json!("remote-live"));
        assert!(
            store.outbox_items().unwrap().is_empty(),
            "pending delete dropped"
        );
    }

    #[test]
    fn multiple_pending_entries_survive_when_incoming_loses_lww() {
        // (6) pending T1 + T3, incoming T2 → no apply (T2 < local T3), both entries survive.
        let store = Store::open_in_memory().unwrap();
        apply_local(&store, "notes", note("n1", 3000, false, "local-latest"));
        enqueue_pending(&store, "n1", 1000);
        enqueue_pending(&store, "n1", 3000);
        let sink = MapSink::new().with("notes", vec![note("n1", 2000, false, "remote-middle")]);

        let res = block(pull(&store, &sink, &["notes"], 9000)).unwrap();

        assert_eq!(res.merged, 0, "incoming T2 < local T3 → no apply");
        assert!(res.superseded.is_empty());
        assert_eq!(
            store.outbox_items().unwrap().len(),
            2,
            "both pending entries survive"
        );
    }

    #[test]
    fn pull_does_not_panic_on_a_malformed_pending_payload() {
        // (7) malformed pending payload → left queued, pull succeeds, no conflict, no panic.
        let store = Store::open_in_memory().unwrap();
        apply_local(&store, "notes", note("n1", 1000, false, "local"));
        store.enqueue("notes", "n1", "not json", 1000).unwrap();
        let sink = MapSink::new().with("notes", vec![note("n1", 2000, false, "remote")]);

        let res = block(pull(&store, &sink, &["notes"], 9000)).unwrap();

        assert_eq!(res.merged, 1);
        assert!(
            res.superseded.is_empty(),
            "an unparseable entry can't be proven stale → not surfaced"
        );
        assert_eq!(
            store.outbox_items().unwrap().len(),
            1,
            "the malformed entry is left queued"
        );
        assert_eq!(
            store.get_row("notes", "n1").unwrap().unwrap()["text"],
            json!("remote")
        );
    }

    // ── SUR-737 array / row convergence ratification ──────────────────────────
    // pull_table is table-agnostic, so these pin the whole-row-LWW convergence of the composite
    // and row-per-pair tables AHEAD of the SUR-726 fan-out. If a future change tries element-level
    // merge (a `tags` union, a `leaf_ids` union), these fail — the convergence contract on
    // `store::synced_schema` is the decision they enforce.

    #[test]
    fn sur737_tags_converge_whole_array_not_union() {
        // A tag set is whole-row LWW: the newer note's `tags` REPLACE the local ones, never union
        // (a union couldn't express a tag deletion). Both directions.
        let store = Store::open_in_memory().unwrap();

        // Local newer → an older remote loses; the local array is kept intact.
        apply_local(
            &store,
            "notes",
            json!({ "id": "n1", "tags": ["a", "b"], "updated_at": 2000, "deleted": false }),
        );
        let sink = MapSink::new().with(
            "notes",
            vec![json!({ "id": "n1", "tags": ["c"], "updated_at": 1000, "deleted": false })],
        );
        block(pull(&store, &sink, &["notes"], 9000)).unwrap();
        assert_eq!(
            store.get_row("notes", "n1").unwrap().unwrap()["tags"],
            json!(["a", "b"]),
            "older remote loses LWW; the local tag array is untouched"
        );

        // Remote newer → replaces the whole array; NOT the union ["a","b","c"].
        let sink = MapSink::new().with(
            "notes",
            vec![json!({ "id": "n1", "tags": ["c"], "updated_at": 3000, "deleted": false })],
        );
        block(pull(&store, &sink, &["notes"], 9001)).unwrap();
        assert_eq!(
            store.get_row("notes", "n1").unwrap().unwrap()["tags"],
            json!(["c"]),
            "newer remote replaces the whole tag array (no element union)"
        );
    }

    #[test]
    fn sur737_lens_leaf_ids_converge_whole_array_not_union() {
        // A lens is ONE authored query — its leaf_ids move whole-row LWW with the combinator +
        // threshold. Unioning leaves would fabricate a query nobody wrote.
        let store = Store::open_in_memory().unwrap();
        apply_local(
            &store,
            "lenses",
            json!({ "id": "l1", "name": "L", "leaf_ids": ["a", "b"], "combinator": "and", "threshold": 1, "updated_at": 1000, "deleted": false }),
        );
        let sink = MapSink::new().with(
            "lenses",
            vec![json!({ "id": "l1", "name": "L", "leaf_ids": ["c"], "combinator": "or", "threshold": 2, "updated_at": 2000, "deleted": false })],
        );
        block(pull(&store, &sink, &["lenses"], 9000)).unwrap();
        let row = store.get_row("lenses", "l1").unwrap().unwrap();
        assert_eq!(
            row["leaf_ids"],
            json!(["c"]),
            "newer lens replaces leaf_ids wholesale (no union)"
        );
        assert_eq!(
            row["combinator"],
            json!("or"),
            "the rest of the authored query moves with it"
        );
    }

    #[test]
    fn sur737_collection_membership_concurrent_adds_converge_then_tombstone() {
        // Memberships are row-per-pair with a DETERMINISTIC pk (membershipId(collection, note)), so
        // two devices adding the same pair share the same id → one row (OR-set add); a remove is a
        // tombstone on that row. (The shared pk + INSERT OR REPLACE is what collapses concurrent
        // adds — no duplicate row can exist.)
        let store = Store::open_in_memory().unwrap();
        let mid = "col1:note1"; // deterministic membershipId(collection, note)

        apply_local(
            &store,
            "collection_memberships",
            json!({ "id": mid, "collection_id": "col1", "note_id": "note1", "updated_at": 1000, "deleted": false }),
        );
        // Remote add of the SAME pair (same id) — merges onto the one row, not a second row.
        let sink = MapSink::new().with(
            "collection_memberships",
            vec![json!({ "id": mid, "collection_id": "col1", "note_id": "note1", "updated_at": 2000, "deleted": false })],
        );
        block(pull(&store, &sink, &["collection_memberships"], 9000)).unwrap();
        let row = store
            .get_row("collection_memberships", mid)
            .unwrap()
            .unwrap();
        assert_eq!(
            row["updated_at"],
            json!(2000_i64),
            "the newer add won the one row"
        );
        assert_eq!(row["deleted"], json!(false), "still a live membership");

        // A newer remove tombstones that row (OR-set remove).
        let sink = MapSink::new().with(
            "collection_memberships",
            vec![json!({ "id": mid, "collection_id": "col1", "note_id": "note1", "updated_at": 3000, "deleted": true })],
        );
        block(pull(&store, &sink, &["collection_memberships"], 9001)).unwrap();
        assert_eq!(
            store
                .get_row("collection_memberships", mid)
                .unwrap()
                .unwrap()["deleted"],
            json!(true),
            "a newer remove tombstones the membership"
        );
    }

    #[test]
    fn sur737_exact_ms_tie_keeps_local_so_replicas_can_diverge() {
        // Accepted residual (plan §8): the compare is strict `>`, so an exact-ms tie keeps local.
        // Two devices that wrote DIFFERENT values at the SAME updated_at do NOT converge — each keeps
        // its own after pulling the other. Pinned so a future tie-break becomes a deliberate,
        // wire-visible decision (PWA+core lockstep), not an accident. The convergence contract on
        // `store::synced_schema` carries this caveat.
        let dev_a = Store::open_in_memory().unwrap();
        let dev_b = Store::open_in_memory().unwrap();
        apply_local(&dev_a, "notes", note("n1", 5000, false, "value-A"));
        apply_local(&dev_b, "notes", note("n1", 5000, false, "value-B"));

        // A pulls B's row (same ts) → keeps A; B pulls A's row (same ts) → keeps B.
        let a_sees_b = MapSink::new().with("notes", vec![note("n1", 5000, false, "value-B")]);
        let b_sees_a = MapSink::new().with("notes", vec![note("n1", 5000, false, "value-A")]);
        block(pull(&dev_a, &a_sees_b, &["notes"], 9000)).unwrap();
        block(pull(&dev_b, &b_sees_a, &["notes"], 9000)).unwrap();

        assert_eq!(
            dev_a.get_row("notes", "n1").unwrap().unwrap()["text"],
            json!("value-A"),
            "device A keeps its own value on the tie"
        );
        assert_eq!(
            dev_b.get_row("notes", "n1").unwrap().unwrap()["text"],
            json!("value-B"),
            "device B keeps its own — ms-identical concurrent edit did NOT reconcile (accepted)"
        );
    }
}
