# Changelog

All notable changes to braird-core are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Version numbers follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added

- **Sync outbox queue and collapse engine** (`src/sync/outbox.rs`).  
  An append-only outbox keyed by `(table, recordId)`. `collapse_outbox_items()` applies
  last-write-wins per field and a sticky `deleted:1` flag (a delete is never overwritten by a
  subsequent field update). `book_id_remap` carry-through repoints child notes from temporary
  local IDs to server-assigned IDs before flush. Mirrors the behaviour of `collapseOutboxItems`
  in the reference JS implementation. (SUR-724)

- **PostgREST HTTP client with host→core token handoff** (`src/sync/http.rs`).  
  Authenticated HTTPS calls to a PostgREST endpoint via reqwest (rustls, trimmed feature set)
  driven by a `tokio current_thread` runtime (see ADR 0001). The host supplies a JWT by calling
  `set_access_token(jwt: String)`; braird-core does not perform its own authentication.
  `upsert_book()` and `upsert_note()` are the two write operations exposed. (SUR-724)

- **`flush_outbox()` push logic** (`src/sync/push.rs`, `src/vault.rs`).  
  Collapse the outbox → apply `bookIdRemap` → call `Vault.seal(enc:v2, AAD=noteId)` on note
  text (only ciphertext is transmitted; plaintext never leaves the device) → dispatch upserts.
  A failed PostgREST response (4xx/5xx/network error) leaves the outbox item in place; nothing
  is silently dropped. (SUR-724)

- **Integration test harness against local Supabase CLI backend**
  (`tests/sync_659b_integration.rs`).  
  Spins up the local Supabase stack (Postgres + GoTrue + PostgREST + migrations + RLS) reusing
  the JS `test:db` instance. `mint_test_user_jwt()` authenticates a test user through GoTrue
  and returns a real JWT for use in tests. Coverage includes: JWT→PostgREST write succeeds,
  outbox collapse (LWW, sticky delete, bookIdRemap), seal-at-flush (only `enc:v2` ciphertext
  reaches the DB), and failed-write retention (simulated 5xx → outbox item persists). Fast unit
  tests cover the token-absent → 401 and network-failure → queue-stays paths without a running
  DB. This harness is designed to be reused by SUR-725 and SUR-726. (SUR-724)
