# Changelog

All notable changes to braird-core are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); every PR to `main` must add an
entry under `[Unreleased]` (CI-enforced, dependabot-exempt).

## [Unreleased]

## [0.12.0] - 2026-07-23

Twenty-second release batch. Minor release: SUR-916 Option 1 part B — core consumes the
synced `books.merged_into` pointer (added PWA-side in surfc#362, v0.12.0 re-vendors it),
so a device that pulled a book merge without the device-local `mergedBookIds` map now
converges a stranded note onto the survivor instead of only detaching it. Ships core's
**first generic local-DB column migration** (`init_schema` diffs `PRAGMA table_info`
against the descriptor and `ALTER TABLE … ADD COLUMN`s anything missing — the next additive
column needs no new code), a PWA-mirrored liveness guard (a survivor that resolves onto a
still-deleted terminus detaches; an unmaterialized survivor is deferred, never pushed), and
a SUR-954-class wire-shape fix (staged books patches now carry the full NOT-NULL shape, so a
merge touching a pulled book can't 23502-wedge the outbox). No FFI or record-shape change →
no bindings regen; consumers bump their pin to v0.12.0, no host code changes required — the
convergence and the accepted undo-window residual both land on the pin bump alone
(SUR-863/877). One accepted residual: a book-merge undo can't reverse a cross-device
straggler another device rehomed via `merged_into` during the undo window (best-effort
within the window, shared with the PWA; see `unmerge_books`).

### Added

- **SUR-1005 — consume the synced `books.merged_into` pointer for stranded-note
  convergence (SUR-916 Option 1 part B).** A book-merge's loser→survivor mapping
  previously lived only in the device-local `mergedBookIds` map, so a device that
  pulled the merge without the map could only *detach* a stranded note. The books
  descriptor now carries `merged_into` (re-vendored from surfc/main after part A,
  surfc#362), `merge_books` stamps it on loser tombstones, `unmerge_books` nulls it
  so undo propagates, and `reconcile_stranded_notes` resolves the survivor from BOTH
  sources — the stored row's `merged_into` first (fleet-wide record), the local map
  as fallback — walking chained A→B→C transitively with `resolve_book_id`'s hop-cap
  discipline, then rehoming via a staged outbox write. A PWA-mirrored liveness guard
  detaches instead of parking a note on a still-deleted terminus (merge cycle /
  plain-deleted chain end); a survivor not materialized locally is DEFERRED (left for
  a later pass), never pushed — core's pull skips absent-row tombstones, so an absent
  terminal's liveness is unknown and a rehome onto it could park the fleet on a ghost
  the later local-only detach can't correct. This is the always-to-survivor convergence the PWA
  gained in surfc#362; deploy order: surfc release → this core release → app pin
  bumps (SUR-863/877 — no host code change expected). **Accepted residual** (founder,
  2026-07-23): a book-merge undo reverses only the reassignments in this device's undo
  token, so a cross-device straggler another device rehomed onto the survivor via the
  synced `merged_into` during the undo window stays on the survivor after undo — a
  best-effort-within-the-window limit shared with the PWA (see `unmerge_books` docs). The
  clean fix (delay fleet-visibility until the window is final) is deferred to a PWA+core
  follow-up.

- **SUR-1005 — generic additive local-DB column migration in `init_schema`.** Core
  had no local-schema migration path: a descriptor-only column addition broke
  existing devices at `apply_row` ("no column named …") because every store
  read/write names the full descriptor column set. `init_schema` now diffs `PRAGMA
  table_info` against the descriptor per synced table and `ALTER TABLE … ADD
  COLUMN`s anything missing (NULL-backfilled, idempotent, order-independent — no
  reader uses `SELECT *`). The next additive descriptor column needs no new code.
  Deliberate skips: `BookRecord`/FFI unchanged (hosts don't consume the pointer; no
  bindings regen, no `touches-ffi`), export/import column maps unchanged (a dropped
  `mergedInto` on import re-materializes from the cloud via `reconcile_books`).

### Fixed

- **SUR-1005 — books staged patches now carry the stored row's full shape (the
  SUR-954 note_links class).** `merge_books`' survivor bump + loser tombstones and
  `unmerge_books`' resurrection + survivor restore staged sparse books patches
  (`{id, deleted/created_at, updated_at}`). `books.title`/`created_at` are NOT NULL
  without defaults server-side, and a PostgREST upsert NOT-NULL-checks its INSERT
  candidate before conflict resolution — so any merge touching a PULLED book (its
  full-shape create no longer queued in front) would 23502 and permanently wedge
  the outbox. Latent today only because no host consumer ships merge yet
  (SUR-863/877). Each site now overlays its changes on the stored row — also the
  PWA's wire contract (`upsertBook` always sends the full record). Wire-payload
  only; local semantics unchanged (`stage_local_write` already merges partials).

## [0.11.1] - 2026-07-23

Twenty-first release batch. Patch release: two sync-engine fixes on the `note_links` /
`note_signals` seam, both merged since v0.11.0. SUR-954 — `repoint_note_links` now stages the full
NOT-NULL `note_links` shape on the content-dedupe merge path (a pre-existing sparse-payload 23502
that could permanently wedge the outbox once the affected edge's create had flushed). SUR-959 — a
margin-child delete recomputes the parent's `has_annotation` the other direction from SUR-956 (the
recompute-to-false leg, staged in the same delete batch, with parent-liveness, monotone-stamp, and
null-`relation_type` guards); `refresh-annotation-signal` flips waived→core in the native-parity
manifest. No FFI or record-shape change → no bindings regen; consumers bump their pin to v0.11.1,
no host code changes required.

### Added
- **A margin-child delete now recomputes the parent's `has_annotation` (SUR-959).** `enqueue_note`'s
  two delete paths (the full-write `deleted: true` arm and the plaintext-free patch arm) now cascade
  the SUR-956 signal the other direction: in the SAME `stage_local_writes` batch as the note + its
  `note_signals` tombstone, they retire the deleted note's `handwritten_annotation` edges for which
  it is the CHILD (full NOT-NULL shape — `note_links` has no sparse-PATCH fallback) and read-merge-
  stage each affected parent's `has_annotation`, recomputed from its surviving live handwritten
  edges (importance re-derived, counters preserved, change-detection no-op when unchanged). Without
  it, deleting the last margin note left the parent's `has_annotation: true` forever, crediting the
  0.3 annotation weight to importance fleet-wide. Mirrors the PWA `deleteNote` → `refreshAnnotationSignal`
  (`useNoteActions.js`/`db.js`), including the `!existing && !hasLive` skip-create guard, and — because
  the child-leg scope leaves a deleted parent's outgoing edge live — a **parent-liveness guard** so a
  child deleted AFTER its parent never resurrects the dead parent's tombstoned signals row (the PWA
  avoids this structurally: its full edge cascade retires the edge with the parent). The edge
  tombstone's `updated_at` is clamped strictly above the stored row (SUR-976 monotonicity — else a
  clock-skewed stamp from a pulled foreign edge lets the server's t01 LWW guard silently drop the
  delete, leaving the edge live fleet-wide), and an absent `relation_type` is treated as
  `handwritten_annotation` (the margins-code default) in both the retire filter and the surviving-edge
  scan. The parent recompute's own `note_signals.updated_at` is likewise clamped strictly above the
  stored parent row (same t01/SUR-976 reason — a clock-behind device would otherwise leave the cloud
  parent at `has_annotation: true`). The recompute fires only from the note-delete path (matching where
  the PWA wires `refreshAnnotationSignal` — `deleteNote` + `replaceHandwrittenAnnotations`); a
  standalone edge remove (`enqueue_note_link(deleted: true)`) does NOT recompute, faithful to the PWA
  whose `deleteNoteLink` is unused and unwired to the reconciler. Scope
  (founder 2026-07-22): handwritten-only, child-leg — a deleted PARENT's outgoing edges and any
  non-`handwritten_annotation` edge are the broader note-delete edge cascade (SUR-84 parity), tracked
  separately. `refresh-annotation-signal` in the native-parity manifest flips from waived to core.
  Spine (sync); sync-reviewer + crypto-reviewer. No FFI change → no bindings regen.
### Fixed
- **`reconcile.rs` `repoint_note_links` now stages the full NOT-NULL shape (SUR-954).** A
  content-dedupe merge (`reconcile_content_dupes` → `merge_into_survivor` → `repoint_note_links`)
  staged sparse `note_links` payloads — a `{id, deleted, updated_at}` tombstone and a
  `{id, from_note_id, to_note_id, updated_at}` repoint — both missing the server's NOT-NULL
  `created_at` (and the tombstone its from/to/relation_type). `note_links` has no sparse-payload
  PATCH flush fallback (`push.rs patch_group` covers `notes` only), so once the affected edge's
  create had already flushed — i.e. almost always, since reconcile runs post-pull on synced data —
  the sparse row stood alone as a fresh PostgREST INSERT candidate and 23502'd on every flush,
  wedging the outbox permanently (and reading as never-synced to the SUR-882 import gate). Both
  branches now carry `from_note_id`/`to_note_id`/`relation_type`/`created_at` (preserved) +
  fresh `updated_at`, mirroring `replace_handwritten_annotations`' SUR-952 edge-tombstone shape;
  the values were already in hand from `list_live`, so no extra read. Pre-existing on `main` (found
  by the SUR-952 pre-push sweep), not introduced by that PR. Spine (sync); sync-reviewer.

## [0.11.0] - 2026-07-22

