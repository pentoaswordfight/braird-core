# SUR-921 Tags-Only Note Patch Implementation Plan

> **For Codex:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task.

**Goal:** Make `NoteUpsert.plaintext` optional so an existing live note can be retagged without any Vault operation or change to its sealed text, content tag, source default, or creation timestamp.

**Architecture:** Keep one `NoteUpsert` FFI record and branch inside `SyncEngine::enqueue_note`. Full writes continue through the existing stage path. Plaintext-absent patches build a narrow partial and use a new store transaction that atomically verifies the target is live, shallow-merges locally, and queues the same narrow payload. The existing clear allowlist and collapse implementation remain unchanged; focused tests pin the behavior they already provide.

**Tech Stack:** Rust, rusqlite, serde_json, UniFFI, Kotlin/JUnit, Swift/XCTest, PostgREST/Supabase integration tests.

---

## Task 1: Add the atomic existing-live store primitive

**Files:**

- Modify: `src/store.rs` near `Store::stage_local_write`
- Test: `src/store.rs` test module near `stage_local_write_rolls_back_when_the_outbox_insert_fails`

**Step 1: Write the failing store tests**

Add tests covering:

```rust
#[test]
fn stage_existing_live_write_rejects_missing_and_tombstoned_rows() {
    // Missing target returns TargetMissing and leaves notes/outbox empty.
    // Seed deleted:true target, retry, and assert the tombstone is byte-identical
    // with no queued item.
}

#[test]
fn stage_existing_live_write_rolls_back_when_outbox_insert_fails() {
    // Seed a live note with tags ["before"], drop outbox, stage tags ["after"],
    // assert the SQL error and that the stored tags remain ["before"].
}
```

Reference a not-yet-defined typed error and helper:

```rust
StageExistingWriteError::TargetMissing
store.stage_local_write_existing_live("notes", "n1", partial, 123)
```

**Step 2: Run the focused tests and confirm RED**

Run:

```powershell
cargo test store::tests::stage_existing_live_write --lib
```

Expected: compilation fails because `StageExistingWriteError` and
`stage_local_write_existing_live` do not exist.

**Step 3: Implement the minimal typed transaction**

In `src/store.rs`, add:

```rust
#[derive(Debug, thiserror::Error)]
pub(crate) enum StageExistingWriteError {
    #[error("patch target missing")]
    TargetMissing,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}
```

Add `Store::stage_local_write_existing_live`:

```rust
pub(crate) fn stage_local_write_existing_live(
    &self,
    table: &str,
    record_id: &str,
    partial: Map<String, Value>,
    created_at: i64,
) -> Result<(), StageExistingWriteError> {
    let tx = self.conn.unchecked_transaction()?;
    let existing = self
        .get_row(table, record_id)?
        .ok_or(StageExistingWriteError::TargetMissing)?;
    if existing
        .get("deleted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(StageExistingWriteError::TargetMissing);
    }
    self.stage_write_inner(table, record_id, partial, created_at)?;
    tx.commit()?;
    Ok(())
}
```

Keep `stage_write_inner`, `stage_local_write`, clearable columns, and outbox behavior unchanged.

**Step 4: Run the focused tests and confirm GREEN**

Run:

```powershell
cargo test store::tests::stage_existing_live_write --lib
```

Expected: both tests pass.

**Step 5: Commit**

```powershell
git add src/store.rs
git commit -m "feat(sync): add atomic live-note patch staging"
```

## Task 2: Define the optional-plaintext FFI contract and typed host error

**Files:**

- Modify: `src/sync/mod.rs` (`SyncError`, `NoteUpsert`, `enqueue_note`, stage helpers, test helpers)
- Modify: `tests/sync_659b_integration.rs`
- Modify: `tests/sync_725_integration.rs`
- Modify: `tests/sync_726_integration.rs`
- Test: `src/sync/mod.rs` test module

**Step 1: Write failing sync tests**

Add a `note_patch` helper whose `plaintext` is `None`, plus focused tests:

```rust
#[test]
fn note_patch_preserves_sealed_and_immutable_fields_and_queues_a_narrow_payload()

#[test]
fn note_patch_rejects_missing_and_tombstoned_targets_with_typed_error()

#[test]
fn note_patch_can_tombstone_a_live_undecryptable_note_without_resealing()
```

The first test seeds a live row with:

- foreign ciphertext;
- a fixed content tag;
- `source: "kindle"`;
- a fixed `created_at`;
- old `updated_at`;
- `tags: ["before"]`.

It then passes a deliberately different `created_at`, `source: None`, and new tags. Assert:

- `get_note` is decrypt-failed before and after;
- local `text`, `content_tag`, `source`, and `created_at` are byte-identical;
- local tags change and `updated_at` advances;
- the sole outbox JSON has no `text`, `content_tag`, `source`, or `created_at`;
- queued tags change and queued `updated_at` advances.

The second test asserts exact variant matching:

```rust
assert!(matches!(error, SyncError::PatchTargetMissing));
assert_eq!(error.to_string(), "note patch requires an existing live row");
```

