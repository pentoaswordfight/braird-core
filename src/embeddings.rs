//! Host-embedder contract + sealed-vector primitives (SUR-997, ADR 0006) — the
//! platform-neutral leg of the SUR-529 native embedding lane (EmbeddingGemma-300M,
//! quantized, Matryoshka-truncated to 256-dim, run by each host's own runtime).
//!
//! Core owns *what* gets embedded and *when*; the host owns the runtime. The [`Embedder`]
//! trait is this repo's **first foreign-implemented trait** (`with_foreign`): Swift/Kotlin
//! register an implementation on the `SyncEngine`, which drives it from the derived embed
//! queue (`Store::pending_embeddings`) and seals every vector with the vault key before it
//! touches disk (`Vault::seal_bytes`, AAD = `emb:{note id}` — the `0x02` byte seal,
//! domain-separated from enc:v2; see [`embed_aad`]). Vectors are **device-local**: nothing
//! here ever reaches the outbox or the server.
//!
//! This module is the pure half — the contract types and the vector math (corpus key,
//! normalization, the f32-LE codec, dot/top-k). The pipeline choreography (locks, decrypt,
//! store writes) lives on `SyncEngine` in `sync/mod.rs`, where the mutexes live.
//!
//! ## arm64 caution (SUR-770/843, in reverse)
//!
//! Rust→foreign trait calls are marshalled by the same machinery whose stack-spill behavior
//! x86-64 CI is structurally blind to. Keep every trait method at ONE argument; widening a
//! method here needs a real arm64 device pass (FTL) before release.

use crate::CryptoError;

/// The identity of a host embedder — everything the vector space depends on. Carried in
/// the corpus key, so ANY change re-keys the corpus and re-queues every note.
///
/// **Prompt-template contract (documented, not enforced):** EmbeddingGemma's query/document
/// prompt templates and tokenization (BOS(2) + prompt + EOS(1), seq-fixed single-input
/// exports) are part of the vector space too, but only the host can see them. A host that
/// changes its template, tokenizer, or sequence length MUST change its declared `model_id`
/// — silently keeping it produces a mixed-space corpus core cannot detect.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EmbedderDescriptor {
    /// The model identity, e.g. `"embeddinggemma-300m-seq512-mixed"`. Must be non-empty and
    /// `|`-free (it is a corpus-key segment).
    pub model_id: String,
    /// Output dimensionality after any Matryoshka truncation (the SUR-529 target is 256).
    pub dims: u32,
    /// The quantization variant of the model artifact (e.g. `"q8-mixed"`). `|`-free.
    pub quantization: String,
}

/// How a host embed call failed — the error the *embedder* throws (`SyncError::Embed` is
/// its engine-side counterpart, travelling the other direction). Deliberately
/// **fieldless**: the error crosses foreign→Rust, and a host-authored message must never
/// transit into core's error strings (the same no-host-content rule as `enqueue_note`'s
/// `source_meta_json` handling) — the host already knows its own error; it threw it, and
/// can log it host-side.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum EmbedderError {
    /// This embed failed (inference error, transient runtime fault). The pipeline counts it
    /// and moves on to the next note.
    #[error("embedder runtime failure")]
    Runtime,
    /// The runtime cannot serve at all right now (model not downloaded / unloaded / device
    /// constrained). The pipeline aborts the whole pass — the host re-drains later.
    #[error("embedder unavailable")]
    Unavailable,
}

/// An UNDECLARED host exception (anything that is not an `EmbedderError` — a stray
/// `IllegalStateException`, an arbitrary Swift `Error`) arrives on UniFFI's
/// unexpected-error lane. Without this impl that lane PANICS core with the host's own
/// message string — both crashing the pass and ferrying host-authored content through
/// core, the two things the fieldless design forbids. Mapping to
/// [`EmbedderError::Runtime`] drops the host string in core and keeps the
/// count-as-failed-and-continue contract (crypto-review finding; pinned by round-trip
/// tests throwing an undeclared exception on both bindings).
impl From<uniffi::UnexpectedUniFFICallbackError> for EmbedderError {
    fn from(_: uniffi::UnexpectedUniFFICallbackError) -> Self {
        EmbedderError::Runtime
    }
}

/// The host-registered embedder (SUR-997 item 1): core calls it with **plaintext** and gets
/// a vector back. Implemented by the host's native runtime (LiteRT on Android, LiteRT/Core
/// ML on iOS) and registered via `SyncEngine::register_embedder`.
///
/// **Crypto boundary (ADR 0006).** These calls are the one place decrypted note text leaves
/// core other than the display DTOs. The host must treat the text as it treats displayed
/// note content: never persist it, never log it, never transmit it. Core never holds a lock
/// across these calls, and hands over at most one note's plaintext at a time.
///
/// Two embed methods, not one: EmbeddingGemma prompts documents and queries differently,
/// and the templates live with the runtime that owns the tokenizer (see
/// [`EmbedderDescriptor`]'s template contract). Both take exactly one argument — see the
/// module's arm64 caution.
#[uniffi::export(with_foreign)]
pub trait Embedder: Send + Sync {
    /// The embedder's identity. Called once at registration; must be constant for the
    /// lifetime of the registration and must never throw.
    fn descriptor(&self) -> EmbedderDescriptor;

