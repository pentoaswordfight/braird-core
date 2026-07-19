use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::sync::SyncError;

const MAX_SCHEMA_VERSION: u32 = 19;
const HANDWRITTEN_ANNOTATION: &str = "handwritten_annotation";

const GREAT_IDEAS_RENAMES: &[(&str, &str)] = &[
    ("Good", "Good and Evil"),
    ("Custom", "Custom and Convention"),
    ("Pleasure", "Pleasure and Pain"),
    ("Virtue", "Virtue and Vice"),
    ("Sign", "Sign and Symbol"),
    ("War", "War and Peace"),
    ("Tyranny", "Tyranny and Despotism"),
    ("Life", "Life and Death"),
    ("Memory", "Memory and Imagination"),
    ("Necessity", "Necessity and Contingency"),
    ("Universal", "Universal and Particular"),
];

const CANON_REMAP_V14: &[(&str, &str)] = &[
    ("Cause", "Causation"),
    ("Chance", "Probability"),
    ("Liberty", "Freedom"),
    ("Honor", "Status"),
    ("Virtue and Vice", "Virtue"),
    ("Animal", "Life"),
    ("Aristocracy", "Power"),
    ("Monarchy", "Power"),
    ("Oligarchy", "Power"),
    ("Tyranny and Despotism", "Power"),
    ("Constitution", "Institutions"),
    ("Government", "Institutions"),
    ("State", "Institutions"),
    ("Citizen", "Institutions"),
    ("Custom and Convention", "Institutions"),
    ("Courage", "Virtue"),
    ("Dialectic", "Reasoning"),
    ("Induction", "Reasoning"),
    ("Logic", "Reasoning"),
    ("Duty", "Obligation"),
    ("Education", "Learning"),
    ("Experience", "Learning"),
    ("Family", "Community"),
    ("Form", "Beauty"),
    ("God", "the Sacred"),
    ("Religion", "the Sacred"),
    ("Theology", "the Sacred"),
    ("Prophecy", "the Sacred"),
    ("Immortality", "the Sacred"),
    ("Hypothesis", "Evidence"),
    ("Labor", "Productivity"),
    ("Mind", "Consciousness"),
    ("Soul", "Consciousness"),
    ("Sense", "Consciousness"),
    ("Poetry", "Art"),
    ("Property", "Markets"),
    ("Wealth", "Markets"),
    ("Prudence", "Strategy"),
    ("Punishment", "Justice"),
    ("Revolution", "Conflict"),
    ("Rhetoric", "Narrative"),
    ("Sign and Symbol", "Language"),
    ("Sin", "Morality"),
    ("Temperance", "Discipline"),
    ("Wisdom", "Judgment"),
    ("Opinion", "Judgment"),
    ("Will", "Motivation"),
    ("World", "Nature"),
    ("Man", "Identity"),
    ("Good and Evil", "Morality"),
    ("Happiness", "Purpose"),
    ("Knowledge", "Truth"),
    ("Law", "Institutions"),
    ("Life and Death", "Life"),
    ("Memory and Imagination", "Memory"),
    ("Pleasure and Pain", "Emotion"),
    ("Slavery", "Freedom"),
    ("War and Peace", "Conflict"),
];

#[cfg_attr(test, derive(Debug))]
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::sync) struct NormalizedRow {
    pub(in crate::sync) primary_key: String,
    pub(in crate::sync) row: Map<String, Value>,
    pub(in crate::sync) updated_at: i64,
}

#[cfg_attr(test, derive(Debug))]
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::sync) struct NormalizedImport {
    pub(in crate::sync) schema_version: u32,
    pub(in crate::sync) books: Vec<NormalizedRow>,
    pub(in crate::sync) notes: Vec<NormalizedRow>,
    pub(in crate::sync) custom_ideas: Vec<NormalizedRow>,
    pub(in crate::sync) note_links: Vec<NormalizedRow>,
    pub(in crate::sync) lenses: Vec<NormalizedRow>,
    pub(in crate::sync) collections: Vec<NormalizedRow>,
    pub(in crate::sync) collection_memberships: Vec<NormalizedRow>,
    pub(in crate::sync) note_signals: Vec<NormalizedRow>,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::sync) fn parse_import_at(
    archive_json: &str,
    now: i64,
) -> Result<NormalizedImport, SyncError> {
    let root: Value =
        serde_json::from_str(archive_json).map_err(|_| invalid("malformed JSON archive"))?;
    let root = root
        .as_object()
        .ok_or_else(|| invalid("archive root must be an object"))?;
    if root.get("_syntopicon") != Some(&Value::Bool(true)) {
        return Err(invalid("archive marker must be literal true"));
    }
    let schema_version = schema_version(root)?;

    let books = normalize_rows(archive_array(root, "books")?, "id", "id", |row, _| {
        normalize_book(row, now)
    })?;

    let mut raw_note_sources = HashMap::new();
    let notes = normalize_rows(archive_array(root, "notes")?, "id", "id", |row, id| {
        let normalized = normalize_note(row, now, schema_version)?;
        raw_note_sources.insert(
            id.to_string(),
            row.get("source")
                .and_then(Value::as_str)
                .map(str::to_string),
        );
        Ok(normalized)
    })?;

    let custom_ideas =
        normalize_rows(archive_array(root, "customIdeas")?, "id", "id", |row, _| {
            normalize_custom_idea(row, now)
        })?;
    let note_links = normalize_rows(archive_array(root, "noteLinks")?, "id", "id", |row, _| {
        normalize_note_link(row, now)
    })?;
    let lenses = normalize_rows(archive_array(root, "lenses")?, "id", "id", |row, _| {
        normalize_lens(row, now)
    })?;
    let collections = normalize_rows(archive_array(root, "collections")?, "id", "id", |row, _| {
        normalize_collection(row, now)
    })?;
    let collection_memberships = normalize_rows(
        archive_array(root, "collectionMemberships")?,
        "id",
        "id",
        |row, _| normalize_membership(row, now),
    )?;
    let note_signals = normalize_rows(
        archive_array(root, "noteSignals")?,
        "noteId",
        "note_id",
        |row, note_id| {
            normalize_note_signal(
                row,
                now,
                raw_note_sources
                    .get(note_id)
                    .and_then(|source| source.as_deref()),
            )
        },
    )?;

    Ok(NormalizedImport {
        schema_version,
        books,
        notes,
        custom_ideas,
        note_links,
        lenses,
        collections,
        collection_memberships,
        note_signals,
    })
}

fn invalid(reason: &str) -> SyncError {
    SyncError::InvalidImport(reason.to_string())
}

fn schema_version(root: &Map<String, Value>) -> Result<u32, SyncError> {
    match root.get("schemaVersion") {
        None | Some(Value::Null) => Ok(1),
        Some(value) => {
            let version = value
                .as_u64()
                .filter(|version| (1..=u64::from(MAX_SCHEMA_VERSION)).contains(version))
                .ok_or_else(|| invalid("schemaVersion must be an integer from 1 through 19"))?;
            Ok(version as u32)
        }
    }
}

