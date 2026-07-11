//! SUR-726 / SUR-659d integration test: the **full 8-store PWA↔native coexistence matrix**, against
//! a REAL local Supabase stack — the Phase-2 make-or-break gate. Extends the SUR-725 notes+books
//! proof to all eight synced stores and asserts:
//!   (1) **round-trip, both directions** — one row per store written by device A pulls into device B;
//!       B's edits pull back into A via `sync()`; per-table cursors advance; note text stays enc:v2
//!       ciphertext at rest and decrypts; a membership's id is the deterministic `membershipId`;
//!   (2) **tombstones propagate + never resurrect** — a delete on each new store reaches a device
//!       that has the row, and is skipped (not inserted) on a device that never had it;
//!   (3) **outbox rebase convergence on a fan-out table** — an offline edit that a newer server row
//!       beat is rebased away and surfaced as `superseded` (SUR-736), proven here on `lenses`;
//!   (4) **deterministic-id convergence** — two devices adding the same note↔collection pair collapse
//!       to ONE server row (the SUR-737 OR-set add);
//!   (5) **export/import parity** — a server row with EVERY column populated (the PWA wire shape)
//!       pulls into the core with every descriptor column verbatim, and a core PARTIAL edit + flush
//!       does not null the untouched server columns.
//!
//! Native-only (the engine is gated off wasm32) and `#[ignore]`d so a bare `cargo test` (no stack)
//! skips it; the CI `sync-integration` job exports `SUPABASE_URL` after `supabase start` and runs
//! `cargo test -- --ignored`. An env guard also early-returns if `SUPABASE_URL` is unset.
//!
//! Server-LWW caveat (accepted, out of scope): the server upsert is still unconditional
//! `merge-duplicates` (last-FLUSH-wins), so a pure concurrent-edit-by-time test would depend on
//! flush order — the durable server guard is SUR-740/PR-3. The in-scope convergence this proves is
//! the CLIENT-side outbox rebase (SUR-736) in (3); whole-row LWW by `updated_at` is pinned by the
//! `sur737_*` / `lww_*` unit tests.
#![cfg(not(target_arch = "wasm32"))]

use braird_core::store::Store;
use braird_core::sync::{membership_id, BookUpsert, NoteUpsert, SyncEngine};
use braird_core::Vault;
use serde_json::json;
use std::sync::Arc;

