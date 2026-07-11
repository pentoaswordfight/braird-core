#!/usr/bin/env node
// check-native-parity — guard the native-parity surface against surfc/main (SUR-842).
//
// surfc's load-bearing sync-behavior registry (SUR-845) drives `syncFromCloud` and
// emits a canonical snapshot at `src/sync/sync-surface.json` — the authoritative set of
// sync/data behaviors a native client must mirror. This repo vendors a copy of that
// snapshot (`vendored/native-parity/sync-surface.json`) plus a hand-maintained coverage
// manifest (`vendored/native-parity/manifest.json`) mapping every registered behavior to
// its native home (core / ios / android) or a reasoned waiver. The `native-parity-drift`
// CI re-fetches the live snapshot from surfc/main and runs this in --check mode.
//
// Two guards close the loop (fail-loud, no silent fallback — ADR 0001 discipline):
//   (a) STALENESS — the vendored snapshot equals surfc/main's live snapshot (so a new
//       surfc behavior can't sit unnoticed), and
//   (b) COVERAGE — every registered behavior has exactly one manifest row, no manifest
//       row is an orphan, every row's status is valid, waivers carry a reason, and every
//       non-waived row names a ticket.
//
// STATUS CONTRACT (the check enforces SHAPE, not ticket truth — the human sync-reviewer
// gate is the backstop, so the shape must not let a lie look green):
//   core | ios | android — the behavior is IMPLEMENTED there TODAY. This is a present-tense
//     coverage claim; the referenced ticket must be LANDED. The check cannot verify the
//     ticket is done, so DO NOT use these for planned/tracked-but-unbuilt work — a `core`
//     row for an absent behavior reports green and silences the guard for the one thing it
//     exists to surface (the reconcile-content-tags/SUR-835 case).
//   waived — the behavior is deliberately NOT covered natively (yet). REQUIRES a reason in
//     `note`; MAY carry a `ticket` that tracks the eventual port. A not-yet-ported behavior
//     lives HERE (an explicit, ratified, tracked gap), never as core-with-a-future-ticket.
//     Flip the row to core/ios/android when the port actually lands.
//
// This is CI tooling, not crate code: pure Node (JSON.parse — no npm dep), the Rust core
// is not touched. Sibling of scripts/extract-sync-schema.mjs; the snapshot is surfc's own
// emitted artifact (not derived), so staleness is a snapshot compare, not a re-derivation.
//
// Usage: node scripts/check-native-parity.mjs <surfc-root> [--check]
//   no --check → prints surfc/main's canonical snapshot to stdout (re-vendor helper).
//   --check    → runs staleness + coverage; exit 1 on any failure, naming each.

import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const VALID_STATUSES = new Set(['core', 'ios', 'android', 'waived']);
const TICKET_RE = /^SUR-\d+$/;

const LIVE_SNAPSHOT = ['src', 'sync', 'sync-surface.json'];
const VENDORED_SNAPSHOT = 'vendored/native-parity/sync-surface.json';
const MANIFEST = 'vendored/native-parity/manifest.json';

const readJson = (path) => {
  const raw = readFileSync(path, 'utf8');
  try {
    return JSON.parse(raw);
  } catch (e) {
    throw new Error(`parse ${path}: ${e.message}`);
  }
};

// Re-serialise so whitespace / line-ending differences can't cause a false drift.
const canonical = (obj) => JSON.stringify(obj, null, 2) + '\n';

const registeredIds = (snapshot) => {
  if (!Array.isArray(snapshot.entries)) throw new Error('snapshot has no `entries` array');
  return snapshot.entries.map((e) => e.id);
};

// (a) STALENESS — the vendored copy equals surfc/main's live snapshot.
function checkStaleness(liveSnapshot, errors) {
  const vendored = readJson(VENDORED_SNAPSHOT);
  if (canonical(vendored) !== canonical(liveSnapshot)) {
    errors.push(
      `${VENDORED_SNAPSHOT} has drifted from surfc/main's src/sync/sync-surface.json.\n` +
        `  A sync behavior was added, removed, or re-described in the SUR-845 registry without re-vendoring.\n` +
        `  Re-vendor: node scripts/check-native-parity.mjs <surfc-root> > ${VENDORED_SNAPSHOT}\n` +
        `  then reconcile ${MANIFEST} (add/remove the affected row) until this check is green.`
    );
  }
}

// (b) COVERAGE — the manifest accounts for exactly the registered behaviors.
function checkCoverage(liveSnapshot, errors) {
  const manifest = readJson(MANIFEST);
  if (!Array.isArray(manifest.entries)) throw new Error(`${MANIFEST} has no \`entries\` array`);

  const ids = registeredIds(liveSnapshot);
  const idSet = new Set(ids);

  const manifestIds = manifest.entries.map((r) => r.id);
  const manifestIdSet = new Set(manifestIds);

  // Duplicate manifest rows.
  const seen = new Set();
  for (const id of manifestIds) {
    if (seen.has(id)) errors.push(`${MANIFEST}: duplicate row for "${id}" — one row per behavior.`);
    seen.add(id);
  }

  // A registered behavior with no manifest row (the core "new behavior slipped in" guard).
  for (const id of ids) {
    if (!manifestIdSet.has(id)) {
      errors.push(
        `registered behavior "${id}" has no row in ${MANIFEST}.\n` +
          `  Add: { "id": "${id}", "status": "core|ios|android|waived", "ticket": "SUR-nnn", "note": "..." }\n` +
          `  (a "core"/"ios"/"android" row needs a ticket; a "waived" row needs a non-empty note).`
      );
    }
  }

  // An orphan manifest row for a behavior no longer registered upstream.
  for (const id of manifestIds) {
    if (!idSet.has(id)) {
      errors.push(
        `${MANIFEST} row "${id}" is not a registered behavior in surfc/main — remove it (or it drifted).`
      );
    }
  }

  // Per-row validity.
  for (const row of manifest.entries) {
    const where = `${MANIFEST} row "${row.id ?? '(missing id)'}"`;
    if (!row.id) errors.push(`${where}: missing "id".`);
    if (!VALID_STATUSES.has(row.status)) {
      errors.push(`${where}: status "${row.status}" is not one of core|ios|android|waived.`);
      continue;
    }
    if (row.status === 'waived') {
      if (typeof row.note !== 'string' || row.note.trim() === '') {
        errors.push(`${where}: a waived behavior REQUIRES a non-empty "note" (the reason).`);
      }
    } else if (!TICKET_RE.test(row.ticket ?? '')) {
      errors.push(`${where}: status "${row.status}" REQUIRES a "ticket" matching SUR-nnn.`);
    }
  }
}

// --- CLI --------------------------------------------------------------------------
const [, , surfcRoot, checkFlag] = process.argv;
if (!surfcRoot) {
  console.error('usage: check-native-parity.mjs <surfc-root> [--check]');
  process.exit(2);
}

const liveSnapshot = readJson(join(surfcRoot, ...LIVE_SNAPSHOT));

if (checkFlag !== '--check') {
  // Re-vendor helper: emit surfc/main's canonical snapshot for `> vendored/...`.
  process.stdout.write(canonical(liveSnapshot));
} else {
  const errors = [];
  checkStaleness(liveSnapshot, errors);
  checkCoverage(liveSnapshot, errors);
  if (errors.length) {
    for (const e of errors) console.error(`::error::${e}`);
    console.error(`\nnative-parity check failed with ${errors.length} issue(s) — see above.`);
    process.exit(1);
  }
  console.log('native-parity: vendored snapshot is current and the manifest covers every registered behavior.');
}