fn archive_array<'a>(root: &'a Map<String, Value>, field: &str) -> Result<&'a [Value], SyncError> {
    match root.get(field) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(rows)) => Ok(rows),
        Some(_) => Err(invalid("archive store must be an array")),
    }
}

fn normalize_rows<F>(
    values: &[Value],
    input_key: &str,
    output_key: &str,
    mut normalize: F,
) -> Result<Vec<NormalizedRow>, SyncError>
where
    F: FnMut(&Map<String, Value>, &str) -> Result<Map<String, Value>, SyncError>,
{
    let mut seen = HashSet::new();
    let mut rows = Vec::with_capacity(values.len());
    for value in values {
        let input = value
            .as_object()
            .ok_or_else(|| invalid("archive store row must be an object"))?;
        validate_deleted(input)?;
        let primary_key = input
            .get(input_key)
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .ok_or_else(|| invalid("archive row primary key must be a non-empty string"))?;
        if !seen.insert(primary_key.to_string()) {
            return Err(invalid("archive store contains a duplicate primary key"));
        }

        let mut row = normalize(input, primary_key)?;
        row.insert(
            output_key.to_string(),
            Value::String(primary_key.to_string()),
        );
        row.insert("deleted".into(), Value::Bool(false));
        let updated_at = row
            .get("updated_at")
            .and_then(Value::as_i64)
            .expect("normalizers always set updated_at");
        rows.push(NormalizedRow {
            primary_key: primary_key.to_string(),
            row,
            updated_at,
        });
    }
    Ok(rows)
}

fn validate_deleted(input: &Map<String, Value>) -> Result<(), SyncError> {
    match input.get("deleted") {
        None | Some(Value::Null | Value::Bool(false)) => Ok(()),
        Some(Value::Number(number)) if number.as_f64() == Some(0.0) => Ok(()),
        Some(_) => Err(invalid("deleted must identify a live archive row")),
    }
}

fn normalize_book(input: &Map<String, Value>, now: i64) -> Result<Map<String, Value>, SyncError> {
    let mut output = Map::new();
    copy_string(input, &mut output, "title", "title", false)?;
    copy_string(input, &mut output, "author", "author", true)?;
    copy_string(input, &mut output, "isbn", "isbn", true)?;
    copy_string(input, &mut output, "coverUrl", "cover_url", true)?;
    copy_string(input, &mut output, "coverSource", "cover_source", true)?;
    copy_integer(
        input,
        &mut output,
        "coverResolvedAt",
        "cover_resolved_at",
        true,
    )?;
    copy_integer(input, &mut output, "createdAt", "created_at", false)?;
    output.insert(
        "updated_at".into(),
        Value::from(defaulted_timestamp(input, "updatedAt", now)?),
    );
    Ok(output)
}

fn normalize_note(
    input: &Map<String, Value>,
    now: i64,
    schema_version: u32,
) -> Result<Map<String, Value>, SyncError> {
    let mut output = Map::new();
    copy_string(input, &mut output, "bookId", "book_id", true)?;
    // Nullable like every other string field here (SUR-934). A note legitimately has no text — an
    // image-only capture, or a legacy row — and the exporter emits all note keys via `map_fields`, so
    // it writes an explicit `"text": null` for one. Rejecting that made the core unable to re-import
    // its OWN export, and `merge`'s `prepare_write` already contracts for it (`None | Some(Null)` →
    // text + content_tag null, no invented tag). An omitted key was always accepted; an explicit null
    // is the same fact stated out loud.
    copy_string(input, &mut output, "text", "text", true)?;
    copy_string(input, &mut output, "page", "page", true)?;
    copy_string(input, &mut output, "imagePath", "image_path", true)?;
    copy_string(input, &mut output, "inkCropPath", "ink_crop_path", true)?;
    copy_string(input, &mut output, "sourceId", "source_id", true)?;
    copy_integer(input, &mut output, "createdAt", "created_at", false)?;
    validate_nullable_string(input, "contentTag")?;

    let source = nullable_string(input, "source")?
        .flatten()
        .unwrap_or("manual");
    output.insert("source".into(), Value::String(source.to_string()));

    let source_id = input.get("sourceId").cloned().unwrap_or(Value::Null);
    if source_id.is_null() {
        output.insert("source_id".into(), Value::Null);
    }

    let chapter = match input.get("chapter") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(value)) => Value::String(value.clone()),
        Some(_) => return Err(invalid("known string field has an invalid type")),
    };
    output.insert("chapter".into(), chapter);

    let source_meta = match input.get("sourceMeta") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(value)) => value.clone(),
        Some(_) => return Err(invalid("sourceMeta must be an object")),
    };
    output.insert("source_meta".into(), Value::Object(source_meta));

    let tags = match input.get("tags") {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => string_array(value)?,
    };
    output.insert(
        "tags".into(),
        Value::Array(
            remap_tags(tags, schema_version)
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
    );
    output.insert(
        "updated_at".into(),
        Value::from(defaulted_timestamp(input, "updatedAt", now)?),
    );
    Ok(output)
}

fn normalize_custom_idea(
    input: &Map<String, Value>,
    now: i64,
) -> Result<Map<String, Value>, SyncError> {
    let mut output = Map::new();
    copy_string(input, &mut output, "name", "name", false)?;
    copy_string(input, &mut output, "description", "description", true)?;
    copy_integer(input, &mut output, "createdAt", "created_at", false)?;
    output.insert(
        "updated_at".into(),
        Value::from(defaulted_timestamp(input, "updatedAt", now)?),
    );
    Ok(output)
}

fn normalize_note_link(
    input: &Map<String, Value>,
    now: i64,
) -> Result<Map<String, Value>, SyncError> {
    let mut output = Map::new();
    copy_string(input, &mut output, "fromNoteId", "from_note_id", false)?;
    copy_string(input, &mut output, "toNoteId", "to_note_id", false)?;
    let relation_type = nullable_string(input, "relationType")?
        .flatten()
        .filter(|value| !value.is_empty())
        .unwrap_or(HANDWRITTEN_ANNOTATION);
    output.insert(
        "relation_type".into(),
        Value::String(relation_type.to_string()),
    );
    output.insert(
        "created_at".into(),
        Value::from(defaulted_timestamp(input, "createdAt", now)?),
    );
    output.insert(
        "updated_at".into(),
        Value::from(defaulted_timestamp(input, "updatedAt", now)?),
    );
    Ok(output)
}

fn normalize_lens(input: &Map<String, Value>, now: i64) -> Result<Map<String, Value>, SyncError> {
    let mut output = Map::new();
    copy_string(input, &mut output, "name", "name", false)?;
    let leaf_ids = match input.get("leafIds") {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => string_array(value)?,
    };
    output.insert(
        "leaf_ids".into(),
        Value::Array(leaf_ids.into_iter().map(Value::String).collect()),
    );
    let combinator = nullable_string(input, "combinator")?
        .flatten()
        .filter(|value| !value.is_empty())
        .unwrap_or("AND");
    output.insert("combinator".into(), Value::String(combinator.to_string()));
    output.insert(
        "threshold".into(),
        Value::from(nullish_integer(input, "threshold", 100)?),
    );
    output.insert(
        "created_at".into(),
        Value::from(defaulted_timestamp(input, "createdAt", now)?),
    );
    output.insert(
        "updated_at".into(),
        Value::from(defaulted_timestamp(input, "updatedAt", now)?),
    );
    Ok(output)
}

