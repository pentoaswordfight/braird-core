//! Native SQLite local store (SUR-723, Phase 2 of ADR 0001) — the on-device mirror of
//! surfc's synced cloud schema, for the iOS/Android clients. The PWA keeps using Dexie;
//! this is its native counterpart, so PWA↔native coexistence round-trips every synced row.
//!
//! Source of truth (founder, SUR-723 Gate-1 remediation):
//!   - the synced COLUMN SET is what `surfc/src/supabase.js` `upsert*` payloads carry (the
//!     authority — `fetchSince` does `select('*')`, so it pulls every column but does not
//!     enumerate the set; SUR-725 verified `user_id` is the only server-only column pull sees,
//!     and `apply_row` projects it — plus any future additive column — out);
//!   - logical TYPES come from the Supabase migrations;
//!   - both are captured in the vendored `vendored/schema/sync-schema.json` fixture, which
//!     [`synced_schema`] mirrors exactly and `tests/schema_parity.rs` reconciles against
//!     (CI re-derives the fixture from surfc/main via `scripts/extract-sync-schema.mjs`).
//!
//! `user_id` is auth-injected at push (the device's own user), never stored — exactly as
//! the Dexie local store omits it. The sync methods that read/write these tables landed in
//! SUR-724 (outbox flush) + SUR-725 (`get_row` / `apply_row` + the per-table pull cursor).

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value};

/// One `outbox` row read back by [`Store::outbox_items`]: `(id, table_name, record_id,
/// payload_json, created_at)`. Aliased so the 5-tuple stays readable at the call site (and
/// keeps clippy's `type_complexity` lint happy).
pub type OutboxRow = (i64, String, Option<String>, String, i64);

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

/// The descriptor for one synced table by name, or `None` if `name` is not a synced table.
/// The read/write helpers (`get_row` / `apply_row`) and the pull loop use this to stay
/// table-generic — SUR-726 fans out to the other six stores by extending the pull table list,
/// not by touching these helpers.
pub fn table_schema(name: &str) -> Option<&'static TableSchema> {
    synced_schema().iter().find(|t| t.name == name)
}

/// Descriptor lookup that fails loudly for a non-synced table (a caller bug, not a data error).
fn schema_or_err(table: &str) -> rusqlite::Result<&'static TableSchema> {
    table_schema(table).ok_or_else(|| {
        rusqlite::Error::InvalidParameterName(format!("unknown synced table: {table}"))
    })
}

/// Coerce one incoming JSON column value to the SQLite value for its declared [`ColType`].
/// Absent / null → SQL NULL. `Json` columns (`tags`, `source_meta`) are stored as their JSON
/// TEXT (≡ the cloud `jsonb`/`text[]`). Off-type values fall back to NULL rather than guessing.
fn json_to_sql(v: Option<&Value>, ty: ColType) -> SqlValue {
    match v {
        None | Some(Value::Null) => SqlValue::Null,
        Some(val) => match ty {
            ColType::Text => match val {
                Value::String(s) => SqlValue::Text(s.clone()),
                _ => SqlValue::Null,
            },
            ColType::Int => val
                .as_i64()
                .map(SqlValue::Integer)
                .unwrap_or(SqlValue::Null),
            ColType::Bool => match val {
                Value::Bool(b) => SqlValue::Integer(*b as i64),
                Value::Number(n) => SqlValue::Integer((n.as_f64().unwrap_or(0.0) != 0.0) as i64),
                _ => SqlValue::Null,
            },
            ColType::Real => val.as_f64().map(SqlValue::Real).unwrap_or(SqlValue::Null),
            ColType::Json => SqlValue::Text(val.to_string()),
        },
    }
}