It verifies no new row and no outbox item for both a missing id and a seeded tombstone.

The third test passes `deleted: true` for a currently live undecryptable row and asserts the
tombstone is written while ciphertext/content-tag bytes survive.

**Step 2: Run the focused tests and confirm RED**

Run:

```powershell
cargo test sync::tests::note_patch --lib
```

Expected: compilation fails because `NoteUpsert.plaintext` is still `String` and
`SyncError::PatchTargetMissing` does not exist.

**Step 3: Widen the record and repair full-write call sites**

Change:

```rust
pub plaintext: Option<String>,
```

Update every existing Rust construction that represents a normal create/edit to use
`plaintext: Some(...)`, including:

- the `note_upsert` test helper in `src/sync/mod.rs`;
- `tests/sync_659b_integration.rs`;
- `tests/sync_725_integration.rs`;
- `tests/sync_726_integration.rs`.

Do not alter the plaintext values or any other full-write field.

**Step 4: Add the stable FFI error**

Add the unit variant:

```rust
#[error("note patch requires an existing live row")]
PatchTargetMissing,
```

Update `SyncError` documentation to identify it as an expected per-note race outcome for bulk
patch flows, distinct from corruption/store failures and safe for hosts to skip then re-query.
Never include the target id or caller content in the message.

**Step 5: Branch inside `enqueue_note`**

Preserve validation ordering: parse `source_meta_json` and validate clear directives before any
write. Build common optional fields once, then branch:

```rust
match plaintext {
    Some(plaintext) => {
        // Existing seal/tag behavior.
        // source None -> "manual".
        // include text, content_tag, created_at.
        self.stage_write("notes", &id, row)
    }
    None => {
        // No Vault call.
        // source None omitted; Some inserted.
        // text/content_tag/created_at omitted.
        self.stage_existing_live_note_patch(&id, row)
    }
}
```

Both branches insert a fresh `updated_at`; tags, deleted, supplied optionals, source metadata, and
clear directives retain existing semantics. Ensure `apply_clears` runs before either staging call.

Add a private `SyncEngine` helper that holds the existing store mutex while calling
`stage_local_write_existing_live`, mapping:

```rust
StageExistingWriteError::TargetMissing => SyncError::PatchTargetMissing
StageExistingWriteError::Sql(error) => SyncError::Store(error.to_string())
```

Update the public docs:

- `Some` is the full seal-at-write path.
- `None` is an existing-live patch with no Vault operation.
- patch-mode `source: None` means keep.
- patch-mode `created_at` is ignored/immutable.
- the next **plaintext-bearing** edit, not every edit, repairs a stale content tag.

**Step 6: Run focused and full Rust tests**

Run:

```powershell
cargo test sync::tests::note_patch --lib
cargo test --lib
cargo test --tests
```

Expected: focused tests and all non-ignored Rust tests pass; existing full-write seal-at-write
tests remain green.

**Step 7: Commit**

```powershell
git add src/sync/mod.rs tests/sync_659b_integration.rs tests/sync_725_integration.rs tests/sync_726_integration.rs
git commit -m "feat(sync): support plaintext-free note patches"
```

## Task 3: Pin outbox collapse ordering

**Files:**

- Test: `src/sync/outbox.rs`

**Step 1: Add the two focused regression tests**

Add:

```rust
#[test]
fn later_tags_only_patch_keeps_earlier_note_sealed_columns()

#[test]
fn later_full_note_write_replaces_earlier_patch_sealed_columns()
```

The first queues a full note payload containing `text`, `content_tag`, and `created_at`, followed by
a narrow patch containing new tags/`updated_at`. Assert the collapsed payload retains all required
create columns from the first item and the later patch fields.

The second queues the narrow patch first and the full note write second. Assert the later
`text`, `content_tag`, `created_at`, tags, and `updated_at` win.

**Step 2: Run the focused tests**

Run:

```powershell
cargo test sync::outbox::tests::later_ --lib
```

Expected: both pass without production changes, proving the existing shallow merge is sufficient
and the unflushed-create INSERT leg retains required sealed columns.

**Step 3: Commit**

```powershell
git add src/sync/outbox.rs
git commit -m "test(sync): pin note patch collapse ordering"
```

## Task 4: Add the real-Supabase server-row byte comparison

**Files:**

- Modify: `tests/sync_659b_integration.rs`

**Step 1: Extend the env-guarded integration test**

After the existing full note create and successful flush:

1. select and retain the server `text`, `content_tag`, `created_at`, and `updated_at`;
2. enqueue a `NoteUpsert` for the same id with `plaintext: None`, changed tags, `source: None`, and a
   deliberately different `created_at`;
3. inspect the local outbox payload and assert `text`, `content_tag`, and `created_at` are absent;
4. flush again;
5. select the server row again and assert:
   - tags changed;
   - `updated_at` is newer;
   - `text`, `content_tag`, and `created_at` are byte-identical.

Use a bounded timestamp strategy already accepted in this test suite (for example a short
millisecond delay before enqueue) so the `updated_at` comparison is deterministic.

