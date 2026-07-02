# ADR 0005 — Read/query API + in-memory lexical search

- **Status:** Proposed (SUR-744; agent under the GCE gate — awaits `crypto-reviewer` + `naming-reviewer` + founder sign-off, per `GATING.md`).
- **Date:** 2026-07-02
- **Context tickets:** SUR-744 (Phase 2b read/query API), hard-blocks SUR-660 M6 / SUR-754 (iOS read surfaces). Extends ADR 0003 (seal-at-write / ciphertext-at-rest) — this is its symmetric read side.

## Context

Phase 2 gave the core writes (`enqueue_*`) and sync (`flush`/`pull`) but **no read surface** — hosts could not list or search books/notes/ideas. The architecture line is **no host-side SQLite reads, ever**: the schema is core-owned, `notes.text` is `enc:v2` ciphertext at rest (ADR 0003 — `pull` stores it verbatim, never decrypts), and search-over-plaintext must happen where the Master Key lives. SUR-744 adds the read surface: `list_books`/`get_book`/`list_notes`/`get_note`/`list_custom_ideas`/`counts`/`search` on `SyncEngine` (it already owns the `Store` and the `Arc<Vault>`), plus their DTOs. Two edges force real decisions: **how note text is decrypted on the way out**, and **how lexical search reproduces the PWA's engine**.

## Options considered

The two highest-lock-in choices are **#1** (the decrypt boundary — a crypto-reviewer concern) and **#2** (the search engine — the parity contract). #3–#5 are lower-stakes and mirror the ticket's founder decisions.

| Decision | Chosen | Rejected alternative | Why rejected |
|---|---|---|---|
| **1. Decrypt boundary** | Decrypt **in core**, per-read, into the DTO / in-memory search doc; ciphertext never crosses the FFI, plaintext never written back | Return ciphertext + let hosts decrypt; **or** decrypt-and-cache to disk | Host-side decrypt means the schema/ciphertext leak across the FFI (breaks "no host SQLite reads"); caching plaintext to disk breaks ADR 0003's ciphertext-at-rest E2EE posture. |
| **2. Search engine** | **Hand-rolled in-memory inverted-free index** mirroring MiniSearch's observable matching | SQLite **FTS5** (`fts5(tokenize=…)`); **or** a heavyweight Rust search crate (e.g. tantivy) | FTS5 has prefix matching but **no custom-stemmer hook** (its Porter stemmer diverges from the PWA's `stem()` on the first `-ing`) and **no fuzzy** — so it cannot reproduce the PWA's verdicts (AC #5). A full search crate writes to disk by default (breaks decision 3), adds a heavy dependency to a spine crypto crate, and still wouldn't match the custom stem+fuzzy semantics. |
| **3. Index posture** | In-memory only, **rebuilt per `search()`** (scan → decrypt → index → query → discard) | Persistent index fed incrementally at `enqueue`/`pull` | Rebuild cost is bounded at personal-archive scale (hundreds–low-thousands of rows = ms). A cached index adds mutable state on `SyncEngine` and invalidation threaded through the write/sync paths — more surface for the crypto-reviewer, for no measured gain. Documented upgrade path if profiling ever demands it. |
| **4. Search scope** | Index **notes + custom_ideas** only | Also index books / lenses / collections (the PWA indexes lenses + collections) | The PWA does **not** index books; lenses/collections have no v1 read surface (SUR-744 decision 1). Indexing the two doc types with a v1 consumer keeps verdict parity on that subset; the rest is a named follow-up. |
| **5. Pagination / ordering** | `limit` + `offset`, `created_at DESC` | Keyset cursors | Archive scale is small; SwiftUI/Compose lazy lists page fine on offset (SUR-744 decision 4). `id DESC` tiebreak makes offset pagination deterministic across `created_at` ties. |

## Decisions

### 1. Decrypt in core, on the way out only

`NoteRecord.text` carries **plaintext**, produced by `Vault::decrypt_note`. A `notes.text` that structurally looks encrypted (`note_encryption::is_encrypted`, the mirror of the PWA's `isEncrypted()`) is decrypted; on failure the row surfaces as `text: None, decrypt_failed: true` and is dropped from the search index — it **never fails the whole page** (mirrors the PWA's `decryptError` skip). A successfully stored ciphertext therefore never crosses the FFI in its `enc:` form: it is either decrypted plaintext or `None`. Nothing is ever written back to the store, so ADR 0003's ciphertext-at-rest boundary is preserved on the read side too.

### 2. Search engine = a MiniSearch port, verdicts exact / ranking approximate

`src/search.rs` reproduces MiniSearch v7.2.0's **observable matching** — the PWA's behavioural oracle (`surfc/src/lib/lexicalSearch.js`, SUR-527):

- **`stem()` / `undouble()` ported verbatim** (plurals + `-ing`/`-ed` + consonant-undoubling), applied identically to indexed and query terms.
- **Tokenizer** = MiniSearch's `SPACE_OR_PUNCTUATION` (`\n\r\p{Z}\p{P}`), using the same real `unicode-general-category` tables `normalize.rs` uses for `\p{P}` (no new dependency).
- **Matching** = exact ∪ prefix ∪ fuzzy (Levenshtein ≤ `min(6, round(len·0.2))`), OR-combined; title field boosted 2×; a `quality` multiplier (number of distinct query terms a doc matched) rewards matching more of the query.

**Parity boundary:** the **acceptance test is verdicts** — the hit/miss/diacritics/stemming/prefix cases of SUR-754, same decisions as the PWA on the same fixtures (AC #5). The port does **not** reproduce MiniSearch's exact BM25 term-frequency saturation — only the relative ordering properties the screens rely on (title > body, more-terms-matched ranks higher, exact > prefix > fuzzy). No SUR-754 case pins an exact score or a full ordering, so this is a deliberate, documented deviation. (The "diacritics" case is fuzzy tolerance of an accent-as-edit, **not** Unicode folding — the PWA does no NFD folding either.)

### 3. In-memory, rebuilt per `search()`

The index is the in-memory `Vec<SearchDoc>` corpus, built from the live store each call and discarded. No plaintext note text is ever written to disk (AC #4) — there is no on-disk index at all. This is the laziest correct posture: no cached state, no invalidation, no coupling into `enqueue`/`pull`.

## Consequences

- The crypto boundary stays crisp: `Store`/`pull`/`push` are ciphertext-only; `Vault` is the only thing that sees plaintext, and only transiently per read. `src/search.rs` is pure text (no crypto, no store) — the decrypt happens one layer up in `sync::read` before docs are handed to it.
- Verdict parity is pinned by ported fixtures in `src/search.rs` (AC #5). A stronger differential test against the live JS engine (as `normalize.rs` does against V8) is a possible future strengthening, noted but not built — SUR-754's fixture set is the stated contract.
- Adding a read for a new synced store (lenses/collections/…) is a follow-up: extend `sync::read` + a new DTO; the `Store::list_live`/`count_live` helpers are already table-generic.