    /// Embed one note's plaintext for storage (the document prompt template). Must return
    /// exactly `descriptor().dims` values; the output need not be pre-normalized (core
    /// normalizes defensively).
    fn embed_document(&self, text: String) -> Result<Vec<f32>, EmbedderError>;

    /// Embed a search query (the query prompt template). Same length contract.
    fn embed_query(&self, text: String) -> Result<Vec<f32>, EmbedderError>;
}

/// One semantic-scan result: a live note and its cosine similarity to the probe, in
/// `[-1, 1]` (vectors are unit-norm, so the dot product IS the cosine). Best-first.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct SemanticHit {
    pub note_id: String,
    pub score: f64,
}

/// What one `embed_pending` pass did. `attempted = embedded + skipped + failed`;
/// `pending` is the derived queue size after the pass — the host's durable
/// rebuild/progress signal (it survives a process restart, unlike a registration-time
/// flag), and the right driver for any "search index is rebuilding" UI. One word for the
/// queue size everywhere: this field, `RegisterEmbedderSummary::pending`, and
/// `pending_embed_count` all name the same number.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EmbedSummary {
    /// Queue items this pass processed (capped by `max_items`).
    pub attempted: u32,
    /// Vectors embedded, sealed, and stored.
    pub embedded: u32,
    /// Notes that produced a skip marker (empty text, decrypt failure) or whose text moved
    /// mid-embed (they re-queue with the new token).
    pub skipped: u32,
    /// Embeds that failed (host error, wrong dimension, non-finite output). Still queued,
    /// but deprioritized: a failed note is re-attempted only after every other pending
    /// note has had its turn, so a failing head can't starve chunked drains (SUR-1010).
    pub failed: u32,
    /// The derived queue size after this pass.
    pub pending: u32,
}

/// What registering an embedder did. `corpus_changed`/`invalidated` are the *immediate*
/// "search model updated" signal (a corpus-key change hard-deletes every stale-key vector);
/// `pending` is the durable one — on a relaunch mid-rebuild the key already matches the
/// partially-rebuilt corpus, so `corpus_changed` is correctly `false` while `pending` still
/// reports the remainder. Drive persistent notification UI off the pending count.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RegisterEmbedderSummary {
    /// The registered corpus key differs from (some of) what was stored — stale vectors
    /// were dropped and their notes re-queued.
    pub corpus_changed: bool,
    /// How many stale-key vectors were dropped.
    pub invalidated: u32,
    /// The derived queue size after registration.
    pub pending: u32,
}

/// The corpus version a vector is keyed by (SUR-997 item 3 — the PWA's
/// `MODEL_CACHE_VERSION` pattern, `surfc/src/embeddings/embeddingModel.js`): model id ×
/// dims × quantization, plus core's own storage-format token. Stored per row in the
/// `embeddings.model_version` column; ANY segment changing invalidates the corpus and
/// re-queues the backfill. Bump `f32le-v1` if core ever changes how vectors are encoded at
/// rest — that re-embeds every device.
pub(crate) fn corpus_key(descriptor: &EmbedderDescriptor) -> String {
    format!(
        "{}|{}|{}|f32le-v1",
        descriptor.model_id, descriptor.dims, descriptor.quantization
    )
}

/// The AAD a note's vector is sealed under: `emb:{note_id}`. The `emb:` prefix
/// domain-separates the `0x02` vector seal from `enc:v2` note ciphertext, which uses the
/// BARE note id as AAD under the same Master Key — without it, neither format header being
/// authenticated means a stored enc:v2 body repackaged as `[0x02][iv][ct]` (or vice versa)
/// would cross-authenticate (crypto-review hardening). Free now — no device holds a sealed
/// vector; after release it would cost a corpus rebuild. The seal primitive and its frozen
/// `0x02` header are untouched; this is purely the caller's AAD choice.
pub(crate) fn embed_aad(note_id: &str) -> String {
    format!("emb:{note_id}")
}

/// Normalize to unit length, or `None` for a zero/non-finite vector (a NaN/Inf anywhere
/// poisons the norm, so one check covers every component). The spike's exports are
/// pre-normalized; normalizing anyway makes the scan a plain dot product and stops a host
/// that forgets from silently wrecking ranking.
pub(crate) fn normalize(mut v: Vec<f32>) -> Option<Vec<f32>> {
    let norm_sq: f64 = v.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
    if !norm_sq.is_finite() || norm_sq <= 0.0 {
        return None;
    }
    let inv = 1.0 / norm_sq.sqrt();
    for x in v.iter_mut() {
        *x = (f64::from(*x) * inv) as f32;
    }
    Some(v)
}

