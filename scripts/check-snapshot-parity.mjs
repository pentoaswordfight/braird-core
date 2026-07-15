#!/usr/bin/env node
// Validate SUR-911's frozen snapshot fixtures against a clean origin/main object from the
// supplied surfc checkout.
//
// Usage: node scripts/check-snapshot-parity.mjs <surfc-root> [--no-fetch]
//        SURFC_ROOT=<surfc-root> node scripts/check-snapshot-parity.mjs [--no-fetch]
//
// The default refreshes origin/main. `--no-fetch` exists only for an explicitly offline
// rerun; either way, source modules are materialized with `git show`, never read from the
// surfc worktree. Precondition: the supplied checkout has its npm dependencies installed,
// including Dexie and fake-indexeddb. The only worktree path used is node_modules for those
// already-installed runtime dependencies.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const FIXTURE_DIR = join(REPO_ROOT, 'vendored', 'snapshot-parity');
const MANIFEST = JSON.parse(readFileSync(join(FIXTURE_DIR, 'manifest.json'), 'utf8'));
const EXPECTED = JSON.parse(readFileSync(join(FIXTURE_DIR, MANIFEST.expectations), 'utf8'));

const CLEAN_MODULE_FILES = [
  'package.json',
  'src/db.js',
  'src/ideaNormalize.js',
  'src/constants.js',
  'src/utils.js',
  'src/scoring.js',
  'src/crypto/noteEncryption.js',
  'src/lib/userAnnotations.js',
];

const ARRAY_TO_TABLE = {
  books: 'books',
  notes: 'notes',
  customIdeas: 'customIdeas',
  noteLinks: 'note_links',
  lenses: 'lenses',
  collections: 'collections',
  collectionMemberships: 'collection_memberships',
  noteSignals: 'note_signals',
};

const CORE_EXPORT_KEYS = {
  books: new Set([
    'id',
    'title',
    'author',
    'isbn',
    'coverUrl',
    'coverSource',
    'coverResolvedAt',
    'createdAt',
    'updatedAt',
    'deleted',
  ]),
  notes: new Set([
    'id',
    'bookId',
    'text',
    'page',
    'tags',
    'imagePath',
    'inkCropPath',
    'source',
    'sourceId',
    'sourceMeta',
    'chapter',
    'contentTag',
    'createdAt',
    'updatedAt',
    'deleted',
    'user_metadata',
  ]),
  customIdeas: new Set(['id', 'name', 'description', 'createdAt', 'updatedAt', 'deleted']),
  noteLinks: new Set([
    'id',
    'fromNoteId',
    'toNoteId',
    'relationType',
    'createdAt',
    'updatedAt',
    'deleted',
  ]),
  lenses: new Set([
    'id',
    'name',
    'leafIds',
    'combinator',
    'threshold',
    'createdAt',
    'updatedAt',
    'deleted',
  ]),
  collections: new Set(['id', 'name', 'createdAt', 'updatedAt', 'deleted']),
  collectionMemberships: new Set([
    'id',
    'noteId',
    'collectionId',
    'createdAt',
    'updatedAt',
    'deleted',
  ]),
  noteSignals: new Set([
    'noteId',
    'sourcePrior',
    'returnVisits',
    'hasAnnotation',
    'stitchSpawns',
    'exposureRecencyAt',
    'engagementRecencyAt',
    'importance',
    'createdAt',
    'updatedAt',
    'deleted',
  ]),
};

const NATIVE_IMPORT_NOTE_KEYS = new Set(
  [...CORE_EXPORT_KEYS.notes].filter((key) => key !== 'user_metadata')
);

const V11_THEN_V14 = [
  ['Good', 'Morality'],
  ['Custom', 'Institutions'],
  ['Pleasure', 'Emotion'],
  ['Virtue', 'Virtue'],
  ['Sign', 'Language'],
  ['War', 'Conflict'],
  ['Tyranny', 'Power'],
  ['Life', 'Life'],
  ['Memory', 'Memory'],
  ['Necessity', 'Necessity and Contingency'],
  ['Universal', 'Universal and Particular'],
];

