//! SUR-725 / SUR-659c integration test: incremental **pull** + tombstones + first coexistence,
//! on books + notes, against a REAL local Supabase stack. Asserts the make-or-break path:
//!   (a) **coexistence, server→core** — a row written to Supabase (here via a core `flush`, the
//!       same enc:v2 wire shape the PWA writes) pulls into a SEPARATE core store, and its
//!       ciphertext decrypts back to the plaintext (PWA↔native round-trip transport);
//!   (b) **ciphertext at rest** — the pulled `notes.text` is enc:v2, never plaintext;
//!   (c) **`content_tag` rides through** — the pulled tag matches the plaintext-derived HMAC;
//!   (d) **tombstones** — a delete propagates to a device that has the row (applied, not lost) and
//!       is NOT resurrected on a device that never had it.
//!
//! Native-only (the engine is gated off wasm32) and `#[ignore]`d so a bare `cargo test` (no stack)
//! skips it; the CI `sync-integration` job exports `SUPABASE_URL` after `supabase start` and runs
//! `cargo test -- --ignored`. An env guard also early-returns if `SUPABASE_URL` is unset.
#![cfg(not(target_arch = "wasm32"))]

use braird_core::store::Store;
use braird_core::sync::{BookUpsert, NoteUpsert, SyncEngine};
use braird_core::Vault;
use std::sync::Arc;

/// Open an engine over a fresh temp store for the same user + token (a distinct "device").
fn open_device(
    env: &test_support::SupabaseEnv,
    user: &test_support::TestUser,
    vault: Arc<Vault>,
    tag: &str,
) -> (Arc<SyncEngine>, String) {
    let db_path = std::env::temp_dir()
        .join(format!("sur725-{tag}-{}.sqlite", user.user_id))
        .to_str()
        .unwrap()
        .to_string();
    let _ = std::fs::remove_file(&db_path); // start clean
    let engine = SyncEngine::open(
        db_path.clone(),
        env.url.clone(),
        env.anon_key.clone(),
        vault,
    )
    .expect("open engine");
    engine.set_access_token(user.access_token.clone());
    (engine, db_path)
}

#[test]
#[ignore = "needs a running local Supabase stack (CI `sync-integration` job, or a local `supabase start`)"]
fn pull_roundtrips_notes_and_books_from_the_server() {
    let Some(env) = test_support::env() else {
        eprintln!("SUPABASE_URL unset — skipping the real-Supabase pull integration test");
        return;
    };
    let user = test_support::mint_test_user_jwt(&env);
    // One user MK, shared across their two "devices" (device-transfer is how the real MK spreads).
    let vault = Vault::generate();

    let book_id = format!("book-{}", user.user_id);
    let note_id = format!("note-{}", user.user_id);
    let plaintext = "The unexamined life is not worth living.";

    // ── Device A: write locally + flush to the server (produces enc:v2 ciphertext on the wire) ──
    let (device_a, db_a) = open_device(&env, &user, vault.clone(), "a");
    device_a
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
    device_a
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
    let pushed = device_a.flush().expect("flush");
    assert_eq!(pushed.pushed, 2, "book + note flushed");

    // ── Device B: a SEPARATE store, same user/token — pull the server rows in ──
    let (device_b, db_b) = open_device(&env, &user, vault.clone(), "b");
    let summary = device_b.pull().expect("pull");
    assert!(
        summary.merged >= 2,
        "book + note merged into device B (merged={})",
        summary.merged
    );

    let store_b = Store::open(&db_b).expect("open B store");

    // (a)+(b) the note pulled, and its text is ciphertext at rest — never plaintext.
    let note = store_b
        .get_row("notes", &note_id)
        .expect("read note")
        .expect("note present on device B");
    let text = note["text"].as_str().expect("text string");
    assert!(
        text.starts_with("enc:v2:"),
        "text at rest is enc:v2: {text}"
    );
    assert!(
        !text.contains("unexamined"),
        "PLAINTEXT AT REST on pull: {text}"
    );

    // (a) coexistence: the pulled ciphertext decrypts back to the original plaintext.
    let decrypted = vault
        .decrypt_note(Some(note_id.clone()), text.to_string())
        .expect("decrypt pulled ciphertext");
    assert_eq!(decrypted, plaintext, "PWA↔native ciphertext round-trip");

    // (c) content_tag rode through and matches the plaintext-derived HMAC.
    let pulled_tag = note["content_tag"].as_str().expect("content_tag present");
    let expected_tag = vault.content_tag(plaintext.to_string(), Some(book_id.clone()));
    assert_eq!(pulled_tag, expected_tag, "content_tag matches");

    // The book pulled too, and the per-table cursor advanced past 0.
    assert!(
        store_b
            .get_row("books", &book_id)
            .expect("read book")
            .is_some(),
        "book pulled into device B"
    );
    assert!(
        store_b
            .get_seq_cursor("notes")
            .expect("cursor")
            .unwrap_or(0)
            > 0,
        "notes change_seq cursor advanced after a successful pull"
    );

    let _ = std::fs::remove_file(&db_a);
    let _ = std::fs::remove_file(&db_b);
}

