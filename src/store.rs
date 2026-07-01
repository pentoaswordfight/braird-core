//! Native SQLite local store (SUR-723, Phase 2 of ADR 0001) — the on-device mirror of
//! surfc's synced cloud schema, for the iOS/Android clients. The PWA keeps using Dexie;
//! this is its native counterpart, so PWA↔native coexistence round-trips every synced row.
//!
//! Source of truth (founder, SUR-723 Gate-1 remediation):
//!   - the synced COLUMN SET is what `surfc/src/supabase.js` `upsert*` payloads carry (the
//!     authority — `fetchSince` does `select('*')`, so it pulls every column but does not
//!     enumerate the set; a future server-stamped pull-only column is a SUR-725 concern);
//!   - logical TYPES come from the Supabase migrations;
//!   - both are captured in the vendored `vendored/schema/sync-schema.json` fixture, which
//!     [`synced_schema`] mirrors exactly and `tests/schema_parity.rs` reconciles against
//!     (CI re-derives the fixture from surfc/main via `scripts/extract-sync-schema.mjs`).
//!
//! `user_id` is auth-injected at push (the device's own user), never stored — exactly as
//! the Dexie local store omits it. The sync methods (outbox flush, pull) that read/write
//! these tables arrive in SUR-724/725; this slice lands the schema + the drift guard only.

use rusqlite::Connection;

/// The core's logical column-type vocabulary — the canonical axis the drift guard
/// compares on (a pg `jsonb` and a `text[]` both round-trip as `Json`; every integer
/// width is `Int`; `boolean` is `Bool`). One normalization map, shared with the
/// `extract-sync-schema.mjs` generator so the fixture and this descriptor agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    Text,
    Int,
    Bool,
    Real,
    Json,
}

impl ColType {
    /// SQLite column affinity. `Bool` stores 0/1 in an INTEGER; `Json` stores the
    /// JSON text in a TEXT column (≡ cloud `jsonb`/`text[]`).
    pub fn sqlite(self) -> &'static str {
        match self {
            ColType::Text | ColType::Json => "TEXT",
            ColType::Int | ColType::Bool => "INTEGER",
            ColType::Real => "REAL",
        }
    }

    /// The logical-type token used in the vendored fixture (`text`/`int`/`bool`/`real`/`json`).
    pub fn logical(self) -> &'static str {
        match self {
            ColType::Text => "text",
            ColType::Int => "int",
            ColType::Bool => "bool",
            ColType::Real => "real",
            ColType::Json => "json",
        }
    }
}

/// A synced table's shape: its name, primary-key column(s), and ordered `(column, type)` set.
pub struct TableSchema {
    pub name: &'static str,
    /// Primary key. Hand-maintained and verified against the migrations; NOT covered by the
    /// drift guard (the fixture carries columns/types only). A surfc PK change won't be
    /// flagged here — acceptable, since a PK change on these tables is a breaking cloud
    /// migration. Revisit (encode PK in the fixture) if that assumption ever weakens.
    pub pk: &'static [&'static str],
    pub columns: &'static [(&'static str, ColType)],
}

use ColType::{Bool, Int, Json, Real, Text};

/// The 8 synced stores (parent SUR-659 §1), mirroring the vendored fixture exactly.
/// Every row carries `updated_at` (epoch bigint) + a `deleted` soft-delete flag.
/// `tests/schema_parity.rs` fails if this descriptor and the fixture diverge.
pub fn synced_schema() -> &'static [TableSchema] {
    &[
        TableSchema {
            name: "books",
            pk: &["id"],
            columns: &[
                ("id", Text),
                ("title", Text),
                ("author", Text),
                ("isbn", Text),
                ("cover_url", Text),
                ("cover_source", Text),
                ("cover_resolved_at", Int),
                ("created_at", Int),
                ("updated_at", Int),
                ("deleted", Bool),
            ],
        },
        TableSchema {
            name: "notes",
            pk: &["id"],
            columns: &[
                ("id", Text),
                ("book_id", Text),
                ("text", Text), // ciphertext (enc:v1/enc:v2) for encrypted users
                ("page", Text),
                ("tags", Json),
                ("image_path", Text),
                ("ink_crop_path", Text),
                ("source", Text),
                ("source_id", Text),
                ("source_meta", Json),
                ("chapter", Text),
                ("content_tag", Text), // SUR-638 synced HMAC fingerprint (plaintext-opaque)
                ("created_at", Int),
                ("updated_at", Int),
                ("deleted", Bool),
            ],
        },
        TableSchema {
            name: "custom_ideas",
            pk: &["id"],
            columns: &[
                ("id", Text),
                ("name", Text),
                ("description", Text),
                ("created_at", Int),
                ("updated_at", Int),
                ("deleted", Bool),
            ],
        },
        TableSchema {
            name: "note_links",
            pk: &["id"],
            columns: &[
                ("id", Text),
                ("from_note_id", Text),
                ("to_note_id", Text),
                ("relation_type", Text),
                ("created_at", Int),
                ("updated_at", Int),
                ("deleted", Bool),
            ],
        },
        TableSchema {
            name: "lenses",
            pk: &["id"],
            columns: &[
                ("id", Text),
                ("name", Text),
                ("leaf_ids", Json), // cloud text[]
                ("combinator", Text),
                ("threshold", Int), // cloud smallint
                ("created_at", Int),
                ("updated_at", Int),
                ("deleted", Bool),
            ],
        },
        TableSchema {
            name: "collections",
            pk: &["id"],
            columns: &[
                ("id", Text),
                ("name", Text),
                ("created_at", Int),
                ("updated_at", Int),
                ("deleted", Bool),
            ],
        },
        TableSchema {
            name: "collection_memberships",
            pk: &["id"],
            columns: &[
                ("id", Text),
                ("note_id", Text),
                ("collection_id", Text),
                ("created_at", Int),
                ("updated_at", Int),
                ("deleted", Bool),
            ],
        },
        TableSchema {
            name: "note_signals",
            pk: &["note_id"],
            columns: &[
                ("note_id", Text),
                ("source_prior", Real),
                ("return_visits", Int),
                ("has_annotation", Bool),
                ("stitch_spawns", Int),
                ("exposure_recency_at", Int),
                ("engagement_recency_at", Int),
                ("importance", Real),
                ("created_at", Int),
                ("updated_at", Int),
                ("deleted", Bool),
            ],
        },
    ]
}

