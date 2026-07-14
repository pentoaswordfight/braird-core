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

    /// SUR-812: unlockFromBlobs picks the wrapper that decrypts out of many, over the FFI. Two
    /// wrappers of one MK under two PRFs → unlockFromBlobs with the asserted PRF recovers it even
    /// when that wrapper is NOT first in the list (a positional pick would fail); a
    /// non-matching PRF throws.
    func testUnlockFromBlobsSelectsMatchingWrapper() throws {
        let vault = Vault.generate()
        let prfA = Data(repeating: 0x0A, count: 32)
        let prfB = Data(repeating: 0x0B, count: 32)
        let blobA = vault.wrapWithPrf(prf: prfA)
        let blobB = vault.wrapWithPrf(prf: prfB)

        // Asserted credential (A) is second in the list — trial-decrypt still finds it.
        let reopened = try Vault.unlockFromBlobs(prf: prfA, blobs: [blobB, blobA])
        let ct = vault.encryptNote(noteId: "note-1", plaintext: "secret 🔐")
        XCTAssertEqual(try reopened.decryptNote(noteId: "note-1", ciphertext: ct), "secret 🔐")

        XCTAssertThrowsError(
            try Vault.unlockFromBlobs(prf: Data(repeating: 0x0C, count: 32), blobs: [blobA, blobB]))
    }

    /// SUR-741: the widened enqueue surface crosses the FFI, and source_meta_json validation
    /// (which runs in Rust) surfaces as a thrown error on the Swift side.
    func testEnqueueNoteWidenedFieldsOverFfi() throws {
        let db = FileManager.default.temporaryDirectory
            .appendingPathComponent("braird-rt-\(UUID().uuidString).sqlite")
        let engine = try SyncEngine.open(
            dbPath: db.path, supabaseUrl: "https://x.supabase.co", anonKey: "anon",
            vault: Vault.generate())
        try engine.enqueueNote(draft: NoteUpsert(
            id: "n1", bookId: "b1", plaintext: "secret", page: "5", tags: ["philosophy"],
            source: "readwise", sourceId: "rw-1", sourceMetaJson: "{\"highlight_id\":\"h1\"}",
            chapter: "1", imagePath: "img/1.jpg", inkCropPath: nil, createdAt: 0, deleted: false,
            clearNullableFields: []))
        XCTAssertThrowsError(
            try engine.enqueueNote(draft: NoteUpsert(
                id: "n2", bookId: nil, plaintext: "x", page: nil, tags: [],
                source: nil, sourceId: nil, sourceMetaJson: "not json", chapter: nil,
                imagePath: nil, inkCropPath: nil, createdAt: 0, deleted: false,
                clearNullableFields: [])))
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

        try engine.enqueueBook(draft: BookUpsert(
            id: "b1", title: "Meditations", author: "Aurelius", isbn: nil, coverUrl: nil,
            coverSource: nil, coverResolvedAt: nil, createdAt: 1, deleted: false,
            clearNullableFields: []))
        try engine.enqueueNote(draft: NoteUpsert(
            id: "n1", bookId: "b1", plaintext: "the unexamined life is not worth living",
            page: nil, tags: ["philosophy"], source: nil, sourceId: nil, sourceMetaJson: nil,
            chapter: nil, imagePath: nil, inkCropPath: nil, createdAt: 10, deleted: false,
            clearNullableFields: []))
        try engine.enqueueNote(draft: NoteUpsert(
            id: "n2", bookId: nil, plaintext: "running toward the good", page: nil, tags: [],
            source: nil, sourceId: nil, sourceMetaJson: nil, chapter: nil, imagePath: nil,
            inkCropPath: nil, createdAt: 20, deleted: false,
            clearNullableFields: []))
        try engine.enqueueCustomIdea(
            id: "i1", name: "Antifragility", description: "gains from disorder",
            createdAt: 5, deleted: false)

        let counts = try engine.counts()
        XCTAssertEqual(counts.books, 1)
        XCTAssertEqual(counts.notes, 2)
        XCTAssertEqual(counts.customIdeas, 1)
        XCTAssertEqual(counts.activeIdeas, 1)  // n1 tagged "philosophy"; n2 untagged → 1 distinct

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

    /// SUR-806: the Home-surface reads — the rolling-7-day `notesThisWeek`, the random "Recently
    /// surfaced" pick, and the `activeIdeas` tag count — each decrypting in core and crossing the
    /// binding as plaintext (never an `enc:` sentinel), exactly as an iOS host consumes them.
    func testHomeSurfaceQueriesOverFfi() throws {
        let db = FileManager.default.temporaryDirectory
            .appendingPathComponent("braird-home-\(UUID().uuidString).sqlite")
        let engine = try SyncEngine.open(
            dbPath: db.path, supabaseUrl: "https://x.supabase.co", anonKey: "anon",
            vault: Vault.generate())

        let now: Int64 = 1_700_000_000_000
        let weekMs: Int64 = 7 * 24 * 60 * 60 * 1000
        try engine.enqueueNote(draft: NoteUpsert(
            id: "fresh", bookId: nil, plaintext: "surfaced this week", page: nil,
            tags: ["philosophy"], source: nil, sourceId: nil, sourceMetaJson: nil, chapter: nil,
            imagePath: nil, inkCropPath: nil, createdAt: now - 1000, deleted: false,
            clearNullableFields: []))
        try engine.enqueueNote(draft: NoteUpsert(
            id: "old", bookId: nil, plaintext: "last month", page: nil, tags: ["ethics"],
            source: nil, sourceId: nil, sourceMetaJson: nil, chapter: nil, imagePath: nil,
            inkCropPath: nil, createdAt: now - weekMs - 1000, deleted: false,
            clearNullableFields: []))

        // Only the in-window note counts; the pick is it, decrypted to plaintext across the FFI.
        XCTAssertEqual(try engine.notesThisWeek(nowMs: now), 1)
        let recent = try engine.recentNote(nowMs: now, seed: 0)
        XCTAssertEqual(recent?.id, "fresh")
        XCTAssertEqual(recent?.text, "surfaced this week")
        XCTAssertFalse((recent?.text ?? "").hasPrefix("enc:v"))

        // active_ideas = distinct tags over ALL live notes (window-independent): philosophy, ethics.
        XCTAssertEqual(try engine.counts().activeIdeas, 2)
    }

    /// SUR-858: the organise reads over the FFI — notes-by-idea, per-idea counts, the
    /// collections/lenses lists, and the untagged work queue. Proves notes decrypt to plaintext
    /// across the binding (notesByIdea/untaggedNotes), the ideaCounts tally matches the PWA oracle,
    /// and the two new stores' first read paths map their rows, as an iOS host consumes them.
    func testOrganiseReadsOverFfi() throws {
        let db = FileManager.default.temporaryDirectory
            .appendingPathComponent("braird-org-\(UUID().uuidString).sqlite")
        let engine = try SyncEngine.open(
            dbPath: db.path, supabaseUrl: "https://x.supabase.co", anonKey: "anon",
            vault: Vault.generate())

        try engine.enqueueNote(draft: NoteUpsert(
            id: "n1", bookId: nil, plaintext: "the unexamined life", page: nil,
            tags: ["philosophy", "ethics"], source: nil, sourceId: nil, sourceMetaJson: nil,
            chapter: nil, imagePath: nil, inkCropPath: nil, createdAt: 10, deleted: false,
            clearNullableFields: []))
        try engine.enqueueNote(draft: NoteUpsert(
            id: "n2", bookId: nil, plaintext: "on stoicism", page: nil, tags: ["philosophy"],
            source: nil, sourceId: nil, sourceMetaJson: nil, chapter: nil, imagePath: nil,
            inkCropPath: nil, createdAt: 20, deleted: false, clearNullableFields: []))
        try engine.enqueueNote(draft: NoteUpsert(
            id: "loose", bookId: nil, plaintext: "untagged thought", page: nil, tags: [],
            source: nil, sourceId: nil, sourceMetaJson: nil, chapter: nil, imagePath: nil,
            inkCropPath: nil, createdAt: 30, deleted: false, clearNullableFields: []))
        try engine.enqueueCollection(id: "c1", name: "Reading list", createdAt: 5, deleted: false)
        try engine.enqueueLens(
            id: "l1", name: "Stoic core", leafIds: ["philosophy", "ethics"], combinator: "OR",
            threshold: 75, createdAt: 6, deleted: false)

        // notes-by-idea: newest-first, decrypted plaintext, never an enc: sentinel.
        let philosophy = try engine.notesByIdea(idea: "philosophy", limit: 50, offset: 0)
        XCTAssertEqual(philosophy.map { $0.id }, ["n2", "n1"])
        XCTAssertEqual(philosophy[0].text, "on stoicism")
        for n in philosophy { XCTAssertFalse((n.text ?? "").hasPrefix("enc:v")) }

        // idea_counts: per-occurrence tally, idea-asc, present-tags-only.
        let counts = try engine.ideaCounts().map { [$0.idea: $0.count] }
        XCTAssertEqual(counts, [["ethics": 1], ["philosophy": 2]])

        // untagged queue + badge count.
        XCTAssertEqual(try engine.untaggedNotes(limit: 50, offset: 0).map { $0.id }, ["loose"])
        XCTAssertEqual(try engine.untaggedNotes(limit: 50, offset: 0)[0].text, "untagged thought")
        XCTAssertEqual(try engine.untaggedNotesCount(), 1)

        // collections + lenses first read paths.
        XCTAssertEqual(try engine.listCollections(limit: 50, offset: 0).map { $0.name }, ["Reading list"])
        let lens = try engine.listLenses(limit: 50, offset: 0)[0]
        XCTAssertEqual(lens.name, "Stoic core")
        XCTAssertEqual(lens.leafIds, ["philosophy", "ethics"])
        XCTAssertEqual(lens.combinator, "OR")
        XCTAssertEqual(lens.threshold, 75)
    }
}
