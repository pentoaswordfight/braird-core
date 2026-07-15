use std::collections::HashMap;

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::store::Store;
use crate::sync::SyncError;
use crate::vault::Vault;

const SCHEMA_VERSION: u32 = 19;
const HANDWRITTEN_ANNOTATION: &str = "handwritten_annotation";

const BOOK_FIELDS: &[(&str, &str)] = &[
    ("id", "id"),
    ("title", "title"),
    ("author", "author"),
    ("isbn", "isbn"),
    ("cover_url", "coverUrl"),
    ("cover_source", "coverSource"),
    ("cover_resolved_at", "coverResolvedAt"),
    ("created_at", "createdAt"),
    ("updated_at", "updatedAt"),
    ("deleted", "deleted"),
];
const NOTE_FIELDS: &[(&str, &str)] = &[
    ("id", "id"),
    ("book_id", "bookId"),
    ("text", "text"),
    ("page", "page"),
    ("tags", "tags"),
    ("image_path", "imagePath"),
    ("ink_crop_path", "inkCropPath"),
    ("source", "source"),
    ("source_id", "sourceId"),
    ("source_meta", "sourceMeta"),
    ("chapter", "chapter"),
    ("content_tag", "contentTag"),
    ("created_at", "createdAt"),
    ("updated_at", "updatedAt"),
    ("deleted", "deleted"),
];
const CUSTOM_IDEA_FIELDS: &[(&str, &str)] = &[
    ("id", "id"),
    ("name", "name"),
    ("description", "description"),
    ("created_at", "createdAt"),
    ("updated_at", "updatedAt"),
    ("deleted", "deleted"),
];
const NOTE_LINK_FIELDS: &[(&str, &str)] = &[
    ("id", "id"),
    ("from_note_id", "fromNoteId"),
    ("to_note_id", "toNoteId"),
    ("relation_type", "relationType"),
    ("created_at", "createdAt"),
    ("updated_at", "updatedAt"),
    ("deleted", "deleted"),
];
const LENS_FIELDS: &[(&str, &str)] = &[
    ("id", "id"),
    ("name", "name"),
    ("leaf_ids", "leafIds"),
    ("combinator", "combinator"),
    ("threshold", "threshold"),
    ("created_at", "createdAt"),
    ("updated_at", "updatedAt"),
    ("deleted", "deleted"),
];
const COLLECTION_FIELDS: &[(&str, &str)] = &[
    ("id", "id"),
    ("name", "name"),
    ("created_at", "createdAt"),
    ("updated_at", "updatedAt"),
    ("deleted", "deleted"),
];
const MEMBERSHIP_FIELDS: &[(&str, &str)] = &[
    ("id", "id"),
    ("note_id", "noteId"),
    ("collection_id", "collectionId"),
    ("created_at", "createdAt"),
    ("updated_at", "updatedAt"),
    ("deleted", "deleted"),
];
const SIGNAL_FIELDS: &[(&str, &str)] = &[
    ("note_id", "noteId"),
    ("source_prior", "sourcePrior"),
    ("return_visits", "returnVisits"),
    ("has_annotation", "hasAnnotation"),
    ("stitch_spawns", "stitchSpawns"),
    ("exposure_recency_at", "exposureRecencyAt"),
    ("engagement_recency_at", "engagementRecencyAt"),
    ("importance", "importance"),
    ("created_at", "createdAt"),
    ("updated_at", "updatedAt"),
    ("deleted", "deleted"),
];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotExport {
    #[serde(rename = "_syntopicon")]
    syntopicon: bool,
    schema_version: u32,
    exported_at: String,
    books: Vec<Value>,
    notes: Vec<Value>,
    custom_ideas: Vec<Value>,
    note_links: Vec<Value>,
    lenses: Vec<Value>,
    collections: Vec<Value>,
    collection_memberships: Vec<Value>,
    note_signals: Vec<Value>,
}

