use std::collections::HashMap;

use serde::Serialize;
use serde_json::{json, Map, Value};
use time::{macros::format_description, OffsetDateTime};

use crate::store::Store;
use crate::sync::read::decrypt_note_text;
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
        exported_at: format_utc_iso8601(now_ms)?,
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

/// Map one live note row to its PWA shape, resolving `text` through the SAME rule the read path uses
/// ([`decrypt_note_text`]) rather than a second, weaker copy of it (SUR-934).
///
/// Only ONE of that rule's four cases is a decryption. A `text` that is NULL, empty, or simply not
/// sealed (no `enc:` sentinel — a supported legacy shape) has nothing to decrypt and must map straight
/// through. The previous code decrypted unconditionally, coercing NULL to `""` via `unwrap_or_default`,
/// so those three shapes each raised a *manufactured* decryption error that aborted the entire archive —
/// a corpus could read perfectly on every screen and still be impossible to export.
///
/// Fail-closed is preserved exactly where it belongs: a genuinely undecryptable note (sealed, wrong key
/// or corrupt) still fails the WHOLE export. Never a partial archive, never ciphertext in place of
/// plaintext, and never a silently dropped row — see `docs/snapshots.md`.
fn map_note(row: &Map<String, Value>, vault: &Vault) -> Result<Value, SyncError> {
    let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
    let (text, decrypt_failed) = decrypt_note_text(row, id, vault);
    if decrypt_failed {
        return Err(SyncError::Store(
            "snapshot export note decryption failed".into(),
        ));
    }
    let mut mapped = map_fields(row, NOTE_FIELDS);
    mapped
        .as_object_mut()
        .expect("mapped note is an object")
        // A note with no text exports `text: null` — the shape import already contracts for ("a
        // supported legacy note with omitted text remains null"), so this round-trips.
        .insert("text".into(), text.map_or(Value::Null, Value::String));
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

fn format_utc_iso8601(epoch_ms: i64) -> Result<String, SyncError> {
    const UTC_MILLISECONDS: &[time::format_description::FormatItem<'static>] =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    let timestamp = OffsetDateTime::from_unix_timestamp_nanos(i128::from(epoch_ms) * 1_000_000)
        .map_err(|_| SyncError::Store("snapshot export timestamp formatting failed".into()))?;
    // JavaScript uses a signed six-digit extended year before 0000, while `time`'s
    // default `[year]` component uses four digits. Production `epoch_ms()` clamps a
    // pre-epoch clock to zero; fail closed for an incompatible injected test clock.
    if timestamp.year() < 0 {
        return Err(SyncError::Store(
            "snapshot export timestamp formatting failed".into(),
        ));
    }
    timestamp
        .format(UTC_MILLISECONDS)
        .map_err(|_| SyncError::Store("snapshot export timestamp formatting failed".into()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};

    use crate::store::Store;
    use crate::sync::SyncEngine;
    use crate::vault::Vault;

    #[cfg(feature = "test-seams")]
    const CORE_EXPORT_FIXTURE: &str =
        include_str!("../../../vendored/snapshot-parity/schema-19-core-export.json");
    #[cfg(feature = "test-seams")]
    const SNAPSHOT_MANIFEST: &str = include_str!("../../../vendored/snapshot-parity/manifest.json");

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

    /// A note row whose `text` is whatever the caller says — NOT sealed. [`note_row`] always encrypts,
    /// which is exactly why the cases below were never covered.
    fn raw_note_row(id: &str, text: Value, created_at: i64) -> Value {
        json!({
            "id": id,
            "book_id": null,
            "text": text,
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
            "updated_at": created_at,
            "deleted": false,
        })
    }

    /// REGRESSION (SUR-934, found on-device by SUR-882): `map_note` used to decrypt `text`
    /// unconditionally, so the three shapes `read.rs::decrypt_note_text` explicitly treats as
    /// NOT-a-failure each aborted the WHOLE export:
    ///
    /// - `text` NULL — `unwrap_or_default()` coerced it to `""`, then `decrypt("")` → Err
    /// - `text` empty — `decrypt("")` → Err
    /// - `text` unsealed — no `enc:` sentinel, so `decrypt(plaintext)` → Err
    ///
    /// The read path passes all three through happily (`decrypt_failed = false`), so a corpus can read
    /// perfectly and still be impossible to export — observed on a real 1,638-note account.
    /// Each case is asserted independently so a failure names the shape.
    #[test]
    fn exports_notes_whose_text_is_null_empty_or_unsealed() {
        for (case, text) in [
            ("null", Value::Null),
            ("empty", json!("")),
            ("unsealed", json!("a legacy note stored as plaintext")),
        ] {
            let store = Store::open_in_memory().unwrap();
            let vault = Vault::generate();
            put(&store, "notes", raw_note_row("n1", text, 1));

            let snapshot = super::build_snapshot_at(&store, &vault, 0);

            assert!(
                snapshot.is_ok(),
                "a {case} note must not fail the export: {:?}",
                snapshot.err(),
            );
        }
    }

    /// The sealed note must still survive an export that also contains an unsealed one — a partial
    /// archive is never acceptable, so the fix must skip decryption for the unsealed row, not the export.
    #[test]
    fn an_unsealed_note_does_not_take_a_sealed_note_down_with_it() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        put(
            &store,
            "notes",
            note_row(&vault, "sealed", "the sealed body", 1, false),
        );
        put(
            &store,
            "notes",
            raw_note_row("unsealed", json!("a plaintext body"), 2),
        );

        let snapshot = super::build_snapshot_at(&store, &vault, 0).expect("export must survive");
        let root: Value = serde_json::from_str(&snapshot).unwrap();
        let notes = root["notes"].as_array().unwrap();

        assert_eq!(notes.len(), 2, "both notes are exported");
        let by_id = |id: &str| notes.iter().find(|n| n["id"] == id).unwrap().clone();
        assert_eq!(
            by_id("sealed")["text"],
            "the sealed body",
            "sealed text decrypts"
        );
        assert_eq!(
            by_id("unsealed")["text"],
            "a plaintext body",
            "unsealed text passes through verbatim"
        );
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
    fn utc_formatter_matches_javascript_at_epoch_and_calendar_edges() {
        for (epoch_ms, expected) in [
            (-2_208_988_800_000, "1900-01-01T00:00:00.000Z"),
            (-1, "1969-12-31T23:59:59.999Z"),
            (0, "1970-01-01T00:00:00.000Z"),
            (951_782_400_000, "2000-02-29T00:00:00.000Z"),
            (1_704_164_645_123, "2024-01-02T03:04:05.123Z"),
            (4_107_542_400_999, "2100-03-01T00:00:00.999Z"),
        ] {
            assert_eq!(super::format_utc_iso8601(epoch_ms).unwrap(), expected);
        }
    }

    #[test]
    fn utc_formatter_fails_closed_for_out_of_range_test_clocks() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();

        for epoch_ms in [i64::MIN, i64::MAX] {
            let error = super::build_snapshot_at(&store, &vault, epoch_ms).unwrap_err();

            assert_eq!(
                error.to_string(),
                "store error: snapshot export timestamp formatting failed"
            );
        }
    }

    #[test]
    fn utc_formatter_fails_closed_for_javascript_incompatible_bce_years() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();

        let error = super::build_snapshot_at(&store, &vault, -62_198_755_200_000).unwrap_err();

        assert_eq!(
            error.to_string(),
            "store error: snapshot export timestamp formatting failed"
        );
    }

    #[cfg(feature = "test-seams")]
    #[test]
    fn fixed_clock_export_matches_the_exact_all_store_fixture() {
        let manifest: Value = serde_json::from_str(SNAPSHOT_MANIFEST).unwrap();
        let vault =
            Vault::__with_raw_mk_hex(manifest["coreExportTestMasterKeyHex"].as_str().unwrap())
                .unwrap();
        let store = Store::open_in_memory().unwrap();

        put(
            &store,
            "books",
            json!({
                "id":"core-b-v19", "title":"Core Export Fixture", "author":"Braird",
                "isbn":"9780000000911", "cover_url":"https://example.invalid/core-cover.jpg",
                "cover_source":"manual", "cover_resolved_at":29001, "created_at":29000,
                "updated_at":29002, "deleted":false
            }),
        );
        for (
            id,
            plaintext,
            page,
            tags,
            image_path,
            ink_crop_path,
            source,
            source_id,
            source_meta,
            chapter,
            created_at,
            updated_at,
        ) in [
            (
                "core-n-v19-parent",
                "Core-supported parent passage",
                "91",
                json!(["Truth", "Justice"]),
                json!("core/source.jpg"),
                Value::Null,
                "manual",
                Value::Null,
                json!({"origin":"core"}),
                json!("IX"),
                29103,
                29104,
            ),
            (
                "core-n-v19-child",
                "Core-supported margin note",
                "91",
                json!(["Memory"]),
                Value::Null,
                json!("core/crop.jpg"),
                "handwritten",
                json!("core-margin:1"),
                json!({}),
                Value::Null,
                29100,
                29101,
            ),
        ] {
            put(
                &store,
                "notes",
                json!({
                    "id":id, "book_id":"core-b-v19",
                    "text":vault.encrypt_note(Some(id.into()), plaintext.into()), "page":page,
                    "tags":tags, "image_path":image_path, "ink_crop_path":ink_crop_path,
                    "source":source, "source_id":source_id, "source_meta":source_meta,
                    "chapter":chapter,
                    "content_tag":vault.content_tag(
                        plaintext.into(), Some("core-b-v19".into())
                    ),
                    "created_at":created_at, "updated_at":updated_at, "deleted":false
                }),
            );
        }
        put(
            &store,
            "custom_ideas",
            json!({
                "id":"core-ci-v19", "name":"Attention",
                "description":"Core-supported custom idea", "created_at":29200,
                "updated_at":29201, "deleted":false
            }),
        );
        put(
            &store,
            "note_links",
            json!({
                "id":"core-link-v19", "from_note_id":"core-n-v19-parent",
                "to_note_id":"core-n-v19-child", "relation_type":"handwritten_annotation",
                "created_at":29300, "updated_at":29301, "deleted":false
            }),
        );
        put(
            &store,
            "lenses",
            json!({
                "id":"core-lens-v19", "name":"Core Lens", "leaf_ids":["Truth","Justice"],
                "combinator":"AND", "threshold":75, "created_at":29400,
                "updated_at":29401, "deleted":false
            }),
        );
        put(
            &store,
            "collections",
            json!({
                "id":"core-col-v19", "name":"Core Collection", "created_at":29500,
                "updated_at":29501, "deleted":false
            }),
        );
        put(
            &store,
            "collection_memberships",
            json!({
                "id":"core-col-v19:core-n-v19-parent",
                "note_id":"core-n-v19-parent", "collection_id":"core-col-v19",
                "created_at":29600, "updated_at":29601, "deleted":false
            }),
        );
        put(
            &store,
            "note_signals",
            json!({
                "note_id":"core-n-v19-parent", "source_prior":0.7, "return_visits":2,
                "has_annotation":true, "stitch_spawns":1, "exposure_recency_at":29700,
                "engagement_recency_at":29701, "importance":123.456,
                "created_at":29702, "updated_at":29703, "deleted":false
            }),
        );

        let actual: Value =
            serde_json::from_str(&snapshot_at(&store, &vault, 1_784_106_611_012)).unwrap();
        let expected: Value = serde_json::from_str(CORE_EXPORT_FIXTURE).unwrap();

        assert_eq!(actual, expected);
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
    fn equal_time_annotations_follow_edge_id_not_insertion_order() {
        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        for row in [
            note_row(&vault, "parent", "printed passage", 1, false),
            note_row(&vault, "a-child", "A margin", 2, false),
            note_row(&vault, "m-child", "M margin", 3, false),
            note_row(&vault, "z-child", "Z margin", 4, false),
        ] {
            put(&store, "notes", row);
        }
        for edge in [
            json!({"id":"z-edge","from_note_id":"parent","to_note_id":"z-child","relation_type":"handwritten_annotation","created_at":10,"updated_at":10,"deleted":false}),
            json!({"id":"m-edge","from_note_id":"parent","to_note_id":"m-child","relation_type":"handwritten_annotation","created_at":10,"updated_at":10,"deleted":false}),
            json!({"id":"a-edge","from_note_id":"parent","to_note_id":"a-child","relation_type":"handwritten_annotation","created_at":10,"updated_at":10,"deleted":false}),
        ] {
            put(&store, "note_links", edge);
        }

        let root: Value = serde_json::from_str(&snapshot_at(&store, &vault, 0)).unwrap();
        let parent = root["notes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|note| note["id"] == "parent")
            .unwrap();

        assert_eq!(
            parent["user_metadata"]["user_annotation"],
            json!(["A margin", "M margin", "Z margin"])
        );
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
