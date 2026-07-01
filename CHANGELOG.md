# Changelog

All notable changes to braird-core are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); every PR to `main` must add an
entry under `[Unreleased]` (CI-enforced, dependabot-exempt).

## [Unreleased]

### Changed
- **GATING.md §3.1 row order (SUR-724).** Reordered so the specific rows (sync engine,
  bindings, crypto-parity) precede the general `src/**` catch-all, and added `src/http.rs`
  to the sync row. The line's classifier (`gce/src/classify-paths.ts`) is **first-match**, so
  `src/**` listed first shadowed the sync/binding rows and silently dropped `sync-reviewer` /
  `naming-reviewer` from persona selection. Prose replaced the "overlay" workaround with the
  ordering rule.
- **GATING.md restructured for the GCE line (SUR-728).** Moved the path→pattern→gate table
  from §2 to **§3** with the canonical four columns (Path · Pattern · Primary gate · Fallback
  gate), so the line's classifier `gce/src/read-gating.ts` — which parses §3 **only** — reads
  braird-core's gates (a §2 table was invisible to it, silently ungating the repo). All seven
  rows preserved, including the SUR-723 sync/store row; grounded to `main` (dropped the
  non-existent `*.udl` / `build.rs`; the binding surface is `#[uniffi::export]`). `CLAUDE.md`
  Layout grounded to match. Verified: the gce parser reads 7 §3 rows and all named personas
  (`crypto-reviewer`, `sync-reviewer`, `naming-reviewer`, `architecture-decision-reviewer`) resolve.
- ADR 0002 (crypto backend: RustCrypto over ring/aws-lc-rs) accepted — crypto-reviewer + founder sign-off (SUR-716 gate).
- **GATING.md:** activated `sync-reviewer` (Phase 2) — added the sync-engine/local-store path row (`src/store.rs`, `src/sync/**`, `vendored/schema/**`, `scripts/extract-sync-schema.mjs`) and removed it from "Not yet in scope" (SUR-723).
- **ADR 0002:** recorded the `rusqlite` (bundled SQLite) dependency choice as a decision note — reversible/routine, folded into the existing core-impl ADR rather than a standalone one (SUR-723).

### Fixed
- **Pull no longer lets a stale outbox edit re-push over a newer server row (SUR-736).** When a pull
  merges a strictly-newer remote row for a record that still has a queued local edit, that edit is now
  dropped from the outbox in the SAME transaction as the apply (`Store::apply_row_rebasing_outbox`).
  Previously it survived and the next unconditional `flush()` re-pushed it over the newer server row (a
  lost remote edit). `flush()`-before-`pull()` is therefore no longer required to avoid this. Only
  entries whose payload `updated_at <= incoming` are dropped (a genuinely-later local edit still
  flushes; a malformed payload is left queued). Note: this does NOT fix SUR-740 — a flush destroying a
  newer *server* row before a pull can see it is the server's job (tracked separately, PR-3).
- **Outbox collapse no longer resurrects a soft-deleted record (SUR-724).** `collapse()` tracked
  `deleted` stickiness per-item, so a delete followed by a normal edit — which the enqueue paths
  stamp with `deleted: false` — had its `deleted: true` overwritten by the field-merge and flushed
  as un-deleted. Stickiness is now accumulated across the group (read from the accumulator before
  the merge), so within a batch a delete wins and can't be resurrected. Two regression tests added.
  The identical latent hole in surfc's PWA `collapseOutboxItems` is filed as SUR-731.
- **ADR numbering collision fixed (SUR-725).** SUR-724 (PR#7) landed the async-HTTP-client ADR as a
  local `0001`, colliding with the repo's unqualified "ADR 0001" = the founding Rust+UniFFI decision
  (surfc#331, referenced in `GATING.md`, ADR 0002/0003, `src/store.rs`). Renumbered to
  `docs/adr/0004-async-http-client.md` so the architecture chain resolves to the right document.

### Added
- **One-call `sync()` + superseded-edit signal (`src/sync/mod.rs`, `src/sync/pull.rs`, `src/store.rs`,
  SUR-736 / SUR-738):** a new `SyncEngine.sync()` UniFFI method pulls THEN flushes — a deliberate
  divergence from the oracle's flush-first (with the outbox rebase, pulling first rebases a stale edit
  away so the flush pushes nothing stale; flush-first would re-push it). **The flush is aborted unless
  the pull was fully clean** — if any table's pull fails (partial OR total), `sync()` errors and does
  NOT flush, so a table that never rebased can't re-push a stale edit over a newer server row (the
  partial-failure hole). `PullSummary` gains `superseded: Vec<SupersededEdit>` (`table` + `record_id` +
  discarded/winning `updated_at` — ids + timestamps only, never payload contents) so a host can tell
  the user an offline edit lost last-write-wins to a newer remote row. New FFI records `SupersededEdit`
  + `SyncSummary`. **Ciphertext-at-rest unchanged** — the rebase touches only already-sealed outbox
  rows; nothing is decrypted or logged. New offline integration test (`tests/sync_736_integration.rs`,
  recording sink) proves the re-push window is closed, a genuinely-newer local edit still flushes, and
  a partial pull failure aborts the flush. **Native-only** (gated off wasm32).
