# Sync convergence remediation — implementation plan (SUR-736 / 737 / 738 / 739 / 740)

**Status:** DRAFT for founder sign-off. Agent-handoff plan.
**Date:** 2026-07-01 · **Author:** Claude (agent under the gate), reviewed against code on branch `feature/sur-725-...` + `surfc/main`.
**Scope:** braird-core sync engine + surfc Supabase migrations + surfc PWA client + surfc-intranet wiki.

---

## 0. The problem map (read first)

Four defects, two seams, one root cause (client-authored `updated_at` + a passive server):

| Ticket | Seam | Defect | Recoverable? |
|---|---|---|---|
| SUR-736 | write (client) | pull merges a newer remote row; the stale outbox entry survives and the next flush re-pushes it over the server's newer row | no |
| SUR-740 | write (server) | generalisation of 736: `merge-duplicates` is unconditional, so the server is last-**flush**-wins. Fires under flush-then-pull too — the flush destroys the newer server row *before* the pull can fetch it | **no** — content destroyed at source of truth |
| SUR-739 | read | `updated_at` is stamped at enqueue but server-visible at flush; a delayed flush lands *behind* other devices' cursors and is never fetched | yes (full re-pull) |
| SUR-738 | UX | all LWW losses are silent; `PullSummary` carries only aggregate counts | n/a |

SUR-737 is the ratification companion: array columns converge whole-row-LWW by design (mirror of the oracle); document + pin it before the SUR-726 fan-out.

**The layered endgame** (why the PR split below is what it is):

1. **Client, now (PR-1/2):** outbox rebase on pull + `sync()` = pull→flush + conflict signal. Shrinks the write-side loss window from "unbounded outbox age" to "in-flight seconds". No wire change, PWA untouched.
2. **Server, one migration window (PR-3):** LWW guard trigger (SUR-740, write side) + `change_seq` visibility watermark (SUR-739, read side) + keyset pagination groundwork (SUR-652). The trigger keeps the newest edit at the server; the watermark guarantees whatever the server holds gets delivered. Either alone leaves divergence; together they give convergence — including for the PWA, with **no PWA flush-code changes**.
3. **Clients consume the watermark (PR-3 PWA leg + PR-4 core):** cursor keyed on `change_seq`, paged.

---

## 1. Non-negotiables (all four repos)

- **No stacked PRs.** Every branch off `origin/main`; every PR bases `main`. PR-2 and PR-4 *wait for their predecessor to merge*, then branch off updated `main` — sequenced, never stacked. If any branch needs `main` merged/rebased in: **STOP and flag the founder.**
- **Agent under the gate.** Propose, don't merge. Name the personas in each PR body. Spine everywhere here.
- braird-core: `CHANGELOG.md` `[Unreleased]` entry per PR (CI-enforced). New FFI surface → `touches-ffi` label (opts into the nightly macOS Swift leg). Conventional Commits with `[SUR-XXX]`.
- surfc: migrations follow `supabase/_TEMPLATE.sql` (explicit REVOKE/GRANT, RLS notes). Run `npm run test:db` locally (Docker) before push; known `db-test` flake on storage-container restart → `gh run rerun --failed`, report which it was. **Data-model change ⇒ parallel `surfc-intranet` wiki PR, same branch name, parallel-merge flagged in the PR body.**
- E2EE invariants: pull stores ciphertext verbatim; rebase touches outbox rows whose `text` is already sealed — **no decrypt anywhere in this plan**; never log payload contents (ids + timestamps only).
- After each merge: `docs/learnings/` entry if anything was non-obvious.

---

## 2. PR-1 — braird-core: outbox rebase + conflict signal + `sync()` (SUR-736 + SUR-738)

