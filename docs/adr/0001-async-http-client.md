# ADR 0001 — Async HTTP client: reqwest + tokio current_thread + rustls

**Date:** 2026-06-30  
**Status:** Accepted  
**Ticket:** SUR-659 (decision), SUR-724 (first implementation)

---

## Context

braird-core is a library crate embedded directly in host applications (mobile, desktop). It needs to make authenticated HTTPS calls to a PostgREST endpoint as part of the sync push layer introduced in SUR-724.

Two constraints shaped the decision:

1. **Library embedding.** A library crate must not spawn a `multi_thread` Tokio runtime. Doing so can conflict with the host's own runtime and multiplies thread counts in ways the host cannot control. A `current_thread` runtime gives full async/await semantics with no background threads — the runtime lives and dies on the calling thread, making its resource footprint predictable and its lifecycle host-visible.

2. **TLS portability.** OpenSSL linkage is unreliable on iOS and cross-compiled Android targets. rustls is a pure-Rust TLS stack with no system-library dependency, keeping the build reproducible across all target triples without additional host toolchain setup.

reqwest was chosen as the HTTP client because it supports both rustls and selective feature disabling (only the features needed for authenticated JSON POSTs are enabled), avoiding unnecessary binary size growth.

---

## Decision

Use **reqwest** (default-features = false, with rustls-tls and json features) as the HTTP client, driven by a **tokio `current_thread` runtime** owned by the `SyncEngine` handle — built once in `SyncEngine::open` and reused by every `flush()` via `block_on`. TLS is handled exclusively by **rustls**; the OpenSSL backend is never compiled in.

The host (or test driver) supplies the JWT access token by calling `set_access_token(jwt: String)` before invoking sync operations. braird-core does not perform authentication itself — it consumes a token it is handed.

---

## Consequences

- `flush_outbox()` blocks the calling thread for the duration of the network round-trips. Callers that need non-blocking behaviour must dispatch to a dedicated thread (e.g. `std::thread::spawn` on mobile, a thread-pool worker on desktop). This is intentional: it keeps scheduling decisions in the host, not the library.
- The `SyncEngine` owns one persistent `current_thread` runtime and one reqwest client, so keep-alive connection pooling persists across flushes. A `current_thread` runtime has no worker-thread pool — futures are driven only while `block_on` holds the calling thread, so there is still no background runtime thread.
- rustls requires Rust 1.63+ and is subject to its own FIPS posture (not certified). If a future target requires a FIPS-validated TLS stack, this decision will need revisiting.
- The `set_access_token` interface is the single token ingress point. Token refresh is the host's responsibility; braird-core will return a 401-equivalent error if the token is absent or expired, and the item remains in the outbox.
