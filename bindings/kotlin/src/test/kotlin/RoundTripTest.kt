package braird.core.test

import com.sun.net.httpserver.HttpServer
import java.io.File
import java.net.InetSocketAddress
import java.util.Base64
import java.util.concurrent.atomic.AtomicInteger
import org.json.JSONArray
import org.json.JSONObject
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import uniffi.braird_core.BookUpsert
import uniffi.braird_core.CryptoException
import uniffi.braird_core.ImportCounts
import uniffi.braird_core.ImportSummary
import uniffi.braird_core.NoteUpsert
import uniffi.braird_core.SearchDocKind
import uniffi.braird_core.SyncEngine
import uniffi.braird_core.SyncException
import uniffi.braird_core.Vault
import uniffi.braird_core.WrappedBlob

private class EmptyJsonLoopbackServer : AutoCloseable {
    private val server = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0)
    val requestCount = AtomicInteger()
    val baseUrl: String
        get() = "http://127.0.0.1:${server.address.port}"

    init {
        server.createContext("/") { exchange ->
            requestCount.incrementAndGet()
            val response = "[]".toByteArray(Charsets.UTF_8)
            exchange.responseHeaders.add("Content-Type", "application/json")
            exchange.sendResponseHeaders(200, response.size.toLong())
            exchange.responseBody.use { it.write(response) }
            exchange.close()
        }
        server.start()
    }

    override fun close() = server.stop(0)
}

/**
 * Kotlin/JVM round-trip parity over the FFI. Decrypts FOREIGN (JS-produced) ciphertext
 * and reproduces the deterministic content tags byte-for-byte, plus a production
 * random-IV round-trip. Uses ONLY the public binding (no determinism seams).
 */
class RoundTripTest {
    // Gradle runs the test with the module dir as CWD, so the single vendored fixture
    // set is two levels up — read directly (no duplication, no drift).
    private val repoRoot = File(System.getProperty("user.dir")).resolve("../..").canonicalFile

    private fun vectors() = JSONArray(File(repoRoot, "vendored/crypto-parity/vectors.json").readText())
    private fun inputs() = JSONObject(File(repoRoot, "vendored/crypto-parity/inputs.json").readText())
    private fun snapshotFixture() =
        File(repoRoot, "vendored/snapshot-parity/schema-19-all-stores.json").readText()

    private fun testJwt(): String {
        val payload = Base64.getUrlEncoder().withoutPadding()
            .encodeToString("{\"sub\":\"snapshot-host-test\"}".toByteArray())
        return "h.$payload.sig"
    }

    private fun assertImportCounts(counts: ImportCounts, notes: UInt) {
        assertEquals(1u, counts.books)
        assertEquals(notes, counts.notes)
        assertEquals(1u, counts.customIdeas)
        assertEquals(1u, counts.noteLinks)
        assertEquals(1u, counts.lenses)
        assertEquals(1u, counts.collections)
        assertEquals(1u, counts.collectionMemberships)
        assertEquals(1u, counts.noteSignals)
    }

    private fun assertZeroImportCounts(counts: ImportCounts) {
        assertEquals(0u, counts.books)
        assertEquals(0u, counts.notes)
        assertEquals(0u, counts.customIdeas)
        assertEquals(0u, counts.noteLinks)
        assertEquals(0u, counts.lenses)
        assertEquals(0u, counts.collections)
        assertEquals(0u, counts.collectionMemberships)
        assertEquals(0u, counts.noteSignals)
    }

    private fun hex(s: String) =
        ByteArray(s.length / 2) {
            ((Character.digit(s[it * 2], 16) shl 4) + Character.digit(s[it * 2 + 1], 16)).toByte()
        }

    private fun JSONArray.objects() = (0 until length()).map { getJSONObject(it) }

