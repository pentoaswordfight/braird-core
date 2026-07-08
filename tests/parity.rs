//! Byte-for-byte parity harness against the frozen crypto-parity golden vectors
//! vendored from `surfc/main`. The JS WebCrypto implementation is the oracle; this
//! core must reproduce every in-scope vector bit-identically.
//!
//! Run with: `cargo test --features test-seams` (or `--all-features`, as CI does).
//! Gated on the feature so a plain `cargo test` — which lacks the determinism seams —
//! still compiles and passes.
#![cfg(feature = "test-seams")]

use braird_core::{Vault, WrappedBlob};
use serde_json::Value;

fn load(name: &str) -> Value {
    let path = format!(
        "{}/vendored/crypto-parity/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn hexb(v: &Value, key: &str) -> Vec<u8> {
    hex::decode(
        v[key]
            .as_str()
            .unwrap_or_else(|| panic!("missing hex field {key}")),
    )
    .unwrap_or_else(|e| panic!("bad hex in {key}: {e}"))
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("missing string field {key}"))
}

fn blob(v: &Value) -> WrappedBlob {
    WrappedBlob {
        wrapped_key: s(v, "wrappedKey").to_string(),
        iv: s(v, "iv").to_string(),
        salt: s(v, "salt").to_string(),
    }
}

fn assert_blob(got: &WrappedBlob, want: &Value, id: &str) {
    assert_eq!(got.wrapped_key, s(want, "wrappedKey"), "{id}: wrappedKey");
    assert_eq!(got.iv, s(want, "iv"), "{id}: iv");
    assert_eq!(got.salt, s(want, "salt"), "{id}: salt");
}

