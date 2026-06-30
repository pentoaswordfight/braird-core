# Changelog

All notable changes to braird-core are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); every PR to `main` must add an
entry under `[Unreleased]` (CI-enforced, dependabot-exempt).

## [Unreleased]

### Added
- Repository scaffolding: `GATING.md` (all-spine GCE policy), `CLAUDE.md` (agent context),
  `README.md`, and CI workflows (`parity`, `vendored-drift`, `changelog-check`,
  `nightly-macos`). Anchored by SUR-716.
- **Crypto core (`src/`)** mirroring `surfc/src/crypto/*` + `src/lib/text.js` byte-for-byte:
  MK generate / PRF wrap+unwrap / re-wrap / PIN transfer (PBKDF2-SHA256 @ 600k), `enc:v1`
  and `enc:v2` (AAD = noteId) note sealing, the HMAC-SHA256 content tag (64-byte HKDF
  subkey), `normalizeForTag`, and the `0x02` embedding seal. Frozen `surfc-*` HKDF info
  strings + the 600k count preserved verbatim (SUR-680 allowlist); standard base64.
- **`Vault` UniFFI handle** (Option B): owns the Master Key in `Zeroizing` memory behind a
  `Mutex`; the MK never crosses the FFI as raw bytes. Production salt/IV are generated
  in-core. The `with_raw_mk` constructor + fixed-salt/IV overrides + raw-MK readback are
  `--features test-seams`-only and **absent from the generated Swift/Kotlin bindings**
  (verified) — closing the naming-reviewer GCM-nonce-reuse-footgun condition.
- **`normalizeForTag` on real Unicode-property tables** (not the spike's hand-coded
  ranges): `\p{Cc}` via std `char::is_control` (Unicode 17.0), NFKC + lowercase via std /
  `unicode-normalization` (17.0), `\p{P}`/`\p{Zs}` via `unicode-general-category` (16.0).
  The 16.0↔17.0 `\p{P}` skew vs the V8/Node anchor is documented (`src/normalize.rs`,
  ADR 0002) as the one residual for the B6 differential fuzz to characterize.
- **`vendored/crypto-parity/`** fixtures (vendored byte-identical from `surfc/main`) and the
  Rust parity harness (`tests/parity.rs`, `--features test-seams`): all **19 in-scope
  golden vectors bit-identical**, plus foreign-ciphertext decrypt (PWA→native coexistence)
  and production random-IV round-trips. `legacy-note` is JS-only and skipped.
- **ADR 0002** — crypto backend decision (RustCrypto over `ring`/`aws-lc-rs`; WASM
  portability + CSPRNG via `getrandom` `js`).