const V14_REMAP = [
  ['Cause', 'Causation'],
  ['Chance', 'Probability'],
  ['Liberty', 'Freedom'],
  ['Honor', 'Status'],
  ['Virtue and Vice', 'Virtue'],
  ['Animal', 'Life'],
  ['Aristocracy', 'Power'],
  ['Monarchy', 'Power'],
  ['Oligarchy', 'Power'],
  ['Tyranny and Despotism', 'Power'],
  ['Constitution', 'Institutions'],
  ['Government', 'Institutions'],
  ['State', 'Institutions'],
  ['Citizen', 'Institutions'],
  ['Custom and Convention', 'Institutions'],
  ['Courage', 'Virtue'],
  ['Dialectic', 'Reasoning'],
  ['Induction', 'Reasoning'],
  ['Logic', 'Reasoning'],
  ['Duty', 'Obligation'],
  ['Education', 'Learning'],
  ['Experience', 'Learning'],
  ['Family', 'Community'],
  ['Form', 'Beauty'],
  ['God', 'the Sacred'],
  ['Religion', 'the Sacred'],
  ['Theology', 'the Sacred'],
  ['Prophecy', 'the Sacred'],
  ['Immortality', 'the Sacred'],
  ['Hypothesis', 'Evidence'],
  ['Labor', 'Productivity'],
  ['Mind', 'Consciousness'],
  ['Soul', 'Consciousness'],
  ['Sense', 'Consciousness'],
  ['Poetry', 'Art'],
  ['Property', 'Markets'],
  ['Wealth', 'Markets'],
  ['Prudence', 'Strategy'],
  ['Punishment', 'Justice'],
  ['Revolution', 'Conflict'],
  ['Rhetoric', 'Narrative'],
  ['Sign and Symbol', 'Language'],
  ['Sin', 'Morality'],
  ['Temperance', 'Discipline'],
  ['Wisdom', 'Judgment'],
  ['Opinion', 'Judgment'],
  ['Will', 'Motivation'],
  ['World', 'Nature'],
  ['Man', 'Identity'],
  ['Good and Evil', 'Morality'],
  ['Happiness', 'Purpose'],
  ['Knowledge', 'Truth'],
  ['Law', 'Institutions'],
  ['Life and Death', 'Life'],
  ['Memory and Imagination', 'Memory'],
  ['Pleasure and Pain', 'Emotion'],
  ['Slavery', 'Freedom'],
  ['War and Peace', 'Conflict'],
];

function parseCli() {
  const args = process.argv.slice(2);
  const noFetch = args.includes('--no-fetch');
  const positional = args.filter((arg) => arg !== '--no-fetch');
  if (positional.length > 1 || args.some((arg) => arg.startsWith('--') && arg !== '--no-fetch')) {
    throw new Error('invalid command line');
  }
  const root = positional[0] ?? process.env.SURFC_ROOT;
  if (!root) throw new Error('missing surfc root');
  return { noFetch, surfcRoot: resolve(root) };
}

function git(surfcRoot, args, encoding = 'utf8') {
  const safeDirectory = surfcRoot.replaceAll('\\', '/');
  return execFileSync(
    'git',
    ['-c', `safe.directory=${safeDirectory}`, '-C', surfcRoot, ...args],
    {
      encoding,
      maxBuffer: 16 * 1024 * 1024,
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
    }
  );
}

function materializeCleanOracle(surfcRoot, oracleSha) {
  const runtimeDir = mkdtempSync(join(tmpdir(), 'sur-911-oracle-'));
  try {
    for (const relativePath of CLEAN_MODULE_FILES) {
      const target = join(runtimeDir, ...relativePath.split('/'));
      mkdirSync(dirname(target), { recursive: true });
      writeFileSync(target, git(surfcRoot, ['show', `${oracleSha}:${relativePath}`]));
    }

    // surfc is bundled by Vite and has no package-wide Node module type. This loader shim
    // changes no oracle bytes; it only lets Node load the exact objects materialized above.
    writeFileSync(join(runtimeDir, 'src/package.json'), '{"type":"module"}\n');

    const sourceNodeModules = join(surfcRoot, 'node_modules');
    assert.ok(existsSync(sourceNodeModules));
    symlinkSync(
      sourceNodeModules,
      join(runtimeDir, 'node_modules'),
      process.platform === 'win32' ? 'junction' : 'dir'
    );
    return runtimeDir;
  } catch {
    rmSync(runtimeDir, { recursive: true, force: true });
    throw new Error('clean oracle materialization failed');
  }
}

function subset(actual, expected, label) {
  const selected = Object.fromEntries(Object.keys(expected).map((key) => [key, actual[key]]));
  assert.deepEqual(selected, expected, label);
}

function byId(rows, idKey = 'id') {
  return [...rows].sort((a, b) => String(a[idKey]).localeCompare(String(b[idKey])));
}

function assertArrayLengths(parsed, rows, expected, label) {
  for (const [arrayName, length] of Object.entries(expected)) {
    assert.equal(parsed[arrayName].length, length, `${label}: parsed ${arrayName} length`);
    assert.equal(rows[arrayName].length, length, `${label}: imported ${arrayName} length`);
  }
}

function assertCompleteRows(actual, expected, label) {
  assert.deepEqual(
    Object.keys(expected),
    Object.keys(ARRAY_TO_TABLE),
    `${label}: expected oracle covers all eight stores`
  );
  assert.deepEqual(actual, expected, `${label}: complete normalized PWA rows`);
}