    @Test
    fun foreignDecryptAndContentTags() {
        val vectors = vectors()
        val inputs = inputs()
        fun vector(id: String) = vectors.objects().first { it.getString("id") == id }

        // Unlock a JS-produced wrapped blob → recovers the frozen MK (0x11*32).
        val wrap = vector("mk-wrap").getJSONObject("expected")
        val blob = WrappedBlob(wrap.getString("wrappedKey"), wrap.getString("iv"), wrap.getString("salt"))
        val vault = Vault.unlock(hex(inputs.getString("prf")), blob)

        // Decrypt JS-produced ciphertext (PWA→native coexistence).
        val plain = inputs.getJSONArray("plaintext").getString(2)
        val noteId = inputs.getString("noteId")
        assertEquals(
            plain,
            vault.decryptNote(noteId, vector("enc-v2[2]").getJSONObject("expected").getString("ciphertext")),
        )
        assertEquals(
            plain,
            vault.decryptNote(null, vector("enc-v1[2]").getJSONObject("expected").getString("ciphertext")),
        )
        assertThrows(CryptoException::class.java) {
            vault.decryptNote("wrong", vector("enc-v2[2]").getJSONObject("expected").getString("ciphertext"))
        }

        // Content tags are deterministic (no IV) → byte-equal via the production API.
        var tagsChecked = 0
        for (v in vectors.objects().filter { it.getString("op") == "content-tag" }) {
            val inp = v.getJSONObject("inputs")
            val tag = vault.contentTag(inp.getString("text"), inp.getString("bookId"))
            assertEquals(v.getJSONObject("expected").getString("tag"), tag, v.getString("id"))
            tagsChecked++
        }
        assertEquals(10, tagsChecked, "expected 10 content-tag vectors")
    }

    @Test
    fun productionRoundTrip() {
        val vault = Vault.generate()
        val prf = ByteArray(32) { 0x07 }
        val reopened = Vault.unlock(prf, vault.wrapWithPrf(prf))

        val ct = vault.encryptNote("note-1", "secret 🔐")
        assertEquals("secret 🔐", reopened.decryptNote("note-1", ct))

        assertEquals(
            vault.contentTag("Hello", "b"),
            reopened.contentTag("Hello", "b"),
        )

        val sealed = vault.sealBytes(byteArrayOf(1, 2, 3, 4), "note-1")
        assertEquals(listOf<Byte>(1, 2, 3, 4), vault.openBytes(sealed, "note-1").toList())
    }

    /** SUR-812: unlockFromBlobs picks the wrapper that decrypts out of many, over the FFI. Two
     * wrappers of one MK under two PRFs → unlockFromBlobs with the asserted PRF recovers it even
     * when that wrapper is NOT first in the list (a positional pick would fail); a
     * non-matching PRF throws. */
    @Test
    fun unlockFromBlobsSelectsMatchingWrapper() {
        val vault = Vault.generate()
        val prfA = ByteArray(32) { 0x0A }
        val prfB = ByteArray(32) { 0x0B }
        val blobA = vault.wrapWithPrf(prfA)
        val blobB = vault.wrapWithPrf(prfB)

        // Asserted credential (A) is second in the list — trial-decrypt still finds it.
        val reopened = Vault.unlockFromBlobs(prfA, listOf(blobB, blobA))
        val ct = vault.encryptNote("note-1", "secret 🔐")
        assertEquals("secret 🔐", reopened.decryptNote("note-1", ct))

        assertThrows(CryptoException::class.java) {
            Vault.unlockFromBlobs(ByteArray(32) { 0x0C }, listOf(blobA, blobB))
        }
    }