Twentieth release batch. Minor release: the note_signals hardening arc lands whole —
SUR-975 (a note delete stages its `note_signals` tombstone in the SAME transaction, both
`enqueue_note` paths; hosts drop the second call they never wired), SUR-976 (the post-pull
`reconcile_note_signals` pass retires orphaned signal rows for fleet-deleted notes — the
cross-device LWW door, merge losers, retired margin children, pre-975 strands, imported orphans —
with a monotone retirement stamp so the server's `t01_lww_guard` can never silently cancel it;
plus the first two-engine in-process sync fixture), and SUR-977 (`importance` inside signal
change-detection with an epsilon compare and a finite-boundary guard on the blind FFI).
`ReconcileSummary` gains `signals_retired` (record-shape change, bindings regenerated). With
SUR-966's visibility guard (v0.10.0), the collection surface is now guarded same-device,
cross-device, and at the change-detection layer. Consumers bump their pin to v0.11.0; no host
code changes required — the Android SUR-890 delete's stranded-signals leak heals on the pin bump
alone.

### Fixed
- **`importance` now participates in signal change-detection (SUR-977).** It was computed and
  staged by `stage_signal_write` but invisible to the `SignalState` no-op compare — sound only
  while every stored value agreed with the formula, and the blind `enqueue_note_signals` FFI can
  write one that doesn't. Such a value then sat on a live row forever, feeding ranking: the exact
  SUR-956/966 `source_prior` bug class one field over. `importance` is now a `SignalState` field
  with an asymmetric contract — `before` reads the STORED value verbatim (the pre-image invariant),
  `after` recomputes from the post-mutation fields — so a disagreement is a plain diff and the next
  signal, even a throttled no-op Exposure, stages the correction (counters and stored prior
  untouched). Zero churn on consistent rows, and the `importance` compare is EPSILON-tolerant
  (1e-9, `signals_agree` — `PartialEq` deliberately dropped from `SignalState`):
  `compute_importance` runs `.exp()`, which has no cross-libm bit-determinism, so an exact compare
  could ping-pong one-ULP "corrections" between devices on divergent platforms. Import already
  recomputed on entry (unchanged), and the PWA recomputes on every write, so the blind FFI was the
  one open door — and it now REJECTS non-finite `source_prior`/`importance` at the trust boundary
  (`json!` silently launders NaN/±inf to a stored JSON null); a legacy live row already holding a
  null importance reads as a NaN pre-image, agrees with nothing, and heals on its next signal
  instead of being shielded by a derived stand-in. One accepted widening, under SUR-737's ratified
  lossiness: a formerly write-silent throttled signal on a LYING row now pushes a whole corrected
  row once, which can beat a concurrent earned-counter push — at most once per lying row per
  device. Internal-only — no FFI surface change, bindings untouched.

