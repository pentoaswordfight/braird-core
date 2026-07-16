# SUR-921 Optional Plaintext Note Patch Design

## Goal

Allow native hosts to update an existing live note without supplying plaintext, decrypting the
note, or changing its stored ciphertext or content tag. This unblocks the founder-sanctioned
tags-only idea-merge paths in braird-android SUR-873 and braird-ios SUR-860, including notes whose
ciphertext the current device cannot decrypt.

SUR-921 changes the pre-1.0 UniFFI record layout and therefore belongs in a later v0.7.0 release,
but this implementation does not cut, tag, or publish that release.

## Contract

`NoteUpsert.plaintext` changes from `String` to `Option<String>`.

- `Some(plaintext)` is the existing full authoring path, byte-for-byte:
  - encrypt with `Vault::encrypt_note(Some(note_id), plaintext)`;
  - derive `content_tag` from the same plaintext and the supplied `book_id`;
  - include both `text` and `content_tag` in the local/outbox partial;
  - retain the create-time PWA default `source: None -> "manual"`.
- `None` is an existing-live-row patch:
  - never call the Vault;
  - omit `text` and `content_tag` from the local/outbox partial;
  - treat `source: None` as keep/omit, while `Some(source)` explicitly updates it;
  - ignore the record's mandatory `created_at` value and omit `created_at` from the partial, so an
    immutable creation stamp cannot be moved by a retag;
  - preserve the other existing `NoteUpsert` field semantics, including supplied `tags`,
    `deleted`, clear directives, and the fresh `updated_at` stamp.

`text` and `content_tag` remain non-clearable. `clearable_columns("notes")` is unchanged.

## Existing-Row Rule and Atomicity

Patch mode accepts only a row that exists locally and is currently live (`deleted != true`).
Missing and already-tombstoned rows both fail with the dedicated unit variant
`SyncError::PatchTargetMissing`. Its static display text is
`"note patch requires an existing live row"` and never includes the note id or other caller input.
This is an expected per-record race outcome for bulk patch flows: a host may skip that note and
re-query its live work list. Hosts must not string-match a generic store error to detect it.

The existence/live check and staging happen in one SQLite transaction under the existing
`SyncEngine.store` mutex. A new store helper performs:

1. open the transaction;
2. load the current row by primary key;
3. reject if absent or soft-deleted;
4. shallow-merge the partial onto that row;
5. persist the merged local row;
6. enqueue the unchanged partial JSON;
7. commit.

This avoids a check-then-stage race and prevents a stale retag request carrying `deleted: false`
from resurrecting a tombstoned note. A patch may carry `deleted: true` to tombstone a currently
live note. Ordinary create/full-authoring writes continue using `stage_local_write`.

The helper returns a typed internal staging error with separate `TargetMissing` and
`Sql(rusqlite::Error)` cases. `enqueue_note` maps `TargetMissing` to the stable
`SyncError::PatchTargetMissing` FFI variant and maps the SQL case through the existing coarse
`SyncError::Store` path.

## Data Flow

`SyncEngine::enqueue_note` validates `source_meta_json` and `clear_nullable_fields` before staging.
It then builds one partial payload:

- common fields retain their current behavior;
- the `Some` branch inserts sealed `text`, derived `content_tag`, and the create-time source
  default, and carries `created_at`;
- the `None` branch inserts neither sealed column and inserts `source` only when explicitly
  supplied; it also omits `created_at`;
- both branches insert a newly stamped `updated_at`.

The `Some` branch stages through the existing ordinary helper. The `None` branch stages through
the new transactional require-existing-live helper.

`Store::stage_write_inner`, the normal `Store::stage_local_write`, and
`sync::outbox::collapse` remain behaviorally unchanged. Their current shallow-merge semantics are
the mechanism being relied upon:

- locally, omitted fields retain the current row values;
- in the outbox, omitted fields do not appear in the patch JSON;
- during collapse, a later patch lacking `text` cannot replace an earlier sealed `text`, while a
  later full write carrying `text` does replace it.

## Error and Security Properties

- Patch mode performs no encrypt, decrypt, or content-tag derivation.
- No plaintext is synthesized for an undecryptable note.
- Ciphertext and content tag remain byte-identical after a successful patch.
- The immutable creation timestamp remains byte-identical after a successful patch.
- The outbox patch contains no `text`, `content_tag`, or `created_at` key.
- Validation and precondition failures stage no local row and no outbox item.
- Host-controlled note ids, source metadata, and field names are not echoed in FFI error strings.
- The master key and crypto wire constants are unchanged.