fn find<'a>(vectors: &'a [Value], id: &str) -> &'a Value {
    vectors
        .iter()
        .find(|v| v["id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("vector {id} not found"))
}

/// Drive every in-scope vector by its `op`. `legacy-note` is JS-only (the core neither
/// produces nor decrypts it — founder-confirmed out of scope), so it is skipped.
#[test]
fn golden_vectors_are_bit_identical() {
    let vectors = load("vectors.json");
    let vectors = vectors.as_array().expect("vectors.json is an array");

    let mut checked = 0u32;
    for v in vectors {
        let id = s(v, "id");
        let op = s(v, "op");
        let inp = &v["inputs"];
        let exp = &v["expected"];

        match op {
            "mk-wrap" => {
                let vault = Vault::__with_raw_mk_hex(s(inp, "mk")).unwrap();
                let got = vault.__wrap_with_prf_fixed(
                    &hexb(inp, "prf"),
                    &hexb(inp, "salt"),
                    &hexb(inp, "iv"),
                );
                assert_blob(&got, exp, id);
            }
            "mk-unwrap" => {
                let vault = Vault::unlock(hexb(inp, "prf"), blob(&inp["blob"])).unwrap();
                assert_eq!(vault.__raw_mk_hex(), s(exp, "mk"), "{id}: unwrapped MK");
            }
            "enc-v1" => {
                let vault = Vault::__with_raw_mk_hex(s(inp, "mk")).unwrap();
                let got = vault.__encrypt_note_fixed(None, s(inp, "plaintext"), &hexb(inp, "iv"));
                assert_eq!(got, s(exp, "ciphertext"), "{id}: enc-v1 ciphertext");
            }
            "enc-v2" => {
                let vault = Vault::__with_raw_mk_hex(s(inp, "mk")).unwrap();
                let got = vault.__encrypt_note_fixed(
                    Some(s(inp, "noteId")),
                    s(inp, "plaintext"),
                    &hexb(inp, "iv"),
                );
                assert_eq!(got, s(exp, "ciphertext"), "{id}: enc-v2 ciphertext");
            }
            "content-tag" => {
                let vault = Vault::__with_raw_mk_hex(s(inp, "mk")).unwrap();
                let got = vault.content_tag(
                    s(inp, "text").to_string(),
                    Some(s(inp, "bookId").to_string()),
                );
                assert_eq!(got, s(exp, "tag"), "{id}: content tag");
            }
            "pin-transfer" => {
                assert_eq!(
                    inp["iterations"].as_u64(),
                    Some(600_000),
                    "{id}: iteration count"
                );
                let vault = Vault::__with_raw_mk_hex(s(inp, "mk")).unwrap();
                let got =
                    vault.__pin_wrap_fixed(s(inp, "pin"), &hexb(inp, "salt"), &hexb(inp, "iv"));
                assert_blob(&got, exp, id);
            }
            "legacy-note" => continue, // JS-only, out of core scope
            other => panic!("{id}: unhandled op {other}"),
        }
        checked += 1;
    }

    // 19 in-scope (20 total − legacy-note): mk-wrap, mk-unwrap, 3×enc-v1, 3×enc-v2,
    // 10×content-tag (1 base + 9 normalization), pin-transfer.
    assert_eq!(
        checked, 19,
        "expected 19 in-scope vectors, checked {checked}"
    );
}

/// FOREIGN-ciphertext decrypt (SUR-716 / crypto-reviewer condition): unlock from a
/// JS-produced wrapped blob, then decrypt JS-produced ciphertext. Nothing here was
/// produced by this core — it proves PWA→native coexistence, not a self round-trip.
#[test]
fn decrypts_foreign_blob_and_ciphertext() {
    let vectors = load("vectors.json");
    let vectors = vectors.as_array().unwrap();
    let inputs = load("inputs.json");

    // The wrapped blob the JS oracle emitted in the mk-wrap vector.
    let foreign_blob = blob(&find(vectors, "mk-wrap")["expected"]);
    let prf = hexb(&inputs, "prf");
    let vault = Vault::unlock(prf, foreign_blob).expect("unlock foreign blob");

    let note_id = s(&inputs, "noteId");
    let expected_plain = inputs["plaintext"][2].as_str().unwrap(); // "café ☕ 日本語 …"

    // enc:v2 — the unicode plaintext, AAD-bound to the noteId.
    let ct_v2 = s(&find(vectors, "enc-v2[2]")["expected"], "ciphertext");
    let got_v2 = vault
        .decrypt_note(Some(note_id.to_string()), ct_v2.to_string())
        .expect("decrypt foreign enc:v2");
    assert_eq!(got_v2, expected_plain, "foreign enc:v2 plaintext");

    // enc:v1 — same plaintext, no AAD.
    let ct_v1 = s(&find(vectors, "enc-v1[2]")["expected"], "ciphertext");
    let got_v1 = vault
        .decrypt_note(None, ct_v1.to_string())
        .expect("decrypt foreign enc:v1");
    assert_eq!(got_v1, expected_plain, "foreign enc:v1 plaintext");

    // enc:v2 with the WRONG noteId must fail the auth-tag check (AAD binding).
    let wrong = vault.decrypt_note(Some("not-the-note".to_string()), ct_v2.to_string());
    assert!(wrong.is_err(), "enc:v2 must reject a mismatched noteId");
}

/// Round-trip the production (random-IV) API: generate → wrap → unlock → decrypt, and
/// the embedding seal. Exercises the non-deterministic paths the golden vectors can't.
#[test]
fn production_random_iv_roundtrips() {
    let vault = Vault::generate();

    let prf = b"a-fake-prf-output-32-bytes-long!".to_vec();
    let wrapped = vault.wrap_with_prf(prf.clone());
    let reopened = Vault::unlock(prf, wrapped).expect("unlock our own blob");

    let ct = vault.encrypt_note(Some("note-1".to_string()), "secret".to_string());
    let pt = reopened
        .decrypt_note(Some("note-1".to_string()), ct)
        .expect("cross-vault decrypt");
    assert_eq!(pt, "secret");

    // Both vaults hold the same MK, so content tags must agree across them.
    assert_eq!(
        vault.content_tag("Hello".to_string(), Some("b".to_string())),
        reopened.content_tag("Hello".to_string(), Some("b".to_string())),
    );

    let sealed = vault.seal_bytes(vec![1, 2, 3, 4], "note-1".to_string());
    let opened = vault
        .open_bytes(sealed, "note-1".to_string())
        .expect("open seal");
    assert_eq!(opened, vec![1, 2, 3, 4]);
}

/// SUR-812: `unlock_from_blobs` selects the right wrapper by TRIAL DECRYPT, not by position.
/// The production bug was a positional "first" pick: with ≥2 active prf-v1 wrappers it
/// throws unless the first row is the asserted credential's. Here three wrappers of ONE
/// MK under three distinct PRFs all recover the same MK regardless of list order; a
/// non-matching PRF and an empty list fail; a malformed candidate is skipped; and a
/// PWA-produced (foreign) wrapper is selected out of a decoy set (coexistence).
#[test]
fn unlock_from_blobs_selects_by_trial_decrypt() {
    let vault = Vault::generate();
    let probe = |v: &Vault| v.content_tag("probe".to_string(), Some("b".to_string()));
    let mk_tag = probe(&vault);

    // Three wrappers of the SAME MK under three distinct PRFs (multi-device rewrap).
    let prfs: [&[u8]; 3] = [b"prf-alpha", b"prf-bravo", b"prf-charlie"];
    let blobs: Vec<WrappedBlob> = prfs
        .iter()
        .map(|p| vault.wrap_with_prf(p.to_vec()))
        .collect();

    // Every PRF recovers the SAME MK, and the pick is order-independent: a reversed
    // candidate list (asserted credential no longer first) must still unlock.
    for (i, prf) in prfs.iter().enumerate() {
        for candidates in [
            blobs.clone(),
            blobs.iter().rev().cloned().collect::<Vec<_>>(),
        ] {
            let reopened = Vault::unlock_from_blobs(prf.to_vec(), candidates)
                .unwrap_or_else(|_| panic!("prf {i} must unlock via trial-decrypt"));
            assert_eq!(probe(&reopened), mk_tag, "prf {i}: same MK recovered");
        }
    }

    // A PRF matching no candidate fails; so does an empty candidate set.
    assert!(Vault::unlock_from_blobs(b"prf-none".to_vec(), blobs.clone()).is_err());
    assert!(Vault::unlock_from_blobs(prfs[0].to_vec(), vec![]).is_err());

    // A malformed candidate (non-base64) is SKIPPED, not fatal — a good blob still unlocks.
    let junk = WrappedBlob {
        wrapped_key: "!!not base64!!".into(),
        iv: "!!".into(),
        salt: "!!".into(),
    };
    Vault::unlock_from_blobs(prfs[1].to_vec(), vec![junk, blobs[1].clone()])
        .expect("skip malformed, unlock via the good blob");

    // Coexistence: a PWA-produced (foreign) prf-v1 wrapper, dropped among decoys, is
    // selected by trial-decrypt — no wire-format change, frozen constants untouched.
    let vectors = load("vectors.json");
    let vectors = vectors.as_array().unwrap();
    let inputs = load("inputs.json");
    let foreign = blob(&find(vectors, "mk-wrap")["expected"]);
    let decoys = vec![blobs[0].clone(), foreign, blobs[2].clone()];
    Vault::unlock_from_blobs(hexb(&inputs, "prf"), decoys)
        .expect("unlock_from_blobs selects the foreign PWA wrapper");
}