fn normalize_collection(
    input: &Map<String, Value>,
    now: i64,
) -> Result<Map<String, Value>, SyncError> {
    let mut output = Map::new();
    copy_string(input, &mut output, "name", "name", false)?;
    output.insert(
        "created_at".into(),
        Value::from(defaulted_timestamp(input, "createdAt", now)?),
    );
    output.insert(
        "updated_at".into(),
        Value::from(defaulted_timestamp(input, "updatedAt", now)?),
    );
    Ok(output)
}

fn normalize_membership(
    input: &Map<String, Value>,
    now: i64,
) -> Result<Map<String, Value>, SyncError> {
    let mut output = Map::new();
    copy_string(input, &mut output, "noteId", "note_id", false)?;
    copy_string(input, &mut output, "collectionId", "collection_id", false)?;
    output.insert(
        "created_at".into(),
        Value::from(defaulted_timestamp(input, "createdAt", now)?),
    );
    output.insert(
        "updated_at".into(),
        Value::from(defaulted_timestamp(input, "updatedAt", now)?),
    );
    Ok(output)
}

fn normalize_note_signal(
    input: &Map<String, Value>,
    now: i64,
    raw_note_source: Option<&str>,
) -> Result<Map<String, Value>, SyncError> {
    let mut output = Map::new();
    let source_prior = match input.get("sourcePrior") {
        None | Some(Value::Null) => source_prior(raw_note_source),
        Some(value) => finite_number(value)?,
    };
    let return_visits = nullish_integer(input, "returnVisits", 0)?;
    let stitch_spawns = nullish_integer(input, "stitchSpawns", 0)?;
    let has_annotation = match input.get("hasAnnotation") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(invalid("hasAnnotation must be a boolean")),
    };
    let exposure_recency_at = nullish_integer(input, "exposureRecencyAt", 0)?;
    let engagement_recency_at = nullish_integer(input, "engagementRecencyAt", 0)?;
    if let Some(value) = input.get("importance") {
        if !value.is_null() {
            finite_number(value)?;
        }
    }
    let importance = compute_importance(source_prior, return_visits, has_annotation, stitch_spawns);

    output.insert("source_prior".into(), Value::from(source_prior));
    output.insert("return_visits".into(), Value::from(return_visits));
    output.insert("has_annotation".into(), Value::Bool(has_annotation));
    output.insert("stitch_spawns".into(), Value::from(stitch_spawns));
    output.insert(
        "exposure_recency_at".into(),
        Value::from(exposure_recency_at),
    );
    output.insert(
        "engagement_recency_at".into(),
        Value::from(engagement_recency_at),
    );
    output.insert("importance".into(), Value::from(importance));
    output.insert(
        "created_at".into(),
        Value::from(defaulted_timestamp(input, "createdAt", now)?),
    );
    output.insert(
        "updated_at".into(),
        Value::from(defaulted_timestamp(input, "updatedAt", now)?),
    );
    Ok(output)
}

fn copy_string(
    input: &Map<String, Value>,
    output: &mut Map<String, Value>,
    input_field: &str,
    output_field: &str,
    nullable: bool,
) -> Result<(), SyncError> {
    match input.get(input_field) {
        None => Ok(()),
        Some(Value::String(value)) => {
            output.insert(output_field.into(), Value::String(value.clone()));
            Ok(())
        }
        Some(Value::Null) if nullable => {
            output.insert(output_field.into(), Value::Null);
            Ok(())
        }
        Some(_) => Err(invalid("known string field has an invalid type")),
    }
}

fn nullable_string<'a>(
    input: &'a Map<String, Value>,
    field: &str,
) -> Result<Option<Option<&'a str>>, SyncError> {
    match input.get(field) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(value)) => Ok(Some(Some(value))),
        Some(_) => Err(invalid("known string field has an invalid type")),
    }
}

fn validate_nullable_string(input: &Map<String, Value>, field: &str) -> Result<(), SyncError> {
    nullable_string(input, field).map(|_| ())
}

fn copy_integer(
    input: &Map<String, Value>,
    output: &mut Map<String, Value>,
    input_field: &str,
    output_field: &str,
    nullable: bool,
) -> Result<(), SyncError> {
    match input.get(input_field) {
        None => Ok(()),
        Some(Value::Null) if nullable => {
            output.insert(output_field.into(), Value::Null);
            Ok(())
        }
        Some(value) => {
            let integer = value
                .as_i64()
                .ok_or_else(|| invalid("known timestamp or count must be an integer"))?;
            output.insert(output_field.into(), Value::from(integer));
            Ok(())
        }
    }
}

fn defaulted_timestamp(
    input: &Map<String, Value>,
    field: &str,
    now: i64,
) -> Result<i64, SyncError> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(now),
        Some(value) => value
            .as_i64()
            .map(|timestamp| if timestamp == 0 { now } else { timestamp })
            .ok_or_else(|| invalid("known timestamp or count must be an integer")),
    }
}

fn nullish_integer(
    input: &Map<String, Value>,
    field: &str,
    default: i64,
) -> Result<i64, SyncError> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_i64()
            .ok_or_else(|| invalid("known timestamp or count must be an integer")),
    }
}

fn finite_number(value: &Value) -> Result<f64, SyncError> {
    value
        .as_f64()
        .filter(|number| number.is_finite())
        .ok_or_else(|| invalid("known signal float must be a finite JSON number"))
}

fn string_array(value: &Value) -> Result<Vec<String>, SyncError> {
    value
        .as_array()
        .ok_or_else(|| invalid("known string-array field must be an array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| invalid("known string-array field contains a non-string"))
        })
        .collect()
}

fn remap_tags(mut tags: Vec<String>, schema_version: u32) -> Vec<String> {
    if schema_version < 11 {
        tags = remap_tag_stage(tags, GREAT_IDEAS_RENAMES);
    }
    if schema_version < 14 {
        tags = remap_tag_stage(tags, CANON_REMAP_V14);
    }
    tags
}

fn remap_tag_stage(tags: Vec<String>, remap: &[(&str, &str)]) -> Vec<String> {
    let mut seen = HashSet::new();
    tags.into_iter()
        .filter_map(|tag| {
            let mapped = remap
                .iter()
                .find_map(|(source, target)| (*source == tag).then_some(*target))
                .map(str::to_string)
                .unwrap_or(tag);
            seen.insert(mapped.clone()).then_some(mapped)
        })
        .collect()
}

pub(crate) fn source_prior(source: Option<&str>) -> f64 {
    match source {
        Some("handwritten" | "readwise_annotation") => 0.9,
        Some("share") => 0.75,
        Some("manual") => 0.7,
        Some("image") => 0.6,
        Some("readwise") => 0.5,
        _ => 0.5,
    }
}

