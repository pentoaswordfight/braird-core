---
date: YYYY-MM-DD
ticket: SUR-XXX        # optional — omit if not driven by a ticket
area: [tag1, tag2]     # e.g. crypto, sync, bindings, ci, governance
gate: GCE              # GCE | CE | CE-with-gate | N/A
verdict: PASS          # PASS | PASS WITH CONCERNS | HOLD | N/A
artefacts_updated:
  - path/to/file.md
  - path/to/other.md
---

# `<one-line title — what this entry is about>`

## What happened

2–3 sentences. The change, the context. Skip if obvious from the title.

## What surprised me

The non-obvious thing — what made you stop and think. If nothing did, this entry probably shouldn't exist.

## What the gate caught

What the persona / pressure-test / sign-off surfaced that wasn't in the original brief. If the gate caught nothing, say so explicitly — "gate found nothing" is also a learning, especially if you expected it to.

## What to compound

The concrete artefact that gets updated as a result. Name the file and the change. The point of writing this entry is committing to making the gate / docs / personas smarter.

## References

- PR / commit: `<link or SHA>`
- Linear ticket: `<SUR-XXX or omit>`
- Files most affected: `path/to/file`, `path/to/other`
- Related learnings: `YYYY-MM-DD-other-entry.md` (optional)
