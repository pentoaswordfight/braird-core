import BrairdCore
import Foundation
import Network
import XCTest

private enum LoopbackServerError: Error {
    case startupTimedOut
    case startupFailed
    case missingPort
}

/// Minimal HTTP/1.1 loopback endpoint for native import preflight. Every request receives an
/// empty JSON array, which is the real PostgREST shape for an empty pull/direct-id fetch.
private final class EmptyJSONLoopbackServer {
    private let listener: NWListener
    private let queue = DispatchQueue(label: "braird.snapshot.loopback")
    private let started = DispatchSemaphore(value: 0)
    private let lock = NSLock()
    private var startupFailed = false
    private var requests = 0

    var requestCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return requests
    }

    var baseURL: String {
        "http://127.0.0.1:\(listener.port!.rawValue)"
    }

    init() throws {
        let parameters = NWParameters.tcp
        parameters.requiredLocalEndpoint = .hostPort(
            host: NWEndpoint.Host("127.0.0.1"),
            port: NWEndpoint.Port.any)
        listener = try NWListener(using: parameters)
        listener.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
                self.started.signal()
            case .failed(_):
                self.lock.lock()
                self.startupFailed = true
                self.lock.unlock()
                self.started.signal()
            default:
                break
            }
        }
        listener.newConnectionHandler = { [weak self] connection in
            self?.accept(connection)
        }
        listener.start(queue: queue)

        guard started.wait(timeout: .now() + 5) == .success else {
            listener.cancel()
            throw LoopbackServerError.startupTimedOut
        }
        lock.lock()
        let failed = startupFailed
        lock.unlock()
        guard !failed else {
            listener.cancel()
            throw LoopbackServerError.startupFailed
        }
        guard listener.port != nil else {
            listener.cancel()
            throw LoopbackServerError.missingPort
        }
    }

    func stop() {
        listener.cancel()
    }

    private func accept(_ connection: NWConnection) {
        connection.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                connection.stateUpdateHandler = nil
                self?.receiveAndRespond(connection)
            case .failed(_), .cancelled:
                connection.stateUpdateHandler = nil
                connection.cancel()
            default:
                break
            }
        }
        connection.start(queue: queue)
    }

    private func receiveAndRespond(_ connection: NWConnection) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] data, _, isComplete, error in
            guard let self else {
                connection.cancel()
                return
            }
            guard let data, !data.isEmpty else {
                if error == nil && !isComplete {
                    self.receiveAndRespond(connection)
                } else {
                    connection.cancel()
                }
                return
            }

            self.lock.lock()
            self.requests += 1
            self.lock.unlock()
            let responseText =
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n" +
                "Connection: close\r\n\r\n[]"
            let response = Data(responseText.utf8)
            connection.send(content: response, completion: .contentProcessed { _ in
                connection.cancel()
            })
        }
    }
}

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

    private func snapshotFixture() throws -> String {
        let url = repoRoot()
            .appendingPathComponent("vendored/snapshot-parity/schema-19-all-stores.json")
        return try String(contentsOf: url, encoding: .utf8)
    }

    private func testJWT() -> String {
        let payload = Data(#"{"sub":"snapshot-host-test"}"#.utf8)
            .base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
        return "h.\(payload).sig"
    }

    private func assertImportCounts(_ counts: ImportCounts, notes: UInt32) {
        XCTAssertEqual(counts.books, 1)
        XCTAssertEqual(counts.notes, notes)
        XCTAssertEqual(counts.customIdeas, 1)
        XCTAssertEqual(counts.noteLinks, 1)
        XCTAssertEqual(counts.lenses, 1)
        XCTAssertEqual(counts.collections, 1)
        XCTAssertEqual(counts.collectionMemberships, 1)
        XCTAssertEqual(counts.noteSignals, 1)
    }

    private func assertZeroImportCounts(_ counts: ImportCounts) {
        XCTAssertEqual(counts.books, 0)
        XCTAssertEqual(counts.notes, 0)
        XCTAssertEqual(counts.customIdeas, 0)
        XCTAssertEqual(counts.noteLinks, 0)
        XCTAssertEqual(counts.lenses, 0)
        XCTAssertEqual(counts.collections, 0)
        XCTAssertEqual(counts.collectionMemberships, 0)
        XCTAssertEqual(counts.noteSignals, 0)
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

    /// SUR-921: nil plaintext reaches the existing-row patch path, so a Vault that cannot
    /// decrypt the note can still retag it without replacing its ciphertext.
    func testTagsOnlyNotePatchOverFfi() throws {
        let db = FileManager.default.temporaryDirectory
            .appendingPathComponent("braird-rt-\(UUID().uuidString).sqlite")
        let vaultA = Vault.generate()
        let writer = try SyncEngine.open(
            dbPath: db.path, supabaseUrl: "https://x.supabase.co", anonKey: "anon",
            vault: vaultA)
        try writer.enqueueNote(draft: NoteUpsert(
            id: "n1", bookId: nil, plaintext: "secret from vault A", page: nil,
            tags: ["before"], source: "kindle", sourceId: nil, sourceMetaJson: nil,
            chapter: nil, imagePath: nil, inkCropPath: nil, createdAt: 10, deleted: false,
            clearNullableFields: []))

        let foreign = try SyncEngine.open(
            dbPath: db.path, supabaseUrl: "https://x.supabase.co", anonKey: "anon",
            vault: Vault.generate())
        let before = try XCTUnwrap(try foreign.getNote(id: "n1"))
        XCTAssertTrue(before.decryptFailed)
        XCTAssertNil(before.text)

        try foreign.enqueueNote(draft: NoteUpsert(
            id: "n1", bookId: nil, plaintext: nil, page: nil, tags: ["after"],
            source: nil, sourceId: nil, sourceMetaJson: nil, chapter: nil, imagePath: nil,
            inkCropPath: nil, createdAt: 999, deleted: false, clearNullableFields: []))
        let stillForeign = try XCTUnwrap(try foreign.getNote(id: "n1"))
        XCTAssertTrue(stillForeign.decryptFailed)
        XCTAssertEqual(stillForeign.tags, ["after"])

        let reader = try SyncEngine.open(
            dbPath: db.path, supabaseUrl: "https://x.supabase.co", anonKey: "anon",
            vault: vaultA)
        let recovered = try XCTUnwrap(try reader.getNote(id: "n1"))
        XCTAssertFalse(recovered.decryptFailed)
        XCTAssertEqual(recovered.text, "secret from vault A")
        XCTAssertEqual(recovered.tags, ["after"])
        XCTAssertEqual(recovered.source, "kindle")
        XCTAssertEqual(recovered.createdAt, 10)

        XCTAssertThrowsError(
            try foreign.enqueueNote(draft: NoteUpsert(
                id: "missing", bookId: nil, plaintext: nil, page: nil, tags: ["after"],
                source: nil, sourceId: nil, sourceMetaJson: nil, chapter: nil, imagePath: nil,
                inkCropPath: nil, createdAt: 999, deleted: false, clearNullableFields: []))
        ) { error in
            XCTAssertEqual(error as? SyncError, SyncError.PatchTargetMissing)
        }
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

    /// SUR-923: the relation reads over the FFI — memberships traversed in both directions,
    /// note-link edges (both endpoints), and the per-collection live-note counts (which join live
    /// notes by founder decision, while note-ids stays join-free for the delete cascade).
    func testRelationReadsOverFfi() throws {
        let db = FileManager.default.temporaryDirectory
            .appendingPathComponent("braird-rel-\(UUID().uuidString).sqlite")
        let engine = try SyncEngine.open(
            dbPath: db.path, supabaseUrl: "https://x.supabase.co", anonKey: "anon",
            vault: Vault.generate())

        func note(_ id: String, _ createdAt: Int64, deleted: Bool = false) -> NoteUpsert {
            NoteUpsert(
                id: id, bookId: nil, plaintext: "text-\(id)", page: nil, tags: [],
                source: nil, sourceId: nil, sourceMetaJson: nil, chapter: nil, imagePath: nil,
                inkCropPath: nil, createdAt: createdAt, deleted: deleted, clearNullableFields: [])
        }
        try engine.enqueueNote(draft: note("n1", 10))
        try engine.enqueueNote(draft: note("n2", 20))
        try engine.enqueueNote(draft: note("ndead", 30, deleted: true))

        try engine.enqueueCollection(id: "beta", name: "Beta", createdAt: 1, deleted: false)
        try engine.enqueueCollectionMembership(noteId: "n1", collectionId: "beta", createdAt: 100, deleted: false)
        try engine.enqueueCollectionMembership(noteId: "n2", collectionId: "beta", createdAt: 200, deleted: false)
        try engine.enqueueCollectionMembership(noteId: "ndead", collectionId: "beta", createdAt: 300, deleted: false)
        try engine.enqueueCollectionMembership(noteId: "n1", collectionId: "alpha", createdAt: 400, deleted: false)
        try engine.enqueueCollectionMembership(noteId: "n1", collectionId: "gone", createdAt: 500, deleted: true)

        // collection-ids-for-note: live membership rows only, newest-first, no collection/notes join.
        XCTAssertEqual(try engine.collectionIdsForNote(noteId: "n1"), ["alpha", "beta"])

        // note-ids-for-collection: join-free — ndead's membership stays visible for the cascade.
        XCTAssertEqual(try engine.noteIdsForCollection(collectionId: "beta"), ["ndead", "n2", "n1"])

        // collection-note-counts: joins live notes (ndead excluded), collection-id asc, count ≥ 1.
        let counts = try engine.collectionNoteCounts().map { [$0.collectionId: $0.count] }
        XCTAssertEqual(counts, [["alpha": 1], ["beta": 2]])

        // note links: both endpoints returned; relation_type defaulted by enqueue when nil.
        try engine.enqueueNoteLink(
            id: "e1", fromNoteId: "parent", toNoteId: "n1", relationType: nil,
            createdAt: 100, deleted: false)
        try engine.enqueueNoteLink(
            id: "e2", fromNoteId: "n1", toNoteId: "child", relationType: "handwritten_annotation",
            createdAt: 200, deleted: false)
        try engine.enqueueNoteLink(
            id: "e3", fromNoteId: "a", toNoteId: "b", relationType: nil,
            createdAt: 300, deleted: false)

        let links = try engine.noteLinksForNote(noteId: "n1")
        XCTAssertEqual(links.map { $0.id }, ["e2", "e1"])
        let e1 = links.first { $0.id == "e1" }!
        XCTAssertEqual(e1.fromNoteId, "parent")
        XCTAssertEqual(e1.toNoteId, "n1")
        XCTAssertEqual(e1.relationType, "handwritten_annotation")
    }

    /// SUR-915: the duplicate-resolution merge verbs over the FFI — merge_books (+ undo) and the
    /// content-merge wrapper. Proves the undo token round-trips as a record, book merge rehomes
    /// notes + tombstones the loser, undo restores, and merge_content_duplicates collapses into a
    /// host-picked survivor, as an iOS host drives them.
    func testMergeContractOverFfi() throws {
        let db = FileManager.default.temporaryDirectory
            .appendingPathComponent("braird-merge-\(UUID().uuidString).sqlite")
        let engine = try SyncEngine.open(
            dbPath: db.path, supabaseUrl: "https://x.supabase.co", anonKey: "anon",
            vault: Vault.generate())

        func book(_ id: String, _ createdAt: Int64) -> BookUpsert {
            BookUpsert(
                id: id, title: "T-\(id)", author: nil, isbn: nil, coverUrl: nil, coverSource: nil,
                coverResolvedAt: nil, createdAt: createdAt, deleted: false, clearNullableFields: [])
        }
        func note(_ id: String, _ bookId: String?) -> NoteUpsert {
            NoteUpsert(
                id: id, bookId: bookId, plaintext: "text-\(id)", page: nil, tags: [], source: nil,
                sourceId: nil, sourceMetaJson: nil, chapter: nil, imagePath: nil, inkCropPath: nil,
                createdAt: 1, deleted: false, clearNullableFields: [])
        }

        try engine.enqueueBook(draft: book("s", 100))
        try engine.enqueueBook(draft: book("l1", 50))
        try engine.enqueueNote(draft: note("n1", "l1"))
        try engine.enqueueNote(draft: note("n2", "l1"))

        // book merge: notes rehome onto the survivor, loser tombstoned, earliest createdAt kept.
        let undo = try engine.mergeBooks(survivorId: "s", loserIds: ["l1"])
        XCTAssertEqual(try engine.listNotes(bookId: "s", limit: 50, offset: 0).map { $0.id }, ["n2", "n1"])
        XCTAssertNil(try engine.getBook(id: "l1"))
        XCTAssertEqual(try engine.getBook(id: "s")?.createdAt, 50)
        XCTAssertEqual(undo.survivorId, "s")
        XCTAssertEqual(undo.loserIds, ["l1"])
        XCTAssertEqual(undo.survivorPriorCreatedAt, 100)
        XCTAssertEqual(Set(undo.reassignments.map { $0.noteId }), ["n1", "n2"])
        XCTAssertTrue(undo.reassignments.allSatisfy { $0.priorBookId == "l1" })

        // undo restores the merge (both notes go back to l1; id-desc tiebreak on equal createdAt).
        try engine.unmergeBooks(undo: undo)
        XCTAssertEqual(try engine.listNotes(bookId: "l1", limit: 50, offset: 0).map { $0.id }, ["n2", "n1"])
        XCTAssertEqual(try engine.getBook(id: "s")?.createdAt, 100)
        XCTAssertEqual(try engine.getBook(id: "l1")?.title, "T-l1")

        // content merge into a host-picked survivor (exact path: same content_tag cluster).
        let db2 = FileManager.default.temporaryDirectory
            .appendingPathComponent("braird-merge2-\(UUID().uuidString).sqlite")
        let e2 = try SyncEngine.open(
            dbPath: db2.path, supabaseUrl: "https://x.supabase.co", anonKey: "anon",
            vault: Vault.generate())
        try e2.enqueueNote(draft: NoteUpsert(
            id: "keep", bookId: nil, plaintext: "same words", page: nil, tags: ["a"], source: nil,
            sourceId: nil, sourceMetaJson: nil, chapter: nil, imagePath: nil, inkCropPath: nil,
            createdAt: 1, deleted: false, clearNullableFields: []))
        try e2.enqueueNote(draft: NoteUpsert(
            id: "dup", bookId: nil, plaintext: "same words", page: nil, tags: ["b"], source: nil,
            sourceId: nil, sourceMetaJson: nil, chapter: nil, imagePath: nil, inkCropPath: nil,
            createdAt: 2, deleted: false, clearNullableFields: []))
        XCTAssertEqual(try e2.mergeContentDuplicates(survivorId: "keep", loserIds: ["dup"], allowCrossCluster: false), 1)
        XCTAssertEqual(try e2.listNotes(bookId: nil, limit: 50, offset: 0).map { $0.id }, ["keep"])
        XCTAssertEqual(try e2.getNote(id: "keep")?.tags, ["a", "b"])
    }

    /// SUR-911: the generated host surface performs a real protective merge against an empty
    /// loopback PostgREST oracle, returns every summary field, then exports plaintext schema 19.
    func testSnapshotTransferSurfaceOverFfi() throws {
        let server = try EmptyJSONLoopbackServer()
        defer { server.stop() }
        let db = FileManager.default.temporaryDirectory
            .appendingPathComponent("braird-snapshot-\(UUID().uuidString).sqlite")
        let engine = try SyncEngine.open(
            dbPath: db.path, supabaseUrl: server.baseURL, anonKey: "anon",
            vault: Vault.generate())
        engine.setAccessToken(jwt: testJWT())

        let summary: ImportSummary = try engine.importMerge(json: snapshotFixture())
        XCTAssertEqual(summary.schemaVersion, 19)
        assertImportCounts(summary.imported, notes: 2)
        assertZeroImportCounts(summary.skippedStale)
        XCTAssertGreaterThanOrEqual(server.requestCount, 16, "pull + direct-fetch preflight used HTTP")

        let exportedText = try engine.exportSnapshot()
        let exported = try JSONSerialization.jsonObject(with: Data(exportedText.utf8))
            as! [String: Any]
        XCTAssertEqual(exported["_syntopicon"] as? Bool, true)
        XCTAssertEqual(exported["schemaVersion"] as? Int, 19)
        let exportedAt = exported["exportedAt"] as! String
        XCTAssertNotNil(
            exportedAt.range(
                of: #"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$"#,
                options: .regularExpression),
            "exportedAt is exact UTC milliseconds")
        for (name, expected) in [
            "books": 1,
            "notes": 2,
            "customIdeas": 1,
            "noteLinks": 1,
            "lenses": 1,
            "collections": 1,
            "collectionMemberships": 1,
            "noteSignals": 1,
        ] {
            XCTAssertEqual((exported[name] as? [Any])?.count, expected, name)
        }
        let notes = exported["notes"] as! [[String: Any]]
        let parent = notes.first { $0["id"] as? String == "n-v19-parent" }!
        let child = notes.first { $0["id"] as? String == "n-v19-child" }!
        XCTAssertEqual(parent["text"] as? String, "A parent passage")
        XCTAssertEqual(child["text"] as? String, "Margin thought")
        XCTAssertTrue(notes.allSatisfy { !(($0["text"] as? String) ?? "").hasPrefix("enc:v") })
        XCTAssertFalse(exportedText.contains("enc:v"), "ciphertext must not cross export FFI")
        XCTAssertFalse(exportedText.contains("data:image/"), "device-local previews are omitted")
        XCTAssertFalse(exportedText.contains("LOCAL_SOURCE"))
        XCTAssertFalse(exportedText.contains("LOCAL_CROP"))
        XCTAssertFalse(exportedText.contains("stale-exporting-master-key-tag"))
        for localTable in ["outbox", "meta", "embeddings", "discovery_jobs"] {
            XCTAssertNil(exported[localTable], "local table \(localTable) must not export")
        }
    }

    /// Parse failures are a distinct generated variant and never echo archive material.
    func testSnapshotImportInvalidVariantIsSanitized() throws {
        let db = FileManager.default.temporaryDirectory
            .appendingPathComponent("braird-snapshot-invalid-\(UUID().uuidString).sqlite")
        let engine = try SyncEngine.open(
            dbPath: db.path, supabaseUrl: "http://127.0.0.1:9", anonKey: "anon",
            vault: Vault.generate())
        let sentinel = "SNAPSHOT-PLAINTEXT-MUST-NOT-ECHO"
        let invalidArchives = [
            "{\"_syntopicon\":false,\"private\":\"\(sentinel)\"}",
            "{\(sentinel)",
        ]

        for invalid in invalidArchives {
            do {
                _ = try engine.importMerge(json: invalid)
                XCTFail("expected InvalidImport")
            } catch let error as SyncError {
                switch error {
                case .InvalidImport(let message):
                    XCTAssertFalse(message.contains(sentinel))
                    XCTAssertFalse(error.localizedDescription.contains(sentinel))
                default:
                    XCTFail("expected InvalidImport, got \(error)")
                }
            }
        }
    }

    /// SUR-911 deliberately exposes protective Merge only; no destructive Replace entrypoint.
    func testGeneratedSnapshotSurfaceHasNoReplaceApi() throws {
        let sourceURL = repoRoot()
            .appendingPathComponent("bindings/swift/Sources/BrairdCore/BrairdCore.swift")
        let source = try String(contentsOf: sourceURL, encoding: .utf8)
        XCTAssertTrue(source.contains("func exportSnapshot("))
        XCTAssertTrue(source.contains("func importMerge("))
        XCTAssertFalse(source.localizedCaseInsensitiveContains("func importReplace("))
        XCTAssertFalse(source.localizedCaseInsensitiveContains("func replaceSnapshot("))
        XCTAssertFalse(source.contains("syncengine_import_replace"))
    }
}
