# ADR 0003 — Sync engine: seal-at-write + books-first flush

- **Status:** Accepted (founder-decided at the SUR-724 / SUR-659b Phase-2 gates)
- **Date:** 2026-07-01
- **Context tickets:** SUR-724 (659b impl), parent SUR-659, implements ADR 0001 (Rust+UniFFI)
- **Supersedes / superseded by:** none. Extends ADR 0002 (the `rusqlite`/native-only addendum).

## Context

Phase 2 adds the on-device write path: an **outbox** the host enqueues writes into, and a
**flush** that pushes them to Supabase. The PWA already does this in `src/supabase.js`
(`collapseOutboxItems` + `flushOutbox`), and that JS is the source of truth — the native core
must mirror its semantics so PWA↔native coexistence round-trips. Two edges force real decisions:
**where note text gets sealed**, and **how offline book-merges are resolved at flush**.

## Decisions

### 1. Seal at write (ciphertext in the outbox)

The outbox stores **ciphertext** for `notes.text`. `enqueue_note` takes plaintext, and while the
plaintext is in hand it (a) seals it via `Vault::encrypt_note(Some(note_id), plaintext)` →
`enc:v2` (AAD = note id), and (b) computes `content_tag` via `Vault::content_tag(plaintext,
book_id)` from the **plaintext** (`contentTag.js` mandates plaintext, never ciphertext). The
stored payload holds only the ciphertext + the tag.

**Why:** no plaintext note text is ever persisted, even transiently in a local queue. At flush,
`text` is already ciphertext → sent as-is, behind an `isEncrypted` guard that skips
re-encryption (mirroring the JS double-encrypt guard). This is the E2EE invariant made
structural: the plaintext exists only for the duration of the `enqueue_note` call.

**Accepted edge — stale `content_tag` after an offline book-merge (do NOT "fix"):** the tag bakes
in `book_id`, but the flush repoints `book_id` via `bookIdRemap` after a merge. So a merged note's
tag reflects the **pre-merge** `book_id`. The JS never recomputes the tag at flush (`flushOutbox`
doesn't touch it), and under seal-at-write we **can't** recompute it anyway (no plaintext left).
We leave the tag as-is: the rare stale-tag self-heals on the note's next edit (which re-enqueues
with a freshly-computed tag). The tag is never NULL (it's computed pre-seal). Documented at the
`enqueue_note` call site so crypto-reviewer / sync-reviewer see the staleness is intentional.

### 2. `updated_at` in epoch milliseconds, stamped at enqueue

The client sends `updated_at` as epoch **ms** (matching the PWA `Date.now()` and the existing
cloud data), stamped at enqueue via `SystemTime`. Never omitted — the migration default is `0`
and there is no server-side trigger, so an omitted value would sort as the oldest write and lose
every LWW race.

### 3. `bookIdRemap` persisted in `meta` (not in-memory)

An offline book gets a temp id; when it flushes to its server id, temp→server is persisted in the
`meta` KV table (key `bookIdRemap`, a JSON map). `resolve_book_id` walks the map transitively
(chained merges A→B→C resolve straight to C), hop-capped at 20 and cycle-safe. Persisting it means
a crash between the book flush and a later note flush doesn't strand child notes on a dead temp id.

### 4. Books-first flush ordering

`collapse → upsert BOOKS first (record each remap in meta on success) → upsert NOTES (book_id
repointed via the remap)`. A note whose parent book flush **failed** stays queued — it is NOT
dispatched with a temp/absent `book_id` (that would be a server FK violation). Failed writes stay
in the outbox; only succeeded outbox ids are cleared.

### 5. Sync FFI, async inside

`SyncEngine` owns a **tokio current-thread runtime** and `block_on`s the async reqwest PostgREST
calls inside its **synchronous** UniFFI methods — the FFI surface stays sync, exactly like
`Vault`. PostgREST upserts POST to `{SUPABASE_URL}/rest/v1/{table}?on_conflict={pk}` with
`apikey` / `Authorization: Bearer <jwt>` / `Prefer: resolution=merge-duplicates`, body = a JSON
array. `user_id` is the JWT `sub`, injected per row at flush, never stored (mirrors Dexie).

## Consequences

- New native-only deps (`reqwest` rustls, `tokio` rt/net/time, `serde`/`serde_json`) live under
  the existing `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]` block; the whole `sync`
  module is `#[cfg(not(target_arch = "wasm32"))]`. The WASM CSPRNG build stays green (verified).
- The flush is proven end-to-end on `books` + `notes` (the tables with the parent/child +
  encryption edges) against a **real local Supabase** stack in a dedicated CI job
  (`sync-integration.yml`) — the six remaining synced tables follow in SUR-659c/d behind the same
  flush, no new orchestration.
- rustls (no OpenSSL/C) keeps the same reproducible-build / narrow-supply-chain principle ADR 0002
  chose for the crypto backend.
