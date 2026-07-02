---
date: 2026-07-02
ticket: SUR-743
area: [sync, migrations, governance]
gate: GCE
verdict: PASS WITH CONCERNS
artefacts_updated:
  - src/sync/http.rs
  - src/sync/pull.rs
  - docs/learnings/2026-07-02-sur743-sequence-is-not-a-commit-ordered-watermark.md
---

# A unique, monotonic sequence is **not** a commit-ordered watermark: `nextval` allocates non-transactionally

## What happened

SUR-739 added a server `change_seq` visibility watermark (surfc migration 0051) stamped from a **per-table Postgres sequence** via `nextval` in a `BEFORE INSERT OR UPDATE` trigger, and the pull cursor (core `sync:seq:<table>` / PWA `lastSeq:<table>`) keyset-paginates on it with an exclusive `change_seq > cursor`. During founder pre-sign-off of the core leg (#12), SUR-743 found a concurrent flush could make the cursor skip a row **permanently**. The fix (surfc 0052) replaced the sequences with a per-user commit-order counter.

## What surprised me

"Unique + monotonically allocated" felt like it should be a safe cursor key. It isn't. `nextval` allocates **eagerly and non-transactionally** — at *statement* time, not *commit* time — so allocation order ≠ commit/visibility order:

1. T1 (device B flush) allocates `change_seq = 100`, stays in-flight.
2. T2 allocates `101` and commits.
3. A puller sees 101 (100 not yet visible), advances its cursor to 101.
4. T1 commits → row 100 becomes visible.
5. Next pull: `change_seq > 101` skips row 100 **forever** (until a full re-pull).

A monotonic watermark alone does not close the delivery hole — the keyset is skip-safe **only if allocation order == commit order**. That property comes from serializing each user's allocating transactions until COMMIT: an `INSERT … ON CONFLICT DO UPDATE … RETURNING` on a per-user counter row holds the row lock to COMMIT (belt-and-braced by a `pg_advisory_xact_lock`), so a later writer can't allocate the next value until the earlier one commits.

## What the gate caught

Two things the original SUR-739 brief never anticipated:

- **Founder pre-sign-off** caught the concurrency hole itself — the SUR-739 review had ratified the watermark as complete; the skip only surfaces under concurrent flushes, which the happy-path tests didn't exercise.
- **The PR-C reviewers** (sync + migration) independently caught that the commit-order *test* proved the wrong thing: it asserted writers **serialize** (block on the advisory lock) but not the top-level **reader-visibility** invariant — *a reader never sees a higher `change_seq` while a lower one is uncommitted*. The guarantee lives in the `ON CONFLICT` **row lock**, so a test that only probes `pg_locks` for the *advisory* lock pins the scaffolding, not the wall — a refactor could drop the advisory lock (correctly redundant) and the test would falsely fail, or break the row lock and the test would falsely pass. A 3-connection reader/skip-safety test was added.

## What to compound

- **When introducing an ordering/watermark column, the test must assert the reader-visibility invariant** (a reader never observes a higher value while a lower one is uncommitted) — not merely that writers serialize. Serialization is the mechanism; skip-safety is the contract.
- **Prefer a per-user counter bumped under a lock held to COMMIT** (or the `ON CONFLICT` row lock) over a bare sequence whenever the value is used as a keyset cursor. Uniqueness is necessary but not sufficient.
- The stale allocation-order caveats in `src/sync/http.rs` (`get_page`) and `src/sync/pull.rs` (module doc) were removed once 0052 landed — a residual caveat that describes a closed hole is worse than none (it misleads the next reader). Close residual notes in the same change that closes the residual.

## References

- PRs: surfc #335 (migration 0052), braird-core #14 (residual-doc cleanup), surfc #334 / braird-core #12 (the SUR-739 watermark that had the latent hole)
- Linear ticket: SUR-743 (follow-up of SUR-739 / SUR-652)
- Files most affected: `src/sync/http.rs`, `src/sync/pull.rs`; surfc `supabase/migrations/0052_sur743_commit_ordered_change_seq.sql`
- Related learnings: `2026-07-02-sur742-uniffi-docstring-checksums.md`