    /** SUR-741: the widened enqueue surface crosses the FFI, and source_meta_json validation
     * (which runs in Rust) surfaces as a thrown SyncException on the Kotlin side. */
    @Test
    fun enqueueNoteWidenedFieldsOverFfi() {
        val db = File.createTempFile("braird-rt", ".sqlite").apply { deleteOnExit() }
        val engine = SyncEngine.open(db.absolutePath, "https://x.supabase.co", "anon", Vault.generate())
        engine.enqueueNote(NoteUpsert(
            id = "n1",
            bookId = "b1",
            plaintext = "secret",
            page = "5",
            tags = listOf("philosophy"),
            source = "readwise",
            sourceId = "rw-1",
            sourceMetaJson = "{\"highlight_id\":\"h1\"}",
            chapter = "1",
            imagePath = "img/1.jpg",
            inkCropPath = null,
            createdAt = 0L,
            deleted = false,
            clearNullableFields = emptyList(),
        ))
        assertThrows(SyncException::class.java) {
            engine.enqueueNote(NoteUpsert(
                id = "n2",
                bookId = null,
                plaintext = "x",
                page = null,
                tags = emptyList(),
                source = null,
                sourceId = null,
                sourceMetaJson = "not json",
                chapter = null,
                imagePath = null,
                inkCropPath = null,
                createdAt = 0L,
                deleted = false,
                clearNullableFields = emptyList(),
            ))
        }
    }

    /** SUR-921: null plaintext reaches the existing-row patch path, so a Vault that cannot
     * decrypt the note can still retag it without replacing its ciphertext. */
    @Test
    fun tagsOnlyNotePatchOverFfi() {
        val db = File.createTempFile("braird-rt", ".sqlite").apply { deleteOnExit() }
        val vaultA = Vault.generate()
        val writer = SyncEngine.open(db.absolutePath, "https://x.supabase.co", "anon", vaultA)
        writer.enqueueNote(NoteUpsert(
            id = "n1",
            bookId = null,
            plaintext = "secret from vault A",
            page = null,
            tags = listOf("before"),
            source = "kindle",
            sourceId = null,
            sourceMetaJson = null,
            chapter = null,
            imagePath = null,
            inkCropPath = null,
            createdAt = 10L,
            deleted = false,
            clearNullableFields = emptyList(),
        ))

        val foreign = SyncEngine.open(
            db.absolutePath, "https://x.supabase.co", "anon", Vault.generate())
        val before = foreign.getNote("n1")!!
        assertTrue(before.decryptFailed)
        assertEquals(null, before.text)

        foreign.enqueueNote(NoteUpsert(
            id = "n1",
            bookId = null,
            plaintext = null,
            page = null,
            tags = listOf("after"),
            source = null,
            sourceId = null,
            sourceMetaJson = null,
            chapter = null,
            imagePath = null,
            inkCropPath = null,
            createdAt = 999L,
            deleted = false,
            clearNullableFields = emptyList(),
        ))
        val stillForeign = foreign.getNote("n1")!!
        assertTrue(stillForeign.decryptFailed)
        assertEquals(listOf("after"), stillForeign.tags)

        val recovered = SyncEngine.open(
            db.absolutePath, "https://x.supabase.co", "anon", vaultA).getNote("n1")!!
        assertFalse(recovered.decryptFailed)
        assertEquals("secret from vault A", recovered.text)
        assertEquals(listOf("after"), recovered.tags)
        assertEquals("kindle", recovered.source)
        assertEquals(10L, recovered.createdAt)

        assertThrows(SyncException.PatchTargetMissing::class.java) {
            foreign.enqueueNote(NoteUpsert(
                id = "missing",
                bookId = null,
                plaintext = null,
                page = null,
                tags = listOf("after"),
                source = null,
                sourceId = null,
                sourceMetaJson = null,
                chapter = null,
                imagePath = null,
                inkCropPath = null,
                createdAt = 999L,
                deleted = false,
                clearNullableFields = emptyList(),
            ))
        }
    }