**Step 2: Run the ignored test when infrastructure is available**

Run:

```powershell
cargo test --test sync_659b_integration -- --ignored --nocapture
```

Expected with configured local/staging Supabase: pass. If the environment is absent or the known
`SURFC_READ_PAT` issue prevents setup, record that as missing external gate evidence rather than
weakening or deleting the assertion.

**Step 3: Commit**

```powershell
git add tests/sync_659b_integration.rs
git commit -m "test(sync): compare server ciphertext after note patch"
```

## Task 5: Regenerate bindings and add native round-trip coverage

**Files:**

- Regenerate: `bindings/kotlin/src/main/kotlin/uniffi/braird_core/braird_core.kt`
- Regenerate: `bindings/swift/Sources/BrairdCore/BrairdCore.swift`
- Modify: `bindings/kotlin/src/test/kotlin/RoundTripTest.kt`
- Modify: `bindings/swift/Tests/BrairdCoreTests/RoundTripTests.swift`
- Modify only if compilation requires it: binding consumer-smoke fixtures

**Step 1: Regenerate the FFI bindings**

Run:

```bash
bash scripts/gen-bindings.sh
```

Confirm the generated surface has nullable/optional plaintext and a distinct generated
`PatchTargetMissing` error type/case. Do not hand-edit generated files.

**Step 2: Add the Kotlin/JVM round-trip**

Add a test that:

1. opens a database with Vault A and creates a normal note;
2. reopens the same database with unrelated Vault B and confirms `decryptFailed`;
3. calls `enqueueNote` with `plaintext = null` and new tags;
4. confirms Vault B still reports decrypt failure but the tags changed;
5. reopens with Vault A and confirms the original plaintext still decrypts and tags remain changed;
6. patches a missing id and asserts `SyncException.PatchTargetMissing`, not a generic store error.

Use a deliberately different patch `createdAt` to exercise the immutable behavior through FFI.

**Step 3: Add the equivalent Swift round-trip**

Mirror the same Vault A/Vault B/shared-database flow with `plaintext: nil`. Pattern-match the
generated `SyncError.PatchTargetMissing` case for a missing target.

**Step 4: Run binding checks available on Windows**

Run:

```powershell
.\gradlew.bat test --no-daemon
node ..\..\scripts\check-ffi-arg-slots.mjs --self-check
node ..\..\scripts\check-ffi-arg-slots.mjs
```

Use `bindings/kotlin` as the Gradle working directory and the repository root for the Node checks
as appropriate. Expected: Kotlin tests pass and the FFI slot guard passes. Swift execution is
deferred to the macOS `parity.yml`/nightly lane, but its generated source and XCTest must compile
there.

**Step 5: Re-run binding generation as a drift check**

Run `bash scripts/gen-bindings.sh` a second time and confirm it introduces no additional diff.

**Step 6: Commit**

```powershell
git add bindings
git commit -m "test(ffi): cover tags-only note patches"
```

## Task 6: Document the breaking unreleased capability

**Files:**

- Modify: `CHANGELOG.md`

**Step 1: Add the `[Unreleased]` entry**

Add an `### Added` section describing:

- SUR-921 optional `NoteUpsert.plaintext`;
- `Some` full seal-at-write behavior;
- `None` existing-live patch preserving sealed text/content tag/source/created timestamp;
- typed `PatchTargetMissing`;
- regenerated Kotlin/Swift bindings;
- pre-1.0 FFI break intended for a separately cut v0.7.0.

Do not change crate/package versions and do not cut or tag v0.7.0.

**Step 2: Run the changelog check if available locally**

Run the repository's changelog validation command from `.github/workflows/changelog-check.yml`, or
at minimum:

```powershell
git diff --check
```

**Step 3: Commit**

```powershell
git add CHANGELOG.md
git commit -m "docs: record SUR-921 note patch API"
```

## Task 7: Run the complete local gate

**Files:**

- Verify all modified files

**Step 1: Format and lint**

Run:

```powershell
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: clean.

**Step 2: Run the full Rust suite and WASM build**

Run:

```powershell
cargo test --all-features
cargo build --target wasm32-unknown-unknown
```

Expected: all non-environment-gated tests pass and WASM compiles.

**Step 3: Re-run Kotlin and static FFI gates**

Run:

```powershell
.\gradlew.bat test --no-daemon
node ..\..\scripts\check-ffi-arg-slots.mjs --self-check
node ..\..\scripts\check-ffi-arg-slots.mjs
```

Expected: green.

**Step 4: Check repository hygiene**

Run:

```powershell
git diff --check
git status --short
git log --oneline --decorate -8
```

Confirm only SUR-921 files are changed/committed, no credential is present, and no merge/rebase or
release action occurred.

**Step 5: Apply the verification and branch-finishing skills**

Use `superpowers:verification-before-completion` to match every completion claim to fresh command
output. Then use `superpowers:finishing-a-development-branch` and leave the branch ready for the
required `sync-reviewer`, `crypto-reviewer`, `naming-reviewer`, CI/macOS/Supabase evidence, and
founder sign-off. Do not merge.