function readFullRows(expected, label) {
  assert.equal(typeof expected.fullRows, 'string', `${label}: full-row oracle reference`);
  const fullRows = JSON.parse(readFileSync(join(FIXTURE_DIR, expected.fullRows), 'utf8'));
  assert.ok(fullRows && typeof fullRows === 'object', `${label}: full-row oracle object`);
  return fullRows;
}

function assertCoreExportSurface(raw) {
  const topLevel = new Set([
    '_syntopicon',
    'schemaVersion',
    'exportedAt',
    ...Object.keys(ARRAY_TO_TABLE),
  ]);
  for (const key of Object.keys(raw)) assert.ok(topLevel.has(key));
  for (const [arrayName, allowed] of Object.entries(CORE_EXPORT_KEYS)) {
    for (const row of raw[arrayName]) {
      for (const key of Object.keys(row)) assert.ok(allowed.has(key));
    }
  }
  assert.deepEqual(raw.notes[0].user_metadata, {
    user_annotation: ['Core-supported margin note'],
  });
  assert.ok(raw.notes[0].createdAt > raw.notes[1].createdAt);
}

async function readImportedRows(db) {
  const result = {};
  for (const [arrayName, tableName] of Object.entries(ARRAY_TO_TABLE)) {
    const idKey = arrayName === 'noteSignals' ? 'noteId' : 'id';
    result[arrayName] = byId(await db.table(tableName).toArray(), idKey);
  }
  return result;
}

async function replaceAndRead(oracle, db, jsonText) {
  const parsed = oracle.parseImport(jsonText);
  await db.delete();
  await db.open();
  await oracle.importReplace(parsed);
  return { parsed, rows: await readImportedRows(db) };
}

async function assertTagMigration(oracle, db, schemaVersion, mappings, label) {
  const notes = mappings.map(([source], index) => ({
    id: `${label}-${index}`,
    text: label,
    tags: [source],
    source: 'manual',
    createdAt: index + 1,
    updatedAt: index + 1,
    deleted: 0,
  }));
  const { rows } = await replaceAndRead(
    oracle,
    db,
    JSON.stringify({ _syntopicon: true, schemaVersion, books: [], notes, customIdeas: [] })
  );
  const byNoteId = new Map(rows.notes.map((note) => [note.id, note]));
  for (const [index, [, target]] of mappings.entries()) {
    assert.deepEqual(byNoteId.get(`${label}-${index}`).tags, [target]);
  }
}

