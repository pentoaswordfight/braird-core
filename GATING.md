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
| `src/**`, `tests/**`, `Cargo.toml`, `Cargo.lock` — the crate, every line of crypto, and the parity harness (`tests/parity.rs`; a harness that lies is worse than none) | **GCE only** | Parity eval green — the 10 in-scope + the normalization vectors **bit-identical**, foreign-ciphertext decrypt passes — + founder sign-off | Founder sign-off after a `crypto-reviewer` pass + a manual round-trip against a real `surfc`-written ciphertext |
| The **sync engine + local store** (Phase 2, SUR-659) — `src/store.rs`, `src/sync/**`, `src/outbox.rs`, `vendored/schema/**`, `scripts/extract-sync-schema.mjs` | **GCE only** | Schema-drift guard green — `vendored/schema/**` reconciles against `surfc/main`'s synced schema (`tests/schema_parity.rs` + `.github/workflows/schema-drift.yml`) — + founder sign-off | Founder sign-off after `sync-reviewer` (engine, PWA↔native coexistence, schema drift) **+** `crypto-reviewer` (the seal-at-flush boundary — note text must never leave unencrypted, SUR-724) pass |
| `bindings/**`, `src/bin/uniffi-bindgen.rs` — the generated Swift/Kotlin surface + its round-trip tests (the public API devs consume) | **GCE only** | Swift **and** Kotlin round-trip parity green + founder sign-off | Founder sign-off after `naming-reviewer` (the API *word*) **+** `crypto-reviewer` (the seam) pass |
| `vendored/crypto-parity/**` — the crypto parity vectors vendored from `surfc/main` | **GCE only** | Vendored-drift guard green — byte-identical to `surfc/main` (§4) — + founder sign-off | `crypto-reviewer` confirms the vectors against `surfc/main` |

### 3.2 Meta / docs / CI paths

| Path | Pattern | Primary gate | Fallback gate (until primary exists) |
|---|---|---|---|
| `docs/adr/**` — architecture decision records (e.g. ADR 0002, the crypto backend) | **GCE only** | Founder sign-off | Founder sign-off after an `architecture-decision-reviewer` pass |
| `.github/workflows/**` — the parity / vendored-drift / schema-drift / changelog / nightly gates themselves | **GCE only** | Founder sign-off — these set the rules | — |
| `GATING.md` (this file), `CLAUDE.md`, `README.md`, `CHANGELOG.md` | **GCE only** | Founder sign-off | — |

**Reviewer overlays the path globs can't isolate.** The `src/**` row is the most general,
so the line's classifier (first-match) attributes a change under `src/` to it. Two surfaces
inside `src/` need an **extra** reviewer that path-globbing can't separate out, so treat them
as overlays on top of `crypto-reviewer`:
- a change to `src/store.rs` / `src/sync/**` / `src/outbox.rs` (or the schema fixture) pulls
  in **`sync-reviewer`** (the sync/store row);
- a change to an exported **type / method / error name** in a `#[uniffi::export]` item
  (chiefly `src/vault.rs`, `src/lib.rs` — this crate has no `.udl`) pulls in **`naming-reviewer`**.

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

- **`naming-reviewer` repo-profile.** The concern-keyed `naming-reviewer` wants an injected
  `gce/shared/personas/repo-profiles/braird-core.md` (developer-facing API-naming mode — the
  audience is iOS/Android integrators, not end users). **It does not exist yet** — a small
  follow-up in `gce/` before the first binding-surface review. Until then, run
  `naming-reviewer` self-contained against the API-naming concern and note the gap.
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
   `surfc/main`, and (for store/schema changes) `vendored/schema/**` reconciles against
   `surfc/main`'s synced schema. See `.github/workflows/`.
6. `crypto-reviewer` (+ `naming-reviewer` for binding changes, + `sync-reviewer` for
   store/schema changes) has passed, or its findings are explicitly accepted with rationale.
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
- `bindings/{swift,kotlin}/` — the generated UniFFI surface + round-trip tests (produced
  from the `#[uniffi::export]` items via `src/bin/uniffi-bindgen.rs`).
- Persona prompts — in the sibling `gce/` repo (`shared/personas/`), referenced by name
  from §3.

---

*Implements ADR 0001 (Accepted, surfc#331). Anchored by SUR-716 (Phase 1 impl), part of
epic SUR-656. GATING §3 reshape tracked by SUR-728 (enabling braird-core for the GCE line).*
