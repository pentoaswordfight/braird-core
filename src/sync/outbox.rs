//! The outbox: enqueue + collapse (SUR-724 / SUR-659b). Mirrors surfc's
//! `collapseOutboxItems` (`src/supabase.js`) faithfully — multiple queued mutations to the
//! same record collapse into one upsert (last-write-wins per field; `deleted` is sticky),
//! and a queued note's `book_id` is repointed transitively through the offline-merge remap.
//!
//! The payload stored in the `outbox` table is a JSON object of the row's *column* values
//! (snake_case, matching the PostgREST wire shape) — NOT the PWA's camelCase Dexie shape.
//! The one JS quirk that does NOT carry over: JS keys the group on `payload.id` and repoints
//! `payload.bookId`; here the columns are already `id` / `book_id`.
//!
//! Seal-at-write (founder, Gate): the `text` value enqueued for a note is ALREADY the
//! enc:v2 ciphertext and `content_tag` is ALREADY computed from the plaintext (both at
//! [`super::SyncEngine::enqueue_note`] time). Collapse never sees plaintext.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// One row read back from the `outbox` table.
pub struct OutboxItem {
    /// The autoincrement `outbox.id` — the unit the flush clears on success.
    pub id: i64,
    pub table_name: String,
    /// The row PK value (`record_id`), or `None` to fall back to `payload["id"]`.
    pub record_id: Option<String>,
    /// The row's column values as a JSON object.
    pub payload: Value,
    pub created_at: i64,
}

/// A collapsed group ready to upsert: the table, the outbox ids it absorbed (cleared as a
/// unit on success), and the merged row payload.
#[derive(Clone)]
pub struct Collapsed {
    pub table: String,
    pub ids: Vec<i64>,
    pub payload: Map<String, Value>,
}

