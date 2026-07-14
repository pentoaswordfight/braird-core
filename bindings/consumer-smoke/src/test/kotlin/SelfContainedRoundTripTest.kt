import java.io.File
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Test
import uniffi.braird_core.BookUpsert
import uniffi.braird_core.NoteUpsert
import uniffi.braird_core.SyncEngine
import uniffi.braird_core.Vault

/**
 * SUR-760 self-containment proof.
 *
 * This module depends on ONLY the desktop jar (+ JNA) and sets NO `jna.library.path`, so the
 * ONLY way `libbraird_core` can resolve is JNA extracting it from the jar's
 * `<RESOURCE_PREFIX>/<mapped-lib>` entry. A green run means a downstream consumer — concretely
 * braird-android's JVM unit tests — can drop in the pinned jar and exercise the real crypto core
 * with zero native setup. UniFFI's runtime contract-version + per-function checksum guard also
 * fires here, so a jar whose bundled native and committed bindings diverged would throw at
 * `Vault.generate()` rather than silently pass — i.e. this doubles as the atomicity check.
 */
class SelfContainedRoundTripTest {
    @Test
    fun vaultRoundTripFromJarOnly() {
        val vault = Vault.generate()
        val prf = ByteArray(32) { 0x07 }
        val reopened = Vault.unlock(prf, vault.wrapWithPrf(prf))

        val ct = vault.encryptNote("note-1", "secret 🔐")
        assertEquals("secret 🔐", reopened.decryptNote("note-1", ct))
        assertEquals(vault.contentTag("Hello", "b"), reopened.contentTag("Hello", "b"))

        val sealed = vault.sealBytes(byteArrayOf(1, 2, 3, 4), "note-1")
        assertEquals(listOf<Byte>(1, 2, 3, 4), vault.openBytes(sealed, "note-1").toList())
    }

    /**
     * SUR-806 AC #4: the desktop-jar round-trip covers all three Home-surface reads. Same
     * jar-only self-containment as above (SyncEngine + rusqlite resolve from the jar with no native
     * setup), and it exercises decrypt-in-core end-to-end — enqueueNote seals, the reads decrypt.
     */
    @Test
    fun homeSurfaceQueriesFromJarOnly() {
        val db = File.createTempFile("braird-home", ".sqlite").apply { deleteOnExit() }
        val engine = SyncEngine.open(db.absolutePath, "https://x.supabase.co", "anon", Vault.generate())
        val now = 1_700_000_000_000L
        engine.enqueueNote(NoteUpsert(
            id = "n1", bookId = null, plaintext = "surfaced this week", page = null,
            tags = listOf("philosophy"), source = null, sourceId = null, sourceMetaJson = null,
            chapter = null, imagePath = null, inkCropPath = null, createdAt = now - 1000L,
            deleted = false, clearNullableFields = emptyList(),
        ))

        assertEquals(1u, engine.notesThisWeek(now))
        assertEquals("surfaced this week", engine.recentNote(now, 0uL)?.text)
        assertEquals(1u, engine.counts().activeIdeas)
    }

    /**
     * SUR-858 AC #3: the desktop-jar round-trip covers the organise reads — notes-by-idea, the
     * per-idea tally, the untagged queue, and the collections/lenses lists — jar-only, exercising
     * decrypt-in-core end-to-end (enqueueNote seals; notesByIdea/untaggedNotes decrypt).
     */
    @Test
    fun organiseReadsFromJarOnly() {
        val db = File.createTempFile("braird-org", ".sqlite").apply { deleteOnExit() }
        val engine = SyncEngine.open(db.absolutePath, "https://x.supabase.co", "anon", Vault.generate())
        engine.enqueueNote(NoteUpsert(
            id = "n1", bookId = null, plaintext = "the unexamined life", page = null,
            tags = listOf("philosophy"), source = null, sourceId = null, sourceMetaJson = null,
            chapter = null, imagePath = null, inkCropPath = null, createdAt = 10L,
            deleted = false, clearNullableFields = emptyList(),
        ))
        engine.enqueueNote(NoteUpsert(
            id = "loose", bookId = null, plaintext = "untagged thought", page = null,
            tags = emptyList(), source = null, sourceId = null, sourceMetaJson = null,
            chapter = null, imagePath = null, inkCropPath = null, createdAt = 20L,
            deleted = false, clearNullableFields = emptyList(),
        ))
        engine.enqueueCollection(id = "c1", name = "Reading list", createdAt = 5L, deleted = false)
        engine.enqueueLens(
            id = "l1", name = "Stoic core", leafIds = listOf("philosophy"), combinator = "AND",
            threshold = 100L, createdAt = 6L, deleted = false,
        )

        assertEquals("the unexamined life", engine.notesByIdea("philosophy", 50u, 0u).single().text)
        assertEquals(listOf("philosophy" to 1u), engine.ideaCounts().map { it.idea to it.count })
        assertEquals("untagged thought", engine.untaggedNotes(50u, 0u).single().text)
        assertEquals(1u, engine.untaggedNotesCount())
        assertEquals(listOf("Reading list"), engine.listCollections(50u, 0u).map { it.name })
        assertEquals(listOf("Stoic core"), engine.listLenses(50u, 0u).map { it.name })
    }

    /**
     * SUR-915: the merge verbs run jar-only — book merge rehomes a note + tombstones the loser and
     * the undo token round-trips to restore it; content merge collapses a same-cluster duplicate.
     */
    @Test
    fun mergeContractFromJarOnly() {
        val db = File.createTempFile("braird-merge", ".sqlite").apply { deleteOnExit() }
        val engine = SyncEngine.open(db.absolutePath, "https://x.supabase.co", "anon", Vault.generate())
        engine.enqueueBook(BookUpsert(
            id = "s", title = "S", author = null, isbn = null, coverUrl = null, coverSource = null,
            coverResolvedAt = null, createdAt = 100L, deleted = false, clearNullableFields = emptyList(),
        ))
        engine.enqueueBook(BookUpsert(
            id = "l1", title = "L", author = null, isbn = null, coverUrl = null, coverSource = null,
            coverResolvedAt = null, createdAt = 50L, deleted = false, clearNullableFields = emptyList(),
        ))
        engine.enqueueNote(NoteUpsert(
            id = "n1", bookId = "l1", plaintext = "note", page = null, tags = emptyList(),
            source = null, sourceId = null, sourceMetaJson = null, chapter = null, imagePath = null,
            inkCropPath = null, createdAt = 1L, deleted = false, clearNullableFields = emptyList(),
        ))

        val undo = engine.mergeBooks("s", listOf("l1"))
        assertEquals(listOf("n1"), engine.listNotes("s", 50u, 0u).map { it.id })
        assertEquals(null, engine.getBook("l1"))
        assertEquals(50L, engine.getBook("s")?.createdAt)

        engine.unmergeBooks(undo)
        assertEquals(listOf("n1"), engine.listNotes("l1", 50u, 0u).map { it.id })
        assertEquals(100L, engine.getBook("s")?.createdAt)
    }
}
