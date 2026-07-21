//! SUR-976 integration: the cross-device `note_signals` orphan interleave, end to end on the
//! two-engine fixture (`tests/common`). Device A deletes note `n` (SUR-975 stages note + signals
//! tombstones atomically) and flushes; device B — which has NOT yet pulled — records a signal,
//! whose live row is stamped newer and legitimately overwrites A's signals tombstone in the cloud
//! (the server's own `t01_lww_guard` accepts equal-or-newer). B then pulls: the note tombstone
//! applies, but B's live signals row survives whole-row LWW — a live `note_signals` row for a
//! fleet-deleted note, which nothing retired before SUR-976's reconcile pass.
//!
//! OFFLINE + deterministic (no Supabase, no env guard, NOT `#[ignore]`d), like
//! `sync_736_integration.rs`.
#![cfg(not(target_arch = "wasm32"))]

mod common;

use braird_core::sync::{pull_then_flush, push, NoteSignalKind, NoteUpsert};
use common::{block, tick, Device, SharedCloud};
use serde_json::json;

const USER: &str = "user-1";
const TABLES: &[&str] = &["notes", "note_signals"];

fn note_upsert(id: &str, plaintext: &str, deleted: bool) -> NoteUpsert {
    NoteUpsert {
        id: id.into(),
        book_id: None,
        plaintext: Some(plaintext.into()),
        page: None,
        tags: vec![],
        source: Some("manual".into()),
        source_id: None,
        source_meta_json: None,
        chapter: None,
        image_path: None,
        ink_crop_path: None,
        created_at: 1,
        deleted,
        clear_nullable_fields: vec![],
    }
}

fn sync(device: &Device, cloud: &SharedCloud) {
    block(pull_then_flush(
        &device.store,
        cloud,
        USER,
        TABLES,
        &device.vault,
    ))
    .expect("clean pull_then_flush");
}

#[test]
fn a_fleet_deleted_notes_live_signals_row_converges_to_a_tombstone_on_every_device() {
    let vault = braird_core::Vault::generate();
    let cloud = SharedCloud::new();
    let a = Device::new(vault.clone());
    let b = Device::new(vault.clone());

    // Seed: A authors note `n` and pushes it; B pulls it live (so B's visibility guard passes).
    a.engine
        .enqueue_note(note_upsert("n", "the passage", false))
        .unwrap();
    sync(&a, &cloud);
    sync(&b, &cloud);
    assert_eq!(
        b.store.get_row("notes", "n").unwrap().unwrap()["deleted"],
        json!(false),
        "precondition: B holds the note live"
    );

    // A deletes: SUR-975 stages the note tombstone + signals tombstone in one transaction; the
    // flush pushes both at tA.
    tick();
    a.engine.enqueue_note(note_upsert("n", "", true)).unwrap();
    sync(&a, &cloud);
    assert_eq!(
        cloud.row("note_signals", "n").unwrap()["deleted"],
        json!(true),
        "precondition: A's signals tombstone reached the cloud"
    );

    // B, not yet pulled, records a signal at tB > tA: its local note is still live so the
    // visibility guard passes, and the flush's live row overwrites A's tombstone in the cloud —
    // the server's t01_lww_guard accepts equal-or-newer. This is the door, not a fixture bug.
    tick();
    assert!(b
        .engine
        .record_note_signal("n".into(), NoteSignalKind::ReturnVisit)
        .unwrap());
    block(push::flush(&b.store, &cloud, USER)).expect("B flush");
    assert_eq!(
        cloud.row("note_signals", "n").unwrap()["deleted"],
        json!(false),
        "precondition: B's live signals row won the cloud row back from A's tombstone"
    );

    // B pulls: the note tombstone (tA) beats B's older live note; B's own signals row (tB) is
    // already local, so LWW leaves it. Pre-SUR-976 this left a live signals row for a
    // fleet-deleted note forever. The reconcile pass inside this same pull_then_flush must retire
    // it — and flush the retirement back to the cloud in the same call.
    tick();
    sync(&b, &cloud);
    assert_eq!(
        b.store.get_row("notes", "n").unwrap().unwrap()["deleted"],
        json!(true),
        "B applied the note tombstone"
    );
    let sig = b.store.get_row("note_signals", "n").unwrap().unwrap();
    assert_eq!(
        sig["deleted"],
        json!(true),
        "a live note_signals row must not survive a fleet-deleted parent note (SUR-976)"
    );
    assert_eq!(
        sig["return_visits"],
        json!(1),
        "the retirement preserves B's earned counters on the tombstone"
    );
    assert_eq!(
        cloud.row("note_signals", "n").unwrap()["deleted"],
        json!(true),
        "the retirement propagated: the CLOUD row converged in the same pull_then_flush"
    );

    // A pulls: the retirement tombstone (newer than A's own) lands — the whole fleet converges.
    tick();
    sync(&a, &cloud);
    assert_eq!(
        a.store.get_row("note_signals", "n").unwrap().unwrap()["deleted"],
        json!(true),
        "A converges to the retirement tombstone"
    );
}

#[test]
fn a_device_that_pulls_before_signalling_never_creates_the_orphan() {
    // The good path, pinned so the fix is provably not load-bearing for it: B pulls FIRST, the
    // visibility guard refuses the late signal, and no live row exists anywhere to retire.
    let vault = braird_core::Vault::generate();
    let cloud = SharedCloud::new();
    let a = Device::new(vault.clone());
    let b = Device::new(vault.clone());

    a.engine
        .enqueue_note(note_upsert("n", "the passage", false))
        .unwrap();
    sync(&a, &cloud);
    sync(&b, &cloud);
    tick();
    a.engine.enqueue_note(note_upsert("n", "", true)).unwrap();
    sync(&a, &cloud);

    tick();
    sync(&b, &cloud); // B pulls the tombstones BEFORE any signal fires.
    assert!(
        !b.engine
            .record_note_signal("n".into(), NoteSignalKind::ReturnVisit)
            .unwrap(),
        "the visibility guard refuses a signal on the pulled note tombstone"
    );
    // B never signalled, so it holds NO local signals row — and the pull tombstone-skip
    // (pull.rs: `incoming_deleted && no local row` → skip) correctly declines to materialize
    // A's signals tombstone as a needless dead row. Absent, not tombstoned, is the good state.
    assert!(
        b.store.get_row("note_signals", "n").unwrap().is_none(),
        "no dead signals row is materialized on a device that never signalled"
    );
    assert_eq!(
        cloud.row("note_signals", "n").unwrap()["deleted"],
        json!(true),
        "the cloud row was never overwritten live"
    );
}
