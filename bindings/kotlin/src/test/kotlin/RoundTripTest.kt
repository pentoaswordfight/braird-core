package braird.core.test

import java.io.File
import org.json.JSONArray
import org.json.JSONObject
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test
import uniffi.braird_core.CryptoException
import uniffi.braird_core.SearchDocKind
import uniffi.braird_core.SyncEngine
import uniffi.braird_core.SyncException
import uniffi.braird_core.Vault
import uniffi.braird_core.WrappedBlob

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

    /** SUR-741: the widened enqueue surface crosses the FFI, and source_meta_json validation
     * (which runs in Rust) surfaces as a thrown SyncException on the Kotlin side. */
    @Test
    fun enqueueNoteWidenedFieldsOverFfi() {
        val db = File.createTempFile("braird-rt", ".sqlite").apply { deleteOnExit() }
        val engine = SyncEngine.open(db.absolutePath, "https://x.supabase.co", "anon", Vault.generate())
        engine.enqueueNote(
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
        )
        assertThrows(SyncException::class.java) {
            engine.enqueueNote(
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
            )
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

        engine.enqueueBook(
            id = "b1", title = "Meditations", author = "Aurelius", isbn = null, coverUrl = null,
            coverSource = null, coverResolvedAt = null, createdAt = 1L, deleted = false,
            clearNullableFields = emptyList(),
        )
        engine.enqueueNote(
            id = "n1", bookId = "b1", plaintext = "the unexamined life is not worth living",
            page = null, tags = listOf("philosophy"), source = null, sourceId = null,
            sourceMetaJson = null, chapter = null, imagePath = null, inkCropPath = null,
            createdAt = 10L, deleted = false,
            clearNullableFields = emptyList(),
        )
        engine.enqueueNote(
            id = "n2", bookId = null, plaintext = "running toward the good", page = null,
            tags = emptyList(), source = null, sourceId = null, sourceMetaJson = null,
            chapter = null, imagePath = null, inkCropPath = null, createdAt = 20L, deleted = false,
            clearNullableFields = emptyList(),
        )
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
        engine.enqueueNote(
            id = "fresh", bookId = null, plaintext = "surfaced this week", page = null,
            tags = listOf("philosophy"), source = null, sourceId = null, sourceMetaJson = null,
            chapter = null, imagePath = null, inkCropPath = null, createdAt = now - 1000L,
            deleted = false, clearNullableFields = emptyList(),
        )
        engine.enqueueNote(
            id = "old", bookId = null, plaintext = "last month", page = null,
            tags = listOf("ethics"), source = null, sourceId = null, sourceMetaJson = null,
            chapter = null, imagePath = null, inkCropPath = null, createdAt = now - weekMs - 1000L,
            deleted = false, clearNullableFields = emptyList(),
        )

        // Only the in-window note counts; the pick is it, decrypted to plaintext across the FFI.
        assertEquals(1u, engine.notesThisWeek(now))
        val recent = engine.recentNote(now, 0uL)
        assertEquals("fresh", recent?.id)
        assertEquals("surfaced this week", recent?.text)
        assertEquals(false, recent?.text?.startsWith("enc:v") ?: false)

        // active_ideas = distinct tags over ALL live notes (window-independent): philosophy, ethics.
        assertEquals(2u, engine.counts().activeIdeas)
    }
}