    /** SUR-744: the read/query surface over the FFI — list/get/counts/search against a populated
     * store. Proves note text crosses the binding as decrypted PLAINTEXT (never an `enc:` sentinel,
     * AC #2), the Library note-count badge, newest-first ordering, and lexical-search parity as an
     * Android host consumes them. */
    @Test
    fun readAndSearchSurfaceOverFfi() {
        val db = File.createTempFile("braird-rt", ".sqlite").apply { deleteOnExit() }
        val engine = SyncEngine.open(db.absolutePath, "https://x.supabase.co", "anon", Vault.generate())

        engine.enqueueBook(BookUpsert(
            id = "b1", title = "Meditations", author = "Aurelius", isbn = null, coverUrl = null,
            coverSource = null, coverResolvedAt = null, createdAt = 1L, deleted = false,
            clearNullableFields = emptyList(),
        ))
        engine.enqueueNote(NoteUpsert(
            id = "n1", bookId = "b1", plaintext = "the unexamined life is not worth living",
            page = null, tags = listOf("philosophy"), source = null, sourceId = null,
            sourceMetaJson = null, chapter = null, imagePath = null, inkCropPath = null,
            createdAt = 10L, deleted = false,
            clearNullableFields = emptyList(),
        ))
        engine.enqueueNote(NoteUpsert(
            id = "n2", bookId = null, plaintext = "running toward the good", page = null,
            tags = emptyList(), source = null, sourceId = null, sourceMetaJson = null,
            chapter = null, imagePath = null, inkCropPath = null, createdAt = 20L, deleted = false,
            clearNullableFields = emptyList(),
        ))
        engine.enqueueCustomIdea(
            id = "i1", name = "Antifragility", description = "gains from disorder",
            createdAt = 5L, deleted = false,
        )

        val counts = engine.counts()
        assertEquals(1u, counts.books)
        assertEquals(2u, counts.notes)
        assertEquals(1u, counts.customIdeas)
        assertEquals(1u, counts.activeIdeas) // n1 tagged "philosophy"; n2 untagged → 1 distinct

        // Library grid: the book carries its live note count.
        val books = engine.listBooks(50u, 0u)
        assertEquals(1, books.size)
        assertEquals("Meditations", books[0].title)
        assertEquals(1u, books[0].noteCount)

        // Commonplace flat list: newest-first, decrypted plaintext, never a ciphertext sentinel.
        val all = engine.listNotes(null, 50u, 0u)
        assertEquals(listOf("n2", "n1"), all.map { it.id })
        assertEquals("the unexamined life is not worth living", all[1].text)
        assertEquals(false, all[1].decryptFailed)
        for (n in all) assertEquals(false, n.text?.startsWith("enc:v") ?: false)

        // Per-book filter + single-note fetch.
        assertEquals(listOf("n1"), engine.listNotes("b1", 50u, 0u).map { it.id })
        val n1 = engine.getNote("n1")
        assertEquals("the unexamined life is not worth living", n1?.text)
        assertEquals(listOf("philosophy"), n1?.tags)

        // AddIdeaSheet "Your Ideas".
        assertEquals(listOf("Antifragility"), engine.listCustomIdeas(50u, 0u).map { it.name })

        // Lexical search: stemming (running ⇄ run) hits the note; idea by name; miss returns [].
        assertEquals(true, engine.search("run", 10u).any { it.refId == "n2" && it.kind == SearchDocKind.NOTE })
        assertEquals(true, engine.search("antifragility", 10u).any { it.refId == "i1" && it.kind == SearchDocKind.IDEA })
        assertEquals(true, engine.search("zzznomatch", 10u).isEmpty())
    }

    /** SUR-806: the Home-surface reads — the rolling-7-day notesThisWeek, the random "Recently
     * surfaced" pick, and the activeIdeas tag count — each decrypting in core and crossing the
     * binding as plaintext (never an `enc:` sentinel), as an Android host consumes them. */
    @Test
    fun homeSurfaceQueriesOverFfi() {
        val db = File.createTempFile("braird-home", ".sqlite").apply { deleteOnExit() }
        val engine = SyncEngine.open(db.absolutePath, "https://x.supabase.co", "anon", Vault.generate())

        val now = 1_700_000_000_000L
        val weekMs = 7L * 24 * 60 * 60 * 1000
        engine.enqueueNote(NoteUpsert(
            id = "fresh", bookId = null, plaintext = "surfaced this week", page = null,
            tags = listOf("philosophy"), source = null, sourceId = null, sourceMetaJson = null,
            chapter = null, imagePath = null, inkCropPath = null, createdAt = now - 1000L,
            deleted = false, clearNullableFields = emptyList(),
        ))
        engine.enqueueNote(NoteUpsert(
            id = "old", bookId = null, plaintext = "last month", page = null,
            tags = listOf("ethics"), source = null, sourceId = null, sourceMetaJson = null,
            chapter = null, imagePath = null, inkCropPath = null, createdAt = now - weekMs - 1000L,
            deleted = false, clearNullableFields = emptyList(),
        ))

        // Only the in-window note counts; the pick is it, decrypted to plaintext across the FFI.
        assertEquals(1u, engine.notesThisWeek(now))
        val recent = engine.recentNote(now, 0uL)
        assertEquals("fresh", recent?.id)
        assertEquals("surfaced this week", recent?.text)
        assertEquals(false, recent?.text?.startsWith("enc:v") ?: false)

        // active_ideas = distinct tags over ALL live notes (window-independent): philosophy, ethics.
        assertEquals(2u, engine.counts().activeIdeas)
    }