/// The local-only / derived stores (parent SUR-659 §1) — present in the mirror but
/// **never synced** and **exempt from the drift guard**: `meta` is the config + the
/// per-table sync cursors (a KV store), `outbox` the pending-write queue keyed
/// `(table, record_id)`, `embeddings` the device-local sealed search vectors, and
/// `discovery_jobs` the local job queue. Raw DDL — they have no cloud counterpart.
const LOCAL_ONLY_DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);",
    "CREATE TABLE IF NOT EXISTS outbox (\
        id INTEGER PRIMARY KEY AUTOINCREMENT, \
        table_name TEXT NOT NULL, \
        record_id TEXT, \
        payload TEXT NOT NULL, \
        created_at INTEGER NOT NULL);",
    "CREATE TABLE IF NOT EXISTS embeddings (\
        note_id TEXT PRIMARY KEY, \
        model_version TEXT, \
        encrypted_vector BLOB, \
        updated_at INTEGER, \
        deleted INTEGER);",
    "CREATE TABLE IF NOT EXISTS discovery_jobs (\
        id TEXT PRIMARY KEY, \
        status TEXT, \
        payload TEXT, \
        created_at INTEGER);",
];

/// `CREATE TABLE IF NOT EXISTS` for a synced table, generated from its descriptor.
fn create_table_sql(t: &TableSchema) -> String {
    let cols: Vec<String> = t
        .columns
        .iter()
        .map(|(name, ty)| format!("{name} {}", ty.sqlite()))
        .collect();
    format!(
        "CREATE TABLE IF NOT EXISTS {} ({}, PRIMARY KEY ({}));",
        t.name,
        cols.join(", "),
        t.pk.join(", "),
    )
}

// ponytail: no secondary indexes here — none are queried in this slice. The
// `updated_at` (pull) and outbox `(table, record_id)` (collapse) indexes land with the
// queries that use them in SUR-724/725, where their cost is justified by a read path.

/// The on-device SQLite store: the 8 synced tables + the 4 local-only stores.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (or create) the store at `path` and ensure the schema exists.
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        Self::from_conn(Connection::open(path)?)
    }

    /// An in-memory store — used by the schema-parity test and as a host scratch DB.
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> rusqlite::Result<Self> {
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    /// Idempotently create every table (synced + local-only). `IF NOT EXISTS`, so
    /// re-opening an existing store is a no-op.
    fn init_schema(&self) -> rusqlite::Result<()> {
        for t in synced_schema() {
            self.conn.execute_batch(&create_table_sql(t))?;
        }
        for ddl in LOCAL_ONLY_DDL {
            self.conn.execute_batch(ddl)?;
        }
        Ok(())
    }

    /// Whether a table exists in the store (for tests / host introspection).
    pub fn table_exists(&self, name: &str) -> rusqlite::Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    /// The actual `(column_name → declared SQLite affinity)` of a created table, read
    /// back via `PRAGMA table_info`. Proves the generated DDL matches the descriptor.
    pub fn table_columns(&self, name: &str) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({name})"))?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_and_creates_every_table() {
        let store = Store::open_in_memory().unwrap();
        for t in synced_schema() {
            assert!(
                store.table_exists(t.name).unwrap(),
                "missing synced table {}",
                t.name
            );
        }
        for name in ["meta", "outbox", "embeddings", "discovery_jobs"] {
            assert!(
                store.table_exists(name).unwrap(),
                "missing local-only table {name}"
            );
        }
    }

    #[test]
    fn ddl_columns_match_the_descriptor() {
        let store = Store::open_in_memory().unwrap();
        for t in synced_schema() {
            let actual: Vec<String> = store
                .table_columns(t.name)
                .unwrap()
                .into_iter()
                .map(|(n, _)| n)
                .collect();
            let expected: Vec<String> = t.columns.iter().map(|(n, _)| n.to_string()).collect();
            assert_eq!(actual, expected, "column set mismatch for {}", t.name);
        }
    }

    #[test]
    fn every_synced_table_has_updated_at_and_deleted() {
        for t in synced_schema() {
            let names: Vec<&str> = t.columns.iter().map(|(n, _)| *n).collect();
            assert!(names.contains(&"updated_at"), "{} lacks updated_at", t.name);
            assert!(names.contains(&"deleted"), "{} lacks deleted", t.name);
        }
    }
}