- **Regenerated Swift + Kotlin bindings (`bindings/swift/**`, `bindings/kotlin/**`, SUR-736):** the
  committed UniFFI bindings now reflect the full FFI surface. They had only ever carried `Vault` — the
  `SyncEngine` handle (SUR-724) + `pull()`/`PullSummary` (SUR-725) had never been regenerated into the
  committed API, so native clients couldn't call sync at all. Regenerated from the compiled library via
  `cargo run --bin uniffi-bindgen` (swiftformat/ktlint unavailable on the dev box → raw uniffi output;
  the macOS/Kotlin CI legs are the compile + round-trip validation, opted in via `touches-ffi`).
- **Incremental pull + tombstones + first coexistence (`src/sync/pull.rs`, `src/store.rs`,
  `src/sync/http.rs`, SUR-725 / SUR-659c):** the `SyncEngine.pull()` UniFFI method mirrors surfc's
  `fetchSince` + `mergeCloudRecords` on `books` + `notes` — per table it GETs rows with
  `updated_at >= cursor` (inclusive, like the JS `.gte`), merges **last-write-wins by `updated_at`**
  (strict `>`, so a tie keeps local), applies **tombstones** (an incoming `deleted:1` is written but
  a soft-deleted row is never *resurrected* — a delete for a row this device never had is skipped),
  and advances a **per-table** cursor (in `meta`, `sync:cursor:<table>`) to the puller's own
  pre-fetch `now()`. **Note text stays ciphertext at rest** — pull stores `enc:v2` verbatim and never
  decrypts (the inverse of push's seal-at-write; the host decrypts on demand via
  `Vault::decrypt_note`). New store helpers `get_row` / `apply_row` (descriptor-driven, projecting
  out the server-only `user_id` + any future additive column) + `get_sync_cursor` / `set_sync_cursor`,
  and the deferred per-table `updated_at` index now lands with its read path. **Offline-first (§4):**
  local writes (`enqueue_book` / `enqueue_note`) now hit the synced table AND the outbox before any
  cloud call. Per-table failure isolation: one table's fetch failing leaves its cursor unadvanced
  (re-pulls next time) while others proceed. **Cursor value decided:** the puller's `now()`, NOT
  `max(updated_at)` — `updated_at` is client-authored (no server trigger; verified in surfc
  migrations 0001…), so a batch max would inherit writer clock skew. Proven on `books` + `notes`;
  the other six tables follow in SUR-726 by extending the pull table list. **Native-only** (gated
  off wasm32). New env-guarded integration test (`tests/sync_725_integration.rs`) proves server→core
  coexistence, ciphertext round-trip + `content_tag`, and tombstone apply / no-resurrect against a
  real local Supabase.
- **ADR 0004 — async HTTP client (`docs/adr/0004-async-http-client.md`, SUR-724 / SUR-659b):**
  records the reqwest + tokio `current_thread` + rustls decision behind the sync push layer — the
  runtime is owned by the `SyncEngine` handle (`block_on` per flush, no background thread), and
  rustls is chosen for iOS/Android TLS portability. Underpins ADR 0003 §Decision 5 (the sync FFI
  runs sync and `block_on`s this client). (Renumbered from a mistaken local 0001 by SUR-725.)