## FFI and Consumer Surface

Regenerate committed Swift and Kotlin bindings with `scripts/gen-bindings.sh`. `NoteUpsert` remains
one `uniffi::Record`, so SUR-843's arm64 integer-slot constraint is unaffected; only the record
field layout changes. `SyncError::PatchTargetMissing` becomes a distinct generated Kotlin
`SyncException.PatchTargetMissing` subclass and Swift `SyncError.PatchTargetMissing` case.

Both generated host tests must exercise `plaintext: nil/null` through the real binding. Kotlin/JVM
round-trip tests run locally on Windows. Swift round-trip execution is macOS-only and is evidenced
by the `parity.yml`/macOS CI lane; the generated Swift source and test are still updated locally.
The consumer-smoke test is updated where its `NoteUpsert` construction must compile against the
new optional field. The host tests also assert that a missing patch target surfaces as the typed
variant rather than a generic store error.

## Test Design

Rust tests are written first and must fail for the missing behavior before production changes.

1. A `None` patch on a seeded live note:
   - changes tags;
   - preserves stored `text`, `content_tag`, and a seeded `source: "kindle"` byte-for-byte;
   - supplies a deliberately different `created_at` but preserves the stored creation stamp;
   - advances the stored and queued `updated_at` beyond the seed value;
   - queues a payload with no `text`, `content_tag`, `created_at`, or implicit `source` key.
2. A `Some` write retains the current seal-at-write behavior and source default.
3. A `None` patch on a missing id returns `SyncError::PatchTargetMissing` and leaves both the notes
   table and outbox unchanged.
4. A `None` patch on a tombstoned row returns the same typed variant, leaves the tombstone
   unchanged, and stages nothing.
5. A `None` patch carrying `deleted: true` can tombstone a currently live row without touching its
   sealed columns.
6. A seeded note with foreign/undecryptable ciphertext can be retagged through `None`; its
   ciphertext survives local storage and outbox collapse unchanged.
7. Outbox collapse ordering is pinned explicitly:
   - full `Some` write followed by a `None` patch keeps the earlier sealed `text`;
   - `None` patch followed by a full `Some` write uses the later sealed `text`.
8. Store tests prove the require-existing-live helper performs no write on missing/tombstoned rows
   and rolls back the local merge if outbox enqueue fails.
9. Kotlin and Swift binding round-trips prove host `null`/`nil` reaches patch mode and preserves an
   undecryptable ciphertext while updating tags; both pin the generated typed missing-target error.
10. The env-guarded real-Supabase test creates and flushes a normal note, records the server
    `text`, `content_tag`, and `created_at`, enqueues a `None` tags patch, flushes again, and asserts:
    - the UPDATE succeeds with no `text` key in the outgoing partial;
    - server tags and `updated_at` change;
    - server `text`, `content_tag`, and `created_at` are byte-identical to their pre-patch values.

The INSERT leg remains safe and is pinned by the first collapse-order test: a bare patch cannot
target a missing local row, while a patch queued behind an unflushed local create collapses with
that earlier full write and therefore sends the create's required `text`, `content_tag`, and
`created_at` keys.

## Alternatives Rejected

1. A separate `patch_note` export would duplicate a large note-shaped FFI record and invite drift
   between create/edit and patch semantics.
2. A generalized sealed-field tri-state would add binding complexity without a valid clear state:
   note text remains non-clearable.
3. A preflight `get_row` in `enqueue_note` followed by ordinary staging would leave a
   check-then-stage race and make the existence rule non-atomic.
4. Allowing tombstoned rows would let a stale tags-only patch carrying the normal
   `deleted: false` resurrect a note without using the repository's explicit resurrection path.

## Gate and Delivery

Touched paths route through the sync/store and binding gates. Required reviewers are
`sync-reviewer`, `crypto-reviewer`, and `naming-reviewer`, followed by founder sign-off.
`CHANGELOG.md` receives an `[Unreleased]` entry. Required local evidence includes Rust parity/tests,
Kotlin/JVM round-trip, generated-binding drift, and the FFI argument-slot check. The server
byte-compare runs in the real-Supabase integration lane. The repository's cross-repo
`SURFC_READ_PAT` is currently known to return HTTP 403; gate evidence therefore requires either a
founder-rotated secret before CI or a documented clean local/staging Supabase run while that
infrastructure issue is outstanding. Swift execution, mobile cross-compile, and the complete
binding/macOS gates run in CI. The agent proposes and implements on a branch but does not merge.
