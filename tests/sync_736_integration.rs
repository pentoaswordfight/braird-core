//! SUR-736/738 integration: the outbox rebase closes the "pull-then-flush re-pushes a stale edit"
//! window, and a genuinely-newer local edit still flushes. Cross-module (store + pull + push) via a
//! recording sink — OFFLINE + deterministic (no Supabase, no env guard, NOT `#[ignore]`d), so the
//! make-or-break 736 assertion runs in the fast per-PR suite, unlike the env-guarded real-Supabase
//! round-trip in `sync_725_integration.rs`. The seam under test is the SAME order `SyncEngine::sync`
//! runs: `pull` (which rebases) THEN `flush`.
#![cfg(not(target_arch = "wasm32"))]

use std::cell::RefCell;
use std::collections::HashMap;

use braird_core::store::Store;
use braird_core::sync::http::PostgrestSink;
use braird_core::sync::{pull, pull_then_flush, push};
use serde_json::{json, Value};

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(f)
}

#[derive(Debug, Clone, PartialEq)]
enum Op {
    Fetch(String),
    Upsert(String),
}

/// A PostgREST stand-in that records every call in order and serves canned remote rows. No network;
/// `upsert` always succeeds. `failing(table)` makes that table's `fetch_page` error (the partial-
/// pull-failure case). Lets us assert call order, that a rebased-away edit is never re-dispatched,
/// and that a failed table's stale edit is never flushed.
struct RecordingSink {
    remote: HashMap<String, Vec<Value>>,
    fail_fetch: Option<String>,
    log: RefCell<Vec<Op>>,
}

impl RecordingSink {
    fn new(remote: HashMap<String, Vec<Value>>) -> Self {
        Self {
            remote,
            fail_fetch: None,
            log: RefCell::new(Vec::new()),
        }
    }
    fn failing(mut self, table: &str) -> Self {
        self.fail_fetch = Some(table.to_string());
        self
    }
    fn upserted(&self, table: &str) -> bool {
        self.log
            .borrow()
            .iter()
            .any(|o| *o == Op::Upsert(table.to_string()))
    }
}

impl PostgrestSink for RecordingSink {
    async fn upsert(&self, table: &str, _on_conflict: &str, _rows: &Value) -> Result<(), String> {
        self.log.borrow_mut().push(Op::Upsert(table.to_string()));
        Ok(())
    }
    async fn fetch_page(
        &self,
        table: &str,
        _after_seq: i64,
        _limit: i64,
    ) -> Result<Vec<Value>, String> {
        self.log.borrow_mut().push(Op::Fetch(table.to_string()));
        if self.fail_fetch.as_deref() == Some(table) {
            return Err(format!("{table} fetch failed"));
        }
        Ok(self.remote.get(table).cloned().unwrap_or_default())
    }
}

fn one(table: &str, rows: Vec<Value>) -> HashMap<String, Vec<Value>> {
    let mut m = HashMap::new();
    m.insert(table.to_string(), rows);
    m
}

#[test]
fn pull_then_flush_does_not_re_push_a_rebased_edit() {
    // The SUR-736 window: a queued local edit (T1) for a record the server has a NEWER row for (T2).
    let store = Store::open_in_memory().unwrap();
    // Offline-first local write: synced row + outbox entry, both T1. `text` stays ciphertext at rest.
    store
        .stage_local_write(
            "notes",
            "n1",
            json!({ "id": "n1", "text": "enc:v2:local-T1", "updated_at": 1000, "deleted": false })
                .as_object()
                .unwrap()
                .clone(),
            1000,
        )
        .unwrap();

    let sink = RecordingSink::new(one(
        "notes",
        vec![
            json!({ "id": "n1", "text": "enc:v2:remote-T2", "content_tag": "tag", "updated_at": 2000, "deleted": false }),
        ],
    ));

    // sync()'s order: pull (which rebases) THEN flush.
    let pulled = block(pull::pull(&store, &sink, &["notes"])).unwrap();
    assert_eq!(pulled.merged, 1);
    assert_eq!(
        pulled.superseded.len(),
        1,
        "the stale local edit is surfaced as superseded"
    );
    assert!(
        store.outbox_items().unwrap().is_empty(),
        "the pull rebased the stale entry away"
    );

    let flushed = block(push::flush(&store, &sink, "user-1")).unwrap();
    assert!(
        flushed.ok.is_empty() && flushed.failed.is_empty(),
        "nothing left to flush"
    );
    assert!(
        !sink.upserted("notes"),
        "the rebased-away edit must NOT be re-pushed over the newer server row (SUR-736)"
    );
    assert_eq!(
        store.get_row("notes", "n1").unwrap().unwrap()["text"],
        json!("enc:v2:remote-T2"),
        "the store converged to the newer server row"
    );
}