/// The PWA's `computeImportance` (surfc `db.js`): behavioural evidence decays the source prior
/// toward 0 (half-life 1.5 evidence units) and adds itself back on top. The 0.3 annotation
/// weight is why `has_annotation` matters to ranking (SUR-956). Single source of truth —
/// used by import normalization AND `replace_handwritten_annotations`' signal refresh.
pub(crate) fn compute_importance(
    source_prior: f64,
    return_visits: i64,
    has_annotation: bool,
    stitch_spawns: i64,
) -> f64 {
    let evidence = return_visits as f64 * 0.1
        + if has_annotation { 0.3 } else { 0.0 }
        + stitch_spawns as f64 * 0.5;
    source_prior * (-std::f64::consts::LN_2 * evidence.max(0.0) / 1.5).exp() + evidence
}

#[cfg(test)]
mod import_tests {
    use std::future::Future;

    use serde_json::{json, Map, Value};

    use super::super::merge::merge_parsed_with_sink;
    use super::{
        compute_importance, parse_import_at, NormalizedImport, NormalizedRow, CANON_REMAP_V14,
        GREAT_IDEAS_RENAMES,
    };
    use crate::store::Store;
    use crate::sync::http::PostgrestSink;
    use crate::sync::{ImportCounts, ImportSummary, SyncError};
    use crate::vault::Vault;

    const NOW: i64 = 9_000;
    const FIXTURE_NOW: i64 = 1_700_000_000_000;
    const SCHEMA_1_FIXTURE: &str =
        include_str!("../../../vendored/snapshot-parity/schema-1-preversioned.json");
    const SCHEMA_10_FIXTURE: &str =
        include_str!("../../../vendored/snapshot-parity/schema-10-pre-v11.json");
    const SCHEMA_11_FIXTURE: &str =
        include_str!("../../../vendored/snapshot-parity/schema-11-pre-v14.json");
    const SCHEMA_14_FIXTURE: &str =
        include_str!("../../../vendored/snapshot-parity/schema-14-current-tags.json");
    const SCHEMA_19_FIXTURE: &str =
        include_str!("../../../vendored/snapshot-parity/schema-19-all-stores.json");
    const SCHEMA_19_EXPECTED: &str =
        include_str!("../../../vendored/snapshot-parity/expected-schema-19-all-stores.json");
    const SCHEMA_19_DEFAULTS_FIXTURE: &str =
        include_str!("../../../vendored/snapshot-parity/schema-19-defaults.json");
    const SCHEMA_19_DEFAULTS_EXPECTED: &str =
        include_str!("../../../vendored/snapshot-parity/expected-schema-19-defaults.json");

    #[test]
    fn compute_importance_pins_the_pwa_formula() {
        // Literal expected values (NOT re-derived from the formula) pin the SUR-956 extraction
        // against drift: cold start == the prior; annotation alone adds 0.3 evidence; the mixed
        // case exercises all three weights (visits 0.1, annotation 0.3, spawns 0.5).
        assert_eq!(
            compute_importance(0.5, 0, false, 0),
            0.5,
            "no evidence → the prior"
        );
        assert!(
            (compute_importance(0.7, 0, true, 0) - 0.909_385_394_307_286_9).abs() < 1e-12,
            "0.7 * 2^(-0.3/1.5) + 0.3"
        );
        assert!(
            (compute_importance(0.9, 5, true, 2) - 2.191_747_753_483_256).abs() < 1e-12,
            "0.9 * 2^(-1.8/1.5) + 1.8"
        );
    }

    fn parse_raw(raw: &str) -> Result<NormalizedImport, SyncError> {
        parse_import_at(raw, NOW)
    }

    fn parse(value: Value) -> Result<NormalizedImport, SyncError> {
        parse_raw(&serde_json::to_string(&value).unwrap())
    }

    fn archive(schema_version: u32) -> Value {
        json!({ "_syntopicon": true, "schemaVersion": schema_version })
    }

    fn row(normalized: &NormalizedRow) -> &Map<String, Value> {
        &normalized.row
    }

    fn assert_invalid(raw: &str) {
        let error = parse_raw(raw).unwrap_err();
        assert!(matches!(error, SyncError::InvalidImport(_)));
    }

    fn run<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    struct EmptySink;

    impl PostgrestSink for EmptySink {
        async fn upsert(
            &self,
            _table: &str,
            _on_conflict: &str,
            _rows: &Value,
        ) -> Result<(), String> {
            panic!("snapshot import must only stage an outbox write")
        }

        async fn fetch_page(
            &self,
            _table: &str,
            _after_seq: i64,
            _limit: i64,
        ) -> Result<Vec<Value>, String> {
            Ok(Vec::new())
        }

        async fn fetch_by_ids(
            &self,
            _table: &str,
            _primary_key: &str,
            _ids: &[String],
        ) -> Result<Vec<Value>, String> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn public_import_result_types_and_invalid_import_variant_have_exact_fields() {
        let counts = ImportCounts {
            books: 1,
            notes: 2,
            custom_ideas: 3,
            note_links: 4,
            lenses: 5,
            collections: 6,
            collection_memberships: 7,
            note_signals: 8,
        };
        let summary = ImportSummary {
            schema_version: 19,
            imported: counts,
            skipped_stale: ImportCounts {
                books: 8,
                notes: 7,
                custom_ideas: 6,
                note_links: 5,
                lenses: 4,
                collections: 3,
                collection_memberships: 2,
                note_signals: 1,
            },
        };

        assert_eq!(summary.schema_version, 19);
        assert_eq!(summary.imported.note_signals, 8);
        assert_eq!(summary.skipped_stale.collection_memberships, 2);
        assert!(matches!(
            SyncError::InvalidImport("invalid archive".into()),
            SyncError::InvalidImport(message) if message == "invalid archive"
        ));
    }

    #[test]
    fn rejects_malformed_json_bad_root_or_non_literal_marker_without_echoing_data() {
        let secret = "PLAINTEXT-MUST-NOT-ECHO";
        let error = parse_raw(&format!("{{{secret}")).unwrap_err();
        assert!(matches!(error, SyncError::InvalidImport(_)));
        assert!(!error.to_string().contains(secret));

        for raw in [
            "null",
            "[]",
            "true",
            "{}",
            r#"{"_syntopicon":false}"#,
            r#"{"_syntopicon":1}"#,
            r#"{"_syntopicon":"true"}"#,
        ] {
            assert_invalid(raw);
        }
    }

