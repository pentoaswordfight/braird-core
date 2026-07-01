#!/usr/bin/env node
// extract-sync-schema — derive the canonical synced-column schema for the native
// SQLite mirror, from surfc's source of truth. This is BOTH the generator for the
// vendored `vendored/schema/sync-schema.json` AND the oracle the `schema-drift`
// CI re-runs against surfc/main to detect drift (SUR-723 §7).
//
// Three-way authority (founder, SUR-723 Gate-1 remediation):
//   - synced COLUMN SET  ← supabase.js `upsert*` payload keys ("what round-trips").
//   - logical TYPES      ← supabase migrations DDL (the server schema).
//   - db.js              ← index hint only; NOT a column source (it declares only
//                          Dexie indexes, never the full column set — using it would
//                          sail right past silent desync, the `content_tag` case).
//
// `user_id` is auth-injected at push (the device's own user; the Dexie local store
// never holds it), so it is dropped — the mirror tracks data columns, like Dexie.
//
// Usage: node scripts/extract-sync-schema.mjs <surfc-root> [--check <fixture.json>]
//   no --check → prints canonical JSON to stdout (regenerate the fixture).
//   --check    → diffs the freshly-extracted schema against <fixture.json>; exit 1 on drift.
//
// ponytail: regex extraction of flat upsert object-literals + simple migration DDL.
// The upsert payloads are flat and the migrations use one-column-per-line DDL, so a
// parser is overkill. If surfc ever nests an upsert payload or inlines multi-column
// DDL, tighten here — the schema_parity.rs test and this check both fail loudly first.

import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';

// The 8 synced cloud tables (parent SUR-659 §1). Everything else in surfc is
// local-only or server-only and is NOT mirrored as a synced store.
const SYNCED_TABLES = [
  'books',
  'notes',
  'custom_ideas',
  'note_links',
  'lenses',
  'collections',
  'collection_memberships',
  'note_signals',
];

// pg type → the core's logical-type vocabulary (text | int | bool | real | json).
// This is the ONE normalization map (founder: "compare (name, logical-type) tuples,
// not physical reps"). jsonb and text[] both collapse to `json` (stored TEXT-JSON in
// SQLite ≡ cloud jsonb/text[]); every integer width collapses to `int`; boolean → bool.
function logicalType(pgType) {
  const t = pgType.toLowerCase().replace(/\s+/g, ' ').trim();
  if (t === 'text' || t === 'uuid') return 'text';
  if (t === 'bigint' || t === 'int8' || t === 'integer' || t === 'int' || t === 'int4' || t === 'smallint' || t === 'int2')
    return 'int';
  if (t === 'boolean' || t === 'bool') return 'bool';
  if (t === 'real' || t === 'double precision' || t === 'float4' || t === 'float8') return 'real';
  if (t === 'jsonb' || t === 'json' || t.endsWith('[]')) return 'json';
  if (t.startsWith('timestamp')) return 'int'; // epoch bigint convention; none today
  throw new Error(`unmapped pg type: "${pgType}"`);
}

// --- synced column SET, from supabase.js upsert* payloads -------------------------
function extractSyncedColumns(supabaseJs) {
  const perTable = {};
  for (const table of SYNCED_TABLES) {
    // `.from('<table>').upsert({ <flat literal> })` — non-greedy to the first `})`.
    const re = new RegExp(`\\.from\\(['"]${table}['"]\\)\\s*\\.upsert\\(\\{([\\s\\S]*?)\\}\\)`);
    const m = supabaseJs.match(re);
    if (!m) throw new Error(`no upsert payload found for table "${table}" in supabase.js`);
    const keys = [...m[1].matchAll(/(\w+)\s*:/g)].map((k) => k[1]);
    // Drop user_id (auth-injected at push, never stored — like the Dexie local store).
    perTable[table] = keys.filter((k) => k !== 'user_id');
  }
  return perTable;
}

// --- logical TYPES, from migration DDL --------------------------------------------
const CONSTRAINT_KEYWORDS = new Set([
  'primary', 'unique', 'check', 'foreign', 'constraint', 'references', 'create',
]);

