# SUR-741 / 742 / 743 + SUR-659 closeout — implementation plan

**Status:** DRAFT for founder sign-off. Agent-handoff plan.
**Date:** 2026-07-02 · **Author:** Claude (agent under the gate), verified against `origin/main` of braird-core (PRs #8–#12 merged) and surfc (#334 merged; migrations through `0051`).
**Goal:** land the three fast-follows, then close **SUR-659** (Phase 2) against its acceptance criteria with an honest evidence matrix — unblocking SUR-660/661.

---

## 0. Where things stand (verified 2026-07-02)

- braird-core `main`: seal-at-write outbox + flush (#6), incremental pull + tombstones (#8), rebase + `sync()` + conflict signal (#9), array-LWW ratification (#10), 8-store fan-out (#11), `change_seq` keyset cursor + pagination (#12). `enqueue_book`/`enqueue_note` still carry the SUR-724-era partial column sets (SUR-741). The SUR-743 residual is documented at `src/sync/http.rs` (get_page caveat) and the `pull.rs` module docs.
- surfc `main`: `0050_sur740_lww_guard.sql` + `0051_sur739_change_seq.sql` live; `fetchSince` is keyset-paged on `change_seq` (`SYNC_PAGE_SIZE`); `supabase/test/sync.changeSeq.test.js` exists. **Next migration number: 0052.**
- Advisory-lock precedent for PR-A: `0018_apply_ai_usage_month_delta_self_healing.sql` (per-user `pg_advisory_xact_lock`, auto-release at COMMIT).
- Wiki target for PR-A's parallel merge: `surfc-intranet/src/content/docs/wiki/data/triggers.md` (the `t02_change_seq` entry).
- CI for PR-B: `.github/workflows/parity.yml` has jobs `parity` + `kotlin-roundtrip`; the bindgen command lives only as a doc-comment in `bindings/kotlin/build.gradle.kts`; `scripts/` has no `gen-bindings.sh` yet.

Non-negotiables carry over from `sur-736-740-sync-convergence-plan.md` §1 verbatim: no stacked PRs (sequence via merged `main`, STOP-and-flag on any rebase need), agent-under-gate, per-PR `CHANGELOG.md [Unreleased]` (core, CI-enforced), `_TEMPLATE.sql` + explicit GRANTs + local `npm run test:db` (surfc, db-test flake → rerun and report), data-model change ⇒ parallel intranet wiki PR with the same branch name, E2EE invariants (ciphertext-only at rest, no payload logging).

---

## 1. PR-A — surfc `0052`: commit-ordered `change_seq` (SUR-743) + parallel wiki PR

**Start immediately — this is the live prod exposure (merged PWA leg).** Independent of PR-B/C.

### Migration `0052_sur743_commit_ordered_change_seq.sql`

1. **Counter table** `user_change_seq(user_id uuid PRIMARY KEY, seq bigint NOT NULL)`. Template conventions: REVOKE ALL from PUBLIC/anon/authenticated; GRANT ALL to service_role; RLS enabled with a comment that no client policy is intentional (written only via the SECURITY DEFINER trigger function).
2. **Seed/backfill:** one-time `INSERT ... SELECT user_id, max(change_seq)` — the GREATEST across all 8 synced tables per user — so new values continue strictly above 0051's backfill.
3. **Replace the `t02_change_seq` function body** (keep the trigger name — alphabetical firing after `t01_lww_guard` is load-bearing):
   ```sql
   PERFORM pg_advisory_xact_lock(<dedicated-namespace>, hashtextextended(NEW.user_id::text, 0));
   INSERT INTO user_change_seq (user_id, seq) VALUES (NEW.user_id, 1)
     ON CONFLICT (user_id) DO UPDATE SET seq = user_change_seq.seq + 1
     RETURNING seq INTO NEW.change_seq;
   ```
   `SECURITY DEFINER`, owner postgres, REVOKE EXECUTE from client roles (0018 is the in-repo pattern). The xact lock is held to COMMIT, so per user, allocation order == commit order — the exclusive keyset becomes skip-safe by construction. Two-arg (namespaced) advisory lock; a hash collision across users merely over-serializes, never mis-orders — note it in the migration header.
4. **Retire 0051's per-table sequences** (DROP after the function swap) so there's one assignment mechanism, not two.
5. **Rollback recipe** in the header: restore the 0051 function body; values keep increasing under either mechanism, so clients never need a cursor reset on rollback.

Semantics preserved, verify each in tests: per-table client cursors (`lastSeq:<t>` / `sync:seq:<t>`) keep working — a table's values are a strictly-increasing subsequence of the per-user counter; a `t01_lww_guard` rejection (`RETURN NULL`) must skip `t02` entirely (no lock taken, no seq consumed); **no PWA or core client code changes** — this is the whole point of the trigger-only fix.

### Tests (`supabase/test/` — extend `sync.changeSeq.test.js`, add `sync.commitOrder.test.js`)

- **The commit-order pin (two connections):** conn1 `BEGIN` + upsert for user U (allocates, holds open); conn2 upserts for U → assert it **blocks** until conn1 commits (timeout + `pg_locks` probe), then allocates higher. A reader can therefore never observe a higher seq while a lower one is invisible.
- Cross-user isolation: different users' writers don't block each other.
- Guard interplay: a stale (older `updated_at`) upsert is rejected AND leaves the counter untouched.
- Per-user monotone across tables; per-table subsequence strictly increasing; `migrations.replay`, RLS, and soft-delete suites stay green.

### Parallel intranet PR (same branch name, parallel-merge flagged)

`wiki/data/triggers.md`: rewrite the `t02_change_seq` entry (commit-ordered via per-user advisory-lock counter; why allocation-order was insufficient). RAID: close the SUR-743 exposure line.

**Gate:** migration-reviewer + sync-reviewer, founder. After merge, the residual caveat doc-comments in braird-core (`http.rs`, `pull.rs`, CHANGELOG note) are stale — **removed in PR-C** (they sit near exported items, and doc changes drift bindings, so fold them into the PR that regenerates bindings anyway).

---

## 2. PR-B — braird-core: bindings-drift guard (SUR-742)

**Start in parallel with PR-A.** Lands **before PR-C** deliberately: SUR-741 is exactly the change class this guard exists for, so PR-C's regenerated bindings get CI-verified by it.

1. **`scripts/gen-bindings.sh`** — the single canonical bindgen invocation (kotlin + swift, library mode): build the cdylib, run `uniffi-bindgen` for both languages into `bindings/{kotlin,swift}` paths. **Formatting must be deterministic:** generate with formatting disabled in this script AND regenerate the committed bindings once through it in this same PR, so the committed state is script-produced by definition (kills the ktlint/swiftformat spurious-diff failure mode the ticket flags). `build-xcframework.sh` and the `build.gradle.kts` doc-comment repoint to the script (DRY).
2. **CI job `bindings-drift`** (extend `parity.yml`, gated on `src/**` changes): build (debug profile — faster, same contract metadata), run `scripts/gen-bindings.sh` into a temp dir, `git diff --no-index --exit-code` against committed `bindings/`, failing with the message *"FFI surface changed — run scripts/gen-bindings.sh and commit the bindings"*. Linux runner (library-mode generation is host-agnostic).
3. **CLAUDE.md → Workflow**, one line: any change to a `#[uniffi::export]` item — **including its docstring** — requires regenerating + committing bindings via `scripts/gen-bindings.sh`.
4. **Prove the guard catches the PR #11 failure mode:** in the PR description, show a red run from a docstring-only edit to an exported method (checksums drift), then green after regen. This also demonstrates the missing-new-symbol case a runtime checksum can't catch — the diff catches it structurally.

**Gate:** founder-only paths (`.github/workflows/**` + `CLAUDE.md`, GATING §3.2); ticket is tagged `gate_debt`. CHANGELOG entry.

---

## 3. PR-C — braird-core: widen `enqueue_book` / `enqueue_note` (SUR-741)

**After PR-B merges** (guard active), ideally after PR-A (to fold the residual-doc cleanup). Branch off updated `main` — never off PR-B's branch.

### Signatures (recommendation: widen in place — no shipped native hosts exist, a clean break is cheapest; the alternative additive-v2-methods path doubles the surface for no consumer)

```rust
pub fn enqueue_book(
    id, title, author: Option<String>,
    isbn: Option<String>, cover_url: Option<String>,
    cover_source: Option<String>, cover_resolved_at: Option<i64>,
    created_at: i64, deleted: bool)

pub fn enqueue_note(
    id, book_id: Option<String>, plaintext: String, page: Option<String>,
    tags: Vec<String>, source: Option<String>,        // None ⇒ "manual" (today's hardcode)
    source_id: Option<String>, source_meta_json: Option<String>,  // JSON object string, validated
    chapter: Option<String>, image_path: Option<String>, ink_crop_path: Option<String>,
    created_at: i64, deleted: bool)
```

Rules that carry the correctness:

- **`None` ⇒ column omitted from the outbox payload.** This is what keeps `merge-duplicates` patch-semantics and the local `stage_write` merge safe — the existing `enqueue_book_edit_preserves_pulled_only_columns` test must keep passing untouched. Do NOT emit explicit nulls for absent optionals: a title-only edit sending `cover_url: null` would clear the server's cover.
- **Consequence to document (founder decision #2):** native can't yet *clear* a field (the PWA clears a cover by sending an explicit null). Tri-state (absent | null | value) over UniFFI is awkward; defer field-clearing to a 660/661-scoped follow-up ticket, noted in the method docs.
- **Mirror the PWA wire shapes** — before coding, read `upsertBook`/`upsertNote` in `surfc/src/supabase.js` and match key names exactly; they are also the schema-extractor's column source, so no new payload key may fall outside `synced_schema()`.
- `source_meta_json` is parse-validated (`serde_json` object) at enqueue; invalid JSON → `SyncError::Store`, nothing staged.
- **Seal-at-write unchanged**: `text` sealed (enc:v2, AAD = note id) and `content_tag` computed from plaintext before anything persists — the crypto-reviewer line. New fields never touch the Vault.

### Tests

Unit: create-book-with-cover full payload; create-note with `image_path`/`source_meta`/`chapter`/`source: "readwise"`; `source: None` defaults `"manual"`; None-omits-column (edit preserves cover/image); invalid `source_meta_json` rejected atomically; ciphertext + content_tag invariants re-asserted on the widened path. Integration: extend the 8-store coexistence test with a native-authored book-with-cover + note-with-source-metadata round-tripping to the PWA fixture shape. Bindings: regenerate via `scripts/gen-bindings.sh` (PR-B guard verifies); extend the Kotlin + Swift round-trip tests to call one widened method.

Also in this PR: delete the SUR-743 residual caveats (`http.rs` get_page caveat, `pull.rs` module-doc residual block, CHANGELOG note) once PR-A is merged — they describe a closed hole.

**Gate:** sync-reviewer + crypto-reviewer + naming-reviewer, `touches-ffi` label (nightly macOS Swift leg), founder. CHANGELOG.

---

## 4. SUR-659 closeout — AC evidence matrix

Run after PR-A/B/C are merged. Closure is **gated on PR-A** (AC 4's completeness claim isn't honest while SUR-743 is open) and on PR-C (native authoring completeness for the stores the AC's coexistence promise feeds). Three ACs close **as-ratified** rather than as-written — the closing comment must say so explicitly; all three divergences are already founder-decided, this just records them against the parent.

| # | AC (as written) | Verdict | Evidence | Divergence note for the closing comment |
|---|---|---|---|---|
| 1 | Full v19 mirror incl. `notes.contentTag`; `deleted`+`updatedAt` everywhere; local-only stores not synced | **Met** | `synced_schema()` (8 tables, `content_tag`), `LOCAL_ONLY_DDL` (meta/outbox/embeddings/discovery_jobs); tests `opens_and_creates_every_table`, `every_synced_table_has_updated_at_and_deleted`; `tests/schema_parity.rs` vs vendored fixture | — (embeddings/discovery_jobs are mirrored tables; their consumers are Phase-3 work, not this AC) |
| 2 | Collapse = per-field LWW, sticky `deleted:1`, `bookIdRemap` | **Met** | `outbox.rs::collapse` + tests (incl. `delete_stays_sticky_when_later_edit_carries_deleted_false` — deliberately *harder* than the JS oracle's per-item check) | Hardening over oracle is documented in-code as founder-decided |
| 3 | Push seals `enc:v2` AAD=noteId **at flush**; only ciphertext leaves; failures stay queued | **Met as-ratified** | `enqueue_note_stores_ciphertext_not_plaintext`, integration seal tests, `note_held_back_when_parent_book_flush_fails` | Seals at **enqueue** (seal-at-write, SUR-724 founder decision) — strictly stronger: no plaintext ever persists. Stale-tag-after-offline-merge edge documented at `enqueue_note` |
| 4 | Incremental pull by **`updatedAt` cursor** per table; tombstones applied, not resurrected | **Met as-ratified** | Pull test suite (LWW strict-`>`, tie-keeps-local, 3 tombstone cases), keyset pagination tests, PR-A commit-order pin | LWW **merge** is by `updated_at` as written; the **cursor** is the server `change_seq` watermark (SUR-739/652/743 chain) — the as-written `updatedAt` cursor had the permanent-miss hole; the ratified design closes it |
| 5 | Sync methods hang off the existing `Vault` handle; token handoff; offline-first ordering | **Met** | `SyncEngine::open(.., Arc<Vault>)` (ADR 0001 Option B: composes the same unlocked handle, crypto surface unchanged); `set_access_token` + JWT-`sub` stamping; `stage_local_write` single-transaction SQLite-before-cloud + rebase atomicity | Reading of "hang off": the engine *holds* the Vault rather than methods living on `Vault` itself — additive, no crypto-surface break, per ADR. Ratify wording |
| 6 | Coexistence, all 8 stores, both directions, incl. `contentTag` + tombstones | **Met** | PR #11's 8-store matrix + #12 re-proof on the watermark cursor; export/import parity leg; PR-C adds native-**authored** full-column round-trips | Until PR-C, native could round-trip-preserve but not author cover/image/source fields — PR-C closes that capability gap before 660/661 consume the FFI |
| 7 | Schema-drift guard fails CI on `db.js` ↔ mirror divergence | **Met** | `schema-drift.yml` + `vendored/schema/sync-schema.json` + `extract-sync-schema.mjs` (three-way authority; payload-keys column source) | PR-B adds the sibling guard for the *FFI* surface (`bindings-drift`) — same regenerate-and-diff pattern, closing the adjacent hole PR #11 hit |

### Closure mechanics

1. Tick the seven checkboxes on SUR-659; post the matrix above as the closing comment with the three as-ratified notes (seal point, cursor column, Vault composition).
2. Verify Linear state: SUR-736/737/738/739/740/652/725/726 all Done; 741/742/743 Done via PR-A/B/C; SUR-659 → Done, which releases its `blocks` edges to SUR-660/661.
3. `docs/learnings/` entry in braird-core (the SUR-742 stale-bindings lesson + the allocation-vs-commit-order lesson are both non-obvious keepers).
4. Confirm the two accepted-residuals registers still say what's true after PR-A: remaining residuals are the in-flight-seconds write race (bounded by SUR-740's guard + rebase), exact-ms tie split-brain, and backward-clock rebase — all §8-class, none data-destroying.

---

## 5. Sequencing

```
Day 0:  PR-A (surfc 0052 + intranet wiki, parallel-merge)   ─┐  independent repos,
        PR-B (core: gen-bindings.sh + bindings-drift CI)     ─┘  run in parallel
Then:   PR-C (core: widen enqueue_book/enqueue_note; folds residual-doc cleanup) — after B merges (guard active), after A merges (docs truthful)
Then:   SUR-659 closeout audit + closing comment + Done → unblocks SUR-660/661
```

## 6. Open decisions for the founder (each PR body names its own)

1. **PR-C:** widen-in-place breaking signatures (recommended — zero shipped hosts) vs additive `enqueue_*_v2`.
2. **PR-C:** `None` = omit-column, field-clearing deferred to a 660/661 follow-up (recommended) vs tri-state now.
3. **PR-C:** `source_meta_json: Option<String>` validated at enqueue (recommended) vs a typed record.
4. **PR-A:** drop 0051's per-table sequences in 0052 (recommended) vs leave orphaned.
5. **Closeout:** accept the three as-ratified AC divergences recorded in the closing comment (recommended — all pre-decided at SUR-724/739 gates; this makes the parent's record match).