/// Collapse queued mutations into one upsert per record. Mirrors JS `collapseOutboxItems`:
///   1. sort by `created_at` (stable, so ties keep insertion order → real LWW);
///   2. group by `table:record_id` (record_id falls back to `payload["id"]`);
///   3. shallow-merge each item's payload over the accumulator (last field wins);
///   4. `deleted` truthy is sticky — a delete absorbs all prior edits AND is not un-set by a
///      later edit carrying `deleted: false` (accumulated across the group, not per-item);
///   5. for notes, repoint `book_id` transitively through `book_id_remap`.
pub fn collapse(
    mut items: Vec<OutboxItem>,
    book_id_remap: &BTreeMap<String, String>,
) -> Vec<Collapsed> {
    // Stable sort by created_at: two edits with the same ms keep enqueue order, so the later
    // enqueue still wins the field — matching JS `[...items].sort((a,b)=>a.createdAt-b.createdAt)`.
    items.sort_by_key(|i| i.created_at);

    // BTreeMap → deterministic group order (JS relies on insertion order of `groups`; a
    // stable order is all the flush needs and it makes the tests order-independent).
    let mut groups: BTreeMap<String, Collapsed> = BTreeMap::new();
    for item in items {
        let record_id = item
            .record_id
            .clone()
            .or_else(|| {
                item.payload
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .unwrap_or_default();
        let key = format!("{}:{}", item.table_name, record_id);

        let group = groups.entry(key).or_insert_with(|| Collapsed {
            table: item.table_name.clone(),
            ids: Vec::new(),
            payload: Map::new(),
        });
        group.ids.push(item.id);

        if let Value::Object(fields) = item.payload {
            // Sticky delete is accumulated across the WHOLE group, not just this item. The
            // enqueue paths stamp EVERY payload with `deleted` (false on a normal edit), so a
            // delete followed by any later edit would otherwise have its `deleted:true`
            // overwritten by the field-merge below and resurrect a soft-deleted row. Read the
            // accumulator's prior `deleted` BEFORE the merge clobbers it, OR in this item's.
            // (Deliberate hardening over JS `collapseOutboxItems`, whose per-item
            // `if (item.payload?.deleted)` has the same latent hole — masked there only because a
            // soft-deleted row leaves the surfc UI and can't be re-edited; the core has no such
            // guarantee. Founder decision: within a batch, delete wins — never resurrect.)
            let sticky_deleted =
                truthy(group.payload.get("deleted")) || truthy(fields.get("deleted"));
            for (k, v) in fields {
                group.payload.insert(k, v);
            }
            if sticky_deleted {
                group.payload.insert("deleted".into(), Value::Bool(true));
            }
        }
    }

    // Repoint each note's book_id onto the final merge survivor (chained offline merges).
    for group in groups.values_mut() {
        if group.table == "notes" {
            if let Some(book_id) = group.payload.get("book_id").and_then(|v| v.as_str()) {
                let resolved = resolve_book_id(book_id, book_id_remap);
                group
                    .payload
                    .insert("book_id".into(), Value::String(resolved));
            }
        }
    }

    groups.into_values().collect()
}

/// Walk the temp→survivor remap to its end (chained merges A→B→C resolve straight to C),
/// cycle-safe and hop-capped. Mirrors JS `resolveMergedId` (maxHops 20). Also the
/// `resolve_book_id` the founder brief names: the map lives in `meta` (persisted by the
/// flush), passed in here as a plain map.
pub fn resolve_book_id(book_id: &str, remap: &BTreeMap<String, String>) -> String {
    let mut id = book_id;
    for _ in 0..20 {
        match remap.get(id) {
            Some(next) if next != id => id = next,
            _ => break,
        }
    }
    id.to_string()
}

/// JS `if (item.payload?.deleted)` truthiness: `true`, a non-zero number, or a non-empty
/// string other than `"false"`/`"0"`. The columns we enqueue use `bool`/`1`, but be liberal.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty() && s != "false" && s != "0",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(id: i64, table: &str, record: &str, created_at: i64, payload: Value) -> OutboxItem {
        OutboxItem {
            id,
            table_name: table.into(),
            record_id: Some(record.into()),
            payload,
            created_at,
        }
    }

    #[test]
    fn lww_per_field_merges_in_created_at_order() {
        // Two edits to the same note; the later one wins `text`, the earlier survives `page`.
        let items = vec![
            item(
                1,
                "notes",
                "n1",
                100,
                json!({ "id": "n1", "text": "enc:v2:a", "page": "5" }),
            ),
            item(
                2,
                "notes",
                "n1",
                200,
                json!({ "id": "n1", "text": "enc:v2:b" }),
            ),
        ];
        let out = collapse(items, &BTreeMap::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ids, vec![1, 2]);
        assert_eq!(out[0].payload["text"], json!("enc:v2:b"));
        assert_eq!(out[0].payload["page"], json!("5"));
    }

    #[test]
    fn out_of_order_arrival_still_resolves_by_created_at() {
        // Enqueued out of order (higher id is older): created_at, not id, decides LWW.
        let items = vec![
            item(2, "notes", "n1", 100, json!({ "id": "n1", "text": "old" })),
            item(1, "notes", "n1", 200, json!({ "id": "n1", "text": "new" })),
        ];
        let out = collapse(items, &BTreeMap::new());
        assert_eq!(out[0].payload["text"], json!("new"));
    }

    #[test]
    fn delete_is_sticky_and_absorbs_later_edits() {
        // A delete followed by a stray edit still flushes as deleted.
        let items = vec![
            item(
                1,
                "notes",
                "n1",
                100,
                json!({ "id": "n1", "deleted": true }),
            ),
            item(
                2,
                "notes",
                "n1",
                200,
                json!({ "id": "n1", "text": "resurrected?" }),
            ),
        ];
        let out = collapse(items, &BTreeMap::new());
        assert_eq!(out[0].payload["deleted"], json!(true));
    }

    #[test]
    fn delete_stays_sticky_when_later_edit_carries_deleted_false() {
        // The real enqueue shape: EVERY payload stamps `deleted` (false on a normal edit). A
        // delete followed by a normal edit must NOT resurrect — this is the regression the
        // per-item sticky check missed (it only re-asserted when THIS item was a delete).
        let items = vec![
            item(
                1,
                "notes",
                "n1",
                100,
                json!({ "id": "n1", "text": "enc:v2:a", "deleted": true }),
            ),
            item(
                2,
                "notes",
                "n1",
                200,
                json!({ "id": "n1", "text": "enc:v2:b", "page": "7", "deleted": false }),
            ),
        ];
        let out = collapse(items, &BTreeMap::new());
        assert_eq!(out.len(), 1);
        // Later fields still win (LWW), but the delete is not un-set.
        assert_eq!(out[0].payload["text"], json!("enc:v2:b"));
        assert_eq!(out[0].payload["page"], json!("7"));
        assert_eq!(out[0].payload["deleted"], json!(true));
    }

    #[test]
    fn interleaved_edits_around_a_delete_stay_deleted() {
        // edit(false) → delete(true) → edit(false): reading the accumulator before each merge
        // keeps it sticky through the trailing live edit.
        let items = vec![
            item(
                1,
                "notes",
                "n1",
                100,
                json!({ "id": "n1", "deleted": false }),
            ),
            item(
                2,
                "notes",
                "n1",
                200,
                json!({ "id": "n1", "deleted": true }),
            ),
            item(
                3,
                "notes",
                "n1",
                300,
                json!({ "id": "n1", "deleted": false }),
            ),
        ];
        let out = collapse(items, &BTreeMap::new());
        assert_eq!(out[0].payload["deleted"], json!(true));
    }

    #[test]
    fn note_book_id_repoints_transitively() {
        let mut remap = BTreeMap::new();
        remap.insert("tempA".to_string(), "tempB".to_string());
        remap.insert("tempB".to_string(), "server-1".to_string());
        let items = vec![item(
            1,
            "notes",
            "n1",
            100,
            json!({ "id": "n1", "book_id": "tempA", "text": "enc:v2:x" }),
        )];
        let out = collapse(items, &remap);
        assert_eq!(out[0].payload["book_id"], json!("server-1"));
    }

    #[test]
    fn remap_cycle_terminates() {
        let mut remap = BTreeMap::new();
        remap.insert("a".to_string(), "b".to_string());
        remap.insert("b".to_string(), "a".to_string());
        // Hop cap must return SOME id, not hang.
        let resolved = resolve_book_id("a", &remap);
        assert!(resolved == "a" || resolved == "b");
    }

    #[test]
    fn book_payload_keeps_its_own_id_even_if_merged_away() {
        // A merged-loser book still upserts under its own id (as a soft-deleted row); the
        // remap only rewrites NOTE book_id references, never a book's own id.
        let mut remap = BTreeMap::new();
        remap.insert("tempA".to_string(), "server-1".to_string());
        let items = vec![item(
            1,
            "books",
            "tempA",
            100,
            json!({ "id": "tempA", "deleted": true }),
        )];
        let out = collapse(items, &remap);
        assert_eq!(out[0].payload["id"], json!("tempA"));
    }

    #[test]
    fn distinct_records_do_not_collapse() {
        let items = vec![
            item(1, "notes", "n1", 100, json!({ "id": "n1", "text": "a" })),
            item(2, "notes", "n2", 100, json!({ "id": "n2", "text": "b" })),
            item(3, "books", "b1", 100, json!({ "id": "b1", "title": "T" })),
        ];
        let out = collapse(items, &BTreeMap::new());
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn truthy_variants() {
        // Truthy: real bool, non-zero number, non-empty string other than "false"/"0".
        assert!(truthy(Some(&json!(true))));
        assert!(truthy(Some(&json!(1))));
        assert!(truthy(Some(&json!("1"))));
        // Falsy: false, zero, "0"/"false", null, absent, and non-scalar values.
        assert!(!truthy(Some(&json!(false))));
        assert!(!truthy(Some(&json!(0))));
        assert!(!truthy(Some(&json!("0"))));
        assert!(!truthy(Some(&json!("false"))));
        assert!(!truthy(Some(&Value::Null)));
        assert!(!truthy(None));
        assert!(!truthy(Some(&json!([1, 2]))));
    }
}
