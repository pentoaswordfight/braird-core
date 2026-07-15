# GATING.md

Engineering gating policy for the **braird-core** repo — the shared Rust + UniFFI
crypto core for Braird's native iOS/Android clients (and the PWA's WASM build).

This file says **which paths get which review before merge** and **what "the gate"
actually is**. It is the working contract between the founder (conductor) and the AI
agents. Referenced by `CLAUDE.md`; agents must read it before proposing any change.

**Scope.** This repo is **all spine.** It produces the cryptographic primitives that
guard every Braird user's end-to-end-encrypted note. There is **no surface tier** here —
every path is GCE (founder sign-off), the same posture as the sibling `gce/` repo. The
"CE / persona-review-is-the-gate" pattern from the product repos does **not** apply.

Source of truth this core mirrors: `surfc/src/crypto/*` + `surfc/src/lib/text.js`
(`normalizeForTag`). Parity oracle: `surfc/test/fixtures/crypto-parity/`. This core must
match the PWA **byte-for-byte** — a divergence breaks PWA↔native coexistence in
production, where it is hardest to detect.

---

## 1. Definitions

- **Spine**: paths whose worst-case failure is unbounded — a crypto break, a key leak,
  silent ciphertext divergence that locks users out of their own notes. A failure here
  can end the product.
- **GCE (Gated / Conducted Engineering)**: human sign-off is required at the gate. Agents
  propose, founder disposes. Gates are explicit and named.

The default, and the only tier here: **everything → GCE.** Triage for a new path is
therefore short — anything touching keys, ciphertext, key derivation, the parity vectors,
the sync store/schema, the FFI surface, or the CI gates is spine → GCE; a change with no
crypto bearing (a typo in this file) is still GCE, just trivial: founder sign-off, no
persona pass needed.

---

## 2. The pattern in one paragraph

**GCE.** The plan is written down (a Linear ticket or a brief). It is pressure-tested
before code — an alternative considered, edge cases enumerated — and the founder reads the
result. The change lands in a branch, never straight on `main`. The gate for the touched
path (§3) runs: parity green, the vendored / schema drift guards green, and the named
persona(s) pass. The founder signs off in writing, a `CHANGELOG.md` `[Unreleased]` entry
is added, and it merges. There is no CE tier and no "small enough to skip" — if a path's
primary (eval) gate isn't built for a new surface yet, the fallback in §3 runs; see §5.

---

## 3. Path → pattern → gate

Paths grounded in the repo as it stands on `main` (SUR-716 crypto core + SUR-723 local
store). Every row is GCE; the **primary gate** is the automated eval that must be green,
and the **fallback gate** is the persona pass + manual check that stands in until (or
unless) a primary is built for a new surface. Personas resolve from the sibling `gce/`
repo by name (`shared/personas/<name>.md`).

> The gate table lives in **§3** (not §2) on purpose: the GCE line's classifier
> (`gce/src/read-gating.ts`) parses only `## 3` / `### 3.x` tables, so a table anywhere
> else is invisible to it and silently ungates the repo (SUR-728). Keep the tables here.

### 3.1 Crypto, sync + binding paths

