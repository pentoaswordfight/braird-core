//! SUR-724 / SUR-659b integration test: outbox collapse + flush on books + notes against a REAL
//! local Supabase stack. Asserts the founder-decided model end to end:
//!   (a) collapse = LWW-per-field / sticky-delete / transitive book_id remap;
//!   (b) only CIPHERTEXT leaves — the server-side `notes.text` is `enc:v2:…`, never plaintext;
//!   (c) `content_tag` is present and correct (matches a re-computation from the plaintext);
//!   (d) token-handoff: a real GoTrue JWT → the core's own PostgREST upsert succeeds.
//!
//! Native-only (the engine is gated off wasm32) and `#[ignore]`d so a bare `cargo test` (no
//! stack) skips it; the CI `integration` job exports `SUPABASE_URL` after `supabase start` and
//! runs `cargo test -- --ignored`. An env guard also early-returns if `SUPABASE_URL` is unset,
//! so `cargo test -- --ignored` off-CI is a no-op rather than a failure.
#![cfg(not(target_arch = "wasm32"))]

use braird_core::store::Store;
use braird_core::sync::outbox::{collapse, OutboxItem};
use braird_core::sync::{BookUpsert, NoteUpsert, SyncEngine};
use braird_core::Vault;
use serde_json::json;
use std::collections::BTreeMap;

// ── (a) collapse — pure, no stack needed (also covered in src/sync/outbox.rs unit tests; this
// is the integration-level restatement that the flush relies on the same semantics) ──────────
#[test]
fn collapse_is_lww_sticky_delete_and_remaps_book_id() {
    let mut remap = BTreeMap::new();
    remap.insert("tempA".to_string(), "server-1".to_string());

    let items = vec![
        OutboxItem {
            id: 1,
            table_name: "notes".into(),
            record_id: Some("n1".into()),
            payload: json!({ "id": "n1", "book_id": "tempA", "text": "enc:v2:a", "page": "5" }),
            created_at: 100,
        },
        OutboxItem {
            id: 2,
            table_name: "notes".into(),
            record_id: Some("n1".into()),
            payload: json!({ "id": "n1", "text": "enc:v2:b" }),
            created_at: 200,
        },
    ];
    let out = collapse(items, &remap);
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0].payload["text"],
        json!("enc:v2:b"),
        "LWW: later text wins"
    );
    assert_eq!(
        out[0].payload["page"],
        json!("5"),
        "LWW: earlier page survives"
    );
    assert_eq!(
        out[0].payload["book_id"],
        json!("server-1"),
        "book_id repointed via remap"
    );
}