    #[test]
    fn defaults_and_bounds_schema_version_and_archive_arrays() {
        assert_eq!(
            parse(json!({ "_syntopicon": true }))
                .unwrap()
                .schema_version,
            1
        );
        assert_eq!(
            parse(json!({ "_syntopicon": true, "schemaVersion": null }))
                .unwrap()
                .schema_version,
            1
        );
        assert_eq!(parse(archive(1)).unwrap().schema_version, 1);
        let parsed = parse(archive(19)).unwrap();
        assert_eq!(parsed.schema_version, 19);
        assert!(parsed.books.is_empty());
        assert!(parsed.notes.is_empty());
        assert!(parsed.custom_ideas.is_empty());
        assert!(parsed.note_links.is_empty());
        assert!(parsed.lenses.is_empty());
        assert!(parsed.collections.is_empty());
        assert!(parsed.collection_memberships.is_empty());
        assert!(parsed.note_signals.is_empty());

        for schema in [json!(0), json!(-1), json!(1.5), json!("1"), json!(20)] {
            assert!(parse(json!({ "_syntopicon": true, "schemaVersion": schema })).is_err());
        }

        let null_arrays = parse(json!({
            "_syntopicon": true,
            "books": null,
            "notes": null,
            "customIdeas": null,
            "noteLinks": null,
            "lenses": null,
            "collections": null,
            "collectionMemberships": null,
            "noteSignals": null
        }))
        .unwrap();
        assert!(null_arrays.books.is_empty() && null_arrays.note_signals.is_empty());

        for field in [
            "books",
            "notes",
            "customIdeas",
            "noteLinks",
            "lenses",
            "collections",
            "collectionMemberships",
            "noteSignals",
        ] {
            let mut root = archive(19);
            root.as_object_mut()
                .unwrap()
                .insert(field.into(), json!({}));
            assert!(parse(root).is_err(), "{field} must be an array");
        }
    }

    #[test]
    fn validates_known_field_types_and_primary_keys_before_any_io() {
        let invalid_rows = [
            ("books", json!({"id":"b","title":1})),
            ("notes", json!({"id":"n","tags":["Truth", 1]})),
            ("notes", json!({"id":"n","sourceMeta":[]})),
            ("notes", json!({"id":"n","sourceId":false})),
            ("notes", json!({"id":"n","updatedAt":1.5})),
            ("lenses", json!({"id":"l","leafIds":["Truth", 1]})),
            ("lenses", json!({"id":"l","threshold":1.5})),
            ("noteSignals", json!({"noteId":"n","sourcePrior":"0.7"})),
            ("noteSignals", json!({"noteId":"n","returnVisits":1.5})),
            ("noteSignals", json!({"noteId":"n","hasAnnotation":1})),
        ];
        for (table, invalid_row) in invalid_rows {
            let mut root = archive(19);
            root.as_object_mut()
                .unwrap()
                .insert(table.into(), json!([invalid_row]));
            assert!(parse(root).is_err(), "bad known field in {table} must fail");
        }

        for (table, key) in [
            ("books", "id"),
            ("notes", "id"),
            ("customIdeas", "id"),
            ("noteLinks", "id"),
            ("lenses", "id"),
            ("collections", "id"),
            ("collectionMemberships", "id"),
            ("noteSignals", "noteId"),
        ] {
            for bad_key in [json!(null), json!(""), json!(1)] {
                let mut item = Map::new();
                item.insert(key.into(), bad_key);
                let mut root = archive(19);
                root.as_object_mut()
                    .unwrap()
                    .insert(table.into(), Value::Array(vec![Value::Object(item)]));
                assert!(parse(root).is_err(), "bad {table}.{key} must fail");
            }

            let mut duplicate_item = Map::new();
            duplicate_item.insert(key.into(), json!("same"));
            let duplicate = Value::Object(duplicate_item);
            let mut root = archive(19);
            root.as_object_mut().unwrap().insert(
                table.into(),
                Value::Array(vec![duplicate.clone(), duplicate]),
            );
            assert!(parse(root).is_err(), "duplicate keys in {table} must fail");
        }
    }

    #[test]
    fn validates_deleted_live_shape_for_all_eight_store_rows() {
        for (table, key) in [
            ("books", "id"),
            ("notes", "id"),
            ("customIdeas", "id"),
            ("noteLinks", "id"),
            ("lenses", "id"),
            ("collections", "id"),
            ("collectionMemberships", "id"),
            ("noteSignals", "noteId"),
        ] {
            for accepted in [Value::Null, json!(0), json!(0.0), json!(false)] {
                let mut item = Map::new();
                item.insert(key.into(), json!("row"));
                item.insert("deleted".into(), accepted);
                let mut root = archive(19);
                root.as_object_mut()
                    .unwrap()
                    .insert(table.into(), Value::Array(vec![Value::Object(item)]));
                assert!(parse(root).is_ok(), "supported {table}.deleted must import");
            }

            for rejected in [
                json!(true),
                json!(1),
                json!(-1),
                json!(0.5),
                json!("PLAINTEXT-MUST-NOT-ECHO"),
                json!([]),
                json!({}),
            ] {
                let mut item = Map::new();
                item.insert(key.into(), json!("row"));
                item.insert("deleted".into(), rejected);
                let mut root = archive(19);
                root.as_object_mut()
                    .unwrap()
                    .insert(table.into(), Value::Array(vec![Value::Object(item)]));
                let error = parse(root).expect_err("malformed deleted must fail");
                assert!(matches!(error, SyncError::InvalidImport(_)));
                assert!(!error.to_string().contains("PLAINTEXT-MUST-NOT-ECHO"));
            }
        }
    }