/// Encode a vector as f32 little-endian bytes — the at-rest format inside the seal
/// (4 bytes/dim; 256-dim ≈ 1 KB/vector, ~5 MB at 5k notes — the spike's non-issue).
pub(crate) fn to_le_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Decode f32-LE bytes, validating the length against the corpus dimensionality. `Err` =
/// a corrupt or foreign blob (the caller drops the row so the note re-queues).
pub(crate) fn from_le_bytes(bytes: &[u8], dims: u32) -> Result<Vec<f32>, CryptoError> {
    if bytes.len() != dims as usize * 4 {
        return Err(CryptoError::BadInput(
            "sealed vector has wrong length".into(),
        ));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Dot product in f64 (over unit-norm vectors = cosine similarity). Callers guarantee
/// equal lengths (both sides are length-validated against `dims` upstream).
pub(crate) fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| f64::from(x) * f64::from(y))
        .sum()
}

/// Best-first top-k: descending score, `note_id` tiebreak (deterministic for fixtures),
/// truncated to `limit`. Brute force — SUR-529: no ANN below ~100k docs, ~20× beyond a
/// heavy personal archive.
pub(crate) fn top_k(mut hits: Vec<SemanticHit>, limit: usize) -> Vec<SemanticHit> {
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.note_id.cmp(&b.note_id))
    });
    hits.truncate(limit);
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(model_id: &str, dims: u32, quantization: &str) -> EmbedderDescriptor {
        EmbedderDescriptor {
            model_id: model_id.into(),
            dims,
            quantization: quantization.into(),
        }
    }

    #[test]
    fn an_undeclared_callback_error_degrades_to_runtime_without_the_host_string() {
        // The unexpected-error lane (crypto-review): an undeclared host exception maps to
        // fieldless Runtime — never a panic, never the host's message in core.
        let e = EmbedderError::from(uniffi::UnexpectedUniFFICallbackError {
            reason: "host secret detail".into(),
        });
        assert!(matches!(e, EmbedderError::Runtime));
        assert!(
            !e.to_string().contains("secret"),
            "host content never transits core error strings"
        );
    }

    #[test]
    fn embed_aad_domain_separates_from_the_bare_note_id() {
        assert_eq!(embed_aad("n1"), "emb:n1");
        assert_ne!(embed_aad("n1"), "n1", "never the enc:v2 AAD namespace");
    }

    #[test]
    fn corpus_key_is_stable_and_changes_on_every_segment() {
        let base = descriptor("gemma-300m", 256, "q8");
        assert_eq!(corpus_key(&base), "gemma-300m|256|q8|f32le-v1");
        // Any identity segment changing re-keys the corpus (→ invalidation + re-embed).
        assert_ne!(
            corpus_key(&base),
            corpus_key(&descriptor("gemma-2", 256, "q8"))
        );
        assert_ne!(
            corpus_key(&base),
            corpus_key(&descriptor("gemma-300m", 128, "q8"))
        );
        assert_ne!(
            corpus_key(&base),
            corpus_key(&descriptor("gemma-300m", 256, "f16"))
        );
    }

    #[test]
    fn normalize_produces_unit_vectors_and_rejects_degenerate_input() {
        let v = normalize(vec![3.0, 4.0]).unwrap();
        assert!((f64::from(v[0]) - 0.6).abs() < 1e-6);
        assert!((f64::from(v[1]) - 0.8).abs() < 1e-6);
        assert!((dot(&v, &v) - 1.0).abs() < 1e-6, "unit norm");
        // An already-unit vector passes through (the expected host case).
        let u = normalize(vec![0.6, 0.8]).unwrap();
        assert!((dot(&u, &u) - 1.0).abs() < 1e-6);
        // Degenerate outputs are rejected, not stored.
        assert!(normalize(vec![0.0, 0.0]).is_none(), "zero vector");
        assert!(normalize(vec![1.0, f32::NAN]).is_none(), "NaN");
        assert!(normalize(vec![1.0, f32::INFINITY]).is_none(), "Inf");
    }

    #[test]
    fn codec_round_trips_and_rejects_wrong_lengths() {
        let v = vec![0.5f32, -1.25, 3.5e-3, 0.0];
        let bytes = to_le_bytes(&v);
        assert_eq!(bytes.len(), 16);
        assert_eq!(from_le_bytes(&bytes, 4).unwrap(), v);
        assert!(from_le_bytes(&bytes, 3).is_err(), "dims mismatch");
        assert!(from_le_bytes(&bytes[1..], 4).is_err(), "truncated blob");
    }

    #[test]
    fn top_k_orders_best_first_with_deterministic_ties() {
        let hit = |id: &str, score: f64| SemanticHit {
            note_id: id.into(),
            score,
        };
        let hits = vec![hit("c", 0.5), hit("a", 0.9), hit("b", 0.9), hit("d", 0.1)];
        let top = top_k(hits, 3);
        assert_eq!(
            top,
            vec![hit("a", 0.9), hit("b", 0.9), hit("c", 0.5)],
            "descending score, note_id tiebreak, truncated to limit"
        );
        assert!(top_k(vec![], 5).is_empty());
    }
}