    /** SUR-858: the organise reads over the FFI — notes-by-idea, per-idea counts, the
     * collections/lenses lists, and the untagged work queue. Proves notes decrypt to plaintext
     * across the binding (notesByIdea/untaggedNotes), the ideaCounts tally matches the PWA oracle,
     * and the two new stores' first read paths map their rows, as an Android host consumes them. */
    @Test
    fun organiseReadsOverFfi() {
        val db = File.createTempFile("braird-org", ".sqlite").apply { deleteOnExit() }
        val engine = SyncEngine.open(db.absolutePath, "https://x.supabase.co", "anon", Vault.generate())

        engine.enqueueNote(NoteUpsert(
            id = "n1", bookId = null, plaintext = "the unexamined life", page = null,
            tags = listOf("philosophy", "ethics"), source = null, sourceId = null,
            sourceMetaJson = null, chapter = null, imagePath = null, inkCropPath = null,
            createdAt = 10L, deleted = false, clearNullableFields = emptyList(),
        ))
        engine.enqueueNote(NoteUpsert(
            id = "n2", bookId = null, plaintext = "on stoicism", page = null,
            tags = listOf("philosophy"), source = null, sourceId = null, sourceMetaJson = null,
            chapter = null, imagePath = null, inkCropPath = null, createdAt = 20L, deleted = false,
            clearNullableFields = emptyList(),
        ))
        engine.enqueueNote(NoteUpsert(
            id = "loose", bookId = null, plaintext = "untagged thought", page = null,
            tags = emptyList(), source = null, sourceId = null, sourceMetaJson = null,
            chapter = null, imagePath = null, inkCropPath = null, createdAt = 30L, deleted = false,
            clearNullableFields = emptyList(),
        ))
        engine.enqueueCollection(id = "c1", name = "Reading list", createdAt = 5L, deleted = false)
        engine.enqueueLens(
            id = "l1", name = "Stoic core", leafIds = listOf("philosophy", "ethics"),
            combinator = "OR", threshold = 75L, createdAt = 6L, deleted = false,
        )

        // notes-by-idea: newest-first, decrypted plaintext, never an enc: sentinel.
        val philosophy = engine.notesByIdea("philosophy", 50u, 0u)
        assertEquals(listOf("n2", "n1"), philosophy.map { it.id })
        assertEquals("on stoicism", philosophy[0].text)
        for (n in philosophy) assertEquals(false, n.text?.startsWith("enc:v") ?: false)

        // idea_counts: per-occurrence tally, idea-asc, present-tags-only.
        assertEquals(
            listOf("ethics" to 1u, "philosophy" to 2u),
            engine.ideaCounts().map { it.idea to it.count },
        )

        // untagged queue + badge count.
        assertEquals(listOf("loose"), engine.untaggedNotes(50u, 0u).map { it.id })
        assertEquals("untagged thought", engine.untaggedNotes(50u, 0u)[0].text)
        assertEquals(1u, engine.untaggedNotesCount())

        // collections + lenses first read paths.
        assertEquals(listOf("Reading list"), engine.listCollections(50u, 0u).map { it.name })
        val lens = engine.listLenses(50u, 0u).single()
        assertEquals("Stoic core", lens.name)
        assertEquals(listOf("philosophy", "ethics"), lens.leafIds)
        assertEquals("OR", lens.combinator)
        assertEquals(75L, lens.threshold)
    }