pub(in crate::sync) fn build_snapshot_at(
    store: &Store,
    vault: &Vault,
    now_ms: i64,
) -> Result<String, SyncError> {
    let books = mapped_live_rows(store, "books", BOOK_FIELDS)?;
    let note_rows = live_rows(store, "notes")?;
    let mut notes = note_rows
        .iter()
        .map(|row| map_note(row, vault))
        .collect::<Result<Vec<_>, _>>()?;
    let custom_ideas = mapped_live_rows(store, "custom_ideas", CUSTOM_IDEA_FIELDS)?;
    let note_link_rows = live_rows(store, "note_links")?;
    attach_user_annotations(&mut notes, &note_link_rows);
    let note_links = note_link_rows
        .iter()
        .map(|row| map_fields(row, NOTE_LINK_FIELDS))
        .collect();
    let lenses = mapped_live_rows(store, "lenses", LENS_FIELDS)?;
    let collections = mapped_live_rows(store, "collections", COLLECTION_FIELDS)?;
    let collection_memberships =
        mapped_live_rows(store, "collection_memberships", MEMBERSHIP_FIELDS)?;
    let note_signals = mapped_live_rows(store, "note_signals", SIGNAL_FIELDS)?;

    serde_json::to_string(&SnapshotExport {
        syntopicon: true,
        schema_version: SCHEMA_VERSION,
        exported_at: format_utc_iso8601(now_ms),
        books,
        notes,
        custom_ideas,
        note_links,
        lenses,
        collections,
        collection_memberships,
        note_signals,
    })
    .map_err(|_| SyncError::Store("snapshot export serialization failed".into()))
}

fn live_rows(store: &Store, table: &str) -> Result<Vec<Map<String, Value>>, SyncError> {
    store
        .list_live(table, None, -1, 0)
        .map_err(|_| SyncError::Store("snapshot export read failed".into()))
}

fn mapped_live_rows(
    store: &Store,
    table: &str,
    fields: &[(&str, &str)],
) -> Result<Vec<Value>, SyncError> {
    Ok(live_rows(store, table)?
        .iter()
        .map(|row| map_fields(row, fields))
        .collect())
}

fn map_fields(row: &Map<String, Value>, fields: &[(&str, &str)]) -> Value {
    let mapped = fields
        .iter()
        .map(|(sqlite, pwa)| {
            let value = if *sqlite == "deleted" && *pwa == "deleted" {
                json!(0)
            } else {
                row.get(*sqlite).cloned().unwrap_or(Value::Null)
            };
            ((*pwa).to_string(), value)
        })
        .collect();
    Value::Object(mapped)
}

fn map_note(row: &Map<String, Value>, vault: &Vault) -> Result<Value, SyncError> {
    let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
    let ciphertext = row.get("text").and_then(Value::as_str).unwrap_or_default();
    let plaintext = vault
        .decrypt_note(Some(id.to_string()), ciphertext.to_string())
        .map_err(|_| SyncError::Store("snapshot export note decryption failed".into()))?;
    let mut mapped = map_fields(row, NOTE_FIELDS);
    mapped
        .as_object_mut()
        .expect("mapped note is an object")
        .insert("text".into(), Value::String(plaintext));
    Ok(mapped)
}

fn attach_user_annotations(notes: &mut [Value], note_links: &[Map<String, Value>]) {
    let child_text: HashMap<_, _> = notes
        .iter()
        .filter_map(|note| {
            Some((
                note.get("id")?.as_str()?.to_string(),
                note.get("text")?.as_str()?.to_string(),
            ))
        })
        .collect();
    let mut edges: Vec<_> = note_links
        .iter()
        .filter(|link| {
            link.get("relation_type").and_then(Value::as_str) == Some(HANDWRITTEN_ANNOTATION)
        })
        .collect();
    edges.sort_by(|left, right| {
        edge_created_at(left)
            .cmp(&edge_created_at(right))
            .then_with(|| edge_id(left).cmp(edge_id(right)))
    });
    let mut annotations: HashMap<String, Vec<String>> = HashMap::new();
    for edge in edges {
        let Some(parent_id) = edge.get("from_note_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(child_id) = edge.get("to_note_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(text) = child_text.get(child_id) else {
            continue;
        };
        annotations
            .entry(parent_id.to_string())
            .or_default()
            .push(text.clone());
    }
    for note in notes {
        let Some(id) = note.get("id").and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        let Some(user_annotation) = annotations.remove(&id) else {
            continue;
        };
        if !user_annotation.is_empty() {
            note.as_object_mut()
                .expect("mapped note is an object")
                .insert(
                    "user_metadata".into(),
                    json!({ "user_annotation": user_annotation }),
                );
        }
    }
}

fn edge_created_at(edge: &Map<String, Value>) -> i64 {
    edge.get("created_at")
        .and_then(Value::as_i64)
        .unwrap_or_default()
}

fn edge_id(edge: &Map<String, Value>) -> &str {
    edge.get("id").and_then(Value::as_str).unwrap_or_default()
}

fn format_utc_iso8601(epoch_ms: i64) -> String {
    let epoch_ms = epoch_ms.max(0);
    let epoch_seconds = epoch_ms / 1_000;
    let milliseconds = epoch_ms % 1_000;
    let days = epoch_seconds / 86_400;
    let seconds_in_day = epoch_seconds % 86_400;
    let hour = seconds_in_day / 3_600;
    let minute = (seconds_in_day % 3_600) / 60;
    let second = seconds_in_day % 60;
    let (year, month, day) = civil_from_unix_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milliseconds:03}Z")
}