function extractColumnTypes(migrationsText) {
  // table → { column → pgType }. Last definition wins (later ALTERs / re-creates).
  const types = {};
  const ensure = (t) => (types[t] ??= {});

  // CREATE TABLE bodies via a paren-depth line scanner — robust where a single
  // non-greedy regex over the whole concatenated migration text is not (an earlier
  // non-synced table's closing paren can mis-pair and swallow a later table).
  const createHead = /^\s*create\s+table\s+(?:if\s+not\s+exists\s+)?(?:public\.)?(\w+)\s*\(/i;
  const lines = migrationsText.split('\n');
  let table = null; // non-null while inside a CREATE TABLE body ('' = a non-synced one)
  let depth = 0;
  for (const raw of lines) {
    const opens = (raw.match(/\(/g) || []).length;
    const closes = (raw.match(/\)/g) || []).length;
    if (table === null) {
      const head = raw.match(createHead);
      if (!head) continue;
      depth = opens - closes;
      table = SYNCED_TABLES.includes(head[1]) ? head[1] : '';
      continue;
    }
    depth += opens - closes;
    if (table !== '') {
      const line = raw.trim().replace(/,$/, '');
      const tok = line.split(/\s+/);
      const col = tok[0]?.toLowerCase();
      if (col && !CONSTRAINT_KEYWORDS.has(col) && !col.startsWith(')')) {
        const pgType = tok[1] === 'double' ? 'double precision' : tok[1];
        if (pgType) ensure(table)[col] = pgType;
      }
    }
    if (depth <= 0) table = null; // block closed
  }

  // ALTER TABLE [public.]<table> ADD COLUMN [if not exists] <col> <type> ...
  const alterRe =
    /alter\s+table\s+(?:public\.)?(\w+)\s+add\s+column\s+(?:if\s+not\s+exists\s+)?(\w+)\s+([\w]+(?:\s*\[\])?(?:\s+precision)?)/gi;
  for (const m of migrationsText.matchAll(alterRe)) {
    const t = m[1];
    if (!SYNCED_TABLES.includes(t)) continue;
    ensure(t)[m[2].toLowerCase()] = m[3].trim();
  }
  return types;
}

function buildSchema(surfcRoot) {
  const supabaseJs = readFileSync(join(surfcRoot, 'src', 'supabase.js'), 'utf8');
  const migDir = join(surfcRoot, 'supabase', 'migrations');
  const migrationsText = readdirSync(migDir)
    .filter((f) => f.endsWith('.sql'))
    .sort()
    .map((f) => readFileSync(join(migDir, f), 'utf8'))
    .join('\n');

  const syncedCols = extractSyncedColumns(supabaseJs);
  const colTypes = extractColumnTypes(migrationsText);

  const tables = {};
  for (const table of SYNCED_TABLES) {
    const cols = {};
    for (const col of syncedCols[table]) {
      const pg = colTypes[table]?.[col];
      if (!pg) throw new Error(`column "${table}.${col}" is synced (upsert) but absent from the migrations`);
      cols[col] = logicalType(pg);
    }
    tables[table] = cols;
  }
  // Canonical: tables in SYNCED_TABLES order; columns sorted for stable diffs.
  const out = {};
  for (const t of SYNCED_TABLES) {
    out[t] = Object.fromEntries(Object.keys(tables[t]).sort().map((c) => [c, tables[t][c]]));
  }
  return out;
}

// --- CLI --------------------------------------------------------------------------
const [, , surfcRoot, checkFlag, fixturePath] = process.argv;
if (!surfcRoot) {
  console.error('usage: extract-sync-schema.mjs <surfc-root> [--check <fixture.json>]');
  process.exit(2);
}
const schema = buildSchema(surfcRoot);
const canonical = JSON.stringify(schema, null, 2) + '\n';

if (checkFlag === '--check') {
  const want = readFileSync(fixturePath, 'utf8');
  // Re-canonicalise the fixture so whitespace can't cause a false diff.
  const wantCanonical = JSON.stringify(JSON.parse(want), null, 2) + '\n';
  if (canonical !== wantCanonical) {
    console.error('::error::sync-schema.json has drifted from surfc/main (supabase.js upsert* / migrations).');
    console.error('A synced column was added, removed, or retyped without re-vendoring the mirror.');
    console.error('Re-run: node scripts/extract-sync-schema.mjs <surfc-root> > vendored/schema/sync-schema.json');
    process.exit(1);
  }
  console.log('sync-schema.json is in sync with surfc/main.');
} else {
  process.stdout.write(canonical);
}