    /** SUR-923: the relation reads over the FFI — memberships traversed in both directions,
     * note-link edges (both endpoints), and the per-collection live-note counts (which join live
     * notes by founder decision, while note-ids stays join-free for the delete cascade). */
    @Test
    fun relationReadsOverFfi() {
        val db = File.createTempFile("braird-rel", ".sqlite").apply { deleteOnExit() }
        val engine = SyncEngine.open(db.absolutePath, "https://x.supabase.co", "anon", Vault.generate())

        fun note(id: String, createdAt: Long, deleted: Boolean = false) = NoteUpsert(
            id = id, bookId = null, plaintext = "text-$id", page = null, tags = emptyList(),
            source = null, sourceId = null, sourceMetaJson = null, chapter = null, imagePath = null,
            inkCropPath = null, createdAt = createdAt, deleted = deleted,
            clearNullableFields = emptyList(),
        )
        engine.enqueueNote(note("n1", 10L))
        engine.enqueueNote(note("n2", 20L))
        engine.enqueueNote(note("ndead", 30L, deleted = true))

        engine.enqueueCollection(id = "beta", name = "Beta", createdAt = 1L, deleted = false)
        engine.enqueueCollectionMembership(noteId = "n1", collectionId = "beta", createdAt = 100L, deleted = false)
        engine.enqueueCollectionMembership(noteId = "n2", collectionId = "beta", createdAt = 200L, deleted = false)
        engine.enqueueCollectionMembership(noteId = "ndead", collectionId = "beta", createdAt = 300L, deleted = false)
        engine.enqueueCollectionMembership(noteId = "n1", collectionId = "alpha", createdAt = 400L, deleted = false)
        engine.enqueueCollectionMembership(noteId = "n1", collectionId = "gone", createdAt = 500L, deleted = true)

        // collection-ids-for-note: live membership rows only, newest-first, no collection/notes join.
        assertEquals(listOf("alpha", "beta"), engine.collectionIdsForNote("n1"))

        // note-ids-for-collection: join-free — ndead's membership stays visible for the cascade.
        assertEquals(listOf("ndead", "n2", "n1"), engine.noteIdsForCollection("beta"))

        // collection-note-counts: joins live notes (ndead excluded), collection-id asc, count ≥ 1.
        assertEquals(
            listOf("alpha" to 1u, "beta" to 2u),
            engine.collectionNoteCounts().map { it.collectionId to it.count },
        )

        // note links: both endpoints returned; relation_type defaulted by enqueue when null.
        engine.enqueueNoteLink(id = "e1", fromNoteId = "parent", toNoteId = "n1", relationType = null, createdAt = 100L, deleted = false)
        engine.enqueueNoteLink(id = "e2", fromNoteId = "n1", toNoteId = "child", relationType = "handwritten_annotation", createdAt = 200L, deleted = false)
        engine.enqueueNoteLink(id = "e3", fromNoteId = "a", toNoteId = "b", relationType = null, createdAt = 300L, deleted = false)

        val links = engine.noteLinksForNote("n1")
        assertEquals(listOf("e2", "e1"), links.map { it.id })
        val e1 = links.single { it.id == "e1" }
        assertEquals("parent", e1.fromNoteId)
        assertEquals("n1", e1.toNoteId)
        assertEquals("handwritten_annotation", e1.relationType)
    }

