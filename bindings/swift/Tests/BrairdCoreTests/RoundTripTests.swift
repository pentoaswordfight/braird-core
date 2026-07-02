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

    /// SUR-744: the read/query surface over the FFI — list/get/counts/search against a populated
    /// store. Proves note text crosses the binding as decrypted PLAINTEXT (never an `enc:` sentinel,
    /// AC #2), the Library note-count badge, newest-first ordering, and lexical-search parity
    /// (stemming, doc-kind typing) exactly as an iOS host consumes them.
    func testReadAndSearchSurfaceOverFfi() throws {
        let db = FileManager.default.temporaryDirectory
            .appendingPathComponent("braird-rt-\(UUID().uuidString).sqlite")
        let engine = try SyncEngine.open(
            dbPath: db.path, supabaseUrl: "https://x.supabase.co", anonKey: "anon",
            vault: Vault.generate())

        try engine.enqueueBook(
            id: "b1", title: "Meditations", author: "Aurelius", isbn: nil, coverUrl: nil,
            coverSource: nil, coverResolvedAt: nil, createdAt: 1, deleted: false)
        try engine.enqueueNote(
            id: "n1", bookId: "b1", plaintext: "the unexamined life is not worth living",
            page: nil, tags: ["philosophy"], source: nil, sourceId: nil, sourceMetaJson: nil,
            chapter: nil, imagePath: nil, inkCropPath: nil, createdAt: 10, deleted: false)
        try engine.enqueueNote(
            id: "n2", bookId: nil, plaintext: "running toward the good", page: nil, tags: [],
            source: nil, sourceId: nil, sourceMetaJson: nil, chapter: nil, imagePath: nil,
            inkCropPath: nil, createdAt: 20, deleted: false)
        try engine.enqueueCustomIdea(
            id: "i1", name: "Antifragility", description: "gains from disorder",
            createdAt: 5, deleted: false)

        let counts = try engine.counts()
        XCTAssertEqual(counts.books, 1)
        XCTAssertEqual(counts.notes, 2)
        XCTAssertEqual(counts.customIdeas, 1)

        // Library grid: the book carries its live note count.
        let books = try engine.listBooks(limit: 50, offset: 0)
        XCTAssertEqual(books.count, 1)
        XCTAssertEqual(books[0].title, "Meditations")
        XCTAssertEqual(books[0].noteCount, 1)

        // Commonplace flat list: newest-first, decrypted plaintext, never a ciphertext sentinel.
        let all = try engine.listNotes(bookId: nil, limit: 50, offset: 0)
        XCTAssertEqual(all.map { $0.id }, ["n2", "n1"])
        XCTAssertEqual(all[1].text, "the unexamined life is not worth living")
        XCTAssertFalse(all[1].decryptFailed)
        for n in all { XCTAssertFalse((n.text ?? "").hasPrefix("enc:v")) }

        // Per-book filter + single-note fetch.
        XCTAssertEqual(try engine.listNotes(bookId: "b1", limit: 50, offset: 0).map { $0.id }, ["n1"])
        let n1 = try engine.getNote(id: "n1")
        XCTAssertEqual(n1?.text, "the unexamined life is not worth living")
        XCTAssertEqual(n1?.tags, ["philosophy"])

        // AddIdeaSheet "Your Ideas".
        XCTAssertEqual(try engine.listCustomIdeas(limit: 50, offset: 0).map { $0.name }, ["Antifragility"])

        // Lexical search: stemming (running ⇄ run) hits the note; idea by name; miss returns [].
        let runHits = try engine.search(query: "run", limit: 10)
        XCTAssertTrue(runHits.contains { $0.refId == "n2" && $0.kind == .note })
        let ideaHits = try engine.search(query: "antifragility", limit: 10)
        XCTAssertTrue(ideaHits.contains { $0.refId == "i1" && $0.kind == .idea })
        XCTAssertTrue(try engine.search(query: "zzznomatch", limit: 10).isEmpty)
    }
}
