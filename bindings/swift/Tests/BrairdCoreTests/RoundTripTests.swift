import BrairdCore
import Foundation
import XCTest

/// Swift round-trip parity over the FFI. Proves the generated Swift binding decrypts
/// FOREIGN (JS-produced) ciphertext and reproduces the deterministic content tags
/// byte-for-byte, plus a production random-IV round-trip. Uses ONLY the public binding
/// (no determinism seams), so it exercises exactly what an iOS host gets.
final class RoundTripTests: XCTestCase {
    /// Repo root, derived from this source file so the single vendored fixture set is
    /// read directly (no duplication, no drift):
    /// bindings/swift/Tests/BrairdCoreTests/RoundTripTests.swift → up 5 → repo root.
    private func repoRoot() -> URL {
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { url.deleteLastPathComponent() }
        return url
    }

    private func fixture(_ name: String) throws -> Any {
        let url = repoRoot().appendingPathComponent("vendored/crypto-parity/\(name)")
        return try JSONSerialization.jsonObject(with: Data(contentsOf: url))
    }

    private func hexData(_ s: String) -> Data {
        var bytes = Data(capacity: s.count / 2)
        var i = s.startIndex
        while i < s.endIndex {
            let j = s.index(i, offsetBy: 2)
            bytes.append(UInt8(s[i..<j], radix: 16)!)
            i = j
        }
        return bytes
    }

    func testForeignDecryptAndContentTags() throws {
        let vectors = try fixture("vectors.json") as! [[String: Any]]
        let inputs = try fixture("inputs.json") as! [String: Any]
        func vector(_ id: String) -> [String: Any] {
            vectors.first { ($0["id"] as? String) == id }!
        }
        func expected(_ id: String) -> [String: Any] { vector(id)["expected"] as! [String: Any] }

        // Unlock a JS-produced wrapped blob → recovers the frozen MK (0x11*32).
        let wrap = expected("mk-wrap")
        let blob = WrappedBlob(
            wrappedKey: wrap["wrappedKey"] as! String,
            iv: wrap["iv"] as! String,
            salt: wrap["salt"] as! String
        )
        let prf = hexData(inputs["prf"] as! String)
        let vault = try Vault.unlock(prf: prf, blob: blob)

        // Decrypt JS-produced ciphertext (PWA→native coexistence).
        let plain = (inputs["plaintext"] as! [String])[2]
        let noteId = inputs["noteId"] as! String
        let ctV2 = expected("enc-v2[2]")["ciphertext"] as! String
        XCTAssertEqual(try vault.decryptNote(noteId: noteId, ciphertext: ctV2), plain)
        let ctV1 = expected("enc-v1[2]")["ciphertext"] as! String
        XCTAssertEqual(try vault.decryptNote(noteId: nil, ciphertext: ctV1), plain)
        XCTAssertThrowsError(try vault.decryptNote(noteId: "wrong", ciphertext: ctV2))

        // Content tags are deterministic (no IV) → byte-equal via the production API.
        var tagsChecked = 0
        for v in vectors where (v["op"] as? String) == "content-tag" {
            let inp = v["inputs"] as! [String: Any]
            let exp = v["expected"] as! [String: Any]
            let tag = vault.contentTag(text: inp["text"] as! String, bookId: inp["bookId"] as? String)
            XCTAssertEqual(tag, exp["tag"] as! String, "content-tag \(v["id"]!)")
            tagsChecked += 1
        }
        XCTAssertEqual(tagsChecked, 10, "expected 10 content-tag vectors")
    }

    func testProductionRoundTrip() throws {
        let vault = Vault.generate()
        let prf = Data(repeating: 0x07, count: 32)
        let wrapped = vault.wrapWithPrf(prf: prf)
        let reopened = try Vault.unlock(prf: prf, blob: wrapped)

        let ct = vault.encryptNote(noteId: "note-1", plaintext: "secret 🔐")
        XCTAssertEqual(try reopened.decryptNote(noteId: "note-1", ciphertext: ct), "secret 🔐")

        XCTAssertEqual(
            vault.contentTag(text: "Hello", bookId: "b"),
            reopened.contentTag(text: "Hello", bookId: "b")
        )

        let sealed = vault.sealBytes(bytes: Data([1, 2, 3, 4]), aad: "note-1")
        XCTAssertEqual(try vault.openBytes(sealed: sealed, aad: "note-1"), Data([1, 2, 3, 4]))
    }

    /// SUR-741: the widened enqueue surface crosses the FFI, and source_meta_json validation
    /// (which runs in Rust) surfaces as a thrown error on the Swift side.
    func testEnqueueNoteWidenedFieldsOverFfi() throws {
        let db = FileManager.default.temporaryDirectory
            .appendingPathComponent("braird-rt-\(UUID().uuidString).sqlite")
        let engine = try SyncEngine.open(
            dbPath: db.path, supabaseUrl: "https://x.supabase.co", anonKey: "anon",
            vault: Vault.generate())
        try engine.enqueueNote(
            id: "n1", bookId: "b1", plaintext: "secret", page: "5", tags: ["philosophy"],
            source: "readwise", sourceId: "rw-1", sourceMetaJson: "{\"highlight_id\":\"h1\"}",
            chapter: "1", imagePath: "img/1.jpg", inkCropPath: nil, createdAt: 0, deleted: false)
        XCTAssertThrowsError(
            try engine.enqueueNote(
                id: "n2", bookId: nil, plaintext: "x", page: nil, tags: [],
                source: nil, sourceId: nil, sourceMetaJson: "not json", chapter: nil,
                imagePath: nil, inkCropPath: nil, createdAt: 0, deleted: false))
    }
}
