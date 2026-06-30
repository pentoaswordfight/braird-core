# ADR 0002 — Crypto backend: RustCrypto (pure Rust)

- **Status:** Accepted (crypto-reviewer + founder sign-off, 2026-06-30 — SUR-716 gate)
- **Date:** 2026-06-30
- **Context tickets:** SUR-716 (Phase 1 impl), implements ADR 0001 (Rust+UniFFI, surfc#331)
- **Supersedes / superseded by:** none

## Context

braird-core must run the *same* AES-GCM / HKDF / HMAC / PBKDF2 primitives on four
surfaces — iOS (Swift over UniFFI), Android (Kotlin over UniFFI), the PWA
(`wasm32-unknown-unknown` in an MV3 service worker), and CI (Linux). The SUR-658 gate
review left an explicit condition: **evaluate `aws-lc-rs` / `ring` vs RustCrypto** for the
production core (audited, hardware AES-GCM, FIPS-track, constant-time), covering paths the
spike did not exercise — `rewrapMasterKey` and **WASM CSPRNG sourcing**.

## Options considered

| | RustCrypto (`aes-gcm`, `hkdf`, `hmac`, `sha2`, `pbkdf2`) | `ring` | `aws-lc-rs` |
|---|---|---|---|
| `wasm32-unknown-unknown` | ✅ pure Rust, builds clean (verified) | ⚠️ C/asm; wasm support partial/awkward | ⚠️ AWS-LC C core; heavy wasm friction |
| iOS arm64 / Android | ✅ | ✅ | ✅ (larger artifact) |
| C/build toolchain dep | none | C + asm | CMake + C |
| Constant-time | yes (subtle/RustCrypto) | yes | yes |
| Hardware AES-GCM | software (or `aes` intrinsics where available) | yes | yes |
| Audited / FIPS | community-audited | BoringSSL lineage | FIPS-track |

## Decision

**RustCrypto**, pure Rust, for all primitives:
`aes-gcm 0.10`, `hkdf 0.12`, `hmac 0.12`, `sha2 0.10`, `pbkdf2 0.12`, plus
`zeroize` for MK lifetime hygiene. CSPRNG via `getrandom` — with the `js` feature on
`wasm32` so the PWA build sources entropy from the browser `crypto.getRandomValues`.

### Why

1. **WASM is a first-class target, not an afterthought.** The PWA shares this core. A
   C-dependent backend (`ring` / `aws-lc-rs`) turns the wasm build from "it just works"
   (verified: `cargo build --target wasm32-unknown-unknown` is green) into a toolchain
   project. The one backend that builds identically on all four surfaces wins.
2. **No C/asm build dependency** keeps CI, the xcframework packaging, and the wasm slice
   reproducible and supply-chain-narrow.
3. **Byte-for-byte parity is the actual security property here**, and it is enforced by
   the vendored golden vectors regardless of backend. The 19 in-scope vectors reproduce
   bit-identically on RustCrypto (verified).
4. **`rewrapMasterKey` + WASM CSPRNG** (the spike's gaps) are both covered: re-wrap is the
   `Vault::rewrap` path, and wasm randomness is `getrandom`'s `js` backend.

### Accepted trade-off

RustCrypto AES-GCM is software (it uses CPU AES intrinsics via the `aes` crate where the
target exposes them, but is not the hand-tuned assembly of BoringSSL/AWS-LC). For a client
that wraps a single Master Key and seals note text, throughput is not the constraint;
portability and parity are. If a future profile shows AES throughput matters on a specific
native target, a backend swap can be revisited **behind the unchanged parity contract** —
the vectors would catch any divergence a swap introduced.

## Consequences

- One dependency set across iOS, Android, wasm, and CI.
- FIPS certification is not on the table with RustCrypto; if a future requirement demands
  it, `aws-lc-rs` on native-only (keeping RustCrypto for wasm) is the fallback, gated by a
  new ADR.
- Constant-time guarantees rely on RustCrypto's `subtle`/implementation choices; the
  crypto-reviewer audits this as part of the SUR-716 gate.

## Open (carried to the gate)

- **Normalization Unicode skew** (separate from the backend choice, but parity-relevant):
  `normalizeForTag`'s `\p{P}`/`\p{Zs}` classification uses `unicode-general-category` 1.1.0
  = **Unicode 16.0**, while the V8/Node parity anchor + std `char`/`unicode-normalization`
  are **17.0**. No real-tables General_Category crate is at 17.0 yet. The current vectors
  are unaffected; the B6 differential fuzz must characterize the residue. See
  `src/normalize.rs`.
