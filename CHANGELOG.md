# Changelog

All notable changes to braird-core are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); every PR to `main` must add an
entry under `[Unreleased]` (CI-enforced, dependabot-exempt).

## [Unreleased]

### Added
- **Tri-state field clearing over the enqueue FFI (SUR-775).** `enqueue_book` / `enqueue_note` gain
  a `clear_nullable_fields: Vec<String>` parameter — the third state past SUR-741's keep (`None`) / set
  (`Some`) pair. A column named in `clear_nullable_fields` is written as an explicit JSON `null`, which flows
  unchanged through the local `stage_local_write` merge (→ SQL NULL) and the flush (→ server column
  patched NULL under `merge-duplicates`), so a native host can now clear a field back to NULL (e.g.
  remove a book's `isbn`/cover, unlink a note from its book, drop a `chapter`). Clearable columns are
  restricted to the surfc `upsert*` `?? null` set (books: `isbn`, `cover_url`, `cover_source`,
  `cover_resolved_at`; notes: `book_id`, `chapter`, `image_path`, `ink_crop_path`, `source_id`) so a
  clear stays a wire shape the PWA can also produce and merge (byte-for-byte parity). `page`/`author`
  (`|| ''`) are deliberately not NULL-clearable — clearing those is `Some("")`; `text` (sealed) and
  `content_tag` (derived) are never clearable. A non-clearable/unknown name, or a column both set and
  cleared, is rejected up front and **nothing is staged** (host-supplied names are kept out of the
  FFI error text). **Binding-surface change** — Swift + Kotlin bindings regenerated (`touches-ffi`).

## [0.1.0] - 2026-07-03

First tagged release. Cuts the accumulated `[Unreleased]` history into `v0.1.0` so
braird-android (SUR-762) has a real published artifact — Android AAR + desktop JVM jar +
`SHA256SUMS.txt` — to pin (tag + per-artifact SHA-256, checksum-verified fetch; `docs/pinning.md`).
No code change vs the prior `main`; this is the release-cut commit only.

### Added
- **Android AAR + self-contained desktop JVM jar packaging, published + pinned via GitHub Releases
  (SUR-760, M0 of the SUR-661 Android app).** The core now ships to braird-android as pinned
  artifacts — no vendoring of core source.
  - **`bindings/android/`** — a new AGP `com.android.library` module (compileSdk 35, minSdk 28)
    that assembles the **AAR**: the single committed UniFFI Kotlin binding (reused from
    `bindings/kotlin` via a srcDir — not duplicated) + per-ABI `libbraird_core.so` for
    **arm64-v8a + x86_64**, every LOAD segment **16 KB-aligned** (targetSdk 35 Play requirement).
    The consumer adds JNA `5.17.0@aar` (ships the aligned `libjnidispatch.so`) alongside.
  - **Self-contained desktop jar** — `bindings/kotlin`'s `jar` now bundles the host
    `libbraird_core` at JNA's classpath-resource path, so a consumer (braird-android's JVM unit
    tests) resolves the native from the jar with **no `jna.library.path`** and no local cargo
    build. `bindings/consumer-smoke` is an external-style project that proves it (round-trip with
    the jar as its only dependency; UniFFI's checksum guard makes it double as the binding↔native
    atomicity check). Release jars carry the **linux-x86-64** native (braird-android CI runs on Linux).
  - **`scripts/build-aar.sh`** (mirrors `build-xcframework.sh`) — refresh binding → cargo-ndk both
    ABIs, 16 KB-aligned via a **pinned NDK r28.2** → AGP assemble → fail if any bundled `.so` is
    under 16 KB-aligned.
  - **CI** — `parity.yml`'s Android smoke now covers **x86_64** as well as arm64; new
    **`android-artifacts.yml`** gates the AAR (alignment) + desktop-jar self-containment per-PR;
    new **`release.yml`** publishes both + a `SHA256SUMS.txt` to a `v*` tag's GitHub Release,
    fail-closed on tag / `Cargo.toml` version / CHANGELOG disagreement.
  - **`docs/pinning.md`** — the pin/bump protocol: pin tag **+ SHA-256 per artifact**,
    checksum-verified (fail-closed) fetch, `chore(core): pin braird-core vX.Y.Z` app-repo PR is the
    integration gate; no floating `latest`, no tag-only pin. Written artifact-agnostically so the
    future iOS xcframework release inherits it.
  - **JNA 5.14.0 → 5.17.0** across both binding paths.