#[test]
fn a_genuinely_newer_local_edit_still_flushes_after_pull() {
    // Guard against over-aggressive dropping: a local edit NEWER than anything the server returns
    // must survive the pull and flush normally — and the fetch (pull) precedes the upsert (flush).
    let store = Store::open_in_memory().unwrap();
    store
        .stage_local_write(
            "books",
            "b1",
            json!({ "id": "b1", "title": "local-newer", "updated_at": 5000, "deleted": false })
                .as_object()
                .unwrap()
                .clone(),
            5000,
        )
        .unwrap();

    // The server returns an OLDER row for the same record (loses LWW → no apply, no rebase).
    let sink = RecordingSink::new(one(
        "books",
        vec![json!({ "id": "b1", "title": "server-older", "updated_at": 3000, "deleted": false })],
    ));

    let pulled = block(pull::pull(&store, &sink, &["books"])).unwrap();
    assert_eq!(pulled.merged, 0, "the older server row loses LWW");
    assert!(pulled.superseded.is_empty());
    assert_eq!(
        store.outbox_items().unwrap().len(),
        1,
        "the newer local edit is still queued"
    );

    let flushed = block(push::flush(&store, &sink, "user-1")).unwrap();
    assert_eq!(flushed.ok.len(), 1, "the newer local edit flushes");
    assert!(sink.upserted("books"), "it IS pushed to the server");
    assert_eq!(
        store.get_row("books", "b1").unwrap().unwrap()["title"],
        json!("local-newer"),
        "the local row kept its newer value"
    );

    // Pull-then-flush ordering: the fetch precedes the upsert (the order that makes rebase work).
    let log = sink.log.borrow();
    let fetch_pos = log.iter().position(|o| matches!(o, Op::Fetch(_)));
    let upsert_pos = log.iter().position(|o| matches!(o, Op::Upsert(_)));
    assert!(
        fetch_pos < upsert_pos,
        "pull's fetch precedes flush's upsert (the sync() order): {log:?}"
    );
}

#[test]
fn partial_pull_failure_aborts_the_flush() {
    // SUR-736 (founder finding): `books` pulls fine but `notes` fetch fails, so the notes outbox
    // never rebased. `sync()` (via pull_then_flush) MUST NOT flush here — flushing would re-push the
    // stale notes edit over the (unseen) newer server row, reopening the lost-edit case sync closes.
    let store = Store::open_in_memory().unwrap();
    // A stale queued notes edit that a *successful* notes pull would have rebased away.
    store
        .stage_local_write(
            "notes",
            "n1",
            json!({ "id": "n1", "text": "enc:v2:local-T1", "updated_at": 1000, "deleted": false })
                .as_object()
                .unwrap()
                .clone(),
            1000,
        )
        .unwrap();
    let sink = RecordingSink::new(one("books", vec![])).failing("notes");

    let outcome = block(pull_then_flush(
        &store,
        &sink,
        "user-1",
        &["books", "notes"],
    ));

    assert!(outcome.is_err(), "a failed table must abort the flush");
    assert!(
        !sink.upserted("notes"),
        "the un-rebased stale notes edit must NOT be flushed (SUR-736)"
    );
    assert!(
        !sink.upserted("books"),
        "no table is flushed when the pull wasn't fully clean"
    );
    assert_eq!(
        store.outbox_items().unwrap().len(),
        1,
        "the stale edit stays queued for a later clean sync"
    );
}

#[test]
fn pull_then_flush_pulls_then_flushes_on_a_clean_pull() {
    // Happy path: a clean pull → the queued local edit flushes, fetch precedes upsert.
    let store = Store::open_in_memory().unwrap();
    store
        .stage_local_write(
            "books",
            "b1",
            json!({ "id": "b1", "title": "local", "updated_at": 5000, "deleted": false })
                .as_object()
                .unwrap()
                .clone(),
            5000,
        )
        .unwrap();
    let sink = RecordingSink::new(one("books", vec![]));

    let (pulled, flushed) =
        block(pull_then_flush(&store, &sink, "user-1", &["books"])).expect("clean sync");

    assert!(pulled.failed_tables.is_empty());
    assert_eq!(flushed.ok.len(), 1, "the local edit flushed");
    assert!(sink.upserted("books"));
    let log = sink.log.borrow();
    let fetch_pos = log.iter().position(|o| matches!(o, Op::Fetch(_)));
    let upsert_pos = log.iter().position(|o| matches!(o, Op::Upsert(_)));
    assert!(fetch_pos < upsert_pos, "pull before flush: {log:?}");
}