/// Open an engine over a fresh temp store for the same user + token (a distinct "device").
fn open_device(
    env: &test_support::SupabaseEnv,
    user: &test_support::TestUser,
    vault: Arc<Vault>,
    tag: &str,
) -> (Arc<SyncEngine>, String) {
    let db_path = std::env::temp_dir()
        .join(format!("sur726-{tag}-{}.sqlite", user.user_id))
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

const TS: i64 = 1_700_000_000_000;

/// Enqueue one live row into each of the eight synced stores on `dev`, wiring the FK edges
/// (note→book, link→notes, membership→note+collection, signals→note). Returns the ids used.
struct Ids {
    book: String,
    n1: String,
    n2: String,
    idea: String,
    link: String,
    lens: String,
    collection: String,
    membership: String,
}
fn enqueue_full_graph(dev: &SyncEngine, user_id: &str) -> Ids {
    let book = format!("book-{user_id}");
    let n1 = format!("n1-{user_id}");
    let n2 = format!("n2-{user_id}");
    let idea = format!("idea-{user_id}");
    let link = format!("link-{user_id}");
    let lens = format!("lens-{user_id}");
    let collection = format!("col-{user_id}");

    dev.enqueue_book(BookUpsert {
        id: book.clone(),
        title: "Apology".into(),
        author: Some("Plato".into()),
        isbn: None,
        cover_url: None,
        cover_source: None,
        cover_resolved_at: None,
        created_at: TS,
        deleted: false,
        clear_nullable_fields: vec![],
    })
    .unwrap();
    dev.enqueue_note(NoteUpsert {
        id: n1.clone(),
        book_id: Some(book.clone()),
        plaintext: "first".into(),
        page: None,
        tags: vec![],
        source: None,
        source_id: None,
        source_meta_json: None,
        chapter: None,
        image_path: None,
        ink_crop_path: None,
        created_at: TS,
        deleted: false,
        clear_nullable_fields: vec![],
    })
    .unwrap();
    dev.enqueue_note(NoteUpsert {
        id: n2.clone(),
        book_id: Some(book.clone()),
        plaintext: "second".into(),
        page: None,
        tags: vec![],
        source: None,
        source_id: None,
        source_meta_json: None,
        chapter: None,
        image_path: None,
        ink_crop_path: None,
        created_at: TS,
        deleted: false,
        clear_nullable_fields: vec![],
    })
    .unwrap();
    dev.enqueue_custom_idea(
        idea.clone(),
        "Justice".into(),
        Some("a virtue".into()),
        TS,
        false,
    )
    .unwrap();
    dev.enqueue_note_link(link.clone(), n1.clone(), n2.clone(), None, TS, false)
        .unwrap();
    dev.enqueue_lens(
        lens.clone(),
        "My Lens".into(),
        vec!["leaf-a".into()],
        None,
        None,
        TS,
        false,
    )
    .unwrap();
    dev.enqueue_collection(collection.clone(), "Reading".into(), TS, false)
        .unwrap();
    dev.enqueue_collection_membership(n1.clone(), collection.clone(), TS, false)
        .unwrap();
    dev.enqueue_note_signals(n1.clone(), 0.5, 2, true, 1, TS, TS, 0.8, TS, false)
        .unwrap();

    Ids {
        membership: membership_id(collection.clone(), n1.clone()),
        book,
        n1,
        n2,
        idea,
        link,
        lens,
        collection,
    }
}

#[test]
#[ignore = "needs a running local Supabase stack (CI `sync-integration` job, or a local `supabase start`)"]
fn eight_store_roundtrip_both_directions() {
    let Some(env) = test_support::env() else {
        eprintln!("SUPABASE_URL unset — skipping the 8-store coexistence integration test");
        return;
    };
    let user = test_support::mint_test_user_jwt(&env);
    let vault = Vault::generate();

    // ── Device A: write one row per store + flush (produces the PWA wire shapes on the server) ──
    let (device_a, db_a) = open_device(&env, &user, vault.clone(), "a");
    let ids = enqueue_full_graph(&device_a, &user.user_id);
    let pushed = device_a.flush().expect("flush A");
    assert_eq!(
        pushed.pushed, 9,
        "book + 2 notes + 6 fan-out rows all flushed"
    );
    assert_eq!(
        pushed.still_queued, 0,
        "nothing held back — the FK topo order dispatched cleanly"
    );

    // ── Device B: a separate store — pull all eight stores in ──
    let (device_b, db_b) = open_device(&env, &user, vault.clone(), "b");
    let summary = device_b.pull().expect("pull B");
    assert!(
        summary.merged >= 9,
        "all rows merged into B (merged={})",
        summary.merged
    );
    let store_b = Store::open(&db_b).expect("open B");

    for (table, id) in [
        ("books", &ids.book),
        ("notes", &ids.n1),
        ("notes", &ids.n2),
        ("custom_ideas", &ids.idea),
        ("note_links", &ids.link),
        ("lenses", &ids.lens),
        ("collections", &ids.collection),
        ("collection_memberships", &ids.membership),
    ] {
        assert!(
            store_b.get_row(table, id).unwrap().is_some(),
            "{table}/{id} round-tripped into device B"
        );
    }
    // note_signals is keyed by note_id.
    assert!(
        store_b.get_row("note_signals", &ids.n1).unwrap().is_some(),
        "note_signals pulled"
    );

    // The membership id is the deterministic join (collection first) — not a random uuid.
    assert_eq!(ids.membership, format!("{}:{}", ids.collection, ids.n1));

    // Note text is ciphertext at rest and decrypts back (PWA↔native transport).
    let note = store_b.get_row("notes", &ids.n1).unwrap().unwrap();
    let text = note["text"].as_str().unwrap();
    assert!(
        text.starts_with("enc:v2:"),
        "note text is enc:v2 at rest: {text}"
    );
    assert_eq!(
        vault
            .decrypt_note(Some(ids.n1.clone()), text.to_string())
            .unwrap(),
        "first",
        "pulled ciphertext decrypts to the original plaintext"
    );

    // Every per-table cursor advanced past 0.
    for table in [
        "books",
        "notes",
        "custom_ideas",
        "note_links",
        "lenses",
        "collections",
        "collection_memberships",
        "note_signals",
    ] {
        assert!(
            store_b.get_seq_cursor(table).unwrap().unwrap_or(0) > 0,
            "{table} change_seq cursor advanced after the pull"
        );
    }

    // ── Reverse direction: B edits two stores, flushes; A pulls them back via sync() ──
    device_b
        .enqueue_collection(
            ids.collection.clone(),
            "Renamed on B".into(),
            TS + 1000,
            false,
        )
        .expect("B renames collection");
    device_b
        .enqueue_custom_idea(
            ids.idea.clone(),
            "Justice".into(),
            Some("edited on B".into()),
            TS + 1000,
            false,
        )
        .expect("B edits idea");
    device_b.flush().expect("flush B edits");

    let a_summary = device_a
        .sync()
        .expect("A sync (pull B's edits, then flush)");
    assert!(a_summary.pull.merged >= 2, "A pulled B's two edits");
    let store_a = Store::open(&db_a).expect("open A");
    assert_eq!(
        store_a
            .get_row("collections", &ids.collection)
            .unwrap()
            .unwrap()["name"],
        json!("Renamed on B"),
        "B's collection rename converged onto A"
    );
    assert_eq!(
        store_a.get_row("custom_ideas", &ids.idea).unwrap().unwrap()["description"],
        json!("edited on B"),
        "B's idea edit converged onto A"
    );

    let _ = std::fs::remove_file(&db_a);
    let _ = std::fs::remove_file(&db_b);
}

#[test]
#[ignore = "needs a running local Supabase stack (CI `sync-integration` job, or a local `supabase start`)"]
fn tombstones_propagate_and_never_resurrect_across_all_new_stores() {
    let Some(env) = test_support::env() else {
        eprintln!("SUPABASE_URL unset — skipping the tombstone-matrix integration test");
        return;
    };
    let user = test_support::mint_test_user_jwt(&env);
    let vault = Vault::generate();

    // A creates the full graph and flushes; B pulls it live.
    let (device_a, db_a) = open_device(&env, &user, vault.clone(), "tomb-a");
    let ids = enqueue_full_graph(&device_a, &user.user_id);
    device_a.flush().expect("flush create");
    let (device_b, db_b) = open_device(&env, &user, vault.clone(), "tomb-b");
    device_b.pull().expect("B pull #1");
    let store_b = Store::open(&db_b).expect("open B");
    assert!(
        store_b.get_row("note_links", &ids.link).unwrap().is_some(),
        "B has the link"
    );

    // A soft-deletes each of the SIX new stores' rows (newer updated_at) and flushes the tombstones.
    device_a
        .enqueue_custom_idea(ids.idea.clone(), "Justice".into(), None, TS + 1000, true)
        .unwrap();
    device_a
        .enqueue_note_link(
            ids.link.clone(),
            ids.n1.clone(),
            ids.n2.clone(),
            None,
            TS + 1000,
            true,
        )
        .unwrap();
    device_a
        .enqueue_lens(
            ids.lens.clone(),
            "My Lens".into(),
            vec![],
            None,
            None,
            TS + 1000,
            true,
        )
        .unwrap();
    device_a
        .enqueue_collection(ids.collection.clone(), "Reading".into(), TS + 1000, true)
        .unwrap();
    device_a
        .enqueue_collection_membership(ids.n1.clone(), ids.collection.clone(), TS + 1000, true)
        .unwrap();
    device_a
        .enqueue_note_signals(
            ids.n1.clone(),
            0.5,
            2,
            true,
            1,
            TS,
            TS,
            0.8,
            TS + 1000,
            true,
        )
        .unwrap();
    device_a.flush().expect("flush tombstones");

    // B pulls again → every tombstone applied (delete propagated, not lost).
    device_b.pull().expect("B pull #2");
    for (table, id) in [
        ("custom_ideas", &ids.idea),
        ("note_links", &ids.link),
        ("lenses", &ids.lens),
        ("collections", &ids.collection),
        ("collection_memberships", &ids.membership),
        ("note_signals", &ids.n1),
    ] {
        assert_eq!(
            store_b.get_row(table, id).unwrap().unwrap()["deleted"],
            json!(true),
            "{table}/{id} tombstone applied on B"
        );
    }

    // Device C has NEVER seen these rows — pulling the six tombstones must NOT resurrect them.
    let (device_c, db_c) = open_device(&env, &user, vault.clone(), "tomb-c");
    let summary_c = device_c.pull().expect("C pull");
    assert!(
        summary_c.skipped_tombstones >= 6,
        "C skipped the six tombstones rather than inserting them (skipped={})",
        summary_c.skipped_tombstones
    );
    let store_c = Store::open(&db_c).expect("open C");
    for (table, id) in [
        ("custom_ideas", &ids.idea),
        ("note_links", &ids.link),
        ("lenses", &ids.lens),
        ("collection_memberships", &ids.membership),
        ("note_signals", &ids.n1),
    ] {
        assert!(
            store_c.get_row(table, id).unwrap().is_none(),
            "{table}/{id}: a delete for a row C never had must not be resurrected"
        );
    }

    let _ = std::fs::remove_file(&db_a);
    let _ = std::fs::remove_file(&db_b);
    let _ = std::fs::remove_file(&db_c);
}

#[test]
#[ignore = "needs a running local Supabase stack (CI `sync-integration` job, or a local `supabase start`)"]
fn offline_edit_rebased_away_by_a_newer_server_row_on_a_fanout_table() {
    // The SUR-736 outbox rebase, proven on a fan-out table (`lenses`) against a real server: B's
    // offline edit is superseded by A's newer edit and rebased out of the outbox — the next flush
    // can't re-push it. (Deterministic: A's edit is stamped strictly after B's via a short sleep.)
    let Some(env) = test_support::env() else {
        eprintln!("SUPABASE_URL unset — skipping the rebase-convergence integration test");
        return;
    };
    let user = test_support::mint_test_user_jwt(&env);
    let vault = Vault::generate();
    let lens = format!("rebase-lens-{}", user.user_id);

    let (device_a, db_a) = open_device(&env, &user, vault.clone(), "reb-a");
    let (device_b, db_b) = open_device(&env, &user, vault.clone(), "reb-b");

    // A creates the lens; B pulls it.
    device_a
        .enqueue_lens(
            lens.clone(),
            "orig".into(),
            vec!["a".into()],
            None,
            None,
            TS,
            false,
        )
        .unwrap();
    device_a.flush().expect("A flush orig");
    device_b.pull().expect("B pull orig");

    // B edits the lens OFFLINE (older), then — 2 ms later — A edits it and flushes (newer server row).
    device_b
        .enqueue_lens(
            lens.clone(),
            "B-stale".into(),
            vec!["b".into()],
            None,
            None,
            TS,
            false,
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    device_a
        .enqueue_lens(
            lens.clone(),
            "A-new".into(),
            vec!["c".into()],
            None,
            None,
            TS,
            false,
        )
        .unwrap();
    device_a.flush().expect("A flush new");

    // B syncs: the pull fetches A's newer lens, rebases B's stale outbox edit away + surfaces it,
    // and the following flush pushes nothing stale.
    let b_sync = device_b.sync().expect("B sync");
    assert!(
        b_sync
            .pull
            .superseded
            .iter()
            .any(|s| s.table == "lenses" && s.record_id == lens),
        "B's stale lens edit is surfaced as superseded"
    );
    let store_b = Store::open(&db_b).expect("open B");
    assert_eq!(
        store_b.get_row("lenses", &lens).unwrap().unwrap()["name"],
        json!("A-new"),
        "B converged onto A's newer lens (its stale edit was rebased away)"
    );
    assert!(
        store_b.outbox_items().unwrap().is_empty(),
        "the stale outbox entry was dropped — no re-push over the newer server row"
    );
    // The server still holds A's newer value (B's flush pushed nothing stale).
    let server = test_support::select(&env, &user.access_token, "lenses", &format!("id=eq.{lens}"));
    assert_eq!(
        server[0]["name"],
        json!("A-new"),
        "server row unchanged by B's sync"
    );

    let _ = std::fs::remove_file(&db_a);
    let _ = std::fs::remove_file(&db_b);
}

#[test]
#[ignore = "needs a running local Supabase stack (CI `sync-integration` job, or a local `supabase start`)"]
fn concurrent_membership_add_converges_to_one_row() {
    // Two devices independently adding the same note↔collection pair derive the SAME deterministic
    // id → the concurrent adds converge to ONE server row (SUR-737 OR-set add), no duplicate.
    let Some(env) = test_support::env() else {
        eprintln!("SUPABASE_URL unset — skipping the membership-convergence integration test");
        return;
    };
    let user = test_support::mint_test_user_jwt(&env);
    let vault = Vault::generate();
    let note = format!("mem-note-{}", user.user_id);
    let collection = format!("mem-col-{}", user.user_id);

    // A creates the parents (note + collection) so the membership FK is satisfiable, then flushes.
    let (device_a, db_a) = open_device(&env, &user, vault.clone(), "mem-a");
    device_a
        .enqueue_note(NoteUpsert {
            id: note.clone(),
            book_id: None,
            plaintext: "n".into(),
            page: None,
            tags: vec![],
            source: None,
            source_id: None,
            source_meta_json: None,
            chapter: None,
            image_path: None,
            ink_crop_path: None,
            created_at: TS,
            deleted: false,
            clear_nullable_fields: vec![],
        })
        .unwrap();
    device_a
        .enqueue_collection(collection.clone(), "C".into(), TS, false)
        .unwrap();
    device_a.flush().expect("A flush parents");

    // B pulls the parents, then BOTH devices add the same pair independently.
    let (device_b, db_b) = open_device(&env, &user, vault.clone(), "mem-b");
    device_b.pull().expect("B pull parents");
    device_a
        .enqueue_collection_membership(note.clone(), collection.clone(), TS, false)
        .unwrap();
    device_b
        .enqueue_collection_membership(note.clone(), collection.clone(), TS + 1, false)
        .unwrap();
    device_a.flush().expect("A flush membership");
    device_b.flush().expect("B flush membership");

    // Exactly one server row for that pair — the shared deterministic id collapsed the two adds.
    let rows = test_support::select(
        &env,
        &user.access_token,
        "collection_memberships",
        &format!("collection_id=eq.{collection}&note_id=eq.{note}"),
    );
    assert_eq!(
        rows.as_array().map(|a| a.len()),
        Some(1),
        "concurrent adds converged to ONE membership row: {rows}"
    );
    assert_eq!(
        rows[0]["id"],
        json!(membership_id(collection.clone(), note.clone()))
    );

    let _ = std::fs::remove_file(&db_a);
    let _ = std::fs::remove_file(&db_b);
}

#[test]
#[ignore = "needs a running local Supabase stack (CI `sync-integration` job, or a local `supabase start`)"]
fn export_import_parity_every_column_roundtrips_and_partial_edit_preserves_columns() {
    // Export/import parity (founder-ratified interpretation): a server row with EVERY column
    // populated (the PWA wire shape) pulls into the core with every descriptor column verbatim, and
    // a core PARTIAL edit + flush does not null the untouched server columns.
    let Some(env) = test_support::env() else {
        eprintln!("SUPABASE_URL unset — skipping the export/import parity integration test");
        return;
    };
    let user = test_support::mint_test_user_jwt(&env);
    let vault = Vault::generate();
    let uid = &user.user_id;
    let book = format!("full-book-{uid}");
    let note = format!("full-note-{uid}");
    let ciphertext = vault.encrypt_note(Some(note.clone()), "seed".into());

    // Seed FULL-column rows for book + note (the columns the partial FFI can't set) directly on the
    // server in the PWA wire shape, plus a fully-specified lens (COOCCUR + threshold 60).
    test_support::upsert(
        &env,
        &user.access_token,
        "books",
        "id",
        &json!([{
            "id": book, "user_id": uid, "title": "Full", "author": "A", "isbn": "978-0",
            "cover_url": "https://cover", "cover_source": "openlibrary", "cover_resolved_at": TS,
            "created_at": TS, "updated_at": TS, "deleted": false
        }]),
    );
    test_support::upsert(
        &env,
        &user.access_token,
        "notes",
        "id",
        &json!([{
            "id": note, "user_id": uid, "book_id": book, "text": ciphertext, "page": "38a",
            "tags": ["philosophy", "ethics"], "image_path": "img/x.png", "ink_crop_path": "ink/y.png",
            "source": "manual", "source_id": "src-1", "source_meta": { "k": "v" }, "chapter": "II",
            "content_tag": "deadbeef", "created_at": TS, "updated_at": TS, "deleted": false
        }]),
    );
    test_support::upsert(
        &env,
        &user.access_token,
        "lenses",
        "id",
        &json!([{
            "id": format!("full-lens-{uid}"), "user_id": uid, "name": "L",
            "leaf_ids": ["a", "b"], "combinator": "COOCCUR", "threshold": 60,
            "created_at": TS, "updated_at": TS, "deleted": false
        }]),
    );

    // A fresh core device pulls; every descriptor column round-trips verbatim.
    let (device, db) = open_device(&env, &user, vault.clone(), "parity");
    device.pull().expect("pull full rows");
    let store = Store::open(&db).expect("open store");

    let b = store.get_row("books", &book).unwrap().unwrap();
    assert_eq!(b["isbn"], json!("978-0"));
    assert_eq!(b["cover_url"], json!("https://cover"));
    assert_eq!(b["cover_source"], json!("openlibrary"));
    assert_eq!(b["cover_resolved_at"], json!(TS));

    let n = store.get_row("notes", &note).unwrap().unwrap();
    assert_eq!(
        n["tags"],
        json!(["philosophy", "ethics"]),
        "jsonb array verbatim"
    );
    assert_eq!(
        n["source_meta"],
        json!({ "k": "v" }),
        "jsonb object verbatim"
    );
    assert_eq!(n["image_path"], json!("img/x.png"));
    assert_eq!(n["ink_crop_path"], json!("ink/y.png"));
    assert_eq!(n["source_id"], json!("src-1"));
    assert_eq!(n["chapter"], json!("II"));
    assert_eq!(n["content_tag"], json!("deadbeef"));

    let l = store
        .get_row("lenses", &format!("full-lens-{uid}"))
        .unwrap()
        .unwrap();
    assert_eq!(l["leaf_ids"], json!(["a", "b"]), "text[] verbatim");
    assert_eq!(l["combinator"], json!("COOCCUR"));
    assert_eq!(l["threshold"], json!(60));

    // A PARTIAL local edit (rename the book — the FFI carries only id/title/author) + flush must NOT
    // null the untouched server columns (the upsert `merge-duplicates` patches only the sent fields).
    device
        .enqueue_book(BookUpsert {
            id: book.clone(),
            title: "Renamed".into(),
            author: Some("A".into()),
            isbn: None,
            cover_url: None,
            cover_source: None,
            cover_resolved_at: None,
            created_at: TS,
            deleted: false,
            clear_nullable_fields: vec![],
        })
        .expect("partial rename");
    device.flush().expect("flush partial edit");
    let server = test_support::select(&env, &user.access_token, "books", &format!("id=eq.{book}"));
    assert_eq!(
        server[0]["title"],
        json!("Renamed"),
        "edit applied on the server"
    );
    assert_eq!(
        server[0]["cover_url"],
        json!("https://cover"),
        "the untouched cover survived a partial edit (no null-out)"
    );

    let _ = std::fs::remove_file(&db);
}

#[test]
#[ignore = "needs a running local Supabase stack (CI `sync-integration` job, or a local `supabase start`)"]
fn native_authors_cover_and_source_metadata_to_the_server() {
    // SUR-741: the widened FFI can AUTHOR (not just preserve) the full column set. A native-created
    // book-with-cover + note-with-source-metadata flushes and lands on the server with those columns
    // populated — the capability the PWA had and native lacked before this ticket.
    let Some(env) = test_support::env() else {
        eprintln!("SUPABASE_URL unset — skipping the native-authoring integration test");
        return;
    };
    let user = test_support::mint_test_user_jwt(&env);
    let vault = Vault::generate();
    let uid = &user.user_id;
    let book = format!("auth-book-{uid}");
    let note = format!("auth-note-{uid}");

    let (device, db) = open_device(&env, &user, vault.clone(), "authoring");
    device
        .enqueue_book(BookUpsert {
            id: book.clone(),
            title: "Meditations".into(),
            author: Some("Aurelius".into()),
            isbn: Some("978-0140449334".into()),
            cover_url: Some("https://cover".into()),
            cover_source: Some("openlibrary".into()),
            cover_resolved_at: Some(TS),
            created_at: TS,
            deleted: false,
            clear_nullable_fields: vec![],
        })
        .expect("author book with cover");
    device
        .enqueue_note(NoteUpsert {
            id: note.clone(),
            book_id: Some(book.clone()),
            plaintext: "a highlighted line".into(),
            page: Some("12".into()),
            tags: vec!["stoicism".into()],
            source: Some("readwise".into()),
            source_id: Some("rw-42".into()),
            source_meta_json: Some(r#"{"highlight_id":"h1"}"#.into()),
            chapter: Some("On Anger".into()),
            image_path: Some("img/n.jpg".into()),
            ink_crop_path: Some("ink/n.jpg".into()),
            created_at: TS,
            deleted: false,
            clear_nullable_fields: vec![],
        })
        .expect("author note with source metadata");
    device.flush().expect("flush authored rows");

    // The server carries the authored columns — native AUTHORED them, not just round-trip-preserved.
    let sb = test_support::select(&env, &user.access_token, "books", &format!("id=eq.{book}"));
    assert_eq!(sb[0]["isbn"], json!("978-0140449334"));
    assert_eq!(sb[0]["cover_url"], json!("https://cover"));
    assert_eq!(sb[0]["cover_source"], json!("openlibrary"));
    assert_eq!(sb[0]["cover_resolved_at"], json!(TS));

    let sn = test_support::select(&env, &user.access_token, "notes", &format!("id=eq.{note}"));
    assert_eq!(sn[0]["source"], json!("readwise"));
    assert_eq!(sn[0]["source_id"], json!("rw-42"));
    assert_eq!(
        sn[0]["source_meta"],
        json!({ "highlight_id": "h1" }),
        "jsonb object authored"
    );
    assert_eq!(sn[0]["chapter"], json!("On Anger"));
    assert_eq!(sn[0]["image_path"], json!("img/n.jpg"));
    assert_eq!(sn[0]["ink_crop_path"], json!("ink/n.jpg"));
    // Seal-at-write still holds on the wire: text is enc:v2 ciphertext, never plaintext.
    let text = sn[0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("enc:v2:"),
        "authored note text is ciphertext on the server"
    );
    assert!(
        !text.contains("a highlighted line"),
        "plaintext never left the device"
    );

    let _ = std::fs::remove_file(&db);
}
