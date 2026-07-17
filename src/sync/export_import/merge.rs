use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::import::{parse_import_at, NormalizedImport, NormalizedRow};
use crate::store::{synced_table_names, table_schema, ImportWrite, Store};
use crate::sync::http::PostgrestSink;
use crate::sync::{pull, ImportCounts, ImportSummary, SyncError};
use crate::vault::Vault;

const FETCH_CHUNK_SIZE: usize = 100;
const IMPORT_PREFLIGHT_FAILED: &str = "snapshot import server preflight failed";
const IMPORT_STORE_FAILED: &str = "snapshot import persistence failed";

/// Parse the archive before entering the caller's operational closure. The public wrapper puts
/// every lock, token check, network call, crypto operation, and write inside this closure; tests
/// inject the generic sink through the same successful parse-to-execute seam.
pub(in crate::sync) fn with_parsed_import_at<T>(
    archive_json: &str,
    import_now: i64,
    execute: impl FnOnce(NormalizedImport) -> Result<T, SyncError>,
) -> Result<T, SyncError> {
    let parsed = parse_import_at(archive_json, import_now)?;
    execute(parsed)
}

/// Redacts transport failures before they can reach pull logging or an FFI error. Production
/// PostgREST errors can include response bodies, which are not safe import diagnostics.
struct SanitizedSink<'a, S>(&'a S);

impl<S: PostgrestSink> PostgrestSink for SanitizedSink<'_, S> {
    async fn upsert(&self, table: &str, on_conflict: &str, rows: &Value) -> Result<(), String> {
        self.0
            .upsert(table, on_conflict, rows)
            .await
            .map_err(|_| IMPORT_PREFLIGHT_FAILED.into())
    }

    async fn fetch_page(
        &self,
        table: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<Value>, String> {
        let rows = self
            .0
            .fetch_page(table, after_seq, limit)
            .await
            .map_err(|_| IMPORT_PREFLIGHT_FAILED.to_string())?;
        validate_pull_page(table, after_seq, &rows)?;
        Ok(rows)
    }

    async fn fetch_by_ids(
        &self,
        table: &str,
        primary_key: &str,
        ids: &[String],
    ) -> Result<Vec<Value>, String> {
        self.0
            .fetch_by_ids(table, primary_key, ids)
            .await
            .map_err(|_| IMPORT_PREFLIGHT_FAILED.into())
    }
}

/// Import's protective pull must fail closed on a malformed server page. Ordinary sync keeps its
/// established per-row defensive skipping/defaulting semantics; this validation lives on the
/// import-only sink wrapper so an incomplete or malformed preflight can never authorize staging.
/// The page must also honor PostgREST's exclusive ascending `change_seq` keyset contract.
fn validate_pull_page(table: &str, after_seq: i64, rows: &[Value]) -> Result<(), String> {
    let primary_key = table_schema(table)
        .ok_or_else(|| IMPORT_PREFLIGHT_FAILED.to_string())?
        .pk[0];
    let mut prior_change_seq = after_seq;

    for value in rows {
        let Some(row) = value.as_object() else {
            return Err(IMPORT_PREFLIGHT_FAILED.into());
        };
        let valid_primary_key = row
            .get(primary_key)
            .and_then(Value::as_str)
            .is_some_and(|primary_key| !primary_key.is_empty());
        let change_seq = row
            .get("change_seq")
            .and_then(Value::as_i64)
            .filter(|change_seq| *change_seq >= 0)
            .ok_or_else(|| IMPORT_PREFLIGHT_FAILED.to_string())?;
        let valid_updated_at = row.get("updated_at").and_then(Value::as_i64).is_some();
        let valid_deleted = row.get("deleted").and_then(Value::as_bool).is_some();

        if !(valid_primary_key && valid_updated_at && valid_deleted)
            || change_seq <= prior_change_seq
        {
            return Err(IMPORT_PREFLIGHT_FAILED.into());
        }
        prior_change_seq = change_seq;
    }

    Ok(())
}

/// Operational half of `SyncEngine::import_merge`, generic over the network seam for deterministic
/// tests. `parsed` has already passed the pure parser before the public method takes any lock.
pub(in crate::sync) async fn merge_parsed_with_sink<S: PostgrestSink>(
    store: &Store,
    sink: &S,
    vault: &Vault,
    parsed: NormalizedImport,
    import_now: i64,
) -> Result<ImportSummary, SyncError> {
    let tables = synced_table_names();
    let sink = SanitizedSink(sink);
    let pulled = pull::pull(store, &sink, &tables)
        .await
        .map_err(|_| SyncError::Flush(IMPORT_PREFLIGHT_FAILED.into()))?;
    if !pulled.failed_tables.is_empty() {
        return Err(SyncError::Flush(IMPORT_PREFLIGHT_FAILED.into()));
    }

    let server = fetch_candidate_state(&sink, &parsed).await?;
    select_prepare_and_stage(store, vault, parsed, server, import_now)
}