// ── (b)+(c)+(d) flush against real Supabase — env-guarded + #[ignore]d ────────────────────────
#[test]
#[ignore = "needs a running local Supabase stack (CI `integration` job, or a local `supabase start`)"]
fn flush_seals_text_and_upserts_via_token_handoff() {
    let Some(env) = test_support::env() else {
        eprintln!("SUPABASE_URL unset — skipping the real-Supabase integration test");
        return;
    };
    let user = test_support::mint_test_user_jwt(&env);

    // A fresh in-memory... no: the engine opens a file store. Use a temp path so the outbox is
    // isolated per run.
    let db_path = std::env::temp_dir().join(format!("sur724-{}.sqlite", user.user_id));
    let db_path = db_path.to_str().unwrap().to_string();

    let vault = Vault::generate();
    let engine = SyncEngine::open(
        db_path.clone(),
        env.url.clone(),
        env.anon_key.clone(),
        vault.clone(),
    )
    .expect("open engine");
    engine.set_access_token(user.access_token.clone());

    // Enqueue a book + a child note. The note text is PLAINTEXT here — the engine seals it.
    let book_id = format!("book-{}", user.user_id);
    let note_id = format!("note-{}", user.user_id);
    let plaintext = "The unexamined life is not worth living.";
    engine
        .enqueue_book(BookUpsert {
            id: book_id.clone(),
            title: "Apology".into(),
            author: Some("Plato".into()),
            isbn: None,
            cover_url: None,
            cover_source: None,
            cover_resolved_at: None,
            created_at: 1_700_000_000_000,
            deleted: false,
            clear_nullable_fields: vec![],
        })
        .expect("enqueue book");
    engine
        .enqueue_note(NoteUpsert {
            id: note_id.clone(),
            book_id: Some(book_id.clone()),
            plaintext: Some(plaintext.to_string()),
            page: Some("38a".into()),
            tags: vec!["philosophy".into()],
            source: None,
            source_id: None,
            source_meta_json: None,
            chapter: None,
            image_path: None,
            ink_crop_path: None,
            created_at: 1_700_000_000_000,
            deleted: false,
            clear_nullable_fields: vec![],
        })
        .expect("enqueue note");

    let summary = engine.flush().expect("flush");
    assert_eq!(
        summary.pushed, 2,
        "both book + note should flush; still_queued={}",
        summary.still_queued
    );
    assert_eq!(summary.still_queued, 0);

    // (b) Only ciphertext left: the server-side text is enc:v2, never the plaintext.
    let notes = test_support::select(
        &env,
        &user.access_token,
        "notes",
        &format!("id=eq.{note_id}"),
    );
    let row = notes
        .as_array()
        .and_then(|a| a.first())
        .expect("note row present");
    let server_text = row["text"].as_str().expect("text is a string").to_string();
    assert!(
        server_text.starts_with("enc:v2:"),
        "text must be enc:v2 ciphertext, got: {server_text}"
    );
    assert!(
        !server_text.contains("unexamined"),
        "PLAINTEXT LEAKED into notes.text: {server_text}"
    );

    // (c) content_tag present + correct: recompute from the plaintext and compare (64-hex HMAC).
    let server_tag = row["content_tag"]
        .as_str()
        .expect("content_tag present")
        .to_string();
    let expected_tag = vault.content_tag(plaintext.to_string(), Some(book_id.clone()));
    assert_eq!(
        server_tag, expected_tag,
        "content_tag must match the plaintext-derived HMAC"
    );
    assert_eq!(server_tag.len(), 64, "content_tag is 64-hex");
    let server_created_at = row["created_at"].clone();
    let server_updated_at = row["updated_at"]
        .as_i64()
        .expect("updated_at is an integer");

    // (d) token handoff already proven by the successful upsert; also assert the book landed.
    let books = test_support::select(
        &env,
        &user.access_token,
        "books",
        &format!("id=eq.{book_id}"),
    );
    assert!(
        books.as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "book upserted via token handoff"
    );

    // Outbox should be drained (both succeeded).
    let store = Store::open(&db_path).expect("reopen store");
    assert!(
        store.outbox_items().expect("outbox").is_empty(),
        "outbox cleared after successful flush"
    );

    // SUR-921: a plaintext-free targeted PATCH must be accepted without a text key, and the
    // server's sealed/immutable columns must survive byte-for-byte.
    std::thread::sleep(std::time::Duration::from_millis(2));
    engine
        .enqueue_note(NoteUpsert {
            id: note_id.clone(),
            book_id: None,
            plaintext: None,
            page: None,
            tags: vec!["ethics".into()],
            source: None,
            source_id: None,
            source_meta_json: None,
            chapter: None,
            image_path: None,
            ink_crop_path: None,
            created_at: 9_999_999_999_999,
            deleted: false,
            clear_nullable_fields: vec![],
        })
        .expect("enqueue plaintext-free note patch");

    let store = Store::open(&db_path).expect("inspect patch outbox");
    let queued = store.outbox_items().expect("patch outbox");
    assert_eq!(queued.len(), 1, "one tags-only patch queued");
    let patch_payload: serde_json::Value =
        serde_json::from_str(&queued[0].3).expect("patch payload JSON");
    for key in ["text", "content_tag", "created_at"] {
        assert!(
            patch_payload.get(key).is_none(),
            "{key} must be absent from the outgoing patch"
        );
    }

    let patch_summary = engine.flush().expect("flush plaintext-free patch");
    assert_eq!(patch_summary.pushed, 1);
    assert_eq!(patch_summary.still_queued, 0);

    let patched_notes = test_support::select(
        &env,
        &user.access_token,
        "notes",
        &format!("id=eq.{note_id}"),
    );
    let patched = patched_notes
        .as_array()
        .and_then(|rows| rows.first())
        .expect("patched note row present");
    assert_eq!(patched["tags"], json!(["ethics"]));
    assert!(
        patched["updated_at"]
            .as_i64()
            .expect("patched updated_at is an integer")
            > server_updated_at,
        "the patch must advance server updated_at"
    );
    assert_eq!(patched["text"], json!(server_text));
    assert_eq!(patched["content_tag"], json!(server_tag));
    assert_eq!(patched["created_at"], server_created_at);

    let _ = std::fs::remove_file(&db_path);
}
