import java.io.File
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Test
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
}
