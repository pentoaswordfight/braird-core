//! Schema-drift parity (SUR-723 §7): the native SQLite mirror's synced
//! `(column, logical-type)` set must equal the vendored `sync-schema.json` — which is
//! derived from `surfc/main`'s `supabase.js` `upsert*` payloads (the synced column set;
//! `fetchSince` is `select('*')` and does not enumerate it) + the migrations (logical
//! types) by `scripts/extract-sync-schema.mjs`.
//!
//! Two guards close the loop:
//!   - the core descriptor [`synced_schema`] matches the vendored fixture (here), and
//!   - the `.github/workflows/schema-drift.yml` CI re-derives the fixture from
//!     surfc/main and fails if IT drifted — so a new synced column in surfc can't
//!     silently desync the native store (the `content_tag` case).
//!
//! Native-only: the store (rusqlite) is gated off wasm32.
#![cfg(not(target_arch = "wasm32"))]

use braird_core::store::synced_schema;
use serde_json::Value;
use std::collections::BTreeMap;

fn load_fixture() -> Value {
    let path = format!(
        "{}/vendored/schema/sync-schema.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

/// `{ table → { column → logical-type } }` from the core descriptor.
fn core_schema() -> BTreeMap<String, BTreeMap<String, String>> {
    synced_schema()
        .iter()
        .map(|t| {
            let cols = t
                .columns
                .iter()
                .map(|(name, ty)| (name.to_string(), ty.logical().to_string()))
                .collect();
            (t.name.to_string(), cols)
        })
        .collect()
}

/// The same shape from the vendored fixture.
fn fixture_schema() -> BTreeMap<String, BTreeMap<String, String>> {
    load_fixture()
        .as_object()
        .expect("fixture is an object")
        .iter()
        .map(|(table, cols)| {
            let cols = cols
                .as_object()
                .unwrap_or_else(|| panic!("{table} is not an object"))
                .iter()
                .map(|(c, ty)| {
                    (
                        c.clone(),
                        ty.as_str().expect("logical type is a string").to_string(),
                    )
                })
                .collect();
            (table.clone(), cols)
        })
        .collect()
}

#[test]
fn core_synced_schema_matches_vendored_fixture() {
    let core = core_schema();
    let fixture = fixture_schema();

    // Same set of synced tables.
    let core_tables: Vec<&String> = core.keys().collect();
    let fixture_tables: Vec<&String> = fixture.keys().collect();
    assert_eq!(
        core_tables, fixture_tables,
        "synced table set diverged from the vendored fixture"
    );

    // Same (column → logical-type) per table — the silent-desync guard: a column in the
    // fixture (= surfc's synced set) but missing/retyped in the core fails here.
    for (table, fixture_cols) in &fixture {
        let core_cols = core.get(table).expect("table present in core");
        assert_eq!(
            core_cols, fixture_cols,
            "column set / types for `{table}` diverged from the vendored fixture \
             (re-run scripts/extract-sync-schema.mjs and update src/store.rs)"
        );
    }
}