/// Inverse of [`json_to_sql`]: a stored SQLite value back to JSON for its declared [`ColType`].
/// `Bool` reads 0/1 back as a JSON bool; `Json` re-parses the stored TEXT to its array/object.
fn sql_to_json(sv: SqlValue, ty: ColType) -> Value {
    match sv {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(i) => match ty {
            ColType::Bool => Value::Bool(i != 0),
            _ => Value::Number(i.into()),
        },
        SqlValue::Real(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        SqlValue::Text(s) => match ty {
            ColType::Json => serde_json::from_str(&s).unwrap_or(Value::Null),
            _ => Value::String(s),
        },
        SqlValue::Blob(_) => Value::Null, // no blob columns in the synced tables
    }
}

/// The 8 synced stores (parent SUR-659 §1), mirroring the vendored fixture exactly.
/// Every row carries `updated_at` (epoch bigint) + a `deleted` soft-delete flag.
/// `tests/schema_parity.rs` fails if this descriptor and the fixture diverge.
///
/// **Convergence contract (SUR-737, ratified).** Every table converges **whole-row last-write-wins
/// by `updated_at`** — a pull applies the newer row's columns wholesale (see `pull_table`), mirroring
/// the oracle's `mergeCloudRecords`. Pinned here, ahead of the SUR-726 fan-out, so the composite-column
/// semantics are a decision and not an accident:
///
/// | Table | Composite col(s) | Convergence | Why |
/// |---|---|---|---|
/// | `books` | — | whole-row LWW | scalar metadata; a null is authoritative (a cover-clear must converge) |
/// | `notes` | `tags`, `source_meta` | whole-row LWW | a tag edit IS a note edit; array *union* can't express a delete — an OR-set would be a wire change (future ticket only if product demands) |
/// | `custom_ideas` | — | whole-row LWW | scalar metadata |
/// | `note_links` | — (row-per-edge) | row-level LWW → set | add = insert a row, remove = tombstone it |
/// | `lenses` | `leaf_ids` | whole-row LWW | a lens is ONE authored query; unioning leaves under one combinator/threshold fabricates a query nobody wrote |
/// | `collections` | — | whole-row LWW | scalar metadata |
/// | `collection_memberships` | — (row-per-pair, deterministic `membershipId(collection, note)`) | row-level LWW → OR-set | concurrent adds of the same pair share a pk → converge to ONE row |
/// | `note_signals` | counters | whole-row LWW → **lossy, accepted** | concurrent increments lose one side; derived data, self-heals on the next signal |
///
/// The composite columns (`tags`, `source_meta`, `leaf_ids`) are stored + compared as opaque JSON
/// TEXT (`ColType::Json`); **no element-level merge happens or is intended.** Any change to that (e.g.
/// an OR-set for `tags`) is **wire-visible** and must land in the PWA (`mergeCloudRecords`) and here
/// in lockstep. Ratification pin tests live in `pull.rs` (`sur737_*`).
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
                ("tags", Json), // SUR-737: whole-row LWW; array union can't express a tag delete
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
                ("leaf_ids", Json), // cloud text[]; SUR-737 whole-row LWW (one authored query — no leaf union)
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
                // SUR-737: these counters converge whole-row LWW — concurrent increments on two
                // devices lose one side. Accepted: signals are derived, and self-heal on the next.
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

/// `CREATE INDEX IF NOT EXISTS` on `updated_at` for a synced table — the incremental-pull
/// read path (`fetchSince`'s `updated_at >= cursor`, SUR-725). Mirrors surfc's server-side
/// `*_updated_at_idx` indexes (migrations 0006/0034/0042/0043/0047). ponytail: the outbox
/// `(table, record_id)` collapse index is still not added — collapse reads the whole queue,
/// so no index earns its cost there yet.
fn create_updated_at_index_sql(t: &TableSchema) -> String {
    format!(
        "CREATE INDEX IF NOT EXISTS {0}_updated_at_idx ON {0}(updated_at);",
        t.name,
    )
}

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
            self.conn.execute_batch(&create_updated_at_index_sql(t))?;
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

    // ── outbox + meta helpers (SUR-724) ──────────────────────────────────────
    // The sync engine's read/write surface over the two local-only tables it drives.
    // Kept on `Store` (which owns the tables) rather than reaching into `conn` from the
    // sync module; the payload here is already-sealed JSON (ciphertext for note text).
    //
    // See the module-level `OutboxRow` alias for the read-back tuple shape.

    /// Enqueue one pending write. `payload_json` is the row's column values as a JSON
    /// object string; for notes its `text` is ALREADY enc:v2 ciphertext (seal-at-write).
    pub fn enqueue(
        &self,
        table_name: &str,
        record_id: &str,
        payload_json: &str,
        created_at: i64,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO outbox (table_name, record_id, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![table_name, record_id, payload_json, created_at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Every queued write, oldest first (see [`OutboxRow`]) — the sync module parses the
    /// payload JSON.
    pub fn outbox_items(&self) -> rusqlite::Result<Vec<OutboxRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, table_name, record_id, payload, created_at FROM outbox ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        rows.collect()
    }

    /// Clear the given outbox ids (a collapsed group that flushed successfully). Failed
    /// groups are simply NOT passed here, so they stay queued for the next flush.
    pub fn clear_outbox(&self, ids: &[i64]) -> rusqlite::Result<()> {
        // Small batch (one flush's worth); a per-id delete is fine and avoids array binding.
        for id in ids {
            self.conn
                .execute("DELETE FROM outbox WHERE id = ?1", [id])?;
        }
        Ok(())
    }

    /// Read a `meta` KV value (e.g. `bookIdRemap`).
    pub fn meta_get(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
    }

    /// Write a `meta` KV value (upsert).
    pub fn meta_set(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    // ── synced-table read/write + pull cursors (SUR-725) ──────────────────────
    // The inverse of the outbox path: `apply_row` merges a remote row INTO a synced table
    // (pull), and `get_row` reads one back (the pull LWW compare + host/test introspection).
    // Both are descriptor-driven, so they cover all 8 synced stores for the SUR-726 fan-out.

    /// Read one synced-table row by primary key as a JSON object (descriptor columns only,
    /// coerced back to JSON per [`ColType`]), or `None` if absent. The pull merge reads
    /// `updated_at` off this for its last-write-wins decision.
    pub fn get_row(&self, table: &str, id: &str) -> rusqlite::Result<Option<Map<String, Value>>> {
        let schema = schema_or_err(table)?;
        let col_list = schema
            .columns
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {col_list} FROM {} WHERE {} = ?1",
            schema.name, schema.pk[0]
        );
        self.conn
            .query_row(&sql, [id], |row| {
                let mut map = Map::new();
                for (i, (name, ty)) in schema.columns.iter().enumerate() {
                    let sv: SqlValue = row.get(i)?;
                    map.insert((*name).to_string(), sql_to_json(sv, *ty));
                }
                Ok(map)
            })
            .optional()
    }

    /// Upsert a remote row into a synced table (the pull sink). The row is **projected onto the
    /// descriptor's known columns** — `user_id` (the one server-only column on the wire) and any
    /// future additive server column are dropped, and `Json` columns are stored as TEXT. A stray
    /// unknown key would otherwise make the generated INSERT reference a non-existent column and
    /// fail the whole pull. `INSERT OR REPLACE` is a full-row replace (last-write-wins is decided
    /// by the caller before this runs), mirroring the JS `db.<table>.put({...})`.
    pub fn apply_row(&self, table: &str, row: &Map<String, Value>) -> rusqlite::Result<()> {
        let schema = schema_or_err(table)?;
        let cols: Vec<&str> = schema.columns.iter().map(|(n, _)| *n).collect();
        let values: Vec<SqlValue> = schema
            .columns
            .iter()
            .map(|(name, ty)| json_to_sql(row.get(*name), *ty))
            .collect();
        let placeholders = (1..=cols.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT OR REPLACE INTO {} ({}) VALUES ({placeholders})",
            schema.name,
            cols.join(", "),
        );
        self.conn
            .execute(&sql, rusqlite::params_from_iter(values))?;
        Ok(())
    }

    /// Apply a pulled LWW-winning row AND drop any now-stale pending outbox entries for the same
    /// record, in ONE transaction (SUR-736). Returns the dropped entries as `(outbox id, payload
    /// updated_at)` so the caller can surface them as conflicts (SUR-738).
    ///
    /// The bug this closes: a pull merges a strictly-newer remote row over a record that still has
    /// a queued local edit; without dropping that entry the next unconditional `flush` re-pushes the
    /// stale edit over the newer server row — a lost remote edit. Dropping it here, atomically with
    /// the apply, closes the window: `apply` without `drop` re-opens the bug, `drop` without `apply`
    /// loses the edit locally AND never pushes it, so both must commit or roll back together.
    ///
    /// Only entries whose payload `updated_at <= incoming_updated` are dropped: a pending edit NEWER
    /// than the row we just applied is a genuinely-later local write and must still flush. A payload
    /// that won't parse (or carries no `updated_at`) is LEFT queued — it can't be proven stale, and
    /// one that won't parse can never flush anyway, so it can't cause the 736 overwrite. The `<=` is
    /// defensively redundant today (`stage_local_write` stamps row + payload together, so a pending
    /// stamp is `<=` the local row ts, which is `<` incoming when the caller decided to apply) but it
    /// self-documents the criterion and protects future enqueue paths.
    pub fn apply_row_rebasing_outbox(
        &self,
        table: &str,
        row: &Map<String, Value>,
        incoming_updated: i64,
    ) -> rusqlite::Result<Vec<(i64, i64)>> {
        let pk = schema_or_err(table)?.pk[0];
        // The core write path always sets record_id; if an incoming row somehow lacks its pk we
        // still apply it, but there's nothing to match in the outbox (scan skipped).
        let record_id = row.get(pk).and_then(Value::as_str);

        // Same `unchecked_transaction` pattern as `stage_local_write`: `Store` is driven behind the
        // SyncEngine's `Mutex`, so no concurrent use of this connection; an early `?` drops the
        // Transaction and rolls back, only `commit()` persists the apply + the drops together.
        let tx = self.conn.unchecked_transaction()?;
        self.apply_row(table, row)?;

        let mut dropped: Vec<(i64, i64)> = Vec::new();
        if let Some(record_id) = record_id {
            // Collect the record's queued entries first (releasing the statement borrow) so the
            // DELETEs below can run on the same connection.
            let entries: Vec<(i64, String)> = {
                let mut stmt = self.conn.prepare(
                    "SELECT id, payload FROM outbox WHERE table_name = ?1 AND record_id = ?2",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![table, record_id], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            for (id, payload) in entries {
                // A stale entry is one we can parse AND whose own stamp is `<=` what we just applied.
                let stamp = serde_json::from_str::<Value>(&payload)
                    .ok()
                    .and_then(|v| v.get("updated_at").and_then(Value::as_i64));
                if let Some(stamp) = stamp {
                    if stamp <= incoming_updated {
                        self.conn
                            .execute("DELETE FROM outbox WHERE id = ?1", [id])?;
                        dropped.push((id, stamp));
                    }
                }
            }
        }
        tx.commit()?;
        Ok(dropped)
    }

    /// Atomically stage a local write: merge the partial edit onto any existing row, upsert the
    /// merged row into the synced table, AND enqueue the outbox payload — all in ONE transaction
    /// (SUR-725 review). If any step fails (e.g. an I/O / disk-full / `SQLITE_BUSY` error mid-write)
    /// the whole thing rolls back, so the store can never end up with a locally-visible edit that
    /// has no queued outbox row (which would silently never flush yet still win an LWW compare).
    ///
    /// The synced row is the MERGED row (a partial edit can't null pulled-only columns like a book
    /// cover); the outbox payload is the PARTIAL row as supplied (the server upsert `merge-duplicates`
    /// patches only the changed columns — sending the merged full row could clobber a newer field).
    pub fn stage_local_write(
        &self,
        table: &str,
        record_id: &str,
        partial: Map<String, Value>,
        created_at: i64,
    ) -> rusqlite::Result<()> {
        // `unchecked_transaction` is safe here: `Store` is driven behind the SyncEngine's `Mutex`,
        // so there is no concurrent use of this connection. On any early `?` the `Transaction` drops
        // and rolls back (its default drop behaviour); only `commit()` persists the pair.
        let tx = self.conn.unchecked_transaction()?;
        let mut merged = self.get_row(table, record_id)?.unwrap_or_default();
        for (k, v) in &partial {
            merged.insert(k.clone(), v.clone());
        }
        self.apply_row(table, &merged)?;
        let payload = Value::Object(partial).to_string();
        self.enqueue(table, record_id, &payload, created_at)?;
        tx.commit()
    }

    /// The per-table incremental-pull cursor (epoch-ms watermark), or `None` on the first pull.
    /// Local-only (in `meta`, keyed `sync:cursor:<table>`) — an intentional divergence from the
    /// PWA's single global `meta.lastSyncAt` (founder, SUR-659): each table advances independently
    /// so one table's fetch failure never skips another's changes.
    pub fn get_sync_cursor(&self, table: &str) -> rusqlite::Result<Option<i64>> {
        Ok(self
            .meta_get(&sync_cursor_key(table))?
            .and_then(|s| s.parse::<i64>().ok()))
    }

    /// Advance a per-table pull cursor (called only after that table's merge succeeds).
    pub fn set_sync_cursor(&self, table: &str, cursor: i64) -> rusqlite::Result<()> {
        self.meta_set(&sync_cursor_key(table), &cursor.to_string())
    }
}

/// The `meta` key holding a table's incremental-pull cursor.
fn sync_cursor_key(table: &str) -> String {
    format!("sync:cursor:{table}")
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

    // ── SUR-724 outbox + meta helpers (fast-gate coverage, no network) ────────

    #[test]
    fn outbox_enqueue_read_and_clear_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        // Enqueue newest-first; outbox_items must return oldest-first by created_at.
        let id_new = store
            .enqueue("notes", "n1", r#"{"id":"n1","text":"enc:v2:b"}"#, 200)
            .unwrap();
        let id_old = store
            .enqueue("notes", "n0", r#"{"id":"n0","text":"enc:v2:a"}"#, 100)
            .unwrap();
        let items = store.outbox_items().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0, id_old, "oldest created_at first");
        assert_eq!(items[1].0, id_new);
        // clear_outbox removes only the named ids, leaving the rest queued.
        store.clear_outbox(&[id_old]).unwrap();
        let left = store.outbox_items().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].0, id_new);
    }

    #[test]
    fn meta_set_get_roundtrip_and_upsert() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.meta_get("bookIdRemap").unwrap(), None);
        store.meta_set("bookIdRemap", r#"{"a":"b"}"#).unwrap();
        // Set twice — the ON CONFLICT upsert replaces the value, not inserts a duplicate.
        store
            .meta_set("bookIdRemap", r#"{"a":"server-1"}"#)
            .unwrap();
        assert_eq!(
            store.meta_get("bookIdRemap").unwrap().as_deref(),
            Some(r#"{"a":"server-1"}"#)
        );
    }

    // ── SUR-725 synced-table read/write + pull cursors ────────────────────────

    #[test]
    fn apply_row_then_get_row_roundtrips_all_coltypes() {
        use serde_json::json;
        let store = Store::open_in_memory().unwrap();
        // A notes row exercising every ColType: Text, Json (tags array), Bool (deleted), Int.
        let row = json!({
            "id": "n1",
            "book_id": "b1",
            "text": "enc:v2:cipher",
            "page": "12",
            "tags": ["philosophy", "ethics"],
            "source_meta": { "author": "Plato" },
            "content_tag": "abc123",
            "created_at": 1_700_000_000_000_i64,
            "updated_at": 1_700_000_000_500_i64,
            "deleted": false
        });
        store.apply_row("notes", row.as_object().unwrap()).unwrap();

        let got = store.get_row("notes", "n1").unwrap().expect("row present");
        assert_eq!(got["text"], json!("enc:v2:cipher"));
        assert_eq!(got["tags"], json!(["philosophy", "ethics"]), "Json → array");
        assert_eq!(got["source_meta"], json!({ "author": "Plato" }));
        assert_eq!(got["updated_at"], json!(1_700_000_000_500_i64), "Int");
        assert_eq!(got["deleted"], json!(false), "Bool 0/1 → JSON bool");
        // A column absent from the incoming row lands as null (additive-nullable).
        assert_eq!(got["image_path"], Value::Null);
    }

    #[test]
    fn apply_row_projects_out_user_id_and_unknown_columns() {
        use serde_json::json;
        let store = Store::open_in_memory().unwrap();
        // `select('*')` returns `user_id` (the one server-only column) — apply_row must drop it
        // (and any future additive server column) rather than fail on an unknown column.
        let row = json!({
            "id": "b1",
            "user_id": "00000000-0000-0000-0000-000000000000",
            "title": "Apology",
            "author": "Plato",
            "created_at": 1_i64,
            "updated_at": 2_i64,
            "deleted": false,
            "some_future_server_column": "ignored"
        });
        store.apply_row("books", row.as_object().unwrap()).unwrap();
        let got = store.get_row("books", "b1").unwrap().expect("row present");
        assert_eq!(got["title"], json!("Apology"));
        assert!(!got.contains_key("user_id"), "user_id not stored locally");
        assert!(!got.contains_key("some_future_server_column"));
    }

    #[test]
    fn apply_row_is_a_full_row_replace_on_conflict() {
        use serde_json::json;
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row(
                "books",
                json!({ "id": "b1", "title": "Old", "updated_at": 1_i64, "deleted": false })
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        store
            .apply_row(
                "books",
                json!({ "id": "b1", "title": "New", "updated_at": 2_i64, "deleted": false })
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        let got = store.get_row("books", "b1").unwrap().unwrap();
        assert_eq!(got["title"], json!("New"));
        assert_eq!(got["updated_at"], json!(2_i64));
    }

    #[test]
    fn get_row_absent_is_none() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.get_row("notes", "nope").unwrap().is_none());
    }

    #[test]
    fn stage_local_write_rolls_back_when_the_outbox_insert_fails() {
        use serde_json::json;
        let store = Store::open_in_memory().unwrap();
        // Force the outbox INSERT to fail (drop the table) so we can prove the synced-row apply is
        // rolled back with it — no locally-visible edit that would never flush (SUR-725 review).
        store.conn.execute_batch("DROP TABLE outbox").unwrap();
        let partial = json!({ "id": "b1", "title": "T", "updated_at": 1, "deleted": false });
        let res = store.stage_local_write("books", "b1", partial.as_object().unwrap().clone(), 100);
        assert!(
            res.is_err(),
            "the outbox insert must fail with the table dropped"
        );
        assert!(
            store.get_row("books", "b1").unwrap().is_none(),
            "apply_row must roll back when the outbox enqueue fails (atomic stage)"
        );
    }

    #[test]
    fn sync_cursor_defaults_none_then_roundtrips_per_table() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.get_sync_cursor("notes").unwrap(), None);
        store.set_sync_cursor("notes", 1_700_000_000_000).unwrap();
        assert_eq!(
            store.get_sync_cursor("notes").unwrap(),
            Some(1_700_000_000_000)
        );
        // Per-table isolation: advancing notes must not touch the books cursor.
        assert_eq!(store.get_sync_cursor("books").unwrap(), None);
    }

    // ── SUR-736 outbox rebase on an LWW win ───────────────────────────────────

    #[test]
    fn rebase_applies_row_and_drops_stale_outbox_entries() {
        use serde_json::json;
        let store = Store::open_in_memory().unwrap();
        // A queued local edit + its local synced row, both stamped T1.
        store
            .apply_row(
                "notes",
                json!({"id":"n1","text":"enc:v2:local","updated_at":1000,"deleted":false})
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        let oid = store
            .enqueue("notes", "n1", r#"{"id":"n1","updated_at":1000}"#, 1000)
            .unwrap();

        // Pull a strictly-newer remote row (T2 > T1) — the caller already decided to apply.
        let remote = json!({"id":"n1","text":"enc:v2:remote","updated_at":2000,"deleted":false});
        let dropped = store
            .apply_row_rebasing_outbox("notes", remote.as_object().unwrap(), 2000)
            .unwrap();

        assert_eq!(
            dropped,
            vec![(oid, 1000)],
            "the stale entry is reported dropped"
        );
        assert!(
            store.outbox_items().unwrap().is_empty(),
            "the stale outbox entry is gone — the next flush can't re-push it (SUR-736)"
        );
        assert_eq!(
            store.get_row("notes", "n1").unwrap().unwrap()["text"],
            json!("enc:v2:remote"),
            "the remote LWW winner is applied in the same transaction"
        );
    }

    #[test]
    fn rebase_keeps_an_outbox_entry_newer_than_the_incoming_row() {
        use serde_json::json;
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row(
                "notes",
                json!({"id":"n1","updated_at":1000,"deleted":false})
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        // A genuinely-later local write (T3) must still flush — it is NOT stale vs the T2 we apply.
        store
            .enqueue("notes", "n1", r#"{"id":"n1","updated_at":3000}"#, 3000)
            .unwrap();

        let remote = json!({"id":"n1","updated_at":2000,"deleted":false});
        let dropped = store
            .apply_row_rebasing_outbox("notes", remote.as_object().unwrap(), 2000)
            .unwrap();

        assert!(dropped.is_empty(), "an entry newer than incoming survives");
        assert_eq!(store.outbox_items().unwrap().len(), 1);
    }

    #[test]
    fn rebase_drops_an_entry_stamped_exactly_at_the_incoming_ts() {
        use serde_json::json;
        let store = Store::open_in_memory().unwrap();
        // Synthetic desync: local row below incoming, but a queued payload stamped == incoming.
        // Proves the guard is `<=`, not `<`.
        store
            .apply_row(
                "notes",
                json!({"id":"n1","updated_at":1000,"deleted":false})
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        let oid = store
            .enqueue("notes", "n1", r#"{"id":"n1","updated_at":2000}"#, 2000)
            .unwrap();

        let remote = json!({"id":"n1","updated_at":2000,"deleted":false});
        let dropped = store
            .apply_row_rebasing_outbox("notes", remote.as_object().unwrap(), 2000)
            .unwrap();

        assert_eq!(
            dropped,
            vec![(oid, 2000)],
            "payload ts == incoming is dropped (<=)"
        );
    }

    #[test]
    fn rebase_leaves_a_malformed_outbox_payload_queued() {
        use serde_json::json;
        let store = Store::open_in_memory().unwrap();
        store
            .apply_row(
                "notes",
                json!({"id":"n1","updated_at":1000,"deleted":false})
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        // A payload that won't parse can't be proven stale — and can never flush anyway.
        store
            .enqueue("notes", "n1", "not valid json", 1000)
            .unwrap();

        let remote = json!({"id":"n1","text":"enc:v2:remote","updated_at":2000,"deleted":false});
        let dropped = store
            .apply_row_rebasing_outbox("notes", remote.as_object().unwrap(), 2000)
            .unwrap();

        assert!(dropped.is_empty(), "malformed payload is not dropped");
        assert_eq!(store.outbox_items().unwrap().len(), 1, "still queued");
        assert_eq!(
            store.get_row("notes", "n1").unwrap().unwrap()["text"],
            json!("enc:v2:remote"),
            "the apply still happened (rebase never blocks the LWW winner)"
        );
    }

    #[test]
    fn synced_tables_have_updated_at_index() {
        let store = Store::open_in_memory().unwrap();
        for t in synced_schema() {
            let idx = format!("{}_updated_at_idx", t.name);
            let n: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='index' AND name=?1",
                    [&idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing {idx}");
        }
    }
}