    #[test]
    fn maps_all_eight_tables_to_exact_store_fields_and_tracks_keys_and_timestamps() {
        let parsed = parse(json!({
            "_syntopicon": true,
            "schemaVersion": 19,
            "books": [{
                "id":"b1", "title":"Book", "author":"Author", "isbn":null,
                "coverUrl":"https://cover", "coverSource":"openlibrary",
                "coverResolvedAt":null, "createdAt":101, "updatedAt":102,
                "deleted":0, "unknown":"ignored"
            }],
            "notes": [{
                "id":"n1", "bookId":"b1", "text":"plaintext", "page":null,
                "tags":["Truth"], "imagePath":"user/image", "inkCropPath":null,
                "source":"manual", "sourceId":null, "sourceMeta":{"case":2},
                "chapter":null, "contentTag":"foreign-key-tag", "createdAt":201,
                "updatedAt":202, "deleted":0, "imageDataUrl":"data:secret",
                "inkCropDataUrl":"data:crop", "user_metadata":{"private":true},
                "unknown":"ignored"
            }],
            "customIdeas": [{
                "id":"ci1", "name":"Mine", "description":null,
                "createdAt":301, "updatedAt":302, "deleted":0
            }],
            "noteLinks": [{
                "id":"link1", "fromNoteId":"n1", "toNoteId":"n2",
                "relationType":"related", "createdAt":401, "updatedAt":402,
                "deleted":0
            }],
            "lenses": [{
                "id":"lens1", "name":"Lens", "leafIds":["Truth", "Justice"],
                "combinator":"COOCCUR", "threshold":60, "createdAt":501,
                "updatedAt":502, "deleted":0
            }],
            "collections": [{
                "id":"col1", "name":"Study", "createdAt":601,
                "updatedAt":602, "deleted":0
            }],
            "collectionMemberships": [{
                "id":"col1:n1", "noteId":"n1", "collectionId":"col1",
                "createdAt":701, "updatedAt":702, "deleted":0
            }],
            "noteSignals": [{
                "noteId":"n1", "sourcePrior":0.7, "returnVisits":4,
                "hasAnnotation":true, "stitchSpawns":1,
                "exposureRecencyAt":801, "engagementRecencyAt":802,
                "importance":999, "createdAt":803, "updatedAt":804,
                "deleted":0, "evidence":999
            }]
        }))
        .unwrap();

        assert_eq!(parsed.books[0].primary_key, "b1");
        assert_eq!(parsed.books[0].updated_at, 102);
        assert_eq!(
            row(&parsed.books[0]),
            json!({
                "id":"b1", "title":"Book", "author":"Author", "isbn":null,
                "cover_url":"https://cover", "cover_source":"openlibrary",
                "cover_resolved_at":null, "created_at":101, "updated_at":102,
                "deleted":false
            })
            .as_object()
            .unwrap()
        );
        assert_eq!(parsed.notes[0].primary_key, "n1");
        assert_eq!(
            row(&parsed.notes[0]),
            json!({
                "id":"n1", "book_id":"b1", "text":"plaintext", "page":null,
                "tags":["Truth"], "image_path":"user/image", "ink_crop_path":null,
                "source":"manual", "source_id":null, "source_meta":{"case":2},
                "chapter":null, "created_at":201, "updated_at":202, "deleted":false
            })
            .as_object()
            .unwrap()
        );
        assert_eq!(
            row(&parsed.custom_ideas[0]),
            json!({"id":"ci1","name":"Mine","description":null,"created_at":301,"updated_at":302,"deleted":false}).as_object().unwrap()
        );
        assert_eq!(
            row(&parsed.note_links[0]),
            json!({"id":"link1","from_note_id":"n1","to_note_id":"n2","relation_type":"related","created_at":401,"updated_at":402,"deleted":false}).as_object().unwrap()
        );
        assert_eq!(
            row(&parsed.lenses[0]),
            json!({"id":"lens1","name":"Lens","leaf_ids":["Truth","Justice"],"combinator":"COOCCUR","threshold":60,"created_at":501,"updated_at":502,"deleted":false}).as_object().unwrap()
        );
        assert_eq!(
            row(&parsed.collections[0]),
            json!({"id":"col1","name":"Study","created_at":601,"updated_at":602,"deleted":false})
                .as_object()
                .unwrap()
        );
        assert_eq!(
            row(&parsed.collection_memberships[0]),
            json!({"id":"col1:n1","note_id":"n1","collection_id":"col1","created_at":701,"updated_at":702,"deleted":false}).as_object().unwrap()
        );
        let signal = row(&parsed.note_signals[0]);
        assert_eq!(signal["note_id"], "n1");
        assert_eq!(signal["importance"].as_f64().unwrap(), 1.6020444242489622);
        assert_eq!(parsed.note_signals[0].primary_key, "n1");
        assert_eq!(parsed.note_signals[0].updated_at, 804);
        assert!(!signal.contains_key("evidence"));
        assert!(!signal.contains_key("unknown"));
    }

    #[test]
    fn applies_pwa_defaults_once_with_the_supplied_now() {
        let parsed = parse(json!({
            "_syntopicon": true,
            "schemaVersion": 19,
            "books": [{"id":"b","title":"B","updatedAt":0}],
            "notes": [{"id":"n","text":"T","updatedAt":null}],
            "customIdeas": [{"id":"i","name":"I"}],
            "noteLinks": [{"id":"e","fromNoteId":"n","toNoteId":"n2"}],
            "lenses": [{"id":"l","name":"L","threshold":0}],
            "collections": [{"id":"c","name":"C"}],
            "collectionMemberships": [{"id":"m","noteId":"n","collectionId":"c"}],
            "noteSignals": [{"noteId":"n"}]
        }))
        .unwrap();

        assert_eq!(parsed.books[0].updated_at, NOW);
        assert_eq!(parsed.custom_ideas[0].updated_at, NOW);
        assert_eq!(
            row(&parsed.notes[0]),
            json!({
                "id":"n", "text":"T", "source":"manual", "source_id":null,
                "source_meta":{}, "chapter":null, "tags":[],
                "updated_at":NOW, "deleted":false
            })
            .as_object()
            .unwrap()
        );
        assert_eq!(
            row(&parsed.note_links[0])["relation_type"],
            "handwritten_annotation"
        );
        assert_eq!(row(&parsed.note_links[0])["created_at"], NOW);
        assert_eq!(row(&parsed.lenses[0])["leaf_ids"], json!([]));
        assert_eq!(row(&parsed.lenses[0])["combinator"], "AND");
        assert_eq!(row(&parsed.lenses[0])["threshold"], 0);
        assert_eq!(row(&parsed.collections[0])["created_at"], NOW);
        assert_eq!(row(&parsed.collection_memberships[0])["updated_at"], NOW);
        let signal = row(&parsed.note_signals[0]);
        assert_eq!(signal["source_prior"], 0.5);
        assert_eq!(signal["return_visits"], 0);
        assert_eq!(signal["has_annotation"], false);
        assert_eq!(signal["stitch_spawns"], 0);
        assert_eq!(signal["exposure_recency_at"], 0);
        assert_eq!(signal["engagement_recency_at"], 0);
        assert_eq!(signal["importance"], 0.5);
        assert_eq!(signal["created_at"], NOW);
        assert_eq!(signal["updated_at"], NOW);
    }

    #[test]
    fn replays_the_exact_case_sensitive_order_preserving_tag_chain() {
        let parse_tags = |schema_version, tags: Value| {
            let parsed = parse(json!({
                "_syntopicon": true,
                "schemaVersion": schema_version,
                "notes": [{"id":"n","text":"T","tags":tags}]
            }))
            .unwrap();
            row(&parsed.notes[0])["tags"].clone()
        };

        assert_eq!(
            parse_tags(
                10,
                json!(["War", "Knowledge", "War and Peace", "Angel", "war"])
            ),
            json!(["Conflict", "Truth", "Angel", "war"])
        );
        assert_eq!(
            parse_tags(11, json!(["War", "Knowledge", "War and Peace"])),
            json!(["War", "Truth", "Conflict"])
        );
        assert_eq!(
            parse_tags(14, json!(["War", "Knowledge", "Knowledge"])),
            json!(["War", "Knowledge", "Knowledge"])
        );
    }