| Path | Pattern | Primary gate | Fallback gate (until primary exists) |
|---|---|---|---|
| The **distributed canon release payloads** — `vendored/canon/great-ideas.json`, `vendored/canon/idea-tree.yaml` | **GCE only** | Canon-drift contract green — ordered GREAT_IDEAS JSON, byte-identical idea-tree YAML, and YAML↔GREAT_IDEAS leaf parity — **and** release checksum/publication integrity green for both public assets — + founder sign-off | Founder sign-off after `release-integrity-reviewer` **+** `sync-reviewer` **+** `crypto-reviewer` pass |
| The **sync engine + local store** (Phase 2, SUR-659; post-pull reconciliation SUR-820) — `src/store.rs`, `src/sync/**`, `src/outbox.rs`, `src/http.rs`, `vendored/schema/**`, `scripts/extract-sync-schema.mjs`, `vendored/canon/**`, `scripts/extract-great-ideas.mjs`, `.github/workflows/canon-drift.yml` | **GCE only** | Schema-drift **and** canon-drift guards green — `vendored/schema/**` reconciles against `surfc/main`'s synced schema (`tests/schema_parity.rs` + `.github/workflows/schema-drift.yml`), and canon proves ordered GREAT_IDEAS JSON, byte-identical idea-tree YAML, and YAML↔GREAT_IDEAS leaf parity (`.github/workflows/canon-drift.yml`) — + founder sign-off | Founder sign-off after `sync-reviewer` (engine, PWA↔native coexistence, schema/canon drift) **+** `crypto-reviewer` (the seal-at-flush boundary — note text must never leave unencrypted, SUR-724) pass |
| The **release / packaging boundary** (SUR-760; row pre-wired by SUR-778) — `bindings/android/**`, `bindings/consumer-smoke/**`, `scripts/build-aar.sh`, `scripts/build-xcframework.sh`, `.github/workflows/release.yml`, `.github/workflows/android-artifacts.yml`, `docs/pinning.md` | **GCE only** | Release CI green — every shipped `.so` 16 KB-aligned (bundled deps included), SHA-256 per artifact published with the release, tag / `Cargo.toml` version / CHANGELOG agree — + founder sign-off | Founder sign-off after a `release-integrity-reviewer` (binding↔native atomicity, tag + SHA-256 pin, fail-closed fetch, alignment gates) pass |
| `bindings/**`, `src/bin/uniffi-bindgen.rs` — the generated Swift/Kotlin surface + its round-trip tests (the public API devs consume) | **GCE only** | Swift **and** Kotlin round-trip parity green + founder sign-off | Founder sign-off after `naming-reviewer` (the API *word*) **+** `crypto-reviewer` (the seam) pass |
| `vendored/crypto-parity/**` — the crypto parity vectors vendored from `surfc/main` | **GCE only** | Vendored-drift guard green — byte-identical to `surfc/main` (§4) — + founder sign-off | `crypto-reviewer` confirms the vectors against `surfc/main` |
| `vendored/native-parity/**`, `scripts/check-native-parity.mjs`, `.github/workflows/native-parity-drift.yml` — the sync-behavior parity surface vendored from `surfc/main`'s SUR-845 registry snapshot + its coverage manifest (SUR-842) | **GCE only** | Native-parity drift guard green — vendored snapshot current with `surfc/main` **and** every registered behavior manifest-covered (ticket or reasoned waiver) — + founder sign-off | Founder sign-off after `security-reviewer` (the cross-repo read PAT / new CI workflow) **+** `sync-reviewer` (what a "synced behavior" is, and that the manifest maps each honestly) pass |
| `src/**`, `tests/**`, `Cargo.toml`, `Cargo.lock` — the crate, every line of crypto, and the parity harness (`tests/parity.rs`; a harness that lies is worse than none). **Catch-all — kept LAST so the specific rows above win first-match** | **GCE only** | Parity eval green — the 10 in-scope + the normalization vectors **bit-identical**, foreign-ciphertext decrypt passes — + founder sign-off | Founder sign-off after a `crypto-reviewer` pass + a manual round-trip against a real `surfc`-written ciphertext |

### 3.2 Meta / docs / CI paths

