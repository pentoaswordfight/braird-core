#!/usr/bin/env node
// extract-great-ideas — derive the canonical GREAT_IDEAS list from surfc's source of
// truth (SUR-820 Canon-102 awareness). This is BOTH the generator for the vendored
// `vendored/canon/great-ideas.json` AND the oracle `canon-drift` CI re-runs against
// surfc/main to detect drift — sibling of `extract-sync-schema.mjs`, same shape.
//
// Source of truth: surfc `src/constants.js` `export const GREAT_IDEAS = [...]`. Order
// matters (surfc's own header comment: "keep this order in lockstep with
// surfc-evals/vendored/constants.js — the prompt-drift check compares the GREAT_IDEAS
// array order-sensitively") — this extractor preserves source order, does not sort.
//
// Usage: node scripts/extract-great-ideas.mjs <surfc-root> [--check <fixture.json>]
//   no --check → prints canonical JSON to stdout (regenerate the fixture).
//   --check    → diffs the freshly-extracted list against <fixture.json>; exit 1 on drift.
//
// ponytail: regex extraction of one flat, non-nested array literal — a parser is
// overkill for a single `export const NAME = [...]` block. Mirrors the same
// extract-then-normalise approach `surfc-evals/prompt-drift.yml` already uses for this
// exact array.

import { readFileSync } from 'node:fs';
import { join } from 'node:path';

function extractGreatIdeas(constantsJs) {
  const m = constantsJs.match(/export const GREAT_IDEAS\s*=\s*\[([\s\S]*?)\]/);
  if (!m) throw new Error('could not locate `export const GREAT_IDEAS = [...]` in constants.js');
  // Quoted string literals only — skips the `// Branch (N)` comment lines between groups.
  return [...m[1].matchAll(/'([^']*)'|"([^"]*)"/g)].map((g) => g[1] ?? g[2]);
}

function buildList(surfcRoot) {
  const constantsJs = readFileSync(join(surfcRoot, 'src', 'constants.js'), 'utf8');
  return extractGreatIdeas(constantsJs);
}

// --- CLI --------------------------------------------------------------------------
const [, , surfcRoot, checkFlag, fixturePath] = process.argv;
if (!surfcRoot) {
  console.error('usage: extract-great-ideas.mjs <surfc-root> [--check <fixture.json>]');
  process.exit(2);
}
const list = buildList(surfcRoot);
const canonical = JSON.stringify(list, null, 2) + '\n';

if (checkFlag === '--check') {
  const want = readFileSync(fixturePath, 'utf8');
  // Re-canonicalise the fixture so whitespace can't cause a false diff.
  const wantCanonical = JSON.stringify(JSON.parse(want), null, 2) + '\n';
  if (canonical !== wantCanonical) {
    console.error('::error::great-ideas.json has drifted from surfc/main (src/constants.js GREAT_IDEAS).');
    console.error('The canon list changed (a leaf added/removed/renamed) without re-vendoring the mirror.');
    console.error('Re-run: node scripts/extract-great-ideas.mjs <surfc-root> > vendored/canon/great-ideas.json');
    process.exit(1);
  }
  console.log('great-ideas.json is in sync with surfc/main.');
} else {
  process.stdout.write(canonical);
}