### Added
- **Post-pull `note_signals` reconciliation — `reconcile_note_signals` (SUR-976).** `note_signals`
  converges by whole-row LWW, so a device that had not yet pulled a note's tombstone could record
  a signal whose newer live row legitimately won the cloud row back from the deleting device's
  tombstone (the server's `t01_lww_guard` accepts equal-or-newer) — a live signals row for a
  fleet-deleted note, permanently, with nothing anywhere to retire it. SUR-966 closed the
  same-device callback half and SUR-975 the same-device delete half; this closes the cross-device
  half, which is inherent to LWW and unfixable at any call site. A seventh reconcile pass (slotted
  right after content-dedup, so same-cycle merge losers retire immediately) sweeps live
  `note_signals` rows and retires any whose LOCAL `notes` row is tombstoned — full-shape tombstone,
  earned counters preserved verbatim, staged through the outbox so the same `pull_then_flush`
  converges the cloud row too. The same sweep closes every other door into the state: dedup merge
  losers (counters deliberately discarded, not folded), retired margin children, pre-SUR-975 crash
  strands arriving by pull, and imported orphans. A signals row with NO local notes row is
  deliberately left alone (founder decision): the pull tombstone-skip makes absent genuinely
  ambiguous (never-synced vs deleted-elsewhere), and the row self-resolves once the note arrives.
  No oracle counterpart — the PWA's 8 post-sync steps have no signals pass, so this is core-first.
  `ReconcileSummary` gains `signals_retired` (bindings regenerated — a record-shape change, PR
  labelled `touches-ffi`); the fabricated-prior resurrect wart stays accepted as pinned (healing
  was reconsidered and re-rejected here — a stored 0.5 is still ambiguous, SUR-956).
- **Two-engine sync fixture (`tests/common`).** The first bidirectional in-process cloud
  (`SharedCloud`: server-side `t01_lww_guard` conflict rule + `t02_change_seq` stamping +
  keyset `fetch_page`) with real-`SyncEngine` `Device`s, driven through the free
  `pull_then_flush`/`push::flush` seams. `tests/sync_976_integration.rs` pins the exact
  cross-device interleave end to end — including the cloud row and the second device converging —
  plus the good path (a device that pulls first never creates the orphan, and the tombstone-skip
  materializes no dead row). Reusable for any future sync-surface interleave test; every SUR-966
  test to date was single-device, which is exactly why this interleave went unpinned.

### Changed
- **`enqueue_note(deleted: true)` also stages the note's `note_signals` tombstone — in the SAME
  transaction (SUR-975).** A native note delete was two separate FFI calls in two transactions
  (`enqueue_note` + `soft_delete_signals_for_note`), so a crash between them committed the note
  tombstone and left the cloud signals row live forever (the Codex durability finding deferred out
  of SUR-966's scope). Core now owns the atomicity: both delete paths — the full-write tombstone
  (Android's SUR-890 shape) and the plaintext-free patch — build the same full-shape signals
  tombstone `soft_delete_signals_for_note` stages and commit it with the note row in one
  transaction (one outbox enqueue stamp). A note tombstone can no longer commit with its signals
  tombstone unqueued. On the patch path the existing-live precondition now guards the whole batch:
  a rejected delete-patch (`PatchTargetMissing`) stages neither row. Live writes (`deleted: false`)
  never touch `note_signals`. No FFI signature change — hosts keep their single `enqueue_note`
  delete call and simply stop needing the second one; a legacy second
  `soft_delete_signals_for_note` call remains a no-op (already-tombstoned guard), and it stays
  exported for the cases `enqueue_note` cannot cover: retiring signals for a note with no LIVE
  local row — absent (the cross-device rule) or already tombstoned (a pre-SUR-975 device's
  crash-stranded orphan arriving by pull; the delete-patch refuses a dead target, so the
  standalone call is the repair entrypoint until SUR-976's reconciler).
- A signals tombstone **born by the full-write delete seeds `source_prior` from the write's own
  `source`** (explicit or the `"manual"` default) — a real prior, not the 0.5 unknown-source
  fallback. The ACCEPTED fallback wart narrows to the standalone no-note-row path (tracked with
  SUR-976). Doc-comment updates moved the UniFFI method checksums; bindings regenerated, new
  Kotlin + Swift round-trip tests drive an actual `enqueueNote(deleted: true)` delete (neither
  harness ever exercised a note delete before) with a throttle-based discriminator proving the
  tombstone landed.

## [0.10.0] - 2026-07-20

Nineteenth release batch. Minor release: the native `note_signals` collection surface (SUR-966) —
`record_note_signal(note_id, kind)` for the three behavioural signals plus
`soft_delete_signals_for_note(note_id)` on note delete, mirroring the PWA's `applyNoteSignal` /
delete-tombstone oracle. Two new FFI methods + the `NoteSignalKind` enum, bindings regenerated
(the enum lowers as one `RustBuffer`, no arm64 >8-slot spill). The counter math lives IN CORE
because the FFI has no `note_signals` read-back — a host cannot safely mutate a counter it cannot
read without clobbering another device's earned counters over whole-row LWW. Also widens the
SUR-956 margins no-op guard so an already-annotated note still records margin engagement. Four
`native-parity` manifest rows (`bump-exposure`, `bump-engagement`, `record-return-visit`,
`soft-delete-signals-for-note`) flip waived → core. Consumers bump their pin to v0.10.0.

### Added
- **`record_note_signal(note_id, kind: NoteSignalKind)` — native `note_signals` collection (SUR-966).**
  Records a behavioural signal for a note, porting the PWA's `applyNoteSignal`. The per-kind mutation
  lives IN CORE (the FFI exposes no signals read-back, so a host bump would clobber earned counters over
  whole-row LWW): reads the stored row — or births defaults with `source_prior` derived from the note's
  `source` — applies the mutation, recomputes `importance`, and whole-row stages it (`deleted: false`, so
  a queued tombstone is dropped — the resurrect rule). `ReturnVisit` → `return_visits += 1` and bumps
  `exposure_recency_at` (deliberately NOT engagement — "re-reading isn't reflection"); `Exposure` → bumps
  `exposure_recency_at`, throttled by a named 1h `SIGNAL_THROTTLE_MS` dedup window (distinct from
  `scoring.js`'s 7d `EXPOSURE_COOLDOWN_MS`), with an absent/epoch-0 stamp treated as "never exposed" so a
  cold-start Exposure after a first-ever signal of another kind always writes (no unsigned underflow, no
  NULL short-circuit); `Engagement` → bumps `engagement_recency_at`, never throttled (deliberate acts are
  rare, so every call is genuine evidence). Returns `true` when a row was staged, `false` on a
  change-detection / throttle no-op (no `updated_at` bump, no outbox churn). Shares one read-merge-stage
  helper with `replace_handwritten_annotations` so the scoring constants stay in one place.
  **Only a locally-visible note earns a signal** — an absent or tombstoned note row is a no-op
  returning `false`. Deleted, because a callback landing after the host's delete would take the
  resurrect path and drop the queued signals tombstone, leaving live signal metadata for a dead note.
  Absent, because with no note there is no `source` to derive `source_prior` from: the row would be
  born at the unknown-source fallback and pinned there — nothing re-derives a stored prior (SUR-956's
  "stored prior kept, not re-derived", v0.9.1) — permanently under-scoring `handwritten` (0.9),
  `share` (0.75) and `manual` (0.7) notes in `compute_importance`. Healing such a row later is not a
  workable alternative: a stored 0.5 is genuinely ambiguous, since `readwise` derives 0.5 and both
  import and the blind `enqueue_note_signals` FFI can write a real 0.5 onto a note whose source
  derives higher, so healing on the value would overwrite legitimate priors. A signal for a note the
  host cannot render is near-unreachable in practice, and signals are cheap and repeat, so dropping
  one racing an unsynced note costs nothing next to storing a wrong prior forever.
  The deleted half of this narrows the orphaned-signals window to the same device; it does not close
  it fleet-wide. A
  device that has not yet pulled the note's tombstone still sees a live local row, so a signal it
  fires there wins on whole-row LWW and leaves the `note_signals` row live for a deleted note — and
  an absent local row is ambiguous (never-synced-down vs deleted-elsewhere, since a pull skips an
  incoming tombstone when no local row exists). Retiring those needs a post-pull signals
  reconciliation pass, which does not exist yet — **SUR-976**.
  Two paths still reach the unknown-source fallback legitimately, and pin there because nothing
  re-derives a stored prior: a **visible note with a null `source`** (correct — there is nothing
  better to derive from; only reachable by pull/import, since `enqueue_note`'s create path defaults
  source to `manual`), and a **tombstone staged with no local note row**, which must stage regardless
  and so fabricates the fallback — a value a later resurrect reads verbatim, so a `handwritten` note
  retired that way comes back pinned at 0.5 rather than 0.9. Both are accepted and pinned by tests;
  the second is tracked in **SUR-976**.
- **`soft_delete_signals_for_note(note_id)` — note_signals tombstone on note delete (SUR-966).** ALWAYS
  stages a whole-shape tombstone even when this device holds no local signals row — the cross-device tail:
  another device may hold a live cloud row this delete must tear down, else orphaned signal metadata
  lingers. Staged through the plain `stage_local_writes` path (which stages a `deleted: true` write
  unconditionally, no existing-live precondition), never `stage_local_write_existing_live` (which would
  silently drop the no-local-row tombstone). Carries the stored row's full NOT-NULL shape (or birth
  defaults) — `note_signals` has no sparse-PATCH flush fallback, so a bare `{note_id, deleted}` would risk
  a NOT-NULL upsert reject that wedges the outbox (the SUR-942 lesson). A repeat call on an already-
  tombstoned row is a no-op.
- **`NoteSignalKind` FFI enum (`Exposure` / `Engagement` / `ReturnVisit`).** The signal taxonomy, defined
  in `src/sync/mod.rs` alongside the collection op that matches on it (there is no `src/ffi.rs` in this
  crate; the `#[uniffi::export]` surface lives in `src/lib.rs` + `src/sync/mod.rs`). Bindings regenerated:
  the enum lowers as a single `RustBuffer`, no arm64 slot spill.

### Changed
- **Widened the SUR-956 margins no-op guard so an already-annotated note still records engagement
  (SUR-966).** `replace_handwritten_annotations` now fires an `engagement_recency_at` bump on EVERY margin
  save, including on a note already carrying `has_annotation: true` — "Add the margins" has a single,
  always-deliberate caller, so it is a genuine engagement signal. The SUR-956 guard skipped an already-
  flagged live row entirely, which would have meant an already-annotated note NEVER recorded margin
  engagement; the shared read-merge-stage helper's change-detection still no-ops only when nothing moved.
  Engagement is never throttled. The recompute-to-false half stays out of scope (SUR-959).
- **Four `vendored/native-parity/manifest.json` rows flip waived → core (SUR-966).** `bump-exposure`,
  `bump-engagement`, `record-return-visit`, and `soft-delete-signals-for-note` are now implemented in
  core, landed in the SAME change as the flip (the status contract: `core` is an implemented-today claim,
  so the manifest cannot go green ahead of the code). `ensure-note-signals` and `record-stitch-spawn`
  stay waived (no standalone host writer / no `StitchSpawn` kind yet).

## [0.9.1] - 2026-07-19

Eighteenth release batch. Patch release: `replace_handwritten_annotations` now refreshes the
parent's `note_signals.has_annotation` (+ recomputed `importance`) inside its single staged
batch (SUR-956) — closing the fleet-wide ranking drift where a native margin add/replace left
the flag false. No FFI surface change (docstring-only bindings regen); the recompute-to-false
half is tracked as SUR-959. Consumers bump their pin to v0.9.1.

### Changed
- **`replace_handwritten_annotations` now refreshes the parent's `note_signals.has_annotation` in the
  same batch (SUR-956).** The op previously staged only the child notes + edges, so a native margin
  add/replace left the parent's `has_annotation` false fleet-wide — importance scoring weights the flag
  at 0.3, so this was ranking drift the host could not compensate for (`enqueue_note_signals` is a
  blind whole-row LWW write that would clobber earned counters, and the FFI has no signals read). The
  op now read-merge-stages the parent's signals row inside its single `stage_local_writes` transaction:
  the stored row — or a birth-defaults row with `source_prior` derived from the parent's `source` — is
  re-staged WHOLE with `has_annotation: true` and `importance` recomputed via the (newly shared)
  PWA `computeImportance` formula, preserving every earned behavioural counter verbatim. Mirrors the
  PWA's `applyNoteSignal` posture exactly: change-detection no-op (an already-flagged live row gets no
  write, no `updated_at` bump), whole-row enqueue, and live-write tombstone drop. This ports the
  `record-annotation` parity behavior (manifest row flipped to core/SUR-956); the recompute-to-false
  half (margins delete) is deliberately out of scope — this op never ends with zero margins — and is
  ticketed as SUR-959 on the `refresh-annotation-signal` row. Internal-only change: no FFI signature
  added or altered (docstring-only bindings regen).

## [0.9.0] - 2026-07-18

Seventeenth release batch. Minor release: the atomic margins op —
`replace_handwritten_annotations` + the `MarginChild` record (SUR-952, for the Android/iOS
SUR-928 "Add the margins" features) — plus its supporting `Store::stage_local_writes`
one-transaction batch primitive. One new FFI symbol + one record, bindings regenerated,
arm64-safe (record lowers as one `RustBuffer`). Hardened through eight adversarial rounds:
full-synced-column create rows, edge-topology-validated id reuse (notes row present or
dangling), full-NOT-NULL-shape edge tombstones, cross-batch tombstone resurrection — the
class held closed by a schema-completeness test, a 27-cell reuse-state grid, and
mutation-verified invariants. Consumers bump their pin to v0.9.0; the Android SUR-928
margins write path unblocks on it.

### Added
- **`replace_handwritten_annotations(parent_id, children)` — atomic "Add the margins" op (SUR-952,
  for SUR-928).** A single FFI op that files a note's margins (handwritten annotations OCR'd from its
  source photo) as linked child notes, replacing any prior set — the transactional form of the PWA's
  `replaceHandwrittenAnnotations`. The host mints the child + link ids (via the new `MarginChild`
  record `{ id, link_id, text, ink_crop_path }`) and trims the texts; core seals each child under the parent's LIVE
  `book_id` (read here, so children file where the parent lives now), creates the child notes +
  parent→child `handwritten_annotation` links, and tombstones the parent's prior handwritten children +
  their edges — **every row staged in ONE transaction** (`Store::stage_local_writes`, new). This
  exists because the host's per-item `enqueue_note` + `enqueue_note_link` were two separate
  transactions: a crash between them orphaned a child note with no edge, which never converged (a
  re-run reads prior children from live edges, so an edgeless orphan is invisible to cleanup). One
  transaction closes that window. Empty `children` is a no-op that leaves existing margins intact (PWA
  early-return parity); the parent must exist and be live; it's allowed on a decrypt-failed parent
  (core never reads the parent's text — only its existence + book — and only the NEW child bodies are
  sealed). Children carry `source = "handwritten"`, empty tags, and `created_at` staggered by index so
  review order survives LWW. Note-links are a random-pk bag (host ids), so a re-run just adds a fresh
  set and tombstones the prior one — no resurrect hazard. `MarginChild.ink_crop_path` (optional) carries
  the uploaded ink-crop storage key for the capture-time handwriting path (iOS / PWA capture card) and is
  stored verbatim on the child like `image_path`; Android's text-only action-sheet path passes `None`.
  Retiring the prior set only ever tombstones this parent's edges — a child NOTE is tombstoned only when
  it's a live handwritten note that no other live edge (any relation/direction) touches and that isn't
  part of the new set (so a repointed regular survivor, a shared child, a child with a non-handwritten
  edge, or an idempotent same-id retry can't lose data). Live writes in the batch drop any still-queued
  tombstone for their id from a previous un-flushed replace (`stage_local_writes` resurrect rule, the
  `stage_import_batch` sibling) — otherwise the SUR-724 sticky collapse would flush a re-created margin
  as deleted with a fresh `updated_at`, a divergence the strict-tie LWW pull can never repair. Prior-edge
  tombstones carry the STORED row's full NOT-NULL shape with `created_at` preserved (the SUR-942
  membership convention; `note_links` has no sparse-PATCH flush fallback, so a bare `{id, deleted}`
  tombstone would 23502 on every flush and wedge the outbox — found by the SUR-952 adversarial sweep).
  Texts are trimmed and blank items dropped in core (PWA filters before its length check — an all-blank
  call preserves existing margins). Create rows write **EVERY synced notes column explicitly** — the
  MarginChild-owned values plus the PWA child literal's cleared shapes (`page: ""`, `chapter`/
  `image_path`/`source_id`: null, `source_meta: {}`; `created_at`/`updated_at` both staggered `now + i`,
  matching the PWA's child AND edge) — because staging merges partials and the server upsert only sets
  the columns a payload names, so any omitted column would let an id-reusing restore resurrect stale
  fields (a whole-page photo, a source, a page number) off the prior row, locally and on the cloud. A
  schema-completeness test pins the create-row shape (and the edge tombstone's) to the drift-guarded
  `vendored/schema/sync-schema.json`, so a new synced column fails the build until covered. Host-minted
  ids are validated fail-loud — reusing an existing id is legal ONLY for this parent's prior handwritten
  margin (retry/repoint/restore) that **no other live edge still touches**: parent collisions, in-call
  duplicates, a child id on any non-margin note or another parent's margin, a reused child id any
  foreign live edge (any relation, either direction — a shared dedupe survivor, a generic `related`
  row, even this parent's own) still references — the retire loop deliberately KEEPS such entangled
  children, so the create loop must never overwrite one — and a link id on any non-handwritten edge
  (including this parent's own `related`/`duplicate_of` edges, which a from-check alone would wave
  through) all reject the whole call; a corrupt p→p self-edge retires the edge only, never the parent.
  The child-id edge checks run **whether or not the child's notes row exists locally**: pull skips
  the tombstone of a row a device never had while edges apply independently (no local FK), so a
  fleet-deleted child can stand locally as dangling live edges with no notes row — a row-existence-
  gated check would let exactly that id bypass validation, resurrect the note over its server
  tombstone on flush, and rewrite what the surviving foreign edge still renders (found pre-push by
  the round-8 adversarial verification; the dangling cells are pinned in the grid). A 27-cell
  reuse-state grid test enumerates every reachable (stored-row × edge-topology × queue) state for a
  reused child/link id and asserts each cell either rejects before staging — with the outbox proven
  byte-empty after — or converges per a post-condition oracle (exact live edge set, sealed text
  round-trip, live-not-sticky flush payloads). New FFI symbol + `MarginChild` record →
  bindings regenerated; 2 args (`String` + `Vec<MarginChild>`), record lowers as one `RustBuffer`, so
  no arm64 >8-slot spill. Consumers bump their pin to pick it up.

## [0.8.2] - 2026-07-18

Sixteenth release batch. Patch release: the collection-membership tombstone now preserves the
membership's filed-at `created_at` instead of overwriting it with the host wall clock (SUR-942). A
toggle-off host can't supply the original — `collection_ids_for_note` exposes no timestamp — so the
enqueue path clobbered it, diverging from surfc's `removeNoteFromCollection` (which tombstones the
stored row) and from reconcile's own `repoint_memberships` preserve. The lookup and the tombstone
stage now share one held store guard, so the read-modify-write can't be torn by a concurrent re-add
from another host thread. Cosmetic today (no membership read consumes `created_at`; whole-row LWW
keys on `updated_at`), so SUR-927 ships correctly on v0.8.1 — but only core can fix it. Internal
staging only — no FFI signature/docstring change, bindings unchanged; consumers bump their pin to
v0.8.2.

### Fixed
- **`enqueue_collection_membership` tombstone now preserves the membership's filed-at `created_at` (SUR-942).**
  A toggle-off host can't supply the original `created_at` (`collection_ids_for_note` exposes no
  timestamp), so it passes the wall clock — and the enqueue path wrote it unconditionally, clobbering
  the filed-at instant on the pushed tombstone. The `deleted = true` path now reads the deterministic
  `{collectionId}:{noteId}` row's stored `created_at` from the local mirror and preserves it, falling
  back to the host value only when no row exists. This mirrors surfc's `removeNoteFromCollection` →
  `softDeleteMembershipRows` (tombstone the stored row, not a reconstruct-from-ids) and its
  `?? { createdAt: now }` fallback, and matches the preserve reconcile's `repoint_memberships` already
  does — closing an internal inconsistency. Active add/re-add still stamps the host clock (parity with
  `addNoteToCollection`). Cosmetic today (no membership read consumes `created_at`; whole-row LWW keys
  on `updated_at`; the server NOT-NULL column is satisfied by any non-null value), so SUR-927 ships
  correctly on v0.8.1 — but the host genuinely can't fix it, only core can. No FFI signature/docstring
  change (internal staging only) — bindings unchanged.

## [0.8.1] - 2026-07-17

Fifteenth release batch. Patch release: a silent lost-write on the collection-membership sync path,
surfaced by the SUR-927 (Android Add-to-collection) sync-reviewer gate — the first host feature to
queue a membership tombstone. An offline (or between-flush) toggle-off→on of the same membership
collapsed in the outbox to a sticky `deleted:true`, dropping the note from the collection on push
while the local mirror still showed it filed. Internal staging only — no FFI signature/docstring
change, bindings unchanged; consumers bump their pin to v0.8.1 for the corrected offline behavior.

### Fixed
- **`enqueue_collection_membership` re-add now resurrects past the outbox sticky-delete (SUR-940).**
  A `deleted = false` file/re-add routes through `stage_local_write_resurrecting` (dropping any
  un-flushed tombstone for the deterministic `{collectionId}:{noteId}` id in the same transaction),
  mirroring SUR-915 `unmerge_books`; a `deleted = true` soft-delete stays on the sticky
  `stage_write` path. Without this, a **file → toggle-off → toggle-on of the same membership within
  one un-flushed batch** (guaranteed offline; reachable online between sync intervals) collapsed to a
  sticky `deleted: true` (the SUR-724 "delete wins, never resurrect" hardening) — the local mirror
  showed the note filed while push sent a tombstone, silently dropping it from the collection on
  every device. Surfaced by the SUR-927 (Android Add-to-collection) sync-reviewer gate, whose
  toggle-off is the first host path to queue a membership tombstone. Membership-only: `note_links`
  use random per-edge pks (a re-add is a new row) and collections/lenses have no re-add UI yet.
  No FFI signature/docstring change (internal staging only) — bindings unchanged. A new engine-level
  test collapses the store's outbox after add→off→on and asserts the pushed payload is `deleted:
  false` (it fails without the fix).

## [0.8.0] - 2026-07-17

Fourteenth release batch. This **SUR-923** minor release is Phase 2b read-API extension #3 —
the **relation reads**: four additive `#[uniffi::export]` queries traversing the
`collection_memberships` and `note_links` relations in both directions. They unblock the
note-action-sheet twins (Add to collection SUR-924/927, Add the margins SUR-925/928), the
Lexicon add/manage twins (SUR-930/931), and the scope-system twins (SUR-932/933). Additive,
read-only, no schema change, and no decryption anywhere — no note text is involved. The version
bump rides this PR (founder decision, 2026-07-17); consumers bump their pin to v0.8.0.

### Added
- **`collection_ids_for_note(note_id)` (SUR-923)** — ids of the live collections whose membership
  row pairs with the note; the AddToCollectionSheet's `memberIds` derivation, mirrored exactly
  (live membership rows only — no collection-liveness check, no notes join). Named for what it
  returns, mirroring `note_ids_for_collection`.
- **`note_links_for_note(note_id)` → `Vec<NoteLinkRecord>` (SUR-923)** — live note-link edges
  where the note is either endpoint, one hop, both directions; the host filters direction and
  `relation_type` exactly as every PWA read does. New `NoteLinkRecord`
  (id, from/to note ids, relation_type, timestamps).
- **`note_ids_for_collection(collection_id)` (SUR-923)** — live member note ids, deduped like the
  PWA's `memberNoteIds` Set. Deliberately **join-free**: the host-side collection-delete cascade
  must see every live membership — including one whose note is already soft-deleted — to
  tombstone them all (`useCollections.removeCollection`); the scoped note list re-checks note
  liveness host-side (`notesInCollection`).
- **`collection_note_counts()` → `Vec<CollectionNoteCount>` (SUR-923)** — per-collection
  live-note counts for the Lexicon Collections tab subtitles, shaped like `idea_counts`
  (collection-id-asc, `count ≥ 1` only). **Founder-decided divergence (2026-07-17):** a
  membership counts only when its note is present and live — the PWA's `noteCountByCollection`
  counts raw live membership rows, but joining live notes keeps the subtitle consistent with the
  scoped note list (`notesInCollection`) and this core's `idea_counts` convention.

## [0.7.2] - 2026-07-17

Thirteenth release batch. This **SUR-934** patch release makes `export_snapshot` usable on a real
corpus: it aborted the entire archive on any note with **nothing to decrypt** — an image-only,
empty, or legacy-unsealed body — because the exporter decrypted note text unconditionally instead of
applying the read path's rule, where only one of four cases is a decryption. Export was impossible
for essentially every real account; a single such note was enough. Fixing it also exposed that the
core could not re-import its own export, so both halves of the snapshot round-trip land here.
Found on-device against a real 1,638-note account (SUR-882) — no unit test could reach it.

### Fixed
- **`export_snapshot` no longer fails on a note with nothing to decrypt (SUR-934).** `map_note`
  decrypted `text` unconditionally, coercing an absent value to `""` via `unwrap_or_default`, so the
  three shapes the read path explicitly treats as *not* a failure — NULL text, empty text, and legacy
  **unsealed** text — each raised a manufactured decryption error that aborted the **entire** archive.
  A corpus could render on every screen and still be impossible to export; found on-device against a
  real 1,638-note account (SUR-882), where one such note blocked export outright. `map_note` now
  resolves `text` through the read path's own `decrypt_note_text`, so there is one rule instead of two.
  Genuine fail-closed behaviour is unchanged: a *sealed* row that fails to decrypt still fails the
  whole export — never a partial archive, never ciphertext in place of plaintext, never a dropped row.
  A note with no text exports `text: null`. `docs/snapshots.md` now states the four-case contract that
  was previously only implied by "Note `text` is plaintext".
- **The core can re-import its own export (SUR-934).** `normalize_note` rejected an explicit
  `"text": null` — `text` was the one string field marked non-nullable, while `bookId`, `page`,
  `imagePath`, `inkCropPath`, and `sourceId` all accept null. Since the exporter emits every note key,
  a text-less note ships `"text": null`, so export→import — the entire point of a backup — failed on
  the shape the fix above makes reachable. An omitted `text` was always accepted; an explicit null is
  the same fact stated out loud, and `merge`'s `prepare_write` already handled it. Caught by
  crypto-reviewer, and now pinned by an end-to-end `build_snapshot_at` → `parse_import_at` round-trip
  test — neither side had one, which is why the two halves could disagree.

## [0.7.1] - 2026-07-16

Twelfth release batch. This **SUR-921** patch release fixes the v0.7.0 sparse-note transport so a
successful PostgREST response cannot silently discard a plaintext-free patch when its server row
is missing. The outbox now drains only after the targeted write confirms exactly one affected row.

### Fixed

- **SUR-921 follow-up — retain sparse note patches when the server target is missing.** Targeted
  note `PATCH` requests now ask PostgREST to return only the affected primary key and clear their
  outbox entries only after exactly one matching row is confirmed. A successful zero-row response
  (for example, when another device removed the server row after the local work list was read) is
  treated as a retryable flush failure instead of silently dropping the plaintext-free patch.

## [0.7.0] - 2026-07-16

Eleventh release batch. **SUR-921** adds the true tags-only note patch required by the native
idea-merge flows: hosts can omit plaintext for an existing live note, preserving its sealed text
and content tag byte-for-byte while changing tags and other safe partial fields. The breaking
pre-1.0 FFI record/error change ships with regenerated Swift and Kotlin bindings, typed
missing-target recovery, and a targeted server PATCH path for sparse note writes.

### Added

- **SUR-921 — true tags-only note patches with optional plaintext.** Change
  `NoteUpsert.plaintext` to an optional field across Rust and the regenerated Kotlin/Swift
  bindings. `Some` retains the existing seal-at-write behavior; `None` patches only an existing
  live note, makes no Vault call, and omits `text`, `content_tag`, and `created_at` so ciphertext,
  its tag, and the immutable creation stamp remain byte-identical. Patch-mode `source: None`
  likewise means keep instead of defaulting to `"manual"`, enabling decrypt-failed notes to be
  retagged safely. Missing or already-tombstoned targets surface as the dedicated
  `SyncError::PatchTargetMissing` host contract. Patch mode rejects setting or clearing `book_id`,
  because the content tag is book-scoped and cannot be recomputed without plaintext. Sparse note
  groups flush through a targeted server `PATCH`; collapsed create-then-patch groups still carry
  `text` and use the normal upsert path. This is a breaking pre-1.0 FFI record/error change released
  in v0.7.0.

## [0.6.0] - 2026-07-15

Tenth release batch. **SUR-918** publishes the checksum-pinned paired canon assets, and **SUR-911**
adds the snapshot export plus protective merge-import boundary for all eight synced stores across
the Rust/UniFFI API and regenerated Swift/Kotlin bindings. Native host acceptance exercises both
public methods plus their summary and sanitized-error types. Snapshot note text crosses this API as
plaintext for portability, then accepted imports are re-tagged and freshly sealed under the active
Vault before one atomic local+outbox batch.

### Added

- **SUR-918 — checksum-pinned canon release assets.** Byte-vendor the surfc idea-tree YAML and
  guard it together with `great-ideas.json`: the combined drift check requires byte equality with
  `surfc/main`, duplicate-free equal counts, and set equality between all 97 YAML leaf assignments
  and `GREAT_IDEAS`. A read-only release gate re-runs the current live contract against the exact
  resolved release commit, then hands only the validated bytes to the publisher. Releases attach
  both files with their own entries in `SHA256SUMS.txt`, so consumers pin the pair to one exact tag
  and both checksums. Their most-specific GATING route combines canon parity review with release
  checksum/publication review, preserving `sync-reviewer` + `crypto-reviewer` while adding
  `release-integrity-reviewer`. Documentation/release-data only; no crate, FFI, or generated binding
  change.
- **SUR-911 — PWA-compatible snapshot export and protective merge import.** Add
  `SyncEngine::export_snapshot()` and `SyncEngine::import_merge(json)` to the public Rust UniFFI
  surface and regenerate the Swift/Kotlin bindings. Kotlin/JVM acceptance drives both generated
  methods end-to-end through a local `HttpServer`; the equivalent Swift macOS test uses a
  self-contained `NWListener`. Both cover `ImportSummary`/all eight `ImportCounts` fields, the
  sanitized generated `InvalidImport` variant, and the deliberate absence of a Replace API. The
  transfer itself provides schema-19 export across the eight synced stores and strict
  schema-1-through-19 import parsing. Export is live-only, decrypts note text, reconstructs handwritten
  `user_metadata.user_annotation`, and excludes local-only tables plus device-local data-URL
  previews. Import parses before operational access, requires a token and clean eight-table pull,
  directly fetches every candidate (including tombstones), and accepts only archive timestamps
  strictly newer than both local and server state. Accepted notes are re-tagged and freshly sealed
  as note-id-bound `enc:v2`, then the complete dependency-ordered batch is staged locally and in
  the outbox in one transaction with no automatic flush. `ImportSummary` reports exact per-store
  imported/skipped counts; there is no destructive Replace API. `docs/snapshots.md` pins the
  plaintext host-storage boundary: restrictive destination-filesystem temporary storage, no
  logging/telemetry/crash capture, verified atomic install, and cleanup on every exit path.

## [0.5.0] - 2026-07-14

Ninth tagged release. Two native-platformization surfaces land over the FFI, batched into one cut:
**SUR-858** — the Phase 2b "organise" read API (notes-by-idea, per-idea counts, collections + lenses
lists, the untagged queue), and **SUR-915** — the duplicate-resolution merge contract
(`merge_books` + `unmerge_books` with an ephemeral undo token, `merge_content_duplicates`, and the
device-local `mergedBookIds` write that feeds the existing stranded-note convergence). All reads are
decrypt-in-core (plaintext-only across the FFI); all merge verbs are key-free store-level patches.
New `uniffi::Record` types (`IdeaCount`, `CollectionRecord`, `LensRecord`, `BookMergeUndo`,
`NoteBookAssignment`); Swift + Kotlin bindings regenerated; every verb clears the SUR-843 arm64
arg-slot guard. Also repoints the surfc-clone CI + the pinning doc at the `braird` org. Delivery: the
`chore(core): pin braird-core v0.5.0` bump in braird-ios + braird-android (`docs/pinning.md`). Full
always-to-survivor convergence of book-merge stragglers a device never saw is deferred to SUR-916
(PWA-parity residual).

### Added

- **SUR-858 — organise reads over the FFI (Phase 2b read-API extension #2).** Six additive,
  decrypt-in-core read methods on `SyncEngine` for the native browse/organise screens (the iOS
  tree/IdeaDetail/RelatedNotes/BulkDiscovery/Lexicon set + the Android siblings), following the
  SUR-744/806 read-surface pattern (soft-delete-excluding, newest-first, plaintext-only across the
  FFI — no `enc:` sentinel):
  - `notes_by_idea(idea, limit, offset)` — live notes carrying a given idea tag, decrypted in core.
    `idea` is the raw tag string as stored in `notes.tags` (== a `CustomIdeaRecord.name`, == an
    `IdeaCount.idea`), matched exactly, so a tag from `idea_counts` round-trips straight back with no
    tag↔id resolution and the internal `cidea_…` id is never exposed. `tags` is a JSON array, so this
    scan-then-filters and windows on the plaintext tag column BEFORE decrypting (only the page pays
    the decrypt cost).
  - `idea_counts()` — the per-idea live-note tally, byte-matching the PWA's `ideaCountsFor`
    (`src/lib/scope.js`): increment per tag occurrence, present-tags-only (`count ≥ 1`), sorted by
    idea name ascending. The Commonplace tree overlays these onto its client-generated canon
    structure (which stays a host constant). Refactored the shared `tag_tally` scan behind both this
    and `counts().active_ideas` (its key count) — one scan, one oracle.
  - `list_collections(limit, offset)` + `list_lenses(limit, offset)` — the first read paths for the
    `collections` and `lenses` stores (write paths since SUR-726). Bare metadata rows, no crypto;
    `LensRecord` carries `leaf_ids` / `combinator` / `threshold`.
  - `untagged_notes(limit, offset)` + `untagged_notes_count()` — notes with no idea tags (the
    BulkDiscovery work queue + its badge), same decrypt-in-core / scan-then-filter shape.
  - New `uniffi::Record` types: `IdeaCount`, `CollectionRecord`, `LensRecord`. Swift + Kotlin
    bindings regenerated via `scripts/gen-bindings.sh`; all six methods are ≤3 FFI args (clear of the
    SUR-843 arm64 arg-slot guard). Round-trips added in Kotlin + Swift and the desktop-jar
    consumer-smoke (AC #3). Decrypt-in-core routes through the existing `decrypt_note_text` gate — no
    second decrypt path, no crypto constants or ciphertext touched. Delivery: cut in the v0.5.0
    release, then the `chore(core): pin braird-core v0.5.0` bump in braird-ios + braird-android.
- **SUR-915 — duplicate-resolution merge contract over the FFI.** Three host-invoked merge verbs on
  `SyncEngine` for the native duplicate-resolution surfaces (iOS SUR-863 / Android SUR-877), the
  byte-mirror of the PWA's `mergeBooks` / `unmergeBooks` / `mergeNotes` (`surfc/src/db.js`). All three
  are **key-free store-level patches** — no vault, no re-seal; a moved note's `content_tag` is nulled
  for the existing self-heal to re-derive, never recomputed here:
  - `merge_books(survivor_id, loser_ids) -> BookMergeUndo` — rehome every live note off each loser
    onto the survivor (narrow `book_id` + `content_tag=null` patch, so decrypt-failed notes rehome too),
    keep the earliest `created_at`, tombstone the losers, and record the loser→survivor redirects in
    the device-local `mergedBookIds` map so the fleet + decrypt-failed stragglers converge via the
    existing `reconcile_stranded_notes` on their next pull. Replay-safe and ordered for crash-safety
    (the core can't span one SQLite transaction across the outbox writes the oracle does in one Dexie
    transaction): redirects recorded FIRST (an interrupted merge still converges), then notes rehomed,
    then losers tombstoned LAST — only after every rehome staged (fail-fast via `?`). A completed-merge
    re-run is a no-op. Returns the `BookMergeUndo` token for the host's 10-second window. The map is
    device-local (PWA parity), so full always-to-survivor convergence of a straggler note the merging
    device never saw is deferred to **SUR-916** (native equivalent of the PWA's deferred server-side
    merge) — native ships at parity here, not behind the web.
  - `unmerge_books(undo)` — the inverse: restore each note's prior book, un-tombstone the losers,
    prune ONLY the redirects still pointing at this merge's survivor, and restore the survivor's
    `created_at` to the **earliest of its pre-merge value and any loser STILL merged into it** (a
    later merge into the same survivor must not be clobbered when an older merge is undone).
    Idempotent. The token is ephemeral (not persisted) — the 10s window is host UX. Un-merging BEFORE
    the merge's outbox flush resurrects the loser atomically (drop pending tombstone + stage
    `deleted:false` in one transaction; the outbox collapse makes `deleted` sticky).
  - Undo-token fidelity: `merge_books` never returns an *undoable* token for a loser it didn't
    faithfully rehome this call — a completed-merge retry (loser already deleted) is skipped, and a
    resumed partial merge (a crashed attempt already moved the notes) is completed but left
    un-undoable, so `unmerge_books` on such a token can't resurrect an empty duplicate or strand the
    already-moved notes.
  - `merge_content_duplicates(survivor_id, loser_ids, allow_cross_cluster) -> u32` — a checked,
    explicit-survivor wrapper over the existing `merge_into_survivor` (union tags, adopt image,
    re-point `note_links` + `collection_memberships`, tombstone loser notes last). The exact path
    (`allow_cross_cluster=false`) requires all selected live notes in one non-empty `content_tag`
    cluster; the host's fuzzy (0.92) path sets the flag to span clusters.
  - New `uniffi::Record` types `BookMergeUndo` + `NoteBookAssignment`; Swift + Kotlin bindings
    regenerated; all verbs ≤3 FFI args (clear of the SUR-843 arg-slot guard). Round-trips added in
    Kotlin + Swift + the desktop-jar consumer-smoke. Batched into the v0.5.0 release with SUR-858 (no
    dedicated cut); pins in braird-ios + braird-android follow the tag.

### Changed

- **CI: point the surfc-clone jobs + the pinning doc at the `braird` org** (`pentoaswordfight` →
  `braird`, after the repo transfer). The five drift/parity/supabase workflows and
  `docs/pinning.md` now use canonical `braird/*` URLs instead of relying on GitHub's transfer
  redirect (the rotated `SURFC_READ_PAT` already resolves either way). No crate change.

## [0.4.4] - 2026-07-14

Eighth tagged release. Completes the `reconcile-content-tags` native-parity behavior: SUR-884 adds
the content-tag **self-heal** half — re-derive a null/empty `content_tag` from a note's decrypted
text so a note tag-nulled by a rehome/detach (SUR-820) is re-tagged and clustered on the next pull
WITHOUT a user edit — the counterpart to SUR-835's collapse half. It's the one reconcile pass that
holds keys (a bounded crossing of the key-less sync layer, following the `sync::read` decrypt-on-read
precedent), and the healed tag is persisted **local-only** (`Store::apply_row`, no `updated_at`
bump), so it can't clobber a concurrent edit under `notes`' whole-row LWW; convergence rides the
dedup pass's propagated loser soft-delete. **No FFI/bindings change** — `reconcile()` and the
`pull_then_flush`/`pull_and_reconcile` free functions gain a `&Vault` param, but no
`#[uniffi::export]` signature or record changes and `ReconcileSummary` is unchanged, so this ships as
a pure core-pin bump with no host code change. No crypto constants or ciphertext touched. Flips the
native-parity manifest row `reconcile-content-tags` `waived` → `core` (both halves now land).
Delivery: the `chore(core): pin braird-core v0.4.4` bump in braird-ios + braird-android
(`docs/pinning.md`).

### Added

- **SUR-884 — content-tag self-heal (the second half of `reconcileContentTags`).** A new pass,
  `reconcile_heal_content_tags` (`src/sync/reconcile.rs`), runs on post-pull reconciliation between
  stranded-notes and content-dedup: for every live note with a null/empty `content_tag` and
  decryptable text, it re-derives the tag (`Vault::content_tag` = the SUR-638 per-user HMAC over
  `normalize(plaintext)` + `book_id`) so the SUR-835 dedup pass — which keys on the STORED tag and
  never decrypts — can cluster it. This closes a real gap: `reconcile_stranded_notes` **nulls**
  `content_tag` on a rehome/detach (the tag bakes in `book_id`), and such a note stayed tagless and
  un-clustered on native until its next user edit; the PWA heals it at load. With this, a
  rehome-nulled duplicate is re-tagged and collapsed in the SAME pass, no edit required — flipping
  the native-parity manifest row `reconcile-content-tags` from `waived` to `core` (both halves now
  land). Byte-matches the PWA's SUR-638 vectors.
- **Decrypt-failure gate + local-only persistence.** Plaintext is read through the exact
  `decrypt_note_text` gate the SUR-744 read surface uses (one source for the `decryptError` skip),
  so an undecryptable note is left tagless, never fingerprinted from unreadable ciphertext. This is
  the ONE reconcile pass that holds keys — a bounded crossing of the otherwise key-less sync layer,
  following the precedent `sync::read` already set; plaintext stays transient (only the opaque HMAC
  tag is written). The healed tag is persisted **local-only** (`Store::apply_row`, no `updated_at`
  bump), mirroring the oracle's no-`updatedAt` heal: it never enters the outbox/LWW path, so a
  tag-only write can't clobber a concurrent edit under `notes`' whole-row LWW. Convergence is
  unchanged — it rides the dedup pass's loser soft-delete (which does propagate), and two devices
  re-derive identical tags and pick the same survivor. Idempotent; best-effort (a heal hiccup never
  fails the pull, retried next pull). **No FFI/bindings change** — `reconcile()` and the
  `pull_then_flush`/`pull_and_reconcile` free functions gain a `&Vault` param, but no
  `#[uniffi::export]` signature or record changes; `ReconcileSummary` is unchanged (the heal count
  is logged, not surfaced), so this ships as a core-pin bump with no host code change. Delivery: the
  `chore(core): pin braird-core vX.Y.Z` bump in braird-ios + braird-android.

## [0.4.3] - 2026-07-11

Seventh tagged release. Ships two new cases on the post-pull reconciliation pass (SUR-820):
**content-tag retroactive dedup** (SUR-835 — collapse duplicate notes into one deterministic,
cross-device-convergent survivor via the full `mergeNotes` port) and **Open Library cover
resolution** (SUR-828 — the core's FIRST non-Supabase egress, behind the SUR-492
`openlibrary_egress` kill-switch, paced at ≤10 Search-API calls per pass and fail-soft). Both ride
the existing SUR-820 pass; `ReconcileSummary` gains two additive `u32` fields (`dupesCollapsed`,
`coversResolved`) and the Kotlin + Swift bindings are regenerated — hosts regenerate when they pin.
Delivery to devices is the `chore(core): pin braird-core v0.4.3` bump in braird-ios (SUR-829) +
braird-android (SUR-857). No crypto constants touched; note text / ciphertext unchanged.

### Added

- **SUR-835 — content-tag retroactive dedup as a reconciliation case.** A fourth case on the
  post-pull reconciliation pass (`src/sync/reconcile.rs`, SUR-820): live notes that share a
  `content_tag` (the SUR-638 per-user HMAC content fingerprint) are collapsed into one survivor,
  porting the PWA's `mergeNotes` (`surfc/src/db.js`) — the losers' tags are unioned onto the
  survivor, its image adopted only when the survivor has none, its `note_links` edges and
  `collection_memberships` re-pointed (self-loops and duplicates dropped/tombstoned), and the
  losers soft-deleted. Every mutation is staged through the outbox (LWW-safe). The survivor is
  chosen deterministically — most tags, then earliest `created_at`, then **lowest `id`** as a total
  tiebreak — so two devices reconciling independently converge on the SAME keeper rather than
  soft-deleting each other's survivor; this final `id` key is stricter than the oracle (which leans
  on JS stable sort over load order) only on a measure-zero exact tie. Dedup keys on the stored
  `content_tag` alone — note text is never decrypted here. Idempotent (a second pass is a no-op);
  best-effort like the dropped-tag pass, so a hiccup never fails the pull. No crypto constants or
  ciphertext touched. The child-row re-points (`note_links`, `collection_memberships`) run BEFORE the
  loser soft-deletes and fail-fast: a loser is only tombstoned once all of its edges/memberships have
  been re-pointed onto the survivor, so a transient write failure defers the whole collapse to the
  next pull rather than stranding a live edge against a tombstoned note (the core can't span the
  oracle's single Dexie transaction across separate outbox writes).

  **FFI:** `ReconcileSummary` (nested on `PullSummary`) gains a `dupesCollapsed: u32` field;
  Kotlin + Swift bindings regenerated via `scripts/gen-bindings.sh`. Purely additive.

- **SUR-828 — Open Library cover resolution as a reconciliation case.** A new case on the
  post-pull reconciliation pass (`src/sync/reconcile.rs`, SUR-820) that resolves book covers for
  natively-created books — SUR-198 parity, since the PWA only resolves covers on its own create
  path, leaving iOS/Android-created books coverless on every client. Mirrors the PWA's `resolveCover`
  (`surfc/src/lib/coverResolver.js`): a book WITH an ISBN gets a deterministic
  `covers.openlibrary.org/b/isbn/<isbn>-M.jpg?default=false` URL by pure construction (no network
  call); a book WITHOUT an ISBN queries the Open Library Search API for a `cover_i` (else a healed
  ISBN — the SUR-566 self-heal). Persists `cover_url` + `cover_source='openlibrary'` +
  `cover_resolved_at` through the outbox (LWW-safe); manual covers are never touched. A miss STAMPS
  `cover_resolved_at` (SUR-566 — so the pass never re-queries the same edition) while a transient
  outage leaves it unstamped to retry. A later metadata edit (new title/author/ISBN via
  `enqueue_book`) bumps `updated_at` past the stamp, re-opening the book for resolution on the next
  pass — mirroring the PWA's create/edit re-resolution, so a corrected book is no longer stuck
  coverless (covered books are left as-is).

  **⚠ New egress boundary — the core's first non-Supabase egress.** Introduced behind a dedicated,
  greppable `CoverEgress` trait (kept OFF `PostgrestSink`) so the boundary is explicit for review.
  Three guards, all mirroring the PWA: (a) **kill-switch** — the global SUR-492 `openlibrary_egress`
  `app_config` flag is read through the existing Supabase client (`fetch_app_config`) and, when
  `{"enabled": false}`, skips the whole pass (zero egress, no new `covers.openlibrary.org` URLs);
  it **fails open** on a missing row / read error / malformed value; (b) **pacing** — at most 10
  Search-API calls per pass (ISBN books are construct-only and free), the rest deferred to the next
  pull; (c) **fail-soft** — an Open Library outage never fails reconciliation or the pull. No crypto
  constants or ciphertext touched.

  **FFI:** `ReconcileSummary` (nested on `PullSummary`) gains a `coversResolved: u32` field;
  Kotlin + Swift bindings regenerated via `scripts/gen-bindings.sh`. Purely additive.

## [0.4.2] - 2026-07-11

Sixth tagged release. Ships the **arm64 `enqueue_book` FFI fix** (SUR-843 — collapsed to a
`BookUpsert` record; BREAKING binding, hosts update their call-site when they pin this) plus
its **static arg-slot guard**, which makes the whole stack-spill class fail x86-64 CI instead
of only a real arm64 device. Also carries two CI-only guards with no crate or artifact change:
the **native-parity drift guard** (SUR-842) and the **AGP 9.2.1 producer-side compat**
verification (SUR-854, docs only). No crypto constants touched; note text / ciphertext unchanged.

### Added

- **SUR-843 — static guard for the arm64 wide-FFI stack-spill class.**
  `scripts/check-ffi-arg-slots.mjs` (run in the `bindings-drift` job) inspects the generated
  Kotlin externs and fails the build on any `#[uniffi::export]` method that lands a by-value
  `RustBuffer` (a lowered `String`/`Option`/`Vec`) at integer-slot ≥9 — the exact arm64
  (AAPCS64 + JNA/libffi #1259) defect that x86-64 CI and the desktop `:core-roundtrip` jar are
  structurally blind to. It counts integer/pointer slots only (`f64`/`f32` ride the FP bank and
  consume none — why `enqueue_note_signals` is safe), so the fix is to collapse the args into a
  `uniffi::Record`. Verified it flags the pre-fix `enqueue_book` (`clearNullableFields` at slot
  11) and nothing else across the whole binding surface. Node/CI tooling only — no crate code.
  The convention is now written into `CLAUDE.md` + `GATING.md`.

- **SUR-842 — native-parity drift guard.** A new CI surface that fails the build when
  surfc's sync-behavior registry (SUR-845, emitted as `src/sync/sync-surface.json`) grows,
  loses, or re-describes a behavior that this repo hasn't accounted for. `vendored/native-parity/sync-surface.json`
  is a byte copy of that snapshot; `vendored/native-parity/manifest.json` maps every one of the
  23 registered behaviors to its native home (`core`/`ios`/`android` ticket) or a reasoned
  waiver. `scripts/check-native-parity.mjs` re-fetches the live snapshot from `surfc/main` and
  asserts (a) the vendored copy is current and (b) the manifest covers every behavior (waivers
  require a reason; ported rows require a ticket) — fail-loud, naming the offending id.
  `.github/workflows/native-parity-drift.yml` runs it per-PR + weekly (mirrors `schema-drift.yml`;
  needs `SURFC_READ_PAT`). **Node/CI tooling only — no crate code, no `Cargo.toml` change.** This
  turns the 2026-07-09/10 PWA-parity audit's class-of-gap from audit-caught into CI-enforced.

### Changed

- **SUR-843 — `enqueue_book` takes a `BookUpsert` record, not 10 positional args (BREAKING
  binding).** Same arm64 FFI fix as `enqueue_note` → `NoteUpsert` (SUR-770): the positional
  signature spilled its trailing `clear_nullable_fields: Vec<String>` to a by-value `RustBuffer`
  at FFI slot 11 — past x7, onto the stack, where JNA's libffi mis-marshals it on arm64. A record
  lowers as ONE `RustBuffer` (3 slots, all in registers). **Latent, not a shipped crash** — no
  host called `enqueue_book` on arm64 yet (book creation is deferred to SUR-819); converted now
  at the cheapest moment (zero call-sites to churn). Field semantics are byte-for-byte the old
  signature; named to pair with the read model `BookRecord`. Hosts update their call-site when
  they pin the release that ships this. No crypto constants touched; no store/schema change.

- **SUR-854 — AGP 9.2.1 producer-side compat verified (docs only).** braird-android bumped
  to AGP 9.2.1 (SUR-853); confirmed the pinned braird-core AAR (`v0.4.1`) resolves under it
  with no change. The released AAR declares `minAndroidGradlePluginVersion=1.0.0` /
  `minCompileSdk=1` (module sets no `aarMetadata{}` override), so any AGP ≥ 1.0.0 consumer
  satisfies it — forward-compat holds. Desktop-jar `:core-roundtrip` is `kotlin("jvm")`, AGP-
  independent, unaffected. Decision recorded: **leave the AAR-build module on AGP 8.13.0** (no
  align-at-next-cut). `docs/pinning.md` now carries a *Toolchain & AGP compatibility* section
  noting the 9.2.1 consumer baseline. No crate code, no artifact change.

## [0.4.1] - 2026-07-10

Fifth tagged release. Ships the **arm64 `enqueue_note` FFI fix** (SUR-770 — collapsed to a
`NoteUpsert` record; BREAKING, hosts update their call-site when they pin this) and the
**post-pull reconciliation pass** (SUR-820). No crypto constants touched; note text /
ciphertext unchanged.

### Added
- **Post-pull reconciliation pass (SUR-820).** After every `pull()`/`sync()`, the core now
  automatically runs three referential/taxonomy repairs the PWA previously ran alone in
  `fetchAllCloud` (SUR-659 explicitly excluded these from the core; briefly re-homed to Android
  at SUR-768): (1) backfill a book referenced by a live note but absent locally, by fetching it
  from the server; (2) repoint a live note stranded on a soft-deleted (offline-merged) book to
  the known survivor, or detach it locally-only when no survivor is known (never pushed — mirrors
  the PWA's LWW-safety rule exactly); (3) convert a live note tag that matches neither the
  vendored canon (`vendored/canon/great-ideas.json`, drift-guarded against `surfc/main`) nor an
  existing custom idea into a new custom idea, using the oracle's exact deterministic id format
  (`cidea_sur597_{userId}_{slug}`) for full coexistence with rows the PWA already created — this
  is a deliberate generalization past the PWA's static 26-name `DROPPED_LEAVES` check, so a
  future canon revision can't orphan tags the way the historical v14 swap did. Reconciliation is
  best-effort: a failure never fails the `pull()`/`sync()` it's attached to (a strengthening past
  the oracle's stricter, non-try-caught 2b/2c behavior — flagged for `sync-reviewer`), and is
  **skipped entirely on a partial pull failure** (mirroring `pull_then_flush`'s existing SUR-736
  guard) — a table that failed to pull this round is stale, and reconciling against stale data
  (e.g. `reconcile_dropped_tags` reading a `custom_ideas` mirror that just missed this round's
  pull) risks recreating or overwriting a row another device already converged. New
  `PullSummary.reconcile: ReconcileSummary` field (additive `#[uniffi::export]` surface) →
  Swift + Kotlin bindings regenerated. New `vendored/canon/**` + `scripts/extract-great-ideas.mjs`
  + `.github/workflows/canon-drift.yml`, added to `GATING.md`'s sync-engine row. No crypto
  constants touched; note text/ciphertext unchanged. Gate: `sync-reviewer` + `crypto-reviewer` +
  `naming-reviewer`.

### Changed
- **`SyncEngine::enqueue_note` now takes a single `NoteUpsert` record instead of 14 positional
  arguments (SUR-770).** BREAKING for hosts (update the call-site). This is a **bug fix**, not just
  ergonomics: a 14-argument UniFFI call lowers to ~16 FFI slots, and on **arm64 Android** the
  arguments past the 8th spill onto the stack, where JNA's bundled libffi mis-marshals the by-value
  `RustBuffer` args (the java-native-access/jna#1259 class of defect — NOT fixed by any released JNA;
  tested 5.17.0 and 5.19.1, both fail identically). The first byte-validated stack argument
  (`deleted`) then failed at runtime with `InternalException: Failed to convert arg 'deleted':
  unexpected byte for Boolean` on the very first real call. x86-64 (SysV) lays the same arguments out
  differently and PASSED, so the `:core-roundtrip` desktop jar and every CI leg were structurally
  blind to it; iOS (UniFFI's Swift backend, no JNA) is unaffected. `NoteUpsert` (a `uniffi::Record`,
  named to pair with the read model `NoteRecord`) lowers as a SINGLE `RustBuffer` → 3 FFI slots, all
  in registers, so nothing spills. Field semantics are byte-for-byte the old positional signature;
  Swift + Kotlin bindings regenerated; the `#[allow(clippy::too_many_arguments)]` is gone. No crypto
  constants touched; note text/ciphertext unchanged. Proven on-device by braird-android's new
  `EnqueueNoteOnDeviceTest` (the arm64 analogue of the x86-64 `PinnedCoreRoundTripTest`). NOTE: the
  sibling wide-arg exports (`enqueue_note_signals`, `enqueue_lens`, `enqueue_note_link`, …) carry the
  same latent arm64 defect and are NOT yet converted — tracked as follow-up. Gate: `crypto-reviewer`
  + `naming-reviewer`.

## [0.4.0] - 2026-07-08

Fourth tagged release. Ships **`Vault::unlock_from_blobs`** (SUR-812) — the trial-decrypt
wrapper-selection primitive the native iOS/Android hosts (and the PWA-WASM host) share, so the
multi-wrapper `OperationError` can't be reinvented per host. Additive `#[uniffi::export]` constructor;
no wire-format change, frozen constants untouched. Cut so braird-android 661e (SUR-765) can pin a core
that carries the primitive its walking-skeleton unlock calls.

### Added
- **`Vault::unlock_from_blobs(prf, blobs)` — trial-decrypt wrapper selection (SUR-812).** A shared-core
  primitive so the native iOS/Android hosts and the PWA-WASM host share one correct wrapper-selection
  rule instead of each reinventing a fragile one. It tries each candidate `prf-v1` wrapped blob with the
  asserted PRF and returns the `Vault` for the one that decrypts (`DecryptFailed` iff none do); a
  malformed candidate is skipped, not fatal. This fixes the multi-wrapper `OperationError`: the old
  host-side "first active `prf-v1` blob" pick throws whenever an account has ≥2 wrappers (linked devices
  / synced passkeys) and the first row isn't the asserted credential's — a `prf-v1` blob only decrypts
  under its own credential's PRF, so correctness must be the trial decrypt, never a positional or
  equality-only pick. Device-transfer create is `unlock_from_blobs(prf, active_prf_v1_blobs)` → `pin_wrap(pin)`;
  the single-blob `unlock` and the `redeem_pin_transfer` redeem path are **unchanged**. The core stays
  credential-agnostic — `WrappedBlob` is unchanged and any `credential_id` ordering is a host-side
  fast-path hint, never a filter. **Additive `#[uniffi::export]` constructor** → Swift + Kotlin bindings
  regenerated via `scripts/gen-bindings.sh` (the `bindings-drift` guard verifies); a Rust parity test +
  Swift/Kotlin round-trips exercise multi-wrapper recovery, order-independence, non-match, malformed-skip,
  and PWA↔native coexistence (a PWA-produced wrapper decrypts via `unlock_from_blobs`). No wire-format change,
  frozen constants untouched. Gate: `crypto-reviewer` + `naming-reviewer`.

## [0.3.0] - 2026-07-08

Third tagged release. Adds the **Home-surface read queries** (SUR-806) so the reinstated iOS
(SUR-807) / Android (SUR-808) Home screens can pin a core that serves their data. Additive,
read-only, decrypt-in-core — no protocol constants or wire format touched.

### Added
- **Home-surface read queries over the FFI (SUR-806).** Three additive reads on `SyncEngine`, so a
  native Home screen gets its stat row + "Recently surfaced" card from the core (never its SQLite),
  decrypting in core exactly like the SUR-744 M6 subset. Additive only — no existing read changed.
  - **`counts()` gains `active_ideas`** — the count of distinct idea **tags** on ≥1 live note (the
    PWA Home's `activeIdeasCount`). Distinct from the existing `custom_ideas` (raw idea-row count,
    for Profile): canon **and** custom tags both count, a tag on no live note doesn't. Tags are a
    plaintext `Json` column, so this never decrypts — a `HashSet` union over `notes.tags` mirroring
    surfc's `ideaCountsFor` (the oracle's `count > 0` filter is a no-op — a key exists only by an
    increment). `StoreCounts` widened in place (additive field; no shipped native consumer yet).
  - **`notes_this_week(now_ms)`** — count of live notes created within the last 7 days whose
    **decrypted** text is non-empty, **byte-matching** the PWA's `notesThisWeek`: a rolling 168h
    window on `created_at` (`now_ms - 7*24*60*60*1000`, inclusive lower bound — pure epoch-ms math,
    no calendar), with empty/whitespace text and decrypt failures excluded. `now_ms` is the host's
    `Date.now()` (this core has no read-side clock), so the count is a pure function — deterministic
    at the window boundary. It window-filters on `created_at` **before** decrypting, so a weekly
    count never pays to decrypt the whole archive.
  - **`recent_note(now_ms, seed)`** — a pseudo-random note from that same "this week" set (the
    "Recently surfaced" card), or `None` when nothing is fresh — reproducing the PWA's
    `fresh[floor(random()*len)]` pick, coupled to the same set (card hidden when empty). `seed` is
    the host's random draw so the pick is deterministic (testable; the host re-rolls to re-surface,
    as the PWA re-runs its memo on a `notes` change). Decrypts in core → `NoteRecord.text` is
    plaintext; no `enc:` ciphertext or key bytes cross the FFI (AC #2/#3, reusing the SUR-744 seam).
  - **FFI:** new binding surface → Swift + Kotlin bindings regenerated via `scripts/gen-bindings.sh`
    (the `bindings-drift` guard verifies). Swift + Kotlin round-trips and the desktop-jar
    consumer-smoke exercise all three (incl. the no-`enc:`-sentinel guard). Rust fixtures pin the
    window boundary, the text/decrypt-failure exclusions, distinct-tag counting, and the
    deterministic `seed` pick. New surface + decrypt path → `naming-reviewer` + `crypto-reviewer`.

## [0.2.0] - 2026-07-04

Second tagged release. Cuts the `[Unreleased]` history accumulated since `v0.1.0` into `v0.2.0`:
the **iOS `BrairdCore.xcframework` release leg** (SUR-745) — so braird-android's sibling
braird-ios (SUR-660) has a published, checksum-pinned xcframework **+ `BrairdCore.swift`** to pin —
and the **tri-state enqueue field-clearing FFI** (SUR-775). This is the first release to exercise
the macOS `build-ios` leg end-to-end (`release.yml` runs on tags only, so the leg had never run
until this tag).

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
- **iOS `BrairdCore.xcframework` release leg — versioned, checksum-pinned SwiftPM binary artifact
  (SUR-745, M0 prerequisite for the SUR-660 iOS app).** The core now ships to braird-ios as a pinned
  artifact on the **same `v*` tag** as the Android AAR/jar — no moving-core build, no UniFFI API
  drift going undetected.
  - **`release.yml` restructured into a `validate → {build-android, build-ios} → publish` DAG.** The
    two build legs run in parallel off one validated tag; a single `publish` job assembles every
    artifact into one `SHA256SUMS.txt` and cuts the release once (create-only immutability
    preserved). The new **`build-ios`** leg (`macos-14`) builds the xcframework and drives the FFI
    round-trip through **two shipped slices** before publish — `swift test` (macOS-host slice) and
    `xcodebuild test` on a real iOS **simulator** (arm64-sim) — mirroring the Android leg's
    consumer self-containment round-trip. `contents: write` is held by the `publish` job **alone**
    (build legs run read-only). Third-party actions (`dtolnay/rust-toolchain`, `Swatinem/rust-cache`)
    are **SHA-pinned** in both legs — they run pre-compile, so a hijacked release could poison the
    artifact before it is checksummed.
  - **`scripts/build-xcframework.sh` takes an optional `[version]`** (mirrors `build-aar.sh`): with a
    version it additionally stages `dist/braird-core-<version>.xcframework.zip` (via `ditto
    --keepParent`, the layout SwiftPM's remote binary target requires) and prints its
    `swift package compute-checksum` value. No version → xcframework-only, so `nightly-macos.yml`'s
    bare call is unchanged.
  - **The Swift wrapper ships as its own checksummed release asset.** The xcframework carries only
    the C FFI + native `.a` slices, not the generated `BrairdCore.swift` wrapper (unlike the AAR,
    which bundles its Kotlin binding). Rather than have the consumer vendor the wrapper from the
    mutable git tag — which would leave half of a checksum-coupled pair pinned to a movable ref —
    `build-ios` publishes the exact `BrairdCore.swift` the two round-trips validated against the
    xcframework, checksummed in `SHA256SUMS.txt`. The iOS consumer pins **both** SHA-256s and fetches
    both from the immutable release. (Fix from the `release-integrity-reviewer` gate.)
  - **`docs/pinning.md`** gains the xcframework + wrapper artifact rows and a *Consumer pin — iOS*
    section: pin the zip by `url` + `checksum` (its `SHA256SUMS.txt` hex is the SwiftPM checksum) and
    fetch-and-verify `BrairdCore.swift` from the release (fail-closed), never from the tag. Slices:
    arm64 iOS device + arm64 iOS simulator + arm64 macOS host. Two slices are FFI-tested pre-publish
    (macOS-host + iOS-sim); the **iOS device slice can't run in CI** and is documented as a residual
    covered by the SUR-660 on-device verification wave. Apple-Silicon-only simulator; an Intel-sim
    (`x86_64`) slice is out of scope.
  - Deliberately **not** changed: `bindings/swift/Package.swift` stays path-based (this repo's own
    `swift test` consumes the local xcframework); the reviewed remote-`binaryTarget` consuming
    manifest lands in braird-ios (SUR-660), as the Android consumer wiring did in braird-android.

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