| Path | Pattern | Primary gate | Fallback gate (until primary exists) |
|---|---|---|---|
| `docs/adr/**` — architecture decision records (e.g. ADR 0002, the crypto backend) | **GCE only** | Founder sign-off | Founder sign-off after an `architecture-decision-reviewer` pass |
| `.github/workflows/**` — the parity / vendored-drift / schema-drift / changelog / nightly gates themselves (release.yml + android-artifacts.yml are carved out to §3.1's release row) | **GCE only** | Founder sign-off — these set the rules | — |
| `GATING.md` (this file), `CLAUDE.md`, `README.md`, `CHANGELOG.md` | **GCE only** | Founder sign-off | — |

**Row order matters — specific rows precede the general `src/**` catch-all.** The line's
classifier (`gce/src/classify-paths.ts`) is **first-match**: it attributes each touched path
to the *first* §3 row whose globs match, and stops. So the specific surfaces inside `src/`
are listed **above** the `src/**` row, and first-match isolates them correctly:
- `vendored/canon/great-ideas.json` / `vendored/canon/idea-tree.yaml`
  → the distributed-canon release row → **`release-integrity-reviewer`** +
  **`sync-reviewer`** + **`crypto-reviewer`**; the following `vendored/canon/**`
  token remains the catch-all for future canon files;
- `src/store.rs` / `src/sync/**` / `src/outbox.rs` / `src/http.rs` (or the schema fixture)
  → the sync/store row → **`sync-reviewer`** (its gate also names `crypto-reviewer`);
- an exported **type / method / error name** in a `#[uniffi::export]` item (chiefly
  `src/vault.rs`, `src/lib.rs` — this crate has no `.udl`) lands in `bindings/**` /
  `src/bin/uniffi-bindgen.rs` → the binding row → **`naming-reviewer`** + `crypto-reviewer`.

Keep `src/**` **last**. Moving it up would shadow the specific rows and silently drop their
reviewer — the sync-engine slice reviewed without `sync-reviewer` (SUR-724 caught this: the
classifier honours order, not the prose, so the prose can't substitute for the ordering).

**SUR-760 landed the AAR/jar packaging module; its paths are now in the §3.1 release row**
(`bindings/android/**`, `bindings/consumer-smoke/**`, `android-artifacts.yml`), kept **above**
the binding row so the module routes to `release-integrity-reviewer`, not the binding row's
gate. Only the packaging-specific `bindings/android/**` + `bindings/consumer-smoke/**` globs were
added — deliberately **not** a broad `bindings/**`: the classifier treats **every** backticked
token in a path cell as one of that row's globs, so a `bindings/**` here would first-match
binding-only PRs (`bindings/kotlin`, `bindings/swift`) onto the release row and silently drop
`naming-reviewer` + `crypto-reviewer`
(the SUR-778 review caught exactly that).

**Wide-export convention — a `#[uniffi::export]` method that could exceed 8 integer/pointer FFI
slots takes a single `uniffi::Record`, and `scripts/check-ffi-arg-slots.mjs` (bindings-drift job)
enforces it (SUR-843).** On arm64 a by-value `RustBuffer` (a lowered `String`/`Option`/`Vec`)
that spills past x7 is mis-marshalled by JNA's libffi (jna#1259) — invisible to x86-64 CI + the
desktop `:core-roundtrip` jar. The guard inspects the generated Kotlin externs and fails on any
`RustBuffer` at slot ≥9 (counting integer/pointer slots only — `f64`/`f32` ride the FP bank), so
the class fails the build instead of shipping to a device. `naming-reviewer` owns the record's
name (pair it with the read model, `NoteUpsert`↔`NoteRecord`, `BookUpsert`↔`BookRecord`); it's a
`crypto-reviewer` note wherever the collapsed method crosses the seal boundary. Fixed cases:
`enqueue_note` → `NoteUpsert` (SUR-770), `enqueue_book` → `BookUpsert` (SUR-843).

**Test-only seams are a `crypto-reviewer` / `naming-reviewer` BLOCKER if they reach the
public binding.** `with_raw_mk` (construct-from-raw-hex) and any IV/salt override on
`wrap_with_prf` live behind the `test-seams` Cargo feature, which is **OFF** for the
production `cdylib` / bindings — a leaked nonce/salt override is a catastrophic GCM
nonce-reuse footgun. Production generates salt/IV internally.

**Frozen wire-format constants (`crypto-reviewer` verifies verbatim):** the `surfc-*` HKDF
info strings (`surfc-master-key-wrap-v1`, `surfc-content-tag-v1`), the 600 000 PBKDF2
iteration count, standard (not URL) base64, the 12-byte IV, the **64-byte** content-tag
HMAC subkey, and the `enc:v1` / `enc:v2` / `0x02` headers. These are protocol constants,
not branding — they stay `surfc-*` despite the Braird rename (SUR-680 allowlist).

### Not yet in scope

- **`simplicity-reviewer`** is optional and size-gated; it **defers to `crypto-reviewer`**
  wherever simplification and safety collide. Advisory, never a blocker here.

---

## 4. What "the gate" actually is

A change is **gateable** when all of the following are true:

1. There is a Linear ticket (or written brief) describing the intended change.
2. The plan was pressure-tested before code (alternative considered, edge cases
   enumerated). The founder reads the result.
3. The change is in a branch (never committed straight to `main` after bootstrap).
4. **Parity is green** — the Rust harness reproduces the 10 in-scope + the normalization
   vectors **bit-identical**, foreign-ciphertext decrypt passes, and (for binding changes)
   the Swift + Kotlin round-trips pass. CI enforces this on every core change.
5. **The drift guards are green** — `vendored/crypto-parity/**` is byte-identical to
   `surfc/main`; `vendored/schema/**` reconciles against the synced schema; and canon proves
   ordered `great-ideas.json`, byte-identical `idea-tree.yaml`, and YAML↔GREAT_IDEAS leaf
   parity. The two distributed canon payloads also pass release checksum/publication integrity.
   See `.github/workflows/`.
6. `crypto-reviewer` (+ `naming-reviewer` for binding changes, + `sync-reviewer` for
   store/schema/canon changes, + `release-integrity-reviewer` for the two distributed canon
   payloads) has passed, or its findings are explicitly accepted with rationale.
7. Founder has signed off in writing (PR comment is fine).
8. A `CHANGELOG.md` `[Unreleased]` entry exists (CI-enforced, dependabot-exempt).

A change failing any of (1)–(8) does not merge. **No exceptions — every path is spine.**

---

## 5. Gate-bypass procedure

There is no "I'll just merge it, it's small." If the parity eval or a binding test is not
yet built for a new surface:

1. The named fallback runs (the `crypto-reviewer` persona pass + a manual round-trip
   against a real `surfc`-written ciphertext). No exceptions.
2. The PR names the gate that *would have* run and why it isn't.
3. A Linear ticket is opened to build the missing gate, tagged `gate-debt`, prioritised
   before the next change on that path.

---

## 6. Related files

- `CLAUDE.md` — agent context for this repo.
- `README.md` — what the core is + the security model.
- `CHANGELOG.md` — release notes (Keep a Changelog).
- `docs/adr/**` — architecture decision records. ADR 0001 (Rust+UniFFI; in `surfc`,
  surfc#331) established this repo; ADR 0002 records the crypto-backend choice (RustCrypto).
- `vendored/crypto-parity/` — crypto parity vectors vendored from `surfc/main`, drift-guarded.
- `vendored/schema/` — the synced-schema fixture, drift-guarded against `surfc/main` (SUR-723).
- `vendored/canon/` — the ordered `GREAT_IDEAS` JSON and byte-vendored idea-tree YAML,
  drift-guarded against `surfc/main` with YAML↔GREAT_IDEAS leaf parity and distributed together
  as checksum-pinned release assets (SUR-820 Canon-102 awareness; SUR-918 release distribution).
- `bindings/{swift,kotlin}/` — the generated UniFFI surface + round-trip tests (produced
  from the `#[uniffi::export]` items via `src/bin/uniffi-bindgen.rs`).
- Persona prompts — in the sibling `gce/` repo (`shared/personas/`), referenced by name
  from §3.

---

*Implements ADR 0001 (Accepted, surfc#331). Anchored by SUR-716 (Phase 1 impl), part of
epic SUR-656. GATING §3 reshape tracked by SUR-728 (enabling braird-core for the GCE line).*