async fn fetch_candidate_state<S: PostgrestSink>(
    sink: &S,
    parsed: &NormalizedImport,
) -> Result<HashMap<&'static str, HashMap<String, Map<String, Value>>>, SyncError> {
    let mut all_server_rows = HashMap::new();
    for (table, candidates) in borrowed_tables(parsed) {
        if candidates.is_empty() {
            continue;
        }
        let schema =
            table_schema(table).ok_or_else(|| SyncError::Store(IMPORT_STORE_FAILED.into()))?;
        let primary_key = schema.pk[0];
        let mut server_rows = HashMap::new();
        for chunk in candidates.chunks(FETCH_CHUNK_SIZE) {
            let ids: Vec<String> = chunk
                .iter()
                .map(|candidate| candidate.primary_key.clone())
                .collect();
            let requested: HashSet<&str> = ids.iter().map(String::as_str).collect();
            let returned = sink
                .fetch_by_ids(table, primary_key, &ids)
                .await
                .map_err(|_| SyncError::Flush(IMPORT_PREFLIGHT_FAILED.into()))?;
            for value in returned {
                let row = value
                    .as_object()
                    .ok_or_else(|| SyncError::Flush(IMPORT_PREFLIGHT_FAILED.into()))?;
                let id = row
                    .get(primary_key)
                    .and_then(Value::as_str)
                    .filter(|id| requested.contains(*id))
                    .ok_or_else(|| SyncError::Flush(IMPORT_PREFLIGHT_FAILED.into()))?;
                if row.get("updated_at").and_then(Value::as_i64).is_none()
                    || row.get("deleted").and_then(Value::as_bool).is_none()
                    || server_rows.contains_key(id)
                {
                    return Err(SyncError::Flush(IMPORT_PREFLIGHT_FAILED.into()));
                }
                server_rows.insert(id.to_string(), row.clone());
            }
        }
        all_server_rows.insert(table, server_rows);
    }
    Ok(all_server_rows)
}

fn select_prepare_and_stage(
    store: &Store,
    vault: &Vault,
    parsed: NormalizedImport,
    server: HashMap<&'static str, HashMap<String, Map<String, Value>>>,
    import_now: i64,
) -> Result<ImportSummary, SyncError> {
    let schema_version = parsed.schema_version;
    let mut imported = ImportCounts::default();
    let mut skipped_stale = ImportCounts::default();
    let mut accepted = Vec::new();
    let mut newest_compared_for_accepted: Option<i64> = None;

    for (table, candidates) in owned_tables(parsed) {
        for candidate in candidates {
            let local = store
                .get_row(table, &candidate.primary_key)
                .map_err(|_| SyncError::Store(IMPORT_STORE_FAILED.into()))?;
            let local_updated = state_timestamp(local.as_ref())?;
            let server_updated = server
                .get(table)
                .and_then(|rows| rows.get(&candidate.primary_key))
                .map(|row| {
                    row.get("updated_at")
                        .and_then(Value::as_i64)
                        .ok_or_else(|| SyncError::Flush(IMPORT_PREFLIGHT_FAILED.into()))
                })
                .transpose()?;
            let newest_existing = local_updated.into_iter().chain(server_updated).max();

            if newest_existing.is_some_and(|updated| candidate.updated_at <= updated) {
                increment(&mut skipped_stale, table)?;
                continue;
            }

            increment(&mut imported, table)?;
            for timestamp in [Some(candidate.updated_at), local_updated, server_updated]
                .into_iter()
                .flatten()
            {
                newest_compared_for_accepted = Some(
                    newest_compared_for_accepted
                        .map_or(timestamp, |current| current.max(timestamp)),
                );
            }
            accepted.push((table, candidate));
        }
    }

    if accepted.is_empty() {
        return Ok(ImportSummary {
            schema_version,
            imported,
            skipped_stale,
        });
    }

    let advanced = newest_compared_for_accepted
        .expect("every accepted candidate contributes its archive timestamp")
        .checked_add(1)
        .ok_or_else(|| {
            SyncError::InvalidImport("snapshot import timestamp cannot advance safely".into())
        })?;
    let batch_timestamp = import_now.max(advanced);

    let writes = accepted
        .into_iter()
        .map(|(table, candidate)| prepare_write(table, candidate, batch_timestamp, vault))
        .collect::<Result<Vec<_>, _>>()?;
    store
        .stage_import_batch(&writes, batch_timestamp)
        .map_err(|_| SyncError::Store(IMPORT_STORE_FAILED.into()))?;

    Ok(ImportSummary {
        schema_version,
        imported,
        skipped_stale,
    })
}