    #[test]
    fn frozen_pwa_fixtures_cover_schemas_1_10_11_14_and_19() {
        let schema_1 = parse_import_at(SCHEMA_1_FIXTURE, FIXTURE_NOW).unwrap();
        assert_eq!(schema_1.schema_version, 1);
        assert_eq!(schema_1.books.len(), 1);
        assert_eq!(schema_1.notes.len(), 1);
        assert_eq!(schema_1.custom_ideas.len(), 1);
        assert!(schema_1.note_links.is_empty());
        assert!(schema_1.lenses.is_empty());
        assert!(schema_1.collections.is_empty());
        assert!(schema_1.collection_memberships.is_empty());
        assert!(schema_1.note_signals.is_empty());
        assert_eq!(row(&schema_1.books[0])["updated_at"], FIXTURE_NOW);
        assert_eq!(row(&schema_1.custom_ideas[0])["updated_at"], FIXTURE_NOW);
        assert_eq!(
            row(&schema_1.notes[0]),
            json!({
                "id":"n-v1", "book_id":"b-v1", "text":"Legacy note", "page":"1",
                "tags":[], "source":"manual", "source_id":null, "source_meta":{},
                "chapter":null, "created_at":2000, "updated_at":FIXTURE_NOW,
                "deleted":false
            })
            .as_object()
            .unwrap()
        );

        let schema_10 = parse_import_at(SCHEMA_10_FIXTURE, FIXTURE_NOW).unwrap();
        assert_eq!(schema_10.schema_version, 10);
        assert_eq!(
            row(&schema_10.notes[0])["tags"],
            json!(["Conflict", "Truth", "Angel", "war"])
        );

        let schema_11 = parse_import_at(SCHEMA_11_FIXTURE, FIXTURE_NOW).unwrap();
        assert_eq!(schema_11.schema_version, 11);
        assert_eq!(
            row(&schema_11.notes[0])["tags"],
            json!(["War", "Truth", "Conflict", "Status", "Power", "Angel"])
        );

        let schema_14 = parse_import_at(SCHEMA_14_FIXTURE, FIXTURE_NOW).unwrap();
        assert_eq!(schema_14.schema_version, 14);
        assert_eq!(
            row(&schema_14.notes[0])["tags"],
            json!(["War", "Knowledge", "Knowledge", "Angel"])
        );
        assert_eq!(
            row(&schema_14.note_links[0]),
            json!({
                "id":"link-v14", "from_note_id":"n-v14-parent",
                "to_note_id":"n-v14-child", "relation_type":"handwritten_annotation",
                "created_at":FIXTURE_NOW, "updated_at":FIXTURE_NOW, "deleted":false
            })
            .as_object()
            .unwrap()
        );

        let schema_19 = parse_import_at(SCHEMA_19_FIXTURE, FIXTURE_NOW).unwrap();
        assert_eq!(schema_19.schema_version, 19);
        assert_eq!(schema_19.books.len(), 1);
        assert_eq!(schema_19.notes.len(), 2);
        assert_eq!(schema_19.custom_ideas.len(), 1);
        assert_eq!(schema_19.note_links.len(), 1);
        assert_eq!(schema_19.lenses.len(), 1);
        assert_eq!(schema_19.collections.len(), 1);
        assert_eq!(schema_19.collection_memberships.len(), 1);
        assert_eq!(schema_19.note_signals.len(), 1);
        let parent = row(&schema_19.notes[0]);
        for ignored in [
            "content_tag",
            "contentTag",
            "imageDataUrl",
            "inkCropDataUrl",
            "user_metadata",
            "futureField",
        ] {
            assert!(!parent.contains_key(ignored));
        }
        let signal = row(&schema_19.note_signals[0]);
        assert_eq!(signal["source_prior"], 0.7);
        assert_eq!(signal["return_visits"], 4);
        assert_eq!(signal["has_annotation"], true);
        assert_eq!(signal["stitch_spawns"], 1);
        assert_eq!(signal["importance"], 1.6020444242489622);
    }

    #[test]
    fn frozen_tag_migration_tables_are_exhaustive() {
        const EXPECTED_V11: &[(&str, &str)] = &[
            ("Good", "Good and Evil"),
            ("Custom", "Custom and Convention"),
            ("Pleasure", "Pleasure and Pain"),
            ("Virtue", "Virtue and Vice"),
            ("Sign", "Sign and Symbol"),
            ("War", "War and Peace"),
            ("Tyranny", "Tyranny and Despotism"),
            ("Life", "Life and Death"),
            ("Memory", "Memory and Imagination"),
            ("Necessity", "Necessity and Contingency"),
            ("Universal", "Universal and Particular"),
        ];
        const EXPECTED_V14: &[(&str, &str)] = &[
            ("Cause", "Causation"),
            ("Chance", "Probability"),
            ("Liberty", "Freedom"),
            ("Honor", "Status"),
            ("Virtue and Vice", "Virtue"),
            ("Animal", "Life"),
            ("Aristocracy", "Power"),
            ("Monarchy", "Power"),
            ("Oligarchy", "Power"),
            ("Tyranny and Despotism", "Power"),
            ("Constitution", "Institutions"),
            ("Government", "Institutions"),
            ("State", "Institutions"),
            ("Citizen", "Institutions"),
            ("Custom and Convention", "Institutions"),
            ("Courage", "Virtue"),
            ("Dialectic", "Reasoning"),
            ("Induction", "Reasoning"),
            ("Logic", "Reasoning"),
            ("Duty", "Obligation"),
            ("Education", "Learning"),
            ("Experience", "Learning"),
            ("Family", "Community"),
            ("Form", "Beauty"),
            ("God", "the Sacred"),
            ("Religion", "the Sacred"),
            ("Theology", "the Sacred"),
            ("Prophecy", "the Sacred"),
            ("Immortality", "the Sacred"),
            ("Hypothesis", "Evidence"),
            ("Labor", "Productivity"),
            ("Mind", "Consciousness"),
            ("Soul", "Consciousness"),
            ("Sense", "Consciousness"),
            ("Poetry", "Art"),
            ("Property", "Markets"),
            ("Wealth", "Markets"),
            ("Prudence", "Strategy"),
            ("Punishment", "Justice"),
            ("Revolution", "Conflict"),
            ("Rhetoric", "Narrative"),
            ("Sign and Symbol", "Language"),
            ("Sin", "Morality"),
            ("Temperance", "Discipline"),
            ("Wisdom", "Judgment"),
            ("Opinion", "Judgment"),
            ("Will", "Motivation"),
            ("World", "Nature"),
            ("Man", "Identity"),
            ("Good and Evil", "Morality"),
            ("Happiness", "Purpose"),
            ("Knowledge", "Truth"),
            ("Law", "Institutions"),
            ("Life and Death", "Life"),
            ("Memory and Imagination", "Memory"),
            ("Pleasure and Pain", "Emotion"),
            ("Slavery", "Freedom"),
            ("War and Peace", "Conflict"),
        ];

        assert_eq!(GREAT_IDEAS_RENAMES, EXPECTED_V11);
        assert_eq!(CANON_REMAP_V14, EXPECTED_V14);
        assert_eq!(EXPECTED_V11.len(), 11);
        assert_eq!(EXPECTED_V14.len(), 58);
    }

