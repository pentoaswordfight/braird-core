# braird-core

The shared cryptographic core for [Braird](https://surfc.app)'s native clients.

One Rust crate, exposed over [UniFFI](https://mozilla.github.io/uniffi-rs/) as a stateful
`Vault` handle to **Swift** (iOS) and **Kotlin** (Android), and compiled to **WASM** for the
PWA's MV3 service worker. It is the single implementation of the end-to-end-encryption
primitives that every Braird client shares.

## Why a shared core

Braird notes are end-to-end encrypted: note text is sealed with a 256-bit AES-GCM Master
Key (MK) that never reaches the server in plaintext. As the product grows from a PWA to
native iOS/Android apps, the crypto must be **identical across every client** — a note
encrypted in the browser has to decrypt on a phone, and vice-versa. Re-implementing the
same AES-GCM/HKDF/HMAC/PBKDF2 logic three times (JS, Swift, Kotlin) invites silent
divergence. One audited Rust core, bound to each platform, removes that risk.

## Security model

- The **Master Key never crosses the FFI as raw bytes.** The `Vault` owns it in memory as
  `Zeroizing<[u8;32]>`; hosts call `vault.encrypt_note(...)` and never receive key material.
  It is never logged and is zeroized after use.
- **Byte-for-byte parity** with the reference implementation is the core's contract,
  enforced in CI against a frozen set of cross-client test vectors. A change that diverges
  from the reference by a single byte fails the build.
- Pure-Rust [RustCrypto](https://github.com/RustCrypto) primitives (constant-time,
  WASM-portable) — `aes-gcm`, `hkdf`, `hmac`, `sha2`, `pbkdf2`. Backend rationale: see
  `docs/adr/0002-crypto-backend-rustcrypto.md`.

## What it implements

- **Master Key lifecycle** — generate, wrap/unwrap (HKDF-SHA256), multi-device re-wrap,
  PIN-based transfer (PBKDF2-SHA256).
- **Note encryption** — AES-GCM-256 with two wire formats (`enc:v1`, and `enc:v2` bound to
  the note id via AAD).
- **Content tags** — HMAC-SHA256 over Unicode-normalized text, for opaque client-side
  matching.
- **Embedding seal** — at-rest AES-GCM protection for device-local binary blobs.

## Build & test

Requires the Rust toolchain; the native bindings additionally require macOS (Xcode) and a
JDK/Kotlin toolchain.

```bash
cargo test          # run the parity harness + unit tests
cargo build         # build the core
# Bindings (macOS): see CLAUDE.md for the uniffi-bindgen → Swift/Kotlin → xcframework flow.
```

## Project status

Phase 1 of the native-client epic. The crypto core, bindings, and CI parity gate are built
under SUR-716; the sync engine and local store follow in Phase 2 (SUR-659).

## License

_TBD._