async function validate() {
  let phase = 'command-line validation';
  let runtimeDir;
  let db;
  let realDateNow;

  try {
    const { noFetch, surfcRoot } = parseCli();
    assert.equal(MANIFEST.sourceCommit, EXPECTED.oracleSha);
    assert.equal(MANIFEST.fixedImportNow, EXPECTED.fixedNow);
    assert.equal(V11_THEN_V14.length, 11);
    assert.equal(V14_REMAP.length, 58);

    phase = 'surfc origin refresh';
    if (!noFetch) {
      git(surfcRoot, [
        'fetch',
        '--quiet',
        'origin',
        '+refs/heads/main:refs/remotes/origin/main',
      ]);
    }

    phase = 'oracle revision resolution';
    const oracleSha = git(surfcRoot, ['rev-parse', '--verify', 'origin/main^{commit}']).trim();
    assert.equal(git(surfcRoot, ['cat-file', '-t', MANIFEST.sourceCommit]).trim(), 'commit');

    phase = 'clean oracle materialization';
    runtimeDir = materializeCleanOracle(surfcRoot, oracleSha);
    const requireFromSurfc = createRequire(join(surfcRoot, 'package.json'));
    requireFromSurfc('fake-indexeddb/auto');

    realDateNow = Date.now;
    Date.now = () => MANIFEST.fixedImportNow;

    phase = 'oracle module loading';
    const oracle = await import(
      `${pathToFileURL(join(runtimeDir, 'src/db.js')).href}?sha=${oracleSha}`
    );
    db = oracle.db;

    phase = 'frozen fixture import';
    const fixtureNames = [...MANIFEST.pwaFixtures, MANIFEST.coreExportFixture];
    const results = new Map();
    for (const fixtureName of fixtureNames) {
      const jsonText = readFileSync(join(FIXTURE_DIR, fixtureName), 'utf8');
      const raw = JSON.parse(jsonText);
      const result = await replaceAndRead(oracle, db, jsonText);
      results.set(fixtureName, { raw, ...result });
    }

    {
      const name = 'schema-1-preversioned.json';
      const { parsed, rows } = results.get(name);
      const expected = EXPECTED.fixtures[name];
      assert.equal(parsed.schemaVersion, expected.schemaVersion);
      for (const arrayName of expected.defaultedEmptyArrays) {
        assert.deepEqual(parsed[arrayName], []);
      }
      assert.equal(rows.books[0].updatedAt, expected.bookUpdatedAt);
      assert.equal(rows.customIdeas[0].updatedAt, expected.customIdeaUpdatedAt);
      subset(rows.notes[0], expected.note, `${name}: note defaults`);
    }

    {
      const name = 'schema-10-pre-v11.json';
      const { parsed, rows } = results.get(name);
      const expected = EXPECTED.fixtures[name];
      assert.equal(parsed.schemaVersion, expected.schemaVersion);
      assert.deepEqual(rows.notes[0].tags, expected.noteTags);
    }

    {
      const name = 'schema-11-pre-v14.json';
      const { parsed, rows } = results.get(name);
      const expected = EXPECTED.fixtures[name];
      assert.equal(parsed.schemaVersion, expected.schemaVersion);
      assert.deepEqual(rows.notes[0].tags, expected.noteTags);
    }

    {
      const name = 'schema-14-current-tags.json';
      const { parsed, rows } = results.get(name);
      const expected = EXPECTED.fixtures[name];
      assert.equal(parsed.schemaVersion, expected.schemaVersion);
      const parent = rows.notes.find((note) => note.id === 'n-v14-parent');
      assert.deepEqual(parent.tags, expected.noteTags);
      subset(rows.noteLinks[0], expected.noteLink, `${name}: legacy edge defaults`);
    }

    {
      const name = 'schema-19-all-stores.json';
      const { parsed, rows } = results.get(name);
      const expected = EXPECTED.fixtures[name];
      const fullRows = readFullRows(expected, name);
      assert.equal(parsed.schemaVersion, expected.schemaVersion);
      assertArrayLengths(parsed, rows, expected.arrayLengths, name);
      assertCompleteRows(rows, fullRows.pwaRows, name);
      const parent = rows.notes.find((note) => note.id === 'n-v19-parent');
      assert.equal(parent.contentTag, expected.parentContentTag);
      for (const key of expected.pwaPreservedExtraKeys) assert.ok(Object.hasOwn(parent, key));
      for (const key of expected.nativeIgnoredExtraKeys) {
        assert.ok(!NATIVE_IMPORT_NOTE_KEYS.has(key));
      }
      subset(rows.noteSignals[0], expected.noteSignal, `${name}: signal recomputation`);
    }

    {
      const name = 'schema-19-defaults.json';
      const { parsed, rows } = results.get(name);
      const expected = EXPECTED.fixtures[name];
      const fullRows = readFullRows(expected, name);
      assert.equal(parsed.schemaVersion, expected.schemaVersion);
      assertArrayLengths(parsed, rows, expected.arrayLengths, name);
      assertCompleteRows(rows, fullRows.pwaRows, name);
    }

    {
      const name = MANIFEST.coreExportFixture;
      const { raw, parsed, rows } = results.get(name);
      const expected = EXPECTED.fixtures[name];
      const fullRows = readFullRows(expected, name);
      assertCoreExportSurface(raw);
      assert.equal(raw.exportedAt, expected.exportedAt);
      assert.equal(parsed.schemaVersion, expected.schemaVersion);
      assertArrayLengths(parsed, rows, expected.arrayLengths, name);
      assertCompleteRows(rows, fullRows.pwaRows, name);
      const parent = rows.notes.find((note) => note.id === 'core-n-v19-parent');
      assert.equal(parent.contentTag, expected.parentContentTag);
      assert.deepEqual(parent.user_metadata.user_annotation, expected.parentUserAnnotation);
      assert.equal(rows.noteSignals[0].importance, expected.noteSignalImportance);
    }

    phase = 'exhaustive PWA tag migration probes';
    await assertTagMigration(oracle, db, 13, V14_REMAP, 'v14');
    await assertTagMigration(oracle, db, 10, V11_THEN_V14, 'v11-v14');

    console.log(`source_commit=${oracleSha}`);
    console.log(`fixture_provenance=${MANIFEST.sourceCommit}`);
    console.log(`fixed_now=${MANIFEST.fixedImportNow}`);
    console.log(`pwa_fixtures_validated=${MANIFEST.pwaFixtures.length}`);
    console.log('core_export_fixtures_validated=1');
    console.log('stores_per_schema19_fixture=8');
    console.log('full_row_oracles_validated=3');
    console.log(`v11_compositions_validated=${V11_THEN_V14.length}`);
    console.log(`v14_remaps_validated=${V14_REMAP.length}`);
    console.log('assertions=passed');
  } catch {
    console.error(`::error::snapshot-parity check failed during ${phase}`);
    process.exitCode = 1;
  } finally {
    if (realDateNow) Date.now = realDateNow;
    if (db) {
      try {
        await db.delete();
      } catch {
        // The primary error is already reported with a sanitized phase.
      }
    }
    if (runtimeDir) rmSync(runtimeDir, { recursive: true, force: true });
  }
}

await validate();