    fn assert_fixture_import_matches_independent_oracle(fixture: &str, expected_json: &str) {
        const TABLES: [&str; 8] = [
            "books",
            "notes",
            "custom_ideas",
            "note_links",
            "lenses",
            "collections",
            "collection_memberships",
            "note_signals",
        ];

        let store = Store::open_in_memory().unwrap();
        let vault = Vault::generate();
        let parsed = parse_import_at(fixture, FIXTURE_NOW).unwrap();

        let summary = run(merge_parsed_with_sink(
            &store,
            &EmptySink,
            &vault,
            parsed,
            FIXTURE_NOW,
        ))
        .unwrap();

        let expected_document: Value = serde_json::from_str(expected_json).unwrap();
        let expected = expected_document["nativeRows"]
            .as_object()
            .expect("fixture must carry an independent complete native-row oracle");
        assert_eq!(expected.len(), TABLES.len());
        let newest_original_updated_at = expected_document["pwaRows"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|rows| rows.as_array().unwrap())
            .filter_map(|row| row.get("updatedAt").and_then(Value::as_i64))
            .max()
            .unwrap();
        let expected_batch_updated_at =
            FIXTURE_NOW.max(newest_original_updated_at.checked_add(1).unwrap());

        let row_count = |table: &str| expected[table].as_array().unwrap().len() as u32;
        assert_eq!(summary.schema_version, 19);
        assert_eq!(
            summary.imported,
            ImportCounts {
                books: row_count("books"),
                notes: row_count("notes"),
                custom_ideas: row_count("custom_ideas"),
                note_links: row_count("note_links"),
                lenses: row_count("lenses"),
                collections: row_count("collections"),
                collection_memberships: row_count("collection_memberships"),
                note_signals: row_count("note_signals"),
            }
        );
        assert_eq!(summary.skipped_stale, ImportCounts::default());

        let mut observed_batch_updated_at = None;
        let mut expected_outbox = Vec::new();
        for table in TABLES {
            for expected_value in expected[table].as_array().unwrap() {
                let expected_row = expected_value.as_object().unwrap().clone();
                let primary_key_field = if table == "note_signals" {
                    "note_id"
                } else {
                    "id"
                };
                let primary_key = expected_row[primary_key_field]
                    .as_str()
                    .unwrap()
                    .to_string();
                let mut actual = store.get_row(table, &primary_key).unwrap().unwrap();
                let actual_updated_at = actual["updated_at"].as_i64().unwrap();
                if let Some(observed) = observed_batch_updated_at {
                    assert_eq!(actual_updated_at, observed);
                } else {
                    observed_batch_updated_at = Some(actual_updated_at);
                }
                // Merge-import deliberately replaces every original timestamp with one
                // collision-safe batch stamp. The independent rows normalize that one
                // documented difference back to the fixed oracle clock for comparison.
                actual.insert("updated_at".into(), expected_row["updated_at"].clone());

                if table == "notes" {
                    let plaintext = expected_row
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap();
                    let ciphertext = actual
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap();
                    assert!(ciphertext.starts_with("enc:v2:"));
                    assert_ne!(ciphertext, plaintext);
                    assert_eq!(
                        vault
                            .decrypt_note(Some(primary_key.clone()), ciphertext)
                            .unwrap(),
                        plaintext
                    );
                    let book_id = expected_row
                        .get("book_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let expected_tag = vault.content_tag(plaintext.clone(), book_id);
                    assert_eq!(
                        actual.get("content_tag"),
                        Some(&Value::String(expected_tag))
                    );
                    // The independent oracle stores plaintext and a null exporting-key tag;
                    // normalize only after proving the persisted row was freshly sealed and
                    // tagged under this Vault key.
                    actual.insert("text".into(), Value::String(plaintext));
                    actual.insert("content_tag".into(), Value::Null);
                }

                assert_eq!(actual, expected_row, "persisted {table} row mismatch");
                expected_outbox.push((table, primary_key));
            }
        }
        assert_eq!(observed_batch_updated_at, Some(expected_batch_updated_at));

        let queued = store.outbox_items().unwrap();
        assert_eq!(queued.len(), expected_outbox.len());
        for ((_, table, record_id, payload, created_at), (expected_table, expected_id)) in
            queued.iter().zip(expected_outbox)
        {
            assert_eq!(table, expected_table);
            assert_eq!(record_id.as_deref(), Some(expected_id.as_str()));
            assert_eq!(*created_at, expected_batch_updated_at);
            let payload: Value = serde_json::from_str(payload).unwrap();
            assert_eq!(
                payload.as_object(),
                store.get_row(table, &expected_id).unwrap().as_ref()
            );
            if table == "notes" {
                let ciphertext = payload["text"].as_str().unwrap();
                assert!(ciphertext.starts_with("enc:v2:"));
                assert_eq!(
                    vault
                        .decrypt_note(Some(expected_id.clone()), ciphertext.to_string())
                        .unwrap(),
                    expected[table]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|row| row["id"] == expected_id)
                        .unwrap()["text"]
                        .as_str()
                        .unwrap()
                );
            }
        }
    }

    #[test]
    fn frozen_schema_19_import_stages_exact_rows_for_all_eight_stores() {
        assert_fixture_import_matches_independent_oracle(SCHEMA_19_FIXTURE, SCHEMA_19_EXPECTED);
    }

    #[test]
    fn frozen_schema_19_defaults_match_the_independent_all_store_oracle() {
        assert_fixture_import_matches_independent_oracle(
            SCHEMA_19_DEFAULTS_FIXTURE,
            SCHEMA_19_DEFAULTS_EXPECTED,
        );
    }

    #[test]
    fn derives_missing_signal_priors_from_raw_note_sources_and_recomputes_importance() {
        let sources = [
            ("handwritten", 0.9),
            ("readwise_annotation", 0.9),
            ("share", 0.75),
            ("manual", 0.7),
            ("image", 0.6),
            ("readwise", 0.5),
            ("future", 0.5),
        ];
        let notes: Vec<_> = sources
            .iter()
            .enumerate()
            .map(
                |(index, (source, _))| json!({"id":format!("n{index}"),"text":"T","source":source}),
            )
            .chain(std::iter::once(json!({"id":"missing-source","text":"T"})))
            .collect();
        let signals: Vec<_> = sources
            .iter()
            .enumerate()
            .map(|(index, _)| json!({"noteId":format!("n{index}"),"importance":999}))
            .chain(std::iter::once(json!({"noteId":"missing-source"})))
            .chain(std::iter::once(json!({"noteId":"missing-note"})))
            .collect();
        let parsed = parse(json!({
            "_syntopicon":true,
            "schemaVersion":19,
            "notes":notes,
            "noteSignals":signals
        }))
        .unwrap();

        let actual: Vec<_> = parsed
            .note_signals
            .iter()
            .map(|signal| row(signal)["source_prior"].as_f64().unwrap())
            .collect();
        let mut expected: Vec<_> = sources.iter().map(|(_, prior)| *prior).collect();
        expected.extend([0.5, 0.5]);
        assert_eq!(actual, expected);
        assert!(parsed
            .note_signals
            .iter()
            .all(|signal| row(signal)["importance"] == row(signal)["source_prior"]));
    }

    #[test]
    fn discards_foreign_content_tags_local_previews_metadata_and_unknown_fields() {
        let parsed = parse(json!({
            "_syntopicon":true,
            "notes":[{
                "id":"n", "text":"secret", "contentTag":"foreign",
                "imageDataUrl":"data:image", "inkCropDataUrl":"data:crop",
                "user_metadata":{"annotation":["private"]}, "futureField":"ignored"
            }]
        }))
        .unwrap();
        let note = row(&parsed.notes[0]);

        for ignored in [
            "content_tag",
            "contentTag",
            "imageDataUrl",
            "inkCropDataUrl",
            "user_metadata",
            "futureField",
        ] {
            assert!(!note.contains_key(ignored));
        }
        assert_eq!(note["text"], "secret");
    }
}
