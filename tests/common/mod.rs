//! Two-engine sync fixture (SUR-976): two simulated devices sharing one simulated cloud, driven
//! entirely in-process — OFFLINE + deterministic (no Supabase, no env guard), like
//! `sync_736_integration.rs` but bidirectional: unlike every canned/log-only stub sink, a
//! [`SharedCloud`] `upsert` genuinely lands rows that a later `fetch_page` (from either device)
//! serves back. Built for the SUR-976 cross-device orphan interleave and reusable for any future
//! sync-surface interleave test: construct one `SharedCloud`, two [`Device`]s on one shared
//! `Vault`, and drive `pull_then_flush`/`push::flush` per device in whatever order the scenario
//! needs.
#![allow(dead_code)] // each integration-test binary compiles its own copy and uses a subset

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use braird_core::store::Store;
use braird_core::sync::http::{CoverEgress, PostgrestSink};
use braird_core::sync::SyncEngine;
use braird_core::Vault;
use serde_json::{json, Value};

pub fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(f)
}

/// The in-process cloud: one map per table, keyed by the upsert's `on_conflict` column, stamping a
/// monotonic `change_seq` on every ACCEPTED write (the server's `t02_change_seq` trigger). Conflict
/// rule mirrors the real server's `t01_lww_guard` BEFORE UPDATE trigger
/// (surfc `supabase/migrations/0050_sur740_lww_guard.sql`): an incoming row STRICTLY older by
/// `updated_at` than the stored one is silently dropped (no error, no `change_seq` bump); equal or
/// newer replaces; a first insert always lands. `fetch_page` is `PagingSink`'s keyset logic
/// (`change_seq > after_seq`, ascending, `limit`-capped) reading the shared, mutated table.
///
/// `RefCell`/`Cell`, not `Mutex`: the harness runs on a single-threaded `block()` executor, the
/// same justification `sync_736_integration.rs`'s `RecordingSink` uses for its `RefCell` log.
// ponytail: single-tenant — no user_id partitioning; add a per-user submap only if a future test
// needs cross-account isolation.
/// One cloud table: rows keyed by the upsert's `on_conflict` value, each carrying the
/// `change_seq` it was accepted at.
type CloudTable = BTreeMap<String, (i64, Value)>;

pub struct SharedCloud {
    tables: RefCell<HashMap<String, CloudTable>>,
    next_seq: Cell<i64>,
}

impl SharedCloud {
    pub fn new() -> Self {
        Self {
            tables: RefCell::new(HashMap::new()),
            next_seq: Cell::new(0),
        }
    }

    /// The stored cloud row for `(table, key)` — for asserting what the fleet would see.
    pub fn row(&self, table: &str, key: &str) -> Option<Value> {
        self.tables
            .borrow()
            .get(table)
            .and_then(|t| t.get(key))
            .map(|(_, row)| row.clone())
    }
}

impl PostgrestSink for SharedCloud {
    async fn upsert(&self, table: &str, on_conflict: &str, rows: &Value) -> Result<(), String> {
        let rows = rows
            .as_array()
            .ok_or_else(|| format!("{table} upsert payload is not an array"))?;
        let mut tables = self.tables.borrow_mut();
        let table_map = tables.entry(table.to_string()).or_default();
        for row in rows {
            let key = row
                .get(on_conflict)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{table} row lacks on_conflict key `{on_conflict}`"))?
                .to_string();
            let incoming_updated = row.get("updated_at").and_then(Value::as_i64).unwrap_or(0);
            if let Some((_, stored)) = table_map.get(&key) {
                let stored_updated = stored
                    .get("updated_at")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                // t01_lww_guard: NEW.updated_at < OLD.updated_at → cancel the row, silently
                // (statement still succeeds; the cancelled row never reaches t02_change_seq).
                if incoming_updated < stored_updated {
                    continue;
                }
            }
            let seq = self.next_seq.get() + 1;
            self.next_seq.set(seq);
            let mut stored = row.clone();
            stored["change_seq"] = json!(seq);
            table_map.insert(key, (seq, stored));
        }
        Ok(())
    }

    async fn fetch_page(
        &self,
        table: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<Value>, String> {
        let tables = self.tables.borrow();
        let mut page: Vec<Value> = tables
            .get(table)
            .map(|t| {
                t.values()
                    .filter(|(seq, _)| *seq > after_seq)
                    .map(|(_, row)| row.clone())
                    .collect()
            })
            .unwrap_or_default();
        page.sort_by_key(|r| r["change_seq"].as_i64().unwrap_or(0));
        page.truncate(limit as usize);
        Ok(page)
    }

    // Keep the SUR-828 Open Library egress off so coverless fixtures never trigger
    // cover-resolution writes mid-scenario (same choice as sync_736's RecordingSink).
    async fn fetch_app_config(&self, _key: &str) -> Result<Option<Value>, String> {
        Ok(Some(json!({ "enabled": false })))
    }
}

impl CoverEgress for SharedCloud {}

/// One simulated device: a real `SyncEngine` (the FFI-level ops — `enqueue_note`,
/// `record_note_signal` — must run the REAL staging logic, not a fixture re-implementation that
/// could drift into asserting invented behavior) plus a second `Store` handle on the same SQLite
/// file for driving the free `pull_then_flush`/`push::flush` fns, which take `&Store`. Two
/// connections on one file is the established in-repo test pattern (`mod.rs` tests open
/// `Store::open(db_path)` alongside a live engine); access here is strictly sequential.
pub struct Device {
    pub engine: Arc<SyncEngine>,
    pub store: Store,
    pub vault: Arc<Vault>,
    _dir: tempfile::TempDir,
}

impl Device {
    pub fn new(vault: Arc<Vault>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("device.sqlite");
        let db_path = db_path.to_str().unwrap().to_string();
        let engine = SyncEngine::open(
            db_path.clone(),
            "https://x.supabase.co".into(),
            "anon".into(),
            vault.clone(),
        )
        .unwrap();
        let store = Store::open(&db_path).unwrap();
        Self {
            engine,
            store,
            vault,
            _dir: dir,
        }
    }
}

/// Strictly order the next wall-clock `epoch_ms()` stamp after every prior one — the repo's
/// established convention for interleave tests with no injectable clock
/// (`sync_659b_integration.rs`, `sync_726_integration.rs`): 2ms comfortably exceeds OS clock
/// granularity, so LWW's strict-`>` compares never land on a nondeterministic tie.
pub fn tick() {
    std::thread::sleep(std::time::Duration::from_millis(2));
}