#[test]
#[ignore = "needs a running local Supabase stack (CI `sync-integration` job, or a local `supabase start`)"]
fn pull_applies_tombstone_and_never_resurrects() {
    let Some(env) = test_support::env() else {
        eprintln!("SUPABASE_URL unset — skipping the real-Supabase tombstone integration test");
        return;
    };
    let user = test_support::mint_test_user_jwt(&env);
    let vault = Vault::generate();

    let book_id = format!("tomb-book-{}", user.user_id);
    let note_id = format!("tomb-note-{}", user.user_id);

    // Device A creates a book + note and flushes.
    let (device_a, db_a) = open_device(&env, &user, vault.clone(), "tomb-a");
    device_a
        .enqueue_book(BookUpsert {
            id: book_id.clone(),
            title: "Book".into(),
            author: None,
            isbn: None,
            cover_url: None,
            cover_source: None,
            cover_resolved_at: None,
            created_at: 1_700_000_000_000,
            deleted: false,
            clear_nullable_fields: vec![],
        })
        .expect("enqueue book");
    device_a
        .enqueue_note(NoteUpsert {
            id: note_id.clone(),
            book_id: Some(book_id.clone()),
            plaintext: Some("to be deleted".into()),
            page: None,
            tags: vec![],
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
    device_a.flush().expect("flush create");

    // Device B pulls the live note.
    let (device_b, db_b) = open_device(&env, &user, vault.clone(), "tomb-b");
    device_b.pull().expect("pull #1");
    let store_b = Store::open(&db_b).expect("open B");
    assert_eq!(
        store_b.get_row("notes", &note_id).unwrap().unwrap()["deleted"],
        serde_json::json!(false),
        "note is live on B after the first pull"
    );

    // Device A soft-deletes the note (a later updated_at) and flushes the tombstone.
    device_a
        .enqueue_note(NoteUpsert {
            id: note_id.clone(),
            book_id: Some(book_id.clone()),
            plaintext: Some("to be deleted".into()),
            page: None,
            tags: vec![],
            source: None,
            source_id: None,
            source_meta_json: None,
            chapter: None,
            image_path: None,
            ink_crop_path: None,
            created_at: 1_700_000_000_000,
            deleted: true,
            clear_nullable_fields: vec![],
        })
        .expect("enqueue delete");
    device_a.flush().expect("flush delete");

    // Device B pulls again → the tombstone is applied (delete propagated, not lost).
    device_b.pull().expect("pull #2");
    assert_eq!(
        store_b.get_row("notes", &note_id).unwrap().unwrap()["deleted"],
        serde_json::json!(true),
        "tombstone applied on B — a delete on one device propagates"
    );

    // Device C has NEVER seen the note. Pulling the tombstone must NOT resurrect it (insert a row).
    let (device_c, db_c) = open_device(&env, &user, vault.clone(), "tomb-c");
    let summary_c = device_c.pull().expect("pull C");
    assert!(
        summary_c.skipped_tombstones >= 1,
        "C skipped the tombstone rather than inserting it"
    );
    let store_c = Store::open(&db_c).expect("open C");
    assert!(
        store_c.get_row("notes", &note_id).unwrap().is_none(),
        "a delete for a row C never had must not be resurrected"
    );

    let _ = std::fs::remove_file(&db_a);
    let _ = std::fs::remove_file(&db_b);
    let _ = std::fs::remove_file(&db_c);
}