- **`release-integrity-reviewer` gate row for the release/packaging boundary (SUR-778).**
  GATING.md §3.1 now routes `scripts/build-aar.sh`, `scripts/build-xcframework.sh`,
  `.github/workflows/release.yml`, and `docs/pinning.md` to the new `release-integrity-reviewer`
  persona (authored in gce, SUR-778): binding↔native atomicity, tag + SHA-256 pinning,
  fail-closed checksum-verified fetch, 16 KB alignment gates. Pre-wired ahead of SUR-760 (the
  first release pipeline) so the line auto-selects the persona for that PR's review; the fallback
  gate stands until release CI exists.
- **Read/query API over the FFI + in-memory lexical search (SUR-744, Phase 2b).** The first read
  surface on the core — hosts can now list and search books/notes/ideas without ever touching the
  core's SQLite (unblocks SUR-660 M6 / SUR-754). New `#[uniffi::export]` methods on `SyncEngine`:
  `list_books`, `get_book`, `list_notes` (`book_id: None` = the Commonplace flat list, `Some` =
  per-book), `get_note`, `list_custom_ideas`, `counts`, and `search`; plus the `BookRecord`
  (with a live `note_count`), `NoteRecord`, `CustomIdeaRecord`, `StoreCounts`, `SearchHit`, and
  `SearchDocKind` DTOs. All reads exclude soft-deleted rows, order `created_at DESC`, and paginate
  on `limit`+`offset`.
  - **Decrypt-in-core (crypto boundary).** `NoteRecord.text` is **plaintext** — decrypted per-read
    via the held `Vault`, so `enc:` ciphertext can never cross the FFI for display. A corrupt /
    foreign-AAD row surfaces as `text: None, decrypt_failed: true` and is excluded from the search
    index, never failing the whole page (mirrors the PWA's `decryptError` skip). Nothing is written
    back to the store — ADR 0003's ciphertext-at-rest posture holds on the read side (ADR 0005).
  - **Lexical search = a MiniSearch port, verdicts exact.** `src/search.rs` reproduces the PWA's
    `lexicalSearch.js` (SUR-527) matching — the `stem()`/`undouble()` stemmer ported verbatim, the
    `\p{Z}\p{P}` tokenizer (reusing `normalize.rs`'s `unicode-general-category` tables — no new
    dep), and exact ∪ prefix ∪ fuzzy(Levenshtein) OR-matching with a 2× title boost. **Not FTS5**
    (its Porter stemmer diverges and it has no fuzzy). Index is **in-memory, rebuilt per `search()`**
    — no plaintext note text ever reaches disk. Scope: notes + custom_ideas (books aren't indexed
    by the PWA; lenses/collections have no v1 read surface). Decision recorded in **ADR 0005**.
  - **Store:** two table-generic read helpers — `Store::list_live` (paginated, soft-delete-filtered,
    optional single-column filter for notes-by-book) and `Store::count_live`; a structural
    `note_encryption::is_encrypted` (mirror of the PWA's `isEncrypted()`).
  - **FFI:** new binding surface → regenerated Swift + Kotlin via `scripts/gen-bindings.sh`; Swift +
    Kotlin round-trip tests exercise list/get/counts/search (incl. the no-`enc:`-sentinel guard).
    New surface → `naming-reviewer` + `crypto-reviewer` gate.
- **`docs/learnings/` — Phase-2 (SUR-659) closeout lessons.** Seed the learnings register (with the
  `_template.md` GATING.md references) and record the two non-obvious keepers from the fast-follows:
  a unique/monotonic sequence is **not** a commit-ordered watermark (`nextval` allocates
  non-transactionally → keyset skip; SUR-743), and UniFFI folds **docstrings** into per-method
  checksums so a doc-only edit drifts the committed bindings while the runtime guard can't see a
  missing symbol (SUR-742). Docs-only; no code, schema, or binding change.
- **FFI bindings-drift guard (SUR-742).** New `scripts/gen-bindings.sh` — the single canonical
  UniFFI bindgen invocation: builds the library and regenerates the committed Swift + Kotlin
  bindings in library mode with **`--no-format`**, so output is deterministic across hosts (no
  ktlint/swiftformat version drift → no spurious diffs; the committed bindings are now
  script-produced by definition). New `bindings-drift` CI job in `parity.yml` (Linux, per-PR,
  on the shared `src/**`/Cargo/tests filter — bindings are generated from `src/**`, so any FFI
  change trips it without firing the macOS smoke on binding-only PRs) regenerates through that
  script and fails with
  *"FFI surface changed — run scripts/gen-bindings.sh and commit the bindings"* on any diff. This
  catches what UniFFI's runtime checksum guard cannot: a newly-exported symbol never regenerated,
  and **docstring-only** changes to `#[uniffi::export]` items (verified — a docstring edit
  propagates into both bindings and trips the guard). `build-xcframework.sh` and the
  `build.gradle.kts` regen doc-comment now delegate to the script (DRY); CLAUDE.md § Workflow
  records the regenerate-and-commit rule. Founder-only paths (`.github/workflows/**` + `CLAUDE.md`).
- **Sync fanned out to all eight synced stores + the full coexistence matrix (SUR-726, closes
  Phase 2 / SUR-659).** Pull and flush now cover every synced table — `custom_ideas`, `note_links`,
  `lenses`, `collections`, `collection_memberships`, `note_signals` — alongside `books`/`notes`.
  - **FFI:** six new `enqueue_*` methods on `SyncEngine` (one per new store), plus an exported
    `membership_id(collection_id, note_id)` free function mirroring surfc's `membershipId` byte-for-byte
    (`collection:note` join, collection first) so concurrent adds of a note↔collection pair converge to
    one row. `enqueue_collection_membership` derives that id internally; `enqueue_note_signals` keys on
    `note_id` (no `id` column) and carries a birth-row-never-enqueued contract (mirror of
    `ensureNoteSignals`); wire defaults match the oracle (`relation_type` `handwritten_annotation`,
    `combinator` `AND`, `threshold` `100`, empty `description`). New binding surface → `touches-ffi`.
  - **Pull scope + flush order from one source.** Both `pull()` and `sync()` now pull every table in
    `store::synced_table_names()` (derived from `synced_schema()`); `flush()` dispatches that same list
    in topological (FK-parent-first) order with a generalized, transitive hold-back — a row whose FK
    points at a parent that failed/held this run stays queued (no server FK violation). This replaces
    SUR-724's hard-coded books→notes loops.
  - **Coexistence matrix** (`tests/sync_726_integration.rs`, `#[ignore]`d, real local Supabase):
    8-store round-trip both directions; tombstone propagation + no-resurrect across all new stores;
    SUR-736 outbox-rebase convergence on a fan-out table; deterministic-id membership convergence;
    export/import parity (every column round-trips verbatim; a partial edit doesn't null untouched
    server columns). Unit coverage: per-table pull/LWW/rebase on the `note_id` pk, enqueue wire shapes,
    `membership_id` parity vectors, and topo/hold-back flush ordering.

### Changed
- **Retired the `naming-reviewer` repo-profile "not yet in scope" note (SUR-777).** `GATING.md`
  § "Not yet in scope" no longer says the `gce` `braird-core.md` repo-profile "does not exist yet" —
  it's landed, so `naming-reviewer` now runs profile-injected rather than self-contained.
- **Widened `enqueue_book` / `enqueue_note` to the full authoring surface (SUR-741).** `enqueue_book`
  now carries `isbn` / `cover_url` / `cover_source` / `cover_resolved_at`; `enqueue_note` now carries
  `source` / `source_id` / `source_meta_json` / `chapter` / `image_path` / `ink_crop_path` — columns
  already in `synced_schema()` but previously unauthorable from native (only round-trip-preserved).
  **Breaking signature change, widened in place** (no shipped native hosts; an additive `_v2` path
  would double the surface for no consumer). Partial-patch semantics: an absent optional is **omitted**
  from the payload — never an explicit null that would clobber a pulled-only column (the
  `enqueue_book_edit_preserves_pulled_only_columns` contract) — so native still can't *clear* a field
  to NULL (tri-state deferred to a 660/661 follow-up, noted in the method docs). `source` is the one
  always-sent optional (`None` → `"manual"`, the PWA's `|| 'manual'` / prior hardcode).
  `source_meta_json` is a JSON **object** string, parse-validated at enqueue — invalid JSON / non-object
  → `SyncError::Store`, nothing staged. Column names mirror surfc `upsertBook` / `upsertNote` exactly,
  so no payload key falls outside `synced_schema()`. Seal-at-write unchanged: only `plaintext` is
  sealed (enc:v2, AAD = note id) + `content_tag` from plaintext; the new fields never touch the
  `Vault`. Bindings regenerated via `scripts/gen-bindings.sh` (the SUR-742 `bindings-drift` guard
  verifies); Kotlin + Swift round-trip tests exercise a widened method incl. the invalid-JSON
  rejection. New binding surface → `touches-ffi`. Gate: `sync-reviewer` + `crypto-reviewer` +
  `naming-reviewer`.
- **Removed the stale SUR-743 allocation-order caveats** from `src/sync/http.rs` (`get_page` doc) and
  `src/sync/pull.rs` (module doc) — they described a hole surfc 0052 (SUR-743) closed; both now note
  the watermark is commit-ordered.

### Fixed
- **Pull now tracks the server `change_seq` watermark + paginates (SUR-739 + SUR-652 core leg).** The
  incremental-pull cursor is keyed on the server-assigned monotonic `change_seq` (surfc migration 0051
  / trigger `t02_change_seq`) instead of the puller's own clock, closing two holes:
  - **SUR-739 (delivery):** a delayed/offline flush lands on the server with a client `updated_at`
    older than other devices' cursors and was skipped forever. `change_seq` is stamped at
    write-visibility, so an exclusive `change_seq > cursor` keyset delivers it the moment it appears —
    retiring the 24h `PULL_CURSOR_OVERLAP_MS` lookback (a bounded heuristic that still missed longer
    delays). LWW is unchanged (still client `updated_at`); only the delivery axis moved.
  - **SUR-652 (pagination):** `get_since` (a single unpaged GET) advanced the cursor past any rows
    beyond PostgREST's `max_rows` cap — a permanent skip on accounts over ~1000 rows/table. `get_page`
    now pages by `change_seq` (`gt`, asc, `limit=1000`) and the pull loops until a short page, advancing
    the cursor per merged page (a consistent prefix: a mid-pull failure resumes from the last merged
    page, never re-pulling or skipping). Matches the PWA's `SYNC_PAGE_SIZE` keyset (surfc PR-3).
  - `change_seq` is server-only ordering metadata: read from the raw incoming row for the cursor, then
    projected away by `apply_row` — never added to a descriptor or outbox payload (keeps the
    vendored-schema drift guard green). New cursor namespace `sync:seq:<table>`; the retired epoch-ms
    `sync:cursor:<table>` key is ignored (absent new key → 0 → one-time full re-pull, also recovering
    rows the old cursor historically skipped) and deleted on the first pull. Tests: keyset paging
    across boundaries, cursor-not-advanced on mid-page failure, legacy-key migration, full-page-missing
    -change_seq guard; the env-guarded coexistence matrix re-proves both directions against live 0051.
  - **Commit-ordered as of SUR-743 (was a known residual).** `change_seq` is now assigned in COMMIT
    order per user — surfc migration 0052 replaced 0051's per-table `nextval` with a per-user
    lock-serialized counter — so the exclusive keyset is skip-safe by construction: the concurrent-flush
    skip (a lower value committing after the cursor passed a higher one) is closed. Server-side +
    trigger-only; **no client change** (the client already consumed a commit-ordered watermark
    correctly). The stale allocation-order caveats in `http.rs` / `pull.rs` were removed with SUR-741.
- **Flush no longer wedges a queued row in a non-`books`/`notes` table (SUR-726).** The pre-fan-out
  flush dispatched only `books`/`notes` groups, so a queued row in any other synced table was neither
  sent nor failed — it sat in the outbox forever. The single topo-ordered dispatch pass sends every
  synced table; regression-tested per new store.

### Changed
- **Ratified whole-row-LWW convergence for array/composite + row-per-pair tables (SUR-737).**
  Docs + pin tests only, **no behaviour change**: documented that every synced table resolves
  concurrent writes whole-row last-write-wins by `updated_at` (strict `>`, so an exact-ms tie keeps
  local — an accepted residual, plan §8: ms-identical concurrent edits with different values do NOT
  converge) — including the composite columns `notes.tags` / `notes.source_meta` / `lenses.leaf_ids`
  (opaque JSON, replaced wholesale, **never element-unioned** — a union can't express a delete), the
  row-per-pair `collection_memberships` (deterministic pk → concurrent adds converge to one row;
  remove = tombstone), and `note_links` (random-uid pk → a **bag**: concurrent adds of the same edge
  do NOT dedup). Convergence table on `store::synced_schema`, a rationale comment at the `pull_table`
  merge site, and `sur737_*` pin tests in `pull.rs` (tags + `leaf_ids` whole-array LWW both directions;
  membership add/remove convergence; exact-ms tie divergence) that pre-lock the semantics ahead of the
  SUR-726 fan-out. Any move to element-level merge or a deterministic tie-break is wire-visible and
  must land in the PWA (`mergeCloudRecords`) and here in lockstep.
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