fn civil_from_unix_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};

    use crate::store::Store;
    use crate::sync::SyncEngine;
    use crate::vault::Vault;

    fn put(store: &Store, table: &str, value: Value) {
        store
            .apply_row(table, value.as_object().expect("test row is an object"))
            .unwrap();
    }

    fn note_row(vault: &Vault, id: &str, plaintext: &str, created_at: i64, deleted: bool) -> Value {
        json!({
            "id": id,
            "book_id": null,
            "text": vault.encrypt_note(Some(id.into()), plaintext.into()),
            "page": "",
            "tags": [],
            "image_path": null,
            "ink_crop_path": null,
            "source": "manual",
            "source_id": null,
            "source_meta": {},
            "chapter": null,
            "content_tag": null,
            "created_at": created_at,
            "updated_at": created_at + 1,
            "deleted": deleted,
        })
    }

    fn snapshot_at(store: &Store, vault: &Vault, now_ms: i64) -> String {
        super::build_snapshot_at(store, vault, now_ms).unwrap()
    }

    #[test]
    fn envelope_has_pwa_key_order_schema_and_javascript_timestamp() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();

        let snapshot = snapshot_at(&store, &vault, 1_704_164_645_123);

        assert_eq!(
            snapshot,
            concat!(
                "{\"_syntopicon\":true,\"schemaVersion\":19,",
                "\"exportedAt\":\"2024-01-02T03:04:05.123Z\",",
                "\"books\":[],\"notes\":[],\"customIdeas\":[],\"noteLinks\":[],",
                "\"lenses\":[],\"collections\":[],\"collectionMemberships\":[],",
                "\"noteSignals\":[]}"
            )
        );
    }

    #[test]
    fn maps_all_eight_synced_stores_to_the_pwa_shape() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        put(
            &store,
            "books",
            json!({
                "id": "b1", "title": "The Republic", "author": "Plato", "isbn": "9781",
                "cover_url": "https://covers/b1", "cover_source": "openlibrary",
                "cover_resolved_at": 101, "created_at": 100, "updated_at": 102,
                "deleted": false
            }),
        );
        put(
            &store,
            "notes",
            json!({
                "id": "n1", "book_id": "b1",
                "text": vault.encrypt_note(Some("n1".into()), "Justice is harmony".into()),
                "page": "42", "tags": ["Justice", "Virtue"],
                "image_path": "user/n1.jpg", "ink_crop_path": "user/n1-crop.jpg",
                "source": "image", "source_id": "scan:1",
                "source_meta": {"case": 2}, "chapter": "IV", "content_tag": "tag-opaque",
                "created_at": 200, "updated_at": 201, "deleted": false
            }),
        );
        put(
            &store,
            "custom_ideas",
            json!({
                "id": "ci1", "name": "Attention", "description": "Reader-defined",
                "created_at": 300, "updated_at": 301, "deleted": false
            }),
        );
        put(
            &store,
            "note_links",
            json!({
                "id": "link1", "from_note_id": "n1", "to_note_id": "n2",
                "relation_type": "related", "created_at": 400, "updated_at": 401,
                "deleted": false
            }),
        );
        put(
            &store,
            "lenses",
            json!({
                "id": "lens1", "name": "Justice and Virtue",
                "leaf_ids": ["Justice", "Virtue"], "combinator": "COOCCUR",
                "threshold": 60, "created_at": 500, "updated_at": 501, "deleted": false
            }),
        );
        put(
            &store,
            "collections",
            json!({
                "id": "col1", "name": "Study", "created_at": 600,
                "updated_at": 601, "deleted": false
            }),
        );
        put(
            &store,
            "collection_memberships",
            json!({
                "id": "col1:n1", "note_id": "n1", "collection_id": "col1",
                "created_at": 700, "updated_at": 701, "deleted": false
            }),
        );
        put(
            &store,
            "note_signals",
            json!({
                "note_id": "n1", "source_prior": 0.75, "return_visits": 3,
                "has_annotation": true, "stitch_spawns": 2,
                "exposure_recency_at": 800, "engagement_recency_at": 801,
                "importance": 0.9, "created_at": 802, "updated_at": 803,
                "deleted": false
            }),
        );

        let root: Value = serde_json::from_str(&snapshot_at(&store, &vault, 0)).unwrap();

        for store_name in [
            "books",
            "notes",
            "customIdeas",
            "noteLinks",
            "lenses",
            "collections",
            "collectionMemberships",
            "noteSignals",
        ] {
            assert_eq!(
                root[store_name][0]["deleted"],
                json!(0),
                "{store_name} must match the PWA's numeric live-row flag"
            );
        }

        assert_eq!(
            root["books"][0],
            json!({
                "id": "b1", "title": "The Republic", "author": "Plato", "isbn": "9781",
                "coverUrl": "https://covers/b1", "coverSource": "openlibrary",
                "coverResolvedAt": 101, "createdAt": 100, "updatedAt": 102,
                "deleted": 0
            })
        );
        assert_eq!(
            root["notes"][0],
            json!({
                "id": "n1", "bookId": "b1", "text": "Justice is harmony", "page": "42",
                "tags": ["Justice", "Virtue"], "imagePath": "user/n1.jpg",
                "inkCropPath": "user/n1-crop.jpg", "source": "image",
                "sourceId": "scan:1", "sourceMeta": {"case": 2}, "chapter": "IV",
                "contentTag": "tag-opaque", "createdAt": 200, "updatedAt": 201,
                "deleted": 0
            })
        );
        assert_eq!(
            root["customIdeas"][0],
            json!({
                "id": "ci1", "name": "Attention", "description": "Reader-defined",
                "createdAt": 300, "updatedAt": 301, "deleted": 0
            })
        );
        assert_eq!(
            root["noteLinks"][0],
            json!({
                "id": "link1", "fromNoteId": "n1", "toNoteId": "n2",
                "relationType": "related", "createdAt": 400, "updatedAt": 401,
                "deleted": 0
            })
        );
        assert_eq!(
            root["lenses"][0],
            json!({
                "id": "lens1", "name": "Justice and Virtue",
                "leafIds": ["Justice", "Virtue"], "combinator": "COOCCUR",
                "threshold": 60, "createdAt": 500, "updatedAt": 501, "deleted": 0
            })
        );
        assert_eq!(
            root["collections"][0],
            json!({
                "id": "col1", "name": "Study", "createdAt": 600,
                "updatedAt": 601, "deleted": 0
            })
        );
        assert_eq!(
            root["collectionMemberships"][0],
            json!({
                "id": "col1:n1", "noteId": "n1", "collectionId": "col1",
                "createdAt": 700, "updatedAt": 701, "deleted": 0
            })
        );
        assert_eq!(
            root["noteSignals"][0],
            json!({
                "noteId": "n1", "sourcePrior": 0.75, "returnVisits": 3,
                "hasAnnotation": true, "stitchSpawns": 2,
                "exposureRecencyAt": 800, "engagementRecencyAt": 801,
                "importance": 0.9, "createdAt": 802, "updatedAt": 803,
                "deleted": 0
            })
        );
    }

    #[test]
    fn exports_live_rows_only_and_never_local_tables_or_outbox() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        put(
            &store,
            "books",
            json!({
                "id": "live", "title": "Live", "created_at": 1, "updated_at": 1,
                "deleted": false
            }),
        );
        put(
            &store,
            "books",
            json!({
                "id": "gone", "title": "Gone", "created_at": 2, "updated_at": 2,
                "deleted": true
            }),
        );
        store
            .meta_set("private-local-state", "must-not-export")
            .unwrap();
        store
            .enqueue("books", "queued", "{\"title\":\"queued-local\"}", 3)
            .unwrap();

        let root: Value = serde_json::from_str(&snapshot_at(&store, &vault, 0)).unwrap();

        assert_eq!(root["books"].as_array().unwrap().len(), 1);
        assert_eq!(root["books"][0]["id"], "live");
        let envelope = root.as_object().unwrap();
        assert_eq!(envelope.len(), 11);
        assert!(!envelope.contains_key("meta"));
        assert!(!envelope.contains_key("outbox"));
        assert!(!envelope.contains_key("embeddings"));
        assert!(!envelope.contains_key("discoveryJobs"));
        assert!(!snapshot_at(&store, &vault, 0).contains("must-not-export"));
        assert!(!snapshot_at(&store, &vault, 0).contains("queued-local"));
    }

    #[test]
    fn reconstructs_annotations_in_edge_order_and_omits_empty_metadata() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        for row in [
            note_row(&vault, "parent", "printed passage", 1, false),
            note_row(&vault, "early", "first margin", 2, false),
            note_row(&vault, "late", "second margin", 3, false),
            note_row(&vault, "plain", "no margins", 4, false),
            note_row(&vault, "deleted-child", "must not appear", 5, true),
        ] {
            put(&store, "notes", row);
        }
        for edge in [
            json!({"id":"late-edge","from_note_id":"parent","to_note_id":"late","relation_type":"handwritten_annotation","created_at":20,"updated_at":20,"deleted":false}),
            json!({"id":"early-edge","from_note_id":"parent","to_note_id":"early","relation_type":"handwritten_annotation","created_at":10,"updated_at":10,"deleted":false}),
            json!({"id":"deleted-edge","from_note_id":"parent","to_note_id":"early","relation_type":"handwritten_annotation","created_at":5,"updated_at":5,"deleted":true}),
            json!({"id":"wrong-kind","from_note_id":"parent","to_note_id":"early","relation_type":"related","created_at":1,"updated_at":1,"deleted":false}),
            json!({"id":"gone-child","from_note_id":"parent","to_note_id":"deleted-child","relation_type":"handwritten_annotation","created_at":2,"updated_at":2,"deleted":false}),
        ] {
            put(&store, "note_links", edge);
        }

        let root: Value = serde_json::from_str(&snapshot_at(&store, &vault, 0)).unwrap();
        let notes = root["notes"].as_array().unwrap();
        let parent = notes.iter().find(|n| n["id"] == "parent").unwrap();
        let plain = notes.iter().find(|n| n["id"] == "plain").unwrap();

        assert_eq!(
            parent["user_metadata"]["user_annotation"],
            json!(["first margin", "second margin"])
        );
        assert!(plain.get("user_metadata").is_none());
    }

    #[test]
    fn omits_device_local_previews_but_retains_synced_paths_and_content_tag() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        let mut row = note_row(&vault, "n1", "passage", 1, false);
        let row = row.as_object_mut().unwrap();
        row.insert("image_path".into(), json!("user/source.jpg"));
        row.insert("ink_crop_path".into(), json!("user/crop.jpg"));
        row.insert("content_tag".into(), json!("opaque-tag"));
        row.insert(
            "imageDataUrl".into(),
            json!("data:image/jpeg;base64,SOURCE"),
        );
        row.insert(
            "inkCropDataUrl".into(),
            json!("data:image/jpeg;base64,CROP"),
        );
        store.apply_row("notes", row).unwrap();

        let root: Value = serde_json::from_str(&snapshot_at(&store, &vault, 0)).unwrap();
        let note = root["notes"][0].as_object().unwrap();

        assert_eq!(note["imagePath"], "user/source.jpg");
        assert_eq!(note["inkCropPath"], "user/crop.jpg");
        assert_eq!(note["contentTag"], "opaque-tag");
        assert!(!note.contains_key("imageDataUrl"));
        assert!(!note.contains_key("inkCropDataUrl"));
    }

    #[test]
    fn decryption_failure_is_fail_closed_and_error_contains_no_note_material() {
        let store = Store::open_in_memory().unwrap();
        let mine = Vault::generate();
        let foreign = Vault::generate();
        put(
            &store,
            "notes",
            note_row(&mine, "good", "good plaintext", 1, false),
        );
        let ciphertext = foreign.encrypt_note(Some("bad".into()), "foreign secret".into());
        let mut bad = note_row(&foreign, "bad", "unused", 2, false);
        bad.as_object_mut()
            .unwrap()
            .insert("text".into(), json!(ciphertext));
        put(&store, "notes", bad);

        let error = super::build_snapshot_at(&store, &mine, 0).unwrap_err();
        let message = error.to_string();

        assert!(!message.contains("enc:v2"));
        assert!(!message.contains("good plaintext"));
        assert!(!message.contains("foreign secret"));
    }

    #[test]
    fn public_engine_export_uses_the_private_clocked_builder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.sqlite");
        let engine: Arc<SyncEngine> = SyncEngine::open(
            path.to_string_lossy().into_owned(),
            "https://x.supabase.co".into(),
            "anon".into(),
            Vault::generate(),
        )
        .unwrap();

        let snapshot = engine.export_snapshot().unwrap();
        let root: Value = serde_json::from_str(&snapshot).unwrap();
        let timestamp = root["exportedAt"].as_str().unwrap();

        assert_eq!(timestamp.len(), 24);
        assert_eq!(&timestamp[4..5], "-");
        assert_eq!(&timestamp[10..11], "T");
        assert_eq!(&timestamp[19..20], ".");
        assert!(timestamp.ends_with('Z'));
    }
}
