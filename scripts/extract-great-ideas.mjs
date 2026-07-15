#!/usr/bin/env node
// extract-great-ideas derives the canonical GREAT_IDEAS list and checks the
// vendored canon fixtures against surfc/main. GREAT_IDEAS order matters; the idea
// tree itself is a byte-for-byte fixture and its leaf names must set-match the list.
//
// Usage: node scripts/extract-great-ideas.mjs <surfc-root> [--check <fixture.json> [--tree <fixture.yaml>]]
//   no --check -> prints canonical JSON to stdout (regenerate the JSON fixture).
//   --check    -> order-sensitively checks the JSON fixture.
//   --tree     -> also byte-checks the YAML fixture and validates its leaf-name set.

import { readFileSync } from 'node:fs';
import { join } from 'node:path';

function extractGreatIdeas(constantsJs) {
  const match = constantsJs.match(/export const GREAT_IDEAS\s*=\s*\[([\s\S]*?)\]/);
  if (!match) throw new Error('could not locate `export const GREAT_IDEAS = [...]` in constants.js');
  return [...match[1].matchAll(/'([^']*)'|"([^"]*)"/g)].map((group) => group[1] ?? group[2]);
}

function buildList(surfcRoot) {
  const constantsJs = readFileSync(join(surfcRoot, 'src', 'constants.js'), 'utf8');
  return extractGreatIdeas(constantsJs);
}