- **Sync engine — outbox + push/flush + token handoff (`src/sync/**`, SUR-724 / SUR-659b):** the
  `SyncEngine` UniFFI handle enqueues writes, seals note text **at write** (enc:v2 ciphertext +
  a plaintext-derived `content_tag`, so no plaintext note text is ever persisted), and flushes to
  Supabase via its OWN authenticated PostgREST calls (`set_access_token` hands it a real GoTrue
  JWT; `user_id` = the token's `sub`). Flush mirrors surfc's `flushOutbox`: collapse (LWW-per-field,
  sticky delete, transitive `bookIdRemap` persisted in `meta`) → books first → notes (book_id
  repointed; a note whose parent book flush failed stays queued → no server FK violation) →
  failed writes stay in the outbox. `updated_at` is stamped in epoch **ms** at enqueue. The FFI
  stays synchronous — the engine owns a tokio current-thread runtime and `block_on`s the async
  reqwest calls (same shape as `Vault`). Proven on `books` + `notes`; the other six synced tables
  follow in SUR-659c/d behind the same flush. **Native-only** — reqwest/tokio/rusqlite are
  target-gated off wasm32 (the WASM CSPRNG build stays green).
- **Cargo workspace + `test-support` member crate (SUR-724):** the crate is now a workspace
  (`.` + `test-support`); `test-support` exposes `mint_test_user_jwt()` (authenticates a real
  test user against local Supabase GoTrue → a real token) + the Supabase bootstrap/select helpers,
  reused across SUR-659b/c/d and the future bindings crate (a `tests/common/mod.rs` only shares
  within one crate's test binaries). Native-only.
- **Sync integration CI (`sync-integration.yml`, SUR-724):** a SEPARATE job (keeps `parity.yml`
  fast) that spins up local Supabase from surfc's migrations, then runs the `#[ignore]`d
  `tests/sync_659b_integration.rs` — asserting collapse semantics, that only **ciphertext**
  reaches `notes.text` (never plaintext), that `content_tag` is present + correct, and that the
  token-handoff upsert succeeds. `cargo test` skips it gracefully when `SUPABASE_URL` is absent.
- **Native SQLite local store (`src/store.rs`, SUR-723 / Phase 2):** the on-device mirror of
  surfc's synced cloud schema for the iOS/Android clients — the 8 synced stores (`books`,
  `notes` incl. `content_tag`+`chapter`, `custom_ideas`, `note_links`, `lenses`, `collections`,
  `collection_memberships`, `note_signals`), each carrying `updated_at`+`deleted`, plus the 4
  local-only stores (`meta`, `outbox`, `embeddings`, `discovery_jobs`). `rusqlite` with
  `bundled` SQLite is **target-gated off wasm32** (the PWA keeps Dexie; the WASM CSPRNG build
  stays green). `user_id` is auth-injected at push, never stored (mirrors Dexie).
- **Synced-schema drift guard (§7, SUR-723):** `vendored/schema/sync-schema.json` (the canonical
  synced `(column, logical-type)` set) generated by `scripts/extract-sync-schema.mjs` from
  surfc/main's `supabase.js` `upsert*` payloads (column set) + migrations (types);
  `tests/schema_parity.rs` reconciles the core descriptor against it, and
  `.github/workflows/schema-drift.yml` re-derives it from surfc/main per-PR + weekly — failing
  CI if a synced column is added/removed/retyped without re-vendoring (the silent-desync guard).
- **Mobile cross-compile smoke CI (SUR-723):** per-PR `cargo build` for `aarch64-apple-ios`
  (macOS) + `aarch64-linux-android` (NDK) in `parity.yml`, so a bundled-SQLite C-compile break
  on a device triple fails the PR rather than surfacing at the next nightly.
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
- **Normalization differential fuzz (B6):** `tests/normalize_oracle.mjs` (V8 `normalizeForTag`
  oracle) + a `#[cfg(test)]` fuzz in `src/normalize.rs` that diffs the Rust port against the
  oracle over a deterministic 20,000-input Unicode-diverse corpus (astral/emoji, combining
  marks, full-width, controls, the `\p{P}`/`\p{Zs}` families, case-fold hotspots, ES
  whitespace). Result: **0 mismatches**. A fence categorizes any 16.0↔17.0 `\p{P}`/`\p{Zs}`
  residue (none hit). The parity workflow pins Node v24.15.0 and verifies the Unicode 17.0
  anchor before running.
- **Zeroization demonstration (criterion #7):** unit test proving the `Vault`'s
  `Zeroizing<[u8;32]>` Master Key wrapper actually wipes its bytes (read-back through a live
  pointer after `zeroize()`); Rust's stable addresses mean no GC can leave an un-wiped copy.
- **Production bindings (B5):** `scripts/build-xcframework.sh` (macOS + iOS + iOS-sim
  arm64 → `BrairdCore.xcframework`) and the generated Swift API; `bindings/swift` SwiftPM
  package + round-trip test; `bindings/kotlin` Gradle project (self-builds the cdylib +
  regenerates the binding, JNA-loaded) + round-trip test. Both round-trips decrypt FOREIGN
  JS-produced ciphertext and reproduce all 10 content tags byte-for-byte through the FFI.
  Swift verified via `swift test`; Kotlin verified via `kotlinc` + JNA offline (this box's
  JVM egress is firewalled, so `./gradlew test` runs in CI). Activates the `kotlin-roundtrip`
  + `nightly-macos` jobs.