fn state_timestamp(row: Option<&Map<String, Value>>) -> Result<Option<i64>, SyncError> {
    row.map(|row| {
        row.get("updated_at")
            .and_then(Value::as_i64)
            .ok_or_else(|| SyncError::Store(IMPORT_STORE_FAILED.into()))
    })
    .transpose()
}

fn prepare_write(
    table: &'static str,
    candidate: NormalizedRow,
    batch_timestamp: i64,
    vault: &Vault,
) -> Result<ImportWrite, SyncError> {
    let record_id = candidate.primary_key;
    let mut row = candidate.row;

    if table == "notes" {
        let text = row.remove("text");
        row.remove("content_tag");
        match text {
            Some(Value::String(plaintext)) => {
                let book_id = match row.get("book_id") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(book_id)) => Some(book_id.clone()),
                    Some(_) => {
                        return Err(SyncError::InvalidImport("invalid normalized note".into()))
                    }
                };
                let content_tag = vault.content_tag(plaintext.clone(), book_id);
                let ciphertext = vault.encrypt_note(Some(record_id.clone()), plaintext);
                row.insert("text".into(), Value::String(ciphertext));
                row.insert("content_tag".into(), Value::String(content_tag));
            }
            None | Some(Value::Null) => {
                row.insert("text".into(), Value::Null);
                row.insert("content_tag".into(), Value::Null);
            }
            Some(_) => return Err(SyncError::InvalidImport("invalid normalized note".into())),
        }
    }

    row.insert("updated_at".into(), Value::from(batch_timestamp));
    row.insert("deleted".into(), Value::Bool(false));
    let schema = table_schema(table).ok_or_else(|| SyncError::Store(IMPORT_STORE_FAILED.into()))?;
    let complete = schema
        .columns
        .iter()
        .map(|(column, _)| {
            (
                (*column).to_string(),
                row.remove(*column).unwrap_or(Value::Null),
            )
        })
        .collect();
    Ok(ImportWrite::new(table, record_id, complete))
}

fn increment(counts: &mut ImportCounts, table: &str) -> Result<(), SyncError> {
    let count = match table {
        "books" => &mut counts.books,
        "notes" => &mut counts.notes,
        "custom_ideas" => &mut counts.custom_ideas,
        "note_links" => &mut counts.note_links,
        "lenses" => &mut counts.lenses,
        "collections" => &mut counts.collections,
        "collection_memberships" => &mut counts.collection_memberships,
        "note_signals" => &mut counts.note_signals,
        _ => return Err(SyncError::Store(IMPORT_STORE_FAILED.into())),
    };
    *count = count
        .checked_add(1)
        .ok_or_else(|| SyncError::InvalidImport("snapshot import row count is too large".into()))?;
    Ok(())
}