    /** SUR-915: the duplicate-resolution merge verbs over the FFI — merge_books (+ undo) and the
     * content-merge wrapper. Proves the undo token round-trips as a record, book merge rehomes notes
     * + tombstones the loser, undo restores, and merge_content_duplicates collapses into a
     * host-picked survivor, as a native host drives them. */
    @Test
    fun mergeContractOverFfi() {
        val db = File.createTempFile("braird-merge", ".sqlite").apply { deleteOnExit() }
        val engine = SyncEngine.open(db.absolutePath, "https://x.supabase.co", "anon", Vault.generate())

        fun book(id: String, createdAt: Long) = BookUpsert(
            id = id, title = "T-$id", author = null, isbn = null, coverUrl = null,
            coverSource = null, coverResolvedAt = null, createdAt = createdAt, deleted = false,
            clearNullableFields = emptyList(),
        )
        fun note(id: String, bookId: String?) = NoteUpsert(
            id = id, bookId = bookId, plaintext = "text-$id", page = null, tags = emptyList(),
            source = null, sourceId = null, sourceMetaJson = null, chapter = null, imagePath = null,
            inkCropPath = null, createdAt = 1L, deleted = false, clearNullableFields = emptyList(),
        )

        engine.enqueueBook(book("s", 100L))
        engine.enqueueBook(book("l1", 50L))
        engine.enqueueNote(note("n1", "l1"))
        engine.enqueueNote(note("n2", "l1"))

        // ── book merge: notes rehome onto the survivor, loser tombstoned, earliest createdAt kept.
        val undo = engine.mergeBooks("s", listOf("l1"))
        assertEquals(listOf("n2", "n1"), engine.listNotes("s", 50u, 0u).map { it.id })
        assertEquals(null, engine.getBook("l1"), "loser tombstoned")
        assertEquals(50L, engine.getBook("s")?.createdAt)
        assertEquals("s", undo.survivorId)
        assertEquals(listOf("l1"), undo.loserIds)
        assertEquals(100L, undo.survivorPriorCreatedAt)
        assertEquals(setOf("n1", "n2"), undo.reassignments.map { it.noteId }.toSet())
        assertEquals(true, undo.reassignments.all { it.priorBookId == "l1" })

        // ── undo restores the merge (both notes go back to l1; id-desc tiebreak on equal createdAt).
        engine.unmergeBooks(undo)
        assertEquals(listOf("n2", "n1"), engine.listNotes("l1", 50u, 0u).map { it.id })
        assertEquals(100L, engine.getBook("s")?.createdAt)
        assertEquals("T-l1", engine.getBook("l1")?.title, "loser un-tombstoned")

        // ── content merge into a host-picked survivor (exact path: shared cluster required).
        val db2 = File.createTempFile("braird-merge2", ".sqlite").apply { deleteOnExit() }
        val e2 = SyncEngine.open(db2.absolutePath, "https://x.supabase.co", "anon", Vault.generate())
        // Two notes that seal to the SAME content_tag (same plaintext + null book) → one cluster.
        e2.enqueueNote(NoteUpsert(
            id = "keep", bookId = null, plaintext = "same words", page = null, tags = listOf("a"),
            source = null, sourceId = null, sourceMetaJson = null, chapter = null, imagePath = null,
            inkCropPath = null, createdAt = 1L, deleted = false, clearNullableFields = emptyList(),
        ))
        e2.enqueueNote(NoteUpsert(
            id = "dup", bookId = null, plaintext = "same words", page = null, tags = listOf("b"),
            source = null, sourceId = null, sourceMetaJson = null, chapter = null, imagePath = null,
            inkCropPath = null, createdAt = 2L, deleted = false, clearNullableFields = emptyList(),
        ))
        assertEquals(1u, e2.mergeContentDuplicates("keep", listOf("dup"), false))
        assertEquals(listOf("keep"), e2.listNotes(null, 50u, 0u).map { it.id })
        assertEquals(listOf("a", "b"), e2.getNote("keep")?.tags)
    }