Branch: `feature/sur-736-outbox-rebase-on-pull` (base `main`, after SUR-725 / PR #8 merges).

### 2.1 `src/store.rs`

New method (same `unchecked_transaction` pattern as `stage_local_write`):

```rust
/// Apply a pulled LWW-winning row AND drop any stale pending outbox entries for the same
/// record, atomically. Returns the dropped entries (outbox id, payload updated_at) so the
/// caller can surface them as conflicts (SUR-738).
pub fn apply_row_rebasing_outbox(
    &self, table: &str, row: &Map<String, Value>, incoming_updated: i64,
) -> rusqlite::Result<Vec<(i64, i64)>>
```

Inside one transaction:
1. `apply_row(table, row)`.
2. `SELECT id, payload FROM outbox WHERE table_name = ?1 AND record_id = ?2` (pk from the incoming row; parse each payload with serde — **no JSON1 dependency**, no new index: the outbox is small and this runs only on LWW wins).
3. `DELETE` the ids whose payload `updated_at <= incoming_updated`.
4. Commit. Any early `?` rolls both back.

Rules: malformed payloads are left queued (they can never flush, so they can't cause the 736 overwrite); `record_id IS NULL` rows are out of scope (the core always sets it). Atomicity is the point — apply-without-drop re-opens the bug; drop-without-apply loses the edit locally *and* never pushes it.

The `<=` guard is defensively redundant today (`stage_write` stamps row + payload together, so pending stamps ≤ local row ts < incoming when `should_apply`), but keep it: it protects future enqueue paths and self-documents the criterion.

### 2.2 `src/sync/pull.rs`

- In `pull_table`, replace the `should_apply → store.apply_row(...)` arm with `apply_row_rebasing_outbox(...)`.
- `TableStats`/`PullResult` gain `conflicts: usize` and `conflicted: Vec<ConflictedRecord>`. One conflict per record with ≥1 dropped entry: `{ table, record_id, discarded_updated_at: max(dropped ts), winning_updated_at: incoming }`.
- No change to the strict-`>` merge, tie-keeps-local, tombstone skip, cursor mechanics.

### 2.3 `src/sync/mod.rs` (FFI surface — `touches-ffi`)

```rust
#[derive(uniffi::Record)]
pub struct ConflictedRecord { table: String, record_id: String, discarded_updated_at: i64, winning_updated_at: i64 }
// PullSummary gains: conflicts: u32, conflicted: Vec<ConflictedRecord>
#[derive(uniffi::Record)]
pub struct SyncSummary { pull: PullSummary, flush: FlushSummary }
pub fn sync(&self) -> Result<SyncSummary, SyncError>   // pull() THEN flush()
```

**`sync()` order is pull-then-flush** — a deliberate, documented divergence from the oracle's flush-first (same class as the per-table cursor divergence). Rationale to state in the doc comment: with rebase, pulling first fetches the server's newer row and rebases the stale entry away, so the flush pushes nothing stale; flush-first *destroys* the newer server row before the pull can see it (SUR-740). Error semantics mirror the PWA: a hard pull failure (all tables) aborts before flushing; a partial pull failure proceeds to flush. Update `pull()`'s doc comment: the flush-before-pull contract is superseded by rebase + `sync()`; hosts calling the granular methods in any order are now safe from 736 (but not from 740 — that's the server's job, PR-3).

### 2.4 Tests (each edge case from the review, pinned)

Unit (`pull.rs` / `store.rs`):
1. Pending T1, incoming T2>T1 → row = remote, outbox empty, `conflicts == 1`, record surfaced.
2. Pending T3, incoming T2<T3 → no apply, outbox intact, `conflicts == 0`.
3. Tie (incoming == local row ts) → keep local, outbox intact (no rebase runs).
4. Incoming **tombstone** T2 over pending edit T1 → tombstone applied, entry dropped, conflict counted.
5. Pending **delete** T1 vs incoming live edit T2 → live row applied, delete dropped, conflict counted. (Edit-beats-older-delete — symmetric with `stale_tombstone_does_not_revive_a_newer_local_row`; not a resurrection violation.)
6. Multi-entry mixed (pending T1+T3, incoming T2) → no apply, both entries survive, later collapse/flush unchanged.
7. Malformed pending payload → left queued, pull succeeds, no panic.
8. Boundary: pending payload ts == incoming ts while row ts < incoming (synthetic) → dropped (`<=`).

Integration (`tests/sync_736_integration.rs`, stub-sink like `sync_725_integration.rs`):
- The ticket's window: enqueue T1 → `pull()` merges T2 → `flush()` → **assert no upsert dispatched** for that record; summaries correct.
- `sync()` call-order test (sink records: fetch before upsert) + abort-on-total-pull-failure test.

### 2.5 Gate

`sync-reviewer` + `crypto-reviewer` (repo standing gate) + `naming-reviewer` (new public names: `sync`, `SyncSummary`, `ConflictedRecord`, `conflicts`/`conflicted`, `apply_row_rebasing_outbox`). CHANGELOG entry. `touches-ffi` label. Founder sign-off; explicitly call out the **pull-then-flush divergence** as the decision being ratified.

---

## 3. PR-2 — braird-core: ratify array LWW (SUR-737)

Branch after PR-1 merges (touches the same files — sequence, don't stack): `feature/sur-737-array-convergence-ratification`.

Docs-and-tests only, no behaviour change:

- Comment block on `synced_schema()` in `src/store.rs`: per-table convergence intent (table below). Per-descriptor one-liners on `notes.tags`, `lenses.leaf_ids`, `note_signals` counters.
- Comment at the `pull_table` merge site: "array columns are whole-row LWW by design (mirror of `mergeCloudRecords`); union without element tombstones can't delete; any convergence change must land PWA+core in lockstep (wire-visible)."
- Pin tests (pull_table is table-agnostic — pre-pin the fan-out tables ahead of SUR-726): two-device `tags` divergence collapses to the newer whole array, both directions; same for `lenses.leaf_ids`; `collection_memberships` row-granular add/remove convergence (deterministic id ⇒ concurrent adds collapse to one row).

| Table | Array/composite | Convergence | Why |
|---|---|---|---|
| books | — | whole-row LWW | scalar metadata; nulls authoritative (cover-clear must converge) |
| notes | `tags`, `source_meta` | whole-row LWW **(ratified)** | a tag edit is a note edit; union can't delete; OR-set = wire change, future ticket only if product demands |
| custom_ideas | — | whole-row LWW | |
| note_links | — (row-per-edge) | row-level LWW ≈ set | add = insert, remove = tombstone |
| lenses | `leaf_ids` | whole-row LWW | a lens is one authored query; unioning leaves under one combinator/threshold fabricates a query nobody wrote |
| collections | — | whole-row LWW | |
| collection_memberships | — (row-per-pair, `membershipId(collectionId, noteId)` deterministic) | row-level LWW ≈ OR-set | concurrent adds converge to the same row |
| note_signals | counters | whole-row LWW — **lossy, accepted** | concurrent increments lose one side; derived data, self-heals |

Close SUR-737 with this table as the ratification record. Gate: `sync-reviewer`; CHANGELOG entry.

---

## 4. PR-3 — surfc: server LWW guard + visibility watermark + PWA cursor (SUR-740 + SUR-739 + SUR-652-server-side)

Branch: `feature/sur-739-change-seq-watermark` (one PR, two migrations — separable rollback). **Parallel `surfc-intranet` PR, same branch name, parallel-merge flagged.**

### 4.1 Migration A — `NNNN_sur740_lww_guard.sql`

- One trigger function `sync_lww_guard()`: `IF NEW.updated_at < OLD.updated_at THEN RETURN NULL; END IF; RETURN NEW;`
- `BEFORE UPDATE` trigger on **all 8 synced tables**, named to fire **first** (Postgres fires same-event triggers alphabetically): e.g. `t01_lww_guard`.
- **Compares, never stamps** — this is NOT the retracted SUR-725 server-time trigger; `updated_at` stays client edit time.
- **Strictly-older only; equality passes.** Pre-landing verification step: grep surfc for cloud writes that re-send a row *without* bumping `updatedAt` (image/ink-crop hydration adjacent paths, `rehomeStrandedNotes`, import flows) and confirm none would be rejected. Equality-pass is what keeps them safe.
- Follow `_TEMPLATE.sql`: REVOKE ALL on the function from PUBLIC/anon/authenticated (no direct EXECUTE needed — trigger fires as owner); comment the intent.
- Document the caveat: service-role backfills that intentionally write an older `updated_at` are silently skipped (note in migration header + ops runbook).

### 4.2 Migration B — `NNNN_sur739_change_seq.sql`

- Per synced table: `ADD COLUMN change_seq bigint`; per-table sequence; **backfill in `updated_at` order** (deterministic); then a `BEFORE INSERT OR UPDATE` trigger `t02_change_seq` setting `NEW.change_seq := nextval(...)` (trigger-only, no column DEFAULT — avoids double-assign); `SET NOT NULL`; index `(user_id, change_seq)` (RLS filters `user_id`, fetch filters/orders `change_seq`).
- Trigger ordering matters and gets a test: guard (`t01`) fires before stamp (`t02`), so a rejected stale write does **not** bump `change_seq` (no spurious re-delivery).
- `change_seq` is server-only ordering metadata: **never add it to any upsert payload** (client or core). This keeps braird-core's vendored-schema drift guard green by construction — the extractor derives the synced column set from `upsert*` payload keys, and `apply_row` already projects unknown incoming columns away (the `user_id` precedent).

### 4.3 `supabase/test/` additions (run `npm run test:db` locally)

- Guard: older rejected (row unchanged, statement succeeds), equal passes, newer passes; stale DELETE rejected, newer DELETE passes.
- Watermark: `change_seq` strictly increases on insert and update; rejected update leaves it unchanged; backfill ordered by `updated_at`.
- Existing RLS / soft-delete / trigger suites stay green.

### 4.4 PWA client (same PR)

- `src/supabase.js` `fetchSince(table, sinceSeq)` → `.gt('change_seq', sinceSeq).order('change_seq', { ascending: true }).limit(1000)`, **paged loop until a short page** (this is the SUR-652 fix, keyed on the same column — exclusive `gt` is correct now: the watermark is server-assigned and monotonic, no boundary re-pull needed).
- `src/db.js`: per-table cursor meta keys (e.g. `lastSeq:<table>`); keep legacy `lastSyncAt` untouched (rollback safety).
- `src/hooks/useAuth.js` `syncFromCloud`: replace the global `nextCheckpoint`/`saveLastSync` with per-table `max(change_seq seen)`, advanced after each table's merge (or per page — each page is a consistent prefix). Cold start: absent cursor → 0 → **one-time full re-pull** (feature, not bug: it also recovers every row historically skipped by the 739/652 holes). Side benefit to note: per-table cursors fix the pre-existing `fetchOptionalTable` hole where a missing table's rows were skipped once the global cursor advanced.
- LWW merge (`mergeCloudRecords`) unchanged — still client `updated_at`. Flush unchanged — the guard makes the PWA's unconditional upsert harmless.

### 4.5 Gate

`migration-reviewer` (both migrations) + `sync-reviewer` (`db.js`/`supabase.js`). Rollback recipe per migration (guard: `DROP TRIGGER`+function, pure revert; watermark: column+triggers stay inert if client code reverts — old `updated_at` path still works). Wiki updates: `architecture/data` (sync protocol: watermark + guard semantics) + RAID entry. Monitor `gh pr checks`; db-test flake → rerun, report which.

---

## 5. PR-4 — braird-core: consume the watermark + pagination (SUR-739 + SUR-652 core leg)

Branch after PR-3 merges: `feature/sur-739-core-change-seq-cursor`.

- `src/sync/http.rs` `get_since` → filter/order/paginate by `change_seq` (`gt`, asc, `limit=1000`, loop until short page). Adjust `PostgrestSink::fetch_since` accordingly (e.g. `fetch_page(table, after_seq, limit)`).
- `src/sync/pull.rs`: cursor = max `change_seq` seen, read from the **raw** incoming JSON before `apply_row` projects it away; advance per page after that page's merge; empty batch → cursor unchanged (drop the pre-fetch `now_ms` plumbing). Remove `PULL_CURSOR_OVERLAP_MS` if it landed in the interim (verify against the branch at implementation time — it was ticketed as a PR #8 mitigation but is not in the working copy as of this plan).
- `src/store.rs`: **new cursor namespace** (e.g. `sync:seq:<table>`) — old `sync:cursor:<table>` values are epoch-ms (~1.7e12) and would skip everything if reinterpreted as sequence numbers. Absent new key → 0 → one-time full re-pull (mirrors the PWA cold start); delete legacy keys after first successful pull.
- Do **not** add `change_seq` to descriptors or outbox payloads (drift guard + §4.2 rule).
- Tests: paged pull across page boundaries; cursor-not-advanced on mid-page failure; seq-cursor namespace migration (stale ms key ignored); integration re-proof of both-ways coexistence against the local Supabase stack (PWA from PR-3 + core from PR-4 — the SUR-725 §6 harness re-run).
- CHANGELOG; gate: `sync-reviewer` + `crypto-reviewer`; founder.

---

## 6. Sequencing

```
PR-1 (core: rebase+signal+sync)  ──►  PR-2 (core: 737 docs+pins)
        │                                   │
        └── SUR-726 fan-out may proceed after PR-2 (inherits client-ts cursor until PR-4 — accepted, same exposure as today)
PR-3 (surfc: guard+watermark+PWA cursor) ──► PR-4 (core: watermark cursor+pagination)
   └─ parallel surfc-intranet wiki PR (same branch name, lands together)
```

PR-1/2 have no dependency on PR-3/4 and should land first (SUR-736 says with-or-before SUR-726). PR-3 needs a founder-scheduled migration window. Tickets to update on the way: close 736+738 at PR-1, 737 at PR-2, 740+739 at PR-3/4; SUR-652's PWA leg closes with PR-3, its core leg with PR-4.

## 7. Open decisions for the founder (each PR body names its own)

1. **`sync()` = pull→flush** (recommended; documented oracle divergence) vs mirror flush→pull. Rebase ships either way; only the residual window size changes.
2. **Per-record `conflicted` list across the FFI** (recommended) vs count-only.
3. **`change_seq` bigint sequence** (recommended: monotonic, clock-free, keyset-paginates for SUR-652) vs `synced_at timestamptz` trigger-stamp.
4. **Guard tie semantics**: reject strictly-older only (required — equality-pass protects same-ts re-upserts; verification step in §4.1).
5. **PWA/core cold-start full re-pull** acceptance (one-time cost, recovers historical misses).
6. Out-of-scope-but-doored: local `conflict_log` table preserving rebase-dropped ciphertext payloads for a host "restore as copy" flow (Vault decrypt → re-encrypt under a new note id — wire-compatible). Not in this plan; new ticket if product wants zero-loss.

## 8. Accepted residuals (state, don't fix)

- Backward clock skew can stamp a local edit below an already-pulled ts → rebased away next pull. Identical exposure in the PWA; NTP-bounded.
- Exact-ms ties: client tie-keeps-local, server tie = last flush. Split-brain on ms-identical concurrent edits is pathological and unchanged from today.
- Conflicts where the loser was already flushed are undetectable client-side (no causality metadata); full detection = server compare-and-set + representation responses — explicitly out of scope (SUR-738 ratifies silence there).
- `pull()` holds the store mutex across network awaits → local writes block during a pull. Pre-existing; not this plan.
