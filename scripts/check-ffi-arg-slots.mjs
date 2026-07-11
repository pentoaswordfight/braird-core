#!/usr/bin/env node
// check-ffi-arg-slots.mjs — fail the build when a UniFFI export would spill a by-value
// `RustBuffer` onto the stack on arm64 (SUR-843; the SUR-770 defect class, generalized).
//
// THE DEFECT. UniFFI lowers each `String`/`Option`/`Vec` arg to a by-value `RustBuffer`.
// On arm64 (AAPCS64) the first 8 integer/pointer args go in x0–x7; the rest spill onto the
// stack — and JNA's bundled libffi mis-marshals a struct-by-value (`RustBuffer`) arg ON THE
// STACK (java-native-access/jna#1259). So a method is at risk **iff a `RustBuffer` lands at
// integer-slot ≥9**. The first byte-validated arg after it then throws "unexpected byte for
// Boolean". x86-64 (SysV) lays args out differently and tolerates it, so plain CI + the
// desktop `:core-roundtrip` jar are structurally blind — only a real arm64 device catches it
// at runtime. This guard catches it STATICALLY, on any CI, by inspecting the generated
// Kotlin externs (true lowering — record-collapse and all).
//
// THE COUNTING RULE (load-bearing subtlety). AAPCS64 has a SEPARATE FP register bank (v0–v7),
// so `Double`/`Float` args do NOT consume an integer slot — they never push a later
// `RustBuffer` onto the stack. This is exactly why `enqueue_note_signals` is safe (its two
// `Double`s keep its scalar tail in registers) while `enqueue_book`'s trailing `Vec` was not.
// Miscounting an f64 as an integer slot would be a false positive — the `--self-check` locks it.
//
// FIX for a flagged method: collapse its args into a single `uniffi::Record` (as `enqueue_note`
// → `NoteUpsert`, `enqueue_book` → `BookUpsert`). A record lowers as ONE `RustBuffer` — 3 FFI
// slots, all in registers.
//
// Usage:  node scripts/check-ffi-arg-slots.mjs [path/to/braird_core.kt]
//         node scripts/check-ffi-arg-slots.mjs --self-check
// Node/CI tooling only — no crate dependency (mirrors check-native-parity.mjs).

import { readFileSync } from "node:fs";

const DEFAULT_KT = "bindings/kotlin/src/main/kotlin/uniffi/braird_core/braird_core.kt";
const MAX_INT_SLOTS = 8; // x0–x7; a RustBuffer at slot 9+ spills onto the stack

// Types that ride the FP register bank (v0–v7) and so consume NO integer/pointer slot.
const FP_TYPES = new Set(["Double", "Float"]);

/** A param's declared type is a by-value RustBuffer (how String/Option/Vec lower). */
const isRustBuffer = (type) => type.startsWith("RustBuffer");

/**
 * Given a raw Kotlin extern param list (the text between `(` and the trailing newline),
 * assign each param an integer-slot index (receiver `ptr` = slot 1; FP types skipped) and
 * return the `RustBuffer` params that land at slot > MAX_INT_SLOTS.
 */
function offenders(paramsRaw) {
  const params = paramsRaw
    .split(",")
    .map((p) => p.trim())
    .filter(Boolean);
  const hits = [];
  let slot = 0;
  for (const p of params) {
    // `name`: Type   OR   name: Type   (the trailing uniffi_out_err has no backticks)
    const type = p.split(":").pop().trim();
    if (FP_TYPES.has(type)) continue; // FP bank — no integer slot consumed
    slot += 1;
    if (isRustBuffer(type) && slot > MAX_INT_SLOTS) {
      const name = (p.match(/^`?([A-Za-z0-9_]+)`?\s*:/) || [, p])[1];
      hits.push({ name, slot });
    }
  }
  return hits;
}

/** Extract every exported extern decl (`uniffi_braird_core_fn_{func,method,constructor}_*`). */
function exportDecls(src) {
  // Scope to OUR exports — not UniFFI's internal `ffi_braird_core_*` runtime scaffolding
  // (rustbuffer alloc/free etc.), whose signatures we neither own nor can change.
  const re = /fun (uniffi_braird_core_fn_\w+)\(([^\n]*)/g;
  const decls = [];
  let m;
  while ((m = re.exec(src)) !== null) decls.push({ name: m[1], paramsRaw: m[2] });
  return decls;
}

function scan(src) {
  return exportDecls(src)
    .map(({ name, paramsRaw }) => ({ name, hits: offenders(paramsRaw) }))
    .filter((d) => d.hits.length > 0);
}

function selfCheck() {
  // A Vec at slot 11 past 7 register scalars — the enqueue_book-before-the-fix shape.
  const bad =
    "`ptr`: Pointer,`id`: RustBuffer.ByValue,`title`: RustBuffer.ByValue,`author`: RustBuffer.ByValue,`isbn`: RustBuffer.ByValue,`coverUrl`: RustBuffer.ByValue,`coverSource`: RustBuffer.ByValue,`coverResolvedAt`: RustBuffer.ByValue,`createdAt`: Long,`deleted`: Byte,`clearNullableFields`: RustBuffer.ByValue,uniffi_out_err: UniffiRustCallStatus, ";
  // enqueue_note_signals: two Doubles in the FP bank keep every stack arg a scalar — SAFE.
  const safeFp =
    "`ptr`: Pointer,`noteId`: RustBuffer.ByValue,`sourcePrior`: Double,`returnVisits`: Long,`hasAnnotation`: Byte,`stitchSpawns`: Long,`exposureRecencyAt`: Long,`engagementRecencyAt`: Long,`importance`: Double,`createdAt`: Long,`deleted`: Byte,uniffi_out_err: UniffiRustCallStatus, ";
  // A record (single RustBuffer at slot 2) — the fix — SAFE.
  const safeRecord = "`ptr`: Pointer,`draft`: RustBuffer.ByValue,uniffi_out_err: UniffiRustCallStatus, ";

  const badHits = offenders(bad);
  assert(badHits.length === 1 && badHits[0].slot === 11, "bad decl must flag the Vec at slot 11");
  assert(offenders(safeFp).length === 0, "FP-bank Doubles must NOT push the tail onto the stack");
  assert(offenders(safeRecord).length === 0, "a single-record RustBuffer at slot 2 is safe");
  console.log("check-ffi-arg-slots: self-check passed");
}

function assert(cond, msg) {
  if (!cond) {
    console.error(`self-check FAILED: ${msg}`);
    process.exit(1);
  }
}

// ── main ─────────────────────────────────────────────────────────────────────
const arg = process.argv[2];
if (arg === "--self-check") {
  selfCheck();
  process.exit(0);
}

const path = arg || DEFAULT_KT;
const violations = scan(readFileSync(path, "utf8"));
if (violations.length > 0) {
  console.error(
    "check-ffi-arg-slots: FFI export(s) spill a by-value RustBuffer onto the arm64 stack " +
      `(RustBuffer at integer-slot >${MAX_INT_SLOTS}). Collapse the args into a uniffi::Record ` +
      "(see BookUpsert / NoteUpsert):",
  );
  for (const { name, hits } of violations) {
    for (const h of hits) console.error(`  ${name}: '${h.name}' at slot ${h.slot}`);
  }
  process.exit(1);
}
console.log(`check-ffi-arg-slots: OK — no RustBuffer past slot ${MAX_INT_SLOTS} in ${path}`);