    /** SUR-911: the generated host surface performs a real protective merge against an empty
     * loopback PostgREST oracle, returns every summary field, then exports plaintext schema 19. */
    @Test
    fun snapshotTransferSurfaceOverFfi() {
        EmptyJsonLoopbackServer().use { server ->
            val db = File.createTempFile("braird-snapshot", ".sqlite").apply { deleteOnExit() }
            val engine = SyncEngine.open(db.absolutePath, server.baseUrl, "anon", Vault.generate())
            try {
                engine.setAccessToken(testJwt())

                val summary: ImportSummary = engine.importMerge(snapshotFixture())
                assertEquals(19u, summary.schemaVersion)
                assertImportCounts(summary.imported, notes = 2u)
                assertZeroImportCounts(summary.skippedStale)
                assertTrue(server.requestCount.get() >= 16, "pull + direct-fetch preflight used HTTP")

                val exportedText = engine.exportSnapshot()
                val exported = JSONObject(exportedText)
                assertTrue(exported.getBoolean("_syntopicon"))
                assertEquals(19, exported.getInt("schemaVersion"))
                assertTrue(
                    Regex("""^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$""")
                        .matches(exported.getString("exportedAt")),
                    "exportedAt is exact UTC milliseconds",
                )
                for ((name, expected) in mapOf(
                    "books" to 1,
                    "notes" to 2,
                    "customIdeas" to 1,
                    "noteLinks" to 1,
                    "lenses" to 1,
                    "collections" to 1,
                    "collectionMemberships" to 1,
                    "noteSignals" to 1,
                )) {
                    assertEquals(expected, exported.getJSONArray(name).length(), name)
                }
                val notes = exported.getJSONArray("notes").objects().associateBy { it.getString("id") }
                assertEquals("A parent passage", notes.getValue("n-v19-parent").getString("text"))
                assertEquals("Margin thought", notes.getValue("n-v19-child").getString("text"))
                assertTrue(notes.values.all { !it.getString("text").startsWith("enc:v") })
                assertFalse(exportedText.contains("enc:v"), "ciphertext must not cross export FFI")
                assertFalse(exportedText.contains("data:image/"), "device-local previews are omitted")
                assertFalse(exportedText.contains("LOCAL_SOURCE"))
                assertFalse(exportedText.contains("LOCAL_CROP"))
                assertFalse(exportedText.contains("stale-exporting-master-key-tag"))
                for (localTable in listOf("outbox", "meta", "embeddings", "discovery_jobs")) {
                    assertFalse(exported.has(localTable), "local table $localTable must not export")
                }
            } finally {
                engine.close()
            }
        }
    }

    /** Parse failures are a distinct generated variant and never echo archive material. */
    @Test
    fun snapshotImportInvalidVariantIsSanitized() {
        val db = File.createTempFile("braird-snapshot-invalid", ".sqlite").apply { deleteOnExit() }
        val engine = SyncEngine.open(
            db.absolutePath,
            "http://127.0.0.1:9",
            "anon",
            Vault.generate(),
        )
        try {
            val sentinel = "SNAPSHOT-PLAINTEXT-MUST-NOT-ECHO"
            for (invalid in listOf(
                """{"_syntopicon":false,"private":"$sentinel"}""",
                "{$sentinel",
            )) {
                val error = assertThrows(SyncException.InvalidImport::class.java) {
                    engine.importMerge(invalid)
                }
                assertFalse(error.v1.contains(sentinel))
                assertFalse(error.message.orEmpty().contains(sentinel))
            }
        } finally {
            engine.close()
        }
    }

    /** SUR-911 deliberately exposes protective Merge only; no destructive Replace entrypoint. */
    @Test
    fun generatedSnapshotSurfaceHasNoReplaceApi() {
        val methods = SyncEngine::class.java.methods.map { it.name }
        assertTrue("exportSnapshot" in methods)
        assertTrue("importMerge" in methods)
        // The invariant is that the SNAPSHOT/IMPORT surface is merge-only — no whole-corpus Replace
        // entrypoint that could wipe a reader's notes. Scope the check to that; the SUR-952 margin op
        // (`replaceHandwrittenAnnotations`) is a scoped, reader-initiated per-note replace named for PWA
        // parity, NOT a snapshot Replace, so it's deliberately allowed.
        assertFalse(
            methods.any {
                it.contains("replace", ignoreCase = true) &&
                    (it.contains("snapshot", ignoreCase = true) || it.contains("import", ignoreCase = true))
            },
        )
    }
}