fn borrowed_tables(parsed: &NormalizedImport) -> [(&'static str, &[NormalizedRow]); 8] {
    [
        ("books", &parsed.books),
        ("notes", &parsed.notes),
        ("custom_ideas", &parsed.custom_ideas),
        ("note_links", &parsed.note_links),
        ("lenses", &parsed.lenses),
        ("collections", &parsed.collections),
        ("collection_memberships", &parsed.collection_memberships),
        ("note_signals", &parsed.note_signals),
    ]
}

fn owned_tables(parsed: NormalizedImport) -> [(&'static str, Vec<NormalizedRow>); 8] {
    [
        ("books", parsed.books),
        ("notes", parsed.notes),
        ("custom_ideas", parsed.custom_ideas),
        ("note_links", parsed.note_links),
        ("lenses", parsed.lenses),
        ("collections", parsed.collections),
        ("collection_memberships", parsed.collection_memberships),
        ("note_signals", parsed.note_signals),
    ]
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};
    use std::future::Future;

    use serde_json::{json, Value};

    use super::super::import::parse_import_at;
    use super::{merge_parsed_with_sink, with_parsed_import_at};
    use crate::store::{synced_schema, synced_table_names, Store};
    use crate::sync::http::PostgrestSink;
    use crate::sync::{ImportCounts, SyncError};
    use crate::vault::Vault;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Pull(String),
        Fetch {
            table: String,
            primary_key: String,
            ids: Vec<String>,
        },
        Upsert(String),
    }

    type CannedResults = RefCell<HashMap<String, VecDeque<Result<Vec<Value>, String>>>>;

    #[derive(Default)]
    struct RecordingSink {
        calls: RefCell<Vec<Call>>,
        pull_results: CannedResults,
        fetch_results: CannedResults,
    }

    impl RecordingSink {
        fn pull_result(&self, table: &str, result: Result<Vec<Value>, &str>) {
            self.pull_results
                .borrow_mut()
                .entry(table.into())
                .or_default()
                .push_back(result.map_err(str::to_string));
        }

        fn fetch_result(&self, table: &str, result: Result<Vec<Value>, &str>) {
            self.fetch_results
                .borrow_mut()
                .entry(table.into())
                .or_default()
                .push_back(result.map_err(str::to_string));
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }

        fn next(configured: &CannedResults, table: &str) -> Result<Vec<Value>, String> {
            configured
                .borrow_mut()
                .get_mut(table)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| Ok(Vec::new()))
        }
    }

    impl PostgrestSink for RecordingSink {
        async fn upsert(
            &self,
            table: &str,
            _on_conflict: &str,
            _rows: &Value,
        ) -> Result<(), String> {
            self.calls.borrow_mut().push(Call::Upsert(table.into()));
            Err("snapshot import must not auto-flush".into())
        }

        async fn fetch_page(
            &self,
            table: &str,
            _after_seq: i64,
            _limit: i64,
        ) -> Result<Vec<Value>, String> {
            self.calls.borrow_mut().push(Call::Pull(table.into()));
            Self::next(&self.pull_results, table)
        }

        async fn fetch_by_ids(
            &self,
            table: &str,
            primary_key: &str,
            ids: &[String],
        ) -> Result<Vec<Value>, String> {
            self.calls.borrow_mut().push(Call::Fetch {
                table: table.into(),
                primary_key: primary_key.into(),
                ids: ids.to_vec(),
            });
            Self::next(&self.fetch_results, table)
        }
    }

    fn run<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn archive_with(field: &str, rows: Vec<Value>) -> String {
        let mut root = json!({
            "_syntopicon": true,
            "schemaVersion": 19,
            "books": [],
            "notes": [],
            "customIdeas": [],
            "noteLinks": [],
            "lenses": [],
            "collections": [],
            "collectionMemberships": [],
            "noteSignals": []
        });
        root[field] = Value::Array(rows);
        root.to_string()
    }

    fn parsed(field: &str, rows: Vec<Value>, now: i64) -> super::super::import::NormalizedImport {
        parse_import_at(&archive_with(field, rows), now).unwrap()
    }

    fn server_row(id: &str, updated_at: i64, deleted: bool) -> Value {
        json!({"id":id,"updated_at":updated_at,"deleted":deleted})
    }

    fn stage_local(store: &Store, id: &str, updated_at: i64, deleted: bool) {
        store
            .apply_row(
                "books",
                json!({
                    "id":id,"title":"local","created_at":1,
                    "updated_at":updated_at,"deleted":deleted
                })
                .as_object()
                .unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn clean_pull_precedes_targeted_fetch_and_partial_or_total_failure_stages_nothing() {
        let archive = parsed(
            "books",
            vec![json!({"id":"import-b","title":"archive","updatedAt":50})],
            10,
        );
        let store = Store::open_in_memory().unwrap();
        let sink = RecordingSink::default();
        sink.pull_result(
            "books",
            Ok(vec![json!({
                "id":"remote-b","title":"pulled","updated_at":20,
                "deleted":false,"change_seq":1
            })]),
        );
        sink.pull_result("notes", Err("SERVER_BODY_SECRET"));

        let error = run(merge_parsed_with_sink(
            &store,
            &sink,
            &Vault::generate(),
            archive,
            10,
        ))
        .unwrap_err();

        assert!(!error.to_string().contains("SERVER_BODY_SECRET"));
        assert!(store.get_row("books", "remote-b").unwrap().is_some());
        assert!(store.get_row("books", "import-b").unwrap().is_none());
        assert!(store.outbox_items().unwrap().is_empty());
        assert!(!sink
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Fetch { .. })));

        let archive = parsed(
            "books",
            vec![json!({"id":"import-b","title":"archive","updatedAt":50})],
            10,
        );
        let store = Store::open_in_memory().unwrap();
        let sink = RecordingSink::default();
        for table in synced_table_names() {
            sink.pull_result(table, Err("TOTAL_FAILURE_BODY"));
        }
        let error = run(merge_parsed_with_sink(
            &store,
            &sink,
            &Vault::generate(),
            archive,
            10,
        ))
        .unwrap_err();
        assert!(!error.to_string().contains("TOTAL_FAILURE_BODY"));
        assert!(store.outbox_items().unwrap().is_empty());
        assert!(!sink
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Fetch { .. })));
    }

    #[test]
    fn malformed_short_pull_rows_fail_preflight_before_fetch_or_import_staging() {
        let scenarios = [
            ("non-object", json!("not an object")),
            (
                "missing primary key",
                json!({"updated_at":20,"deleted":false,"change_seq":1}),
            ),
            (
                "empty primary key",
                json!({"id":"","updated_at":20,"deleted":false,"change_seq":1}),
            ),
            (
                "missing change_seq",
                json!({"id":"remote-b","updated_at":20,"deleted":false}),
            ),
            (
                "negative change_seq",
                json!({"id":"remote-b","updated_at":20,"deleted":false,"change_seq":-1}),
            ),
            (
                "non-integer change_seq",
                json!({"id":"remote-b","updated_at":20,"deleted":false,"change_seq":"bad"}),
            ),
            (
                "missing updated_at",
                json!({"id":"remote-b","deleted":false,"change_seq":1}),
            ),
            (
                "non-integer updated_at",
                json!({"id":"remote-b","updated_at":"bad","deleted":false,"change_seq":1}),
            ),
            (
                "missing deleted",
                json!({"id":"remote-b","updated_at":20,"change_seq":1}),
            ),
            (
                "invalid deleted",
                json!({"id":"remote-b","updated_at":20,"deleted":0,"change_seq":1}),
            ),
        ];
        let mut accepted = Vec::new();

        for (name, malformed) in scenarios {
            let archive = parsed(
                "books",
                vec![json!({"id":"import-b","title":"archive","updatedAt":50})],
                10,
            );
            let store = Store::open_in_memory().unwrap();
            let sink = RecordingSink::default();
            sink.pull_result("books", Ok(vec![malformed]));

            let outcome = run(merge_parsed_with_sink(
                &store,
                &sink,
                &Vault::generate(),
                archive,
                10,
            ));
            let fetched = sink
                .calls()
                .iter()
                .any(|call| matches!(call, Call::Fetch { .. }));
            let staged = store.get_row("books", "import-b").unwrap().is_some()
                || !store.outbox_items().unwrap().is_empty();
            match outcome {
                Ok(_) => accepted.push(name),
                Err(_) if fetched || staged => accepted.push(name),
                Err(error) => assert!(!error.to_string().contains("remote-b")),
            }
        }

        assert!(
            accepted.is_empty(),
            "malformed pull rows passed import preflight: {accepted:?}"
        );
    }

    #[test]
    fn replayed_or_non_monotonic_pull_pages_fail_before_fetch_or_import_staging() {
        let scenarios = [
            (
                "sequence equal to cursor",
                10,
                vec![json!({
                    "id":"remote-a","updated_at":20,"deleted":false,"change_seq":10
                })],
            ),
            (
                "sequence below cursor",
                10,
                vec![json!({
                    "id":"remote-a","updated_at":20,"deleted":false,"change_seq":9
                })],
            ),
            (
                "out-of-order sequence",
                0,
                vec![
                    json!({
                        "id":"remote-a","updated_at":20,"deleted":false,"change_seq":2
                    }),
                    json!({
                        "id":"remote-b","updated_at":20,"deleted":false,"change_seq":1
                    }),
                ],
            ),
            (
                "duplicate sequence",
                0,
                vec![
                    json!({
                        "id":"remote-a","updated_at":20,"deleted":false,"change_seq":1
                    }),
                    json!({
                        "id":"remote-b","updated_at":20,"deleted":false,"change_seq":1
                    }),
                ],
            ),
        ];
        let mut accepted = Vec::new();

        for (name, cursor, page) in scenarios {
            let archive = parsed(
                "books",
                vec![json!({"id":"import-b","title":"archive","updatedAt":50})],
                10,
            );
            let store = Store::open_in_memory().unwrap();
            store.set_seq_cursor("books", cursor).unwrap();
            let sink = RecordingSink::default();
            sink.pull_result("books", Ok(page));

            let outcome = run(merge_parsed_with_sink(
                &store,
                &sink,
                &Vault::generate(),
                archive,
                10,
            ));
            let fetched = sink
                .calls()
                .iter()
                .any(|call| matches!(call, Call::Fetch { .. }));
            let staged = store.get_row("books", "import-b").unwrap().is_some()
                || !store.outbox_items().unwrap().is_empty();
            match outcome {
                Ok(_) => accepted.push(name),
                Err(_) if fetched || staged => accepted.push(name),
                Err(error) => {
                    let message = error.to_string();
                    assert!(!message.contains("remote-a"));
                    assert!(!message.contains("remote-b"));
                }
            }
        }

        assert!(
            accepted.is_empty(),
            "invalid pull pagination passed import preflight: {accepted:?}"
        );
    }

    #[test]
    fn public_success_operation_seam_parses_then_executes_with_a_test_sink() {
        let store = Store::open_in_memory().unwrap();
        let sink = RecordingSink::default();
        let archive = archive_with(
            "books",
            vec![json!({"id":"b1","title":"Imported","updatedAt":50})],
        );
        let summary = with_parsed_import_at(&archive, 10, |parsed| {
            run(merge_parsed_with_sink(
                &store,
                &sink,
                &Vault::generate(),
                parsed,
                10,
            ))
        })
        .unwrap();

        assert_eq!(summary.schema_version, 19);
        assert_eq!(summary.imported.books, 1);
        assert!(store.get_row("books", "b1").unwrap().is_some());
        assert!(!sink
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Upsert(_))));
    }

    #[test]
    fn targeted_fetch_chunks_all_candidates_and_uses_descriptor_primary_key() {
        let mut root: Value = serde_json::from_str(&archive_with("books", Vec::new())).unwrap();
        root["books"] = Value::Array(
            (0..201)
                .map(|i| json!({"id":format!("b{i}"),"title":"B","updatedAt":50}))
                .collect(),
        );
        root["noteSignals"] = json!([{"noteId":"n-signal","updatedAt":50}]);
        let parsed = parse_import_at(&root.to_string(), 10).unwrap();
        let store = Store::open_in_memory().unwrap();
        let sink = RecordingSink::default();

        let summary = run(merge_parsed_with_sink(
            &store,
            &sink,
            &Vault::generate(),
            parsed,
            10,
        ))
        .unwrap();

        assert_eq!(summary.imported.books, 201);
        assert_eq!(summary.imported.note_signals, 1);
        let fetches: Vec<_> = sink
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Fetch {
                    table,
                    primary_key,
                    ids,
                } => Some((table, primary_key, ids)),
                _ => None,
            })
            .collect();
        assert_eq!(
            fetches
                .iter()
                .filter(|(table, _, _)| table == "books")
                .map(|(_, _, ids)| ids.len())
                .collect::<Vec<_>>(),
            vec![100, 100, 1]
        );
        assert!(fetches.iter().any(|(table, pk, ids)| {
            table == "note_signals" && pk == "note_id" && ids == &["n-signal"]
        }));
        let calls = sink.calls();
        let first_fetch = calls
            .iter()
            .position(|call| matches!(call, Call::Fetch { .. }))
            .unwrap();
        assert_eq!(first_fetch, 8, "all eight pull calls must precede fetches");
        assert!(!calls.iter().any(|call| matches!(call, Call::Upsert(_))));
    }

    #[test]
    fn targeted_fetch_fails_closed_on_error_or_malformed_duplicate_unrequested_rows() {
        let scenarios = vec![
            Err("FETCH_BODY_SECRET"),
            Ok(vec![json!("not an object")]),
            Ok(vec![json!({"updated_at":1,"deleted":false})]),
            Ok(vec![json!({"id":"b1","updated_at":"bad","deleted":false})]),
            Ok(vec![json!({"id":"b1","updated_at":1,"deleted":"bad"})]),
            Ok(vec![server_row("b1", 1, false), server_row("b1", 1, false)]),
            Ok(vec![server_row("unrequested-secret-id", 1, false)]),
        ];

        for result in scenarios {
            let store = Store::open_in_memory().unwrap();
            let sink = RecordingSink::default();
            sink.fetch_result("books", result);
            let archive = parsed(
                "books",
                vec![json!({"id":"b1","title":"archive","updatedAt":50})],
                10,
            );

            let error = run(merge_parsed_with_sink(
                &store,
                &sink,
                &Vault::generate(),
                archive,
                10,
            ))
            .unwrap_err();

            let message = error.to_string();
            assert!(!message.contains("FETCH_BODY_SECRET"));
            assert!(!message.contains("unrequested-secret-id"));
            assert!(store.get_row("books", "b1").unwrap().is_none());
            assert!(store.outbox_items().unwrap().is_empty());
        }
    }

    #[test]
    fn strict_local_and_server_lww_includes_tombstones_and_clock_skew() {
        let rows = [
            "missing",
            "local-older",
            "local-equal",
            "local-newer",
            "server-older",
            "server-equal",
            "server-newer",
            "tombstone-newer",
            "tombstone-older",
        ]
        .into_iter()
        .map(|id| json!({"id":id,"title":"archive","updatedAt":50}))
        .collect();
        let archive = parsed("books", rows, 10);
        let store = Store::open_in_memory().unwrap();
        stage_local(&store, "local-older", 49, false);
        stage_local(&store, "local-equal", 50, false);
        stage_local(&store, "local-newer", 51, false);
        let sink = RecordingSink::default();
        sink.fetch_result(
            "books",
            Ok(vec![
                server_row("server-older", 49, false),
                server_row("server-equal", 50, false),
                server_row("server-newer", 51, false),
                server_row("tombstone-newer", 51, true),
                server_row("tombstone-older", 49, true),
            ]),
        );

        let summary = run(merge_parsed_with_sink(
            &store,
            &sink,
            &Vault::generate(),
            archive,
            10,
        ))
        .unwrap();

        assert_eq!(summary.imported.books, 4);
        assert_eq!(summary.skipped_stale.books, 5);
        for id in ["missing", "local-older", "server-older", "tombstone-older"] {
            let row = store.get_row("books", id).unwrap().unwrap();
            assert_eq!(row["updated_at"], json!(51), "clock-skew stamp for {id}");
            assert_eq!(row["deleted"], json!(false));
        }
        assert_eq!(
            store.get_row("books", "local-equal").unwrap().unwrap()["title"],
            json!("local")
        );
        assert!(store.get_row("books", "server-equal").unwrap().is_none());

        let store = Store::open_in_memory().unwrap();
        let summary = run(merge_parsed_with_sink(
            &store,
            &RecordingSink::default(),
            &Vault::generate(),
            parsed(
                "books",
                vec![json!({"id":"clock-ahead","title":"A","updatedAt":50})],
                100,
            ),
            100,
        ))
        .unwrap();
        assert_eq!(summary.imported.books, 1);
        assert_eq!(
            store.get_row("books", "clock-ahead").unwrap().unwrap()["updated_at"],
            json!(100)
        );
    }

    #[test]
    fn timestamp_overflow_fails_before_encryption_or_writes() {
        let store = Store::open_in_memory().unwrap();
        let marker = "overflow-plaintext-secret";
        let error = run(merge_parsed_with_sink(
            &store,
            &RecordingSink::default(),
            &Vault::generate(),
            parsed(
                "notes",
                vec![json!({"id":"n1","text":marker,"updatedAt":i64::MAX})],
                10,
            ),
            10,
        ))
        .unwrap_err();

        assert!(matches!(error, SyncError::InvalidImport(_)));
        assert!(!error.to_string().contains(marker));
        assert!(store.get_row("notes", "n1").unwrap().is_none());
        assert!(store.outbox_items().unwrap().is_empty());
    }

    /// `n-legacy` OMITS `text`; `n-null-text` sets it explicitly to `null` — the shape the exporter
    /// actually emits, since `map_fields` writes every note key (SUR-934). Both must import as a
    /// text-less note with no invented content tag. Only the omitted form was covered before, which is
    /// why `normalize_note` could reject an explicit null and nobody noticed the core was unable to
    /// re-import its own export.
    #[test]
    fn imported_notes_are_sealed_and_retagged_while_omitted_or_null_legacy_text_stays_null() {
        let marker = "snapshot plaintext must never persist";
        let vault = Vault::generate();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("encrypted-import.sqlite");
        let mut root: Value = serde_json::from_str(&archive_with("notes", Vec::new())).unwrap();
        root["notes"] = json!([
            {"id":"n-secret","bookId":"b1","text":marker,"contentTag":"foreign","updatedAt":50},
            {"id":"n-legacy","bookId":null,"updatedAt":50},
            {"id":"n-null-text","bookId":null,"text":null,"updatedAt":50}
        ]);
        let parsed = parse_import_at(&root.to_string(), 10).unwrap();
        let store = Store::open(db.to_str().unwrap()).unwrap();

        let summary = run(merge_parsed_with_sink(
            &store,
            &RecordingSink::default(),
            &vault,
            parsed,
            10,
        ))
        .unwrap();

        assert_eq!(summary.imported.notes, 3);
        let secret = store.get_row("notes", "n-secret").unwrap().unwrap();
        let ciphertext = secret["text"].as_str().unwrap();
        assert!(ciphertext.starts_with("enc:v2:"));
        assert!(!ciphertext.contains(marker));
        let decrypted = vault
            .decrypt_note(Some("n-secret".into()), ciphertext.into())
            .unwrap();
        assert!(
            decrypted == marker,
            "imported note did not decrypt to its original text"
        );
        assert_eq!(
            secret["content_tag"],
            json!(vault.content_tag(marker.into(), Some("b1".into())))
        );
        assert_ne!(secret["content_tag"], json!("foreign"));
        // Both text-less shapes — omitted key and explicit null — land as a null body with no invented tag.
        for id in ["n-legacy", "n-null-text"] {
            let legacy = store.get_row("notes", id).unwrap().unwrap();
            assert!(legacy["text"].is_null(), "{id} kept a null body");
            assert!(
                legacy["content_tag"].is_null(),
                "{id} was not given an invented tag"
            );
        }

        for (_, table, _, payload, _) in store.outbox_items().unwrap() {
            assert_eq!(table, "notes");
            assert!(!payload.contains(marker));
            let payload: Value = serde_json::from_str(&payload).unwrap();
            let local = store
                .get_row("notes", payload["id"].as_str().unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(payload, Value::Object(local));
        }
        drop(store);
        let database_bytes = std::fs::read(&db).unwrap();
        assert!(
            !database_bytes
                .windows(marker.len())
                .any(|window| window == marker.as_bytes()),
            "plaintext marker reached the SQLite file"
        );
    }

    #[test]
    fn all_eight_stores_stage_complete_rows_in_dependency_order_and_reimport_skips() {
        let archive = json!({
            "_syntopicon":true,"schemaVersion":19,
            "books":[{"id":"b1","title":"Book","updatedAt":50}],
            "notes":[{"id":"n1","bookId":"b1","text":"secret","updatedAt":50}],
            "customIdeas":[{"id":"i1","name":"Idea","updatedAt":50}],
            "noteLinks":[{"id":"l1","fromNoteId":"n1","toNoteId":"n1","updatedAt":50}],
            "lenses":[{"id":"lens1","name":"Lens","updatedAt":50}],
            "collections":[{"id":"c1","name":"Collection","updatedAt":50}],
            "collectionMemberships":[{"id":"m1","noteId":"n1","collectionId":"c1","updatedAt":50}],
            "noteSignals":[{"noteId":"n1","updatedAt":50}]
        })
        .to_string();
        let vault = Vault::generate();
        let store = Store::open_in_memory().unwrap();
        let sink = RecordingSink::default();
        let summary = run(merge_parsed_with_sink(
            &store,
            &sink,
            &vault,
            parse_import_at(&archive, 10).unwrap(),
            10,
        ))
        .unwrap();

        let ones = ImportCounts {
            books: 1,
            notes: 1,
            custom_ideas: 1,
            note_links: 1,
            lenses: 1,
            collections: 1,
            collection_memberships: 1,
            note_signals: 1,
        };
        assert_eq!(summary.schema_version, 19);
        assert_eq!(summary.imported, ones);
        assert_eq!(summary.skipped_stale, ImportCounts::default());
        let queued = store.outbox_items().unwrap();
        assert_eq!(
            queued
                .iter()
                .map(|item| item.1.as_str())
                .collect::<Vec<_>>(),
            synced_table_names()
        );
        for schema in synced_schema() {
            let id = if schema.name == "note_signals" {
                "n1"
            } else {
                match schema.name {
                    "books" => "b1",
                    "notes" => "n1",
                    "custom_ideas" => "i1",
                    "note_links" => "l1",
                    "lenses" => "lens1",
                    "collections" => "c1",
                    "collection_memberships" => "m1",
                    _ => unreachable!(),
                }
            };
            let row = store.get_row(schema.name, id).unwrap().unwrap();
            assert_eq!(
                row.keys()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>(),
                schema.columns.iter().map(|(name, _)| *name).collect(),
                "complete descriptor row for {}",
                schema.name
            );
        }
        assert!(!sink
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Upsert(_))));

        let before = store.outbox_items().unwrap().len();
        let second = run(merge_parsed_with_sink(
            &store,
            &RecordingSink::default(),
            &vault,
            parse_import_at(&archive, 20).unwrap(),
            20,
        ))
        .unwrap();
        assert_eq!(second.imported, ImportCounts::default());
        assert_eq!(second.skipped_stale, ones);
        assert_eq!(store.outbox_items().unwrap().len(), before);
    }

    #[test]
    fn accepted_resurrection_drops_only_matching_tombstone_and_stale_import_changes_nothing() {
        let store = Store::open_in_memory().unwrap();
        stage_local(&store, "accepted", 40, true);
        stage_local(&store, "stale", 60, true);
        store
            .enqueue(
                "books",
                "accepted",
                r#"{"id":"accepted","updated_at":40,"deleted":true}"#,
                40,
            )
            .unwrap();
        store
            .enqueue(
                "books",
                "stale",
                r#"{"id":"stale","updated_at":60,"deleted":true}"#,
                60,
            )
            .unwrap();
        let archive = parsed(
            "books",
            vec![
                json!({"id":"accepted","title":"live","updatedAt":50}),
                json!({"id":"stale","title":"must not win","updatedAt":50}),
            ],
            10,
        );

        let summary = run(merge_parsed_with_sink(
            &store,
            &RecordingSink::default(),
            &Vault::generate(),
            archive,
            10,
        ))
        .unwrap();

        assert_eq!(summary.imported.books, 1);
        assert_eq!(summary.skipped_stale.books, 1);
        let queued = store.outbox_items().unwrap();
        assert_eq!(queued.len(), 2);
        assert!(queued.iter().any(|item| {
            item.2.as_deref() == Some("stale")
                && serde_json::from_str::<Value>(&item.3).unwrap()["deleted"] == json!(true)
        }));
        assert!(queued.iter().any(|item| {
            item.2.as_deref() == Some("accepted")
                && serde_json::from_str::<Value>(&item.3).unwrap()["deleted"] == json!(false)
        }));
        assert_eq!(
            store.get_row("books", "stale").unwrap().unwrap()["deleted"],
            json!(true)
        );
    }
}