function extractIdeaTreeLeafNames(yaml) {
  const lines = yaml.split(/\r?\n/);
  const names = [];
  const firstLineByName = new Map();
  let leavesBlocks = 0;
  let block = null;

  function finishBlock() {
    if (block && block.count === 0) {
      throw new Error(`empty \`leaves:\` block at line ${block.line}`);
    }
  }

  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index];
    const lineNumber = index + 1;
    const leavesDeclaration = line.match(/^( *)leaves\s*:(.*)$/);
    const leavesMatch = line.match(/^( *)leaves:\s*(?:#.*)?$/);

    if (leavesDeclaration) {
      if (!leavesMatch) {
        throw new Error(`unsupported \`leaves:\` declaration at line ${lineNumber}: ${line.trim()}`);
      }
      finishBlock();
      block = { indent: leavesMatch[1].length, line: lineNumber, count: 0 };
      leavesBlocks += 1;
      continue;
    }

    if (!block || /^\s*(?:#.*)?$/.test(line)) continue;

    const indentation = line.match(/^ */)[0].length;
    if (indentation <= block.indent) {
      finishBlock();
      block = null;
      continue;
    }

    const inline = line.match(/^\s*-\s*\{(.*?)\}\s*(?:#.*)?$/);
    if (!inline) {
      throw new Error(`unsupported/non-inline leaf entry at line ${lineNumber}: ${line.trim()}`);
    }

    const nameProperty = inline[1].match(/(?:^|,)\s*name\s*:\s*("(?:\\.|[^"\\])*")(?=\s*(?:,|$))/);
    if (!nameProperty) {
      throw new Error(`unsupported inline leaf entry without a double-quoted name at line ${lineNumber}`);
    }

    let name;
    try {
      name = JSON.parse(nameProperty[1]);
    } catch {
      throw new Error(`invalid double-quoted leaf name at line ${lineNumber}`);
    }
    if (!name) throw new Error(`empty leaf name at line ${lineNumber}`);

    const firstLine = firstLineByName.get(name);
    if (firstLine !== undefined) {
      throw new Error(`duplicate YAML leaf name ${JSON.stringify(name)} at lines ${firstLine} and ${lineNumber}`);
    }
    firstLineByName.set(name, lineNumber);
    names.push(name);
    block.count += 1;
  }

  finishBlock();
  if (leavesBlocks === 0 || names.length === 0) {
    throw new Error('missing/empty `leaves:` surface');
  }
  return names;
}

function reportReadError(label, path, error) {
  console.error(`::error::could not read ${label} fixture at ${path}: ${error.message}`);
}

function reportGreatIdeasRevendor() {
  console.error('Re-run: node scripts/extract-great-ideas.mjs <surfc-root> > vendored/canon/great-ideas.json');
}

function reportIdeaTreeRevendor() {
  console.error('Re-vendor src/constants/surfc-idea-tree.yaml from surfc/main byte-for-byte as vendored/canon/idea-tree.yaml.');
}

const [, , surfcRoot, checkFlag, fixturePath, treeFlag, treeFixturePath, ...extraArgs] = process.argv;
const usage = 'usage: extract-great-ideas.mjs <surfc-root> [--check <fixture.json> [--tree <fixture.yaml>]]';
if (!surfcRoot) {
  console.error(usage);
  process.exit(2);
}

const list = buildList(surfcRoot);
const canonical = JSON.stringify(list, null, 2) + '\n';

if (checkFlag === '--check') {
  if (!fixturePath || (treeFlag !== undefined && treeFlag !== '--tree') || (treeFlag === '--tree' && !treeFixturePath) || extraArgs.length > 0) {
    console.error(usage);
    process.exit(2);
  }
  let failed = false;

  try {
    const want = readFileSync(fixturePath, 'utf8');
    const wantCanonical = JSON.stringify(JSON.parse(want), null, 2) + '\n';
    if (canonical !== wantCanonical) {
      console.error('::error::great-ideas.json has drifted from surfc/main (src/constants.js GREAT_IDEAS).');
      console.error('The canon list changed (a leaf added/removed/renamed/reordered) without re-vendoring the mirror.');
      reportGreatIdeasRevendor();
      failed = true;
    } else {
      console.log('great-ideas.json is in sync with surfc/main.');
    }
  } catch (error) {
    reportReadError('great-ideas.json', fixturePath, error);
    reportGreatIdeasRevendor();
    failed = true;
  }

  if (treeFlag === '--tree') {
    const liveTreePath = join(surfcRoot, 'src', 'constants', 'surfc-idea-tree.yaml');
    let liveTree;
    let treeFixture;

    try {
      liveTree = readFileSync(liveTreePath);
    } catch (error) {
      console.error(`::error::could not read live idea tree at ${liveTreePath}: ${error.message}`);
      failed = true;
    }
    try {
      treeFixture = readFileSync(treeFixturePath);
    } catch (error) {
      reportReadError('idea-tree.yaml', treeFixturePath, error);
      reportIdeaTreeRevendor();
      failed = true;
    }

    if (liveTree && treeFixture) {
      if (!liveTree.equals(treeFixture)) {
        console.error('::error::idea-tree.yaml has drifted from surfc/main (src/constants/surfc-idea-tree.yaml).');
        console.error('The vendored idea tree must be a byte-for-byte copy of the live source.');
        reportIdeaTreeRevendor();
        failed = true;
      } else {
        console.log('idea-tree.yaml is in sync with surfc/main byte-for-byte.');
      }
    }

    if (liveTree) {
      try {
        const leafNames = extractIdeaTreeLeafNames(liveTree.toString('utf8'));
        const leafSet = new Set(leafNames);
        const greatIdeasSet = new Set(list);
        const missingFromTree = [...greatIdeasSet].filter((name) => !leafSet.has(name));
        const extraInTree = [...leafSet].filter((name) => !greatIdeasSet.has(name));

        if (missingFromTree.length > 0 || extraInTree.length > 0) {
          console.error('::error::idea-tree leaf names differ from live GREAT_IDEAS.');
          if (missingFromTree.length > 0) console.error(`Missing from idea tree: ${missingFromTree.join(', ')}`);
          if (extraInTree.length > 0) console.error(`Extra in idea tree: ${extraInTree.join(', ')}`);
          console.error('Update GREAT_IDEAS and surfc-idea-tree.yaml together, then re-vendor both canon fixtures.');
          failed = true;
        } else {
          console.log(`${leafSet.size} unique leaf names match live GREAT_IDEAS.`);
        }
      } catch (error) {
        console.error(`::error::unsupported idea-tree leaf surface: ${error.message}`);
        console.error('Use double-quoted inline records under each `leaves:` block, then re-vendor the idea tree.');
        failed = true;
      }
    }
  }

  if (failed) process.exit(1);
} else {
  if (checkFlag !== undefined) {
    console.error(usage);
    process.exit(2);
  }
  process.stdout.write(canonical);
}
