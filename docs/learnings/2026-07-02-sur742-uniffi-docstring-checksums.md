---
date: 2026-07-02
ticket: SUR-742
area: [bindings, ci, governance]
gate: GCE
verdict: PASS WITH CONCERNS
artefacts_updated:
  - scripts/gen-bindings.sh
  - .github/workflows/parity.yml
  - CLAUDE.md
  - docs/learnings/2026-07-02-sur742-uniffi-docstring-checksums.md
---

# UniFFI hashes **docstrings** into per-method checksums — a doc-only edit drifts the committed bindings, and the runtime guard can't see a missing symbol

## What happened

SUR-726 (#11) added six `enqueue_*` methods + an exported `membership_id` and edited `pull()`'s docstring, but didn't regenerate the committed `bindings/{kotlin,swift}`. The `Kotlin/JVM round-trip` CI job failed **cryptically** — UniFFI's runtime checksum guard threw a `RuntimeException` at init (committed-binding checksum ≠ freshly-built lib), which failed two *pre-existing, unrelated* crypto round-trip tests with no hint that stale bindings were the cause. SUR-742 added a `bindings-drift` CI guard (regenerate via a canonical `scripts/gen-bindings.sh`, then `git status --porcelain -- bindings/`).

## What surprised me

- **UniFFI folds docstrings into the per-method checksum.** A *doc-only* change to a `#[uniffi::export]` item drifts the binding (`pull` checksum 4639→18621 in #11), which is completely counter-intuitive — you expect a comment to be inert.
- **The runtime checksum catches signature drift but NOT a missing new symbol.** A stale binding that simply *lacks* a newly-added method reports no mismatch for the symbols it does know about, so the runtime guard is silent on exactly the "I forgot to regenerate after adding an API" case. Only a **regenerate-and-diff** (structural) check catches that.
- **The failure was maximally misleading:** stale bindings broke *unrelated* crypto tests at module init, pointing the investigation everywhere except the actual cause.

## What the gate caught

- **The guard policed its own author's next PR.** In PR-C (SUR-741) a *naming*-reviewer fix edited an `enqueue_note` **docstring** — which (per the surprise above) drifts the bindings. The sync-reviewer caught that committing it without re-running `scripts/gen-bindings.sh` would fail the very `bindings-drift` guard added two PRs earlier. The guard worked as designed on the first real test — including on me.
- **A regression-reviewer caught a cost regression** in the guard's own wiring: widening the shared `parity.yml` `on.paths` to include `bindings/**` dragged the 10×-billed macOS `ios-crosscompile` job onto every binding-only PR. Reverted — the guard rides the `src/**` trigger (bindings are generated *from* `src/**`), so it fires on every real FFI change without the macOS tax.

## What to compound

- **`scripts/gen-bindings.sh` is the single canonical bindgen invocation** (kotlin + swift, `--no-format`). `--no-format` is load-bearing: it makes output host-agnostic (no ktlint/swiftformat version drift → no spurious diffs), so the committed bindings are script-produced by definition and the drift check is deterministic across Linux CI and any dev box.
- **`bindings-drift` uses `git status --porcelain`, not the runtime checksum** — structural diff catches the missing-new-symbol case the checksum can't, and fails fast with an actionable message instead of a cryptic init `RuntimeException`.
- **CLAUDE.md § Workflow** now states: any change to a `#[uniffi::export]` item — *including its docstring* — requires regenerating + committing the bindings.
- **CI path-filter hygiene:** don't widen a *shared* workflow's `on.paths` to add one cheap job — it drags every expensive job in that file onto unrelated PRs. Trigger off the path that already implies the change (here, `src/**`), or split the job into its own workflow.

## References

- PRs: braird-core #13 (the guard), braird-core #11 (the failure it hardens against), braird-core #14 (the guard catching a docstring-drift on its author's next PR)
- Linear ticket: SUR-742 (follow-up of SUR-726)
- Files most affected: `scripts/gen-bindings.sh`, `.github/workflows/parity.yml`, `CLAUDE.md`
- Related learnings: `2026-07-02-sur743-sequence-is-not-a-commit-ordered-watermark.md`
