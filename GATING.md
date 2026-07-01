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

The default, and the only tier here: **everything → GCE.**

---

## 2. Path → reviewer → gate

The named personas resolve from the sibling `gce/` repo (`shared/personas/<name>.md`).

| Path | Primary gate | Reviewer persona(s) |
|---|---|---|
| `src/**`, `Cargo.toml`, `Cargo.lock`, `build.rs`, `*.udl` — the crate + every line of crypto | **GCE only** | `crypto-reviewer` (whole crate) + founder sign-off |
| The **sync engine + local store** (Phase 2, SUR-659) — `src/store.rs`, `src/sync/**`, `src/outbox.rs`, the synced-schema fixture `vendored/schema/**`, `scripts/extract-sync-schema.mjs` | **GCE only** | `sync-reviewer` (engine, PWA↔native coexistence, schema-drift guard) **+** `crypto-reviewer` (the seal-at-flush boundary — note text must never leave unencrypted, from SUR-724) + founder |
| The **public binding surface** — `#[uniffi::export]` items / `*.udl`, `bindings/**`, exported type & method & error names | **GCE only** | `naming-reviewer` (the *word* devs consume) **+** `crypto-reviewer` (the seam) + founder |
| `vendored/crypto-parity/**` — the parity vectors vendored from `surfc/main` | **GCE only** | `crypto-reviewer` — must match `surfc/main` (drift-guarded in CI; see §4) |
| `docs/adr/**` — architecture decision records (e.g. ADR 0002 crypto backend) | **GCE only** | `architecture-decision-reviewer` + founder |
| `.github/workflows/**` — the parity / drift / changelog / nightly gates themselves | **GCE only** | founder sign-off (these set the rules) |
| `GATING.md` (this file), `CLAUDE.md`, `README.md`, `CHANGELOG.md` | **GCE only** | founder sign-off |

**Test-only seams are a crypto-reviewer BLOCKER if they reach the public binding.**
`with_raw_mk` (construct-from-raw-hex) and any IV/salt override on `wrap_with_prf` MUST be
`#[cfg(test)]` / `__test`-gated out of the generated Swift/Kotlin surface — a leaked
nonce/salt override is a catastrophic GCM nonce-reuse footgun. The `naming-reviewer` also
flags these if they surface in the binding.

**Frozen wire-format constants (crypto-reviewer verifies verbatim):** the `surfc-*` HKDF
info strings (`surfc-master-key-wrap-v1`, `surfc-content-tag-v1`), the 600 000 PBKDF2
iteration count, standard (not URL) base64, the 12-byte IV, the **64-byte** content-tag
HMAC subkey, and the `enc:v1`/`enc:v2`/`0x02` headers. These are protocol constants, not
branding — they stay `surfc-*` despite the Braird rename (SUR-680 allowlist).

### Not yet in scope

- **`naming-reviewer` repo-profile.** The concern-keyed `naming-reviewer` needs an injected
  `gce/shared/personas/repo-profiles/braird-core.md` (developer-facing API-naming mode —
  the audience is iOS/Android integrators, not end users). **It does not exist yet** — a
  small follow-up in `gce/` before the first binding-surface review. Until then, run
  `naming-reviewer` self-contained against the API-naming concern and note the gap.
- **`simplicity-reviewer`** is optional and size-gated; it **defers to `crypto-reviewer`**
  wherever simplification and safety collide. Advisory, never a blocker here.

---

## 3. Triage for new paths

This repo is all-spine, so the triage is short. Anything that touches keys, ciphertext,
key derivation, the parity vectors, the FFI surface, or the CI gates → **GCE**. There is
no "surface" answer. When something genuinely has no crypto bearing (a typo in this file),
it is still GCE-trivial: founder sign-off, no persona pass needed.

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
5. **The vendored-drift guard is green** — `vendored/crypto-parity/**` is byte-identical
   to `surfc/main` (the `surfc-evals/vendored/*` pattern; see `.github/workflows/`).
6. `crypto-reviewer` (+ `naming-reviewer` for binding changes) has passed, or its findings
   are explicitly accepted with rationale.
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
- `docs/adr/**` — architecture decision records. ADR 0001 (Rust+UniFFI; in `surfc`)
  established this repo; ADR 0002 records the crypto-backend choice (RustCrypto).
- `vendored/crypto-parity/` — parity vectors vendored from `surfc/main`, drift-guarded.
- Persona prompts — in the sibling `gce/` repo (`shared/personas/`), referenced by name
  from §2.

---

*Implements ADR 0001 (Accepted, surfc#331). Anchored by SUR-716 (Phase 1 impl), part of
epic SUR-656. Blocked-by SUR-658; blocks SUR-659/660/661.*
