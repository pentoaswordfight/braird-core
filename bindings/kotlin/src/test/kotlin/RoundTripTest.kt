package braird.core.test

import java.io.File
import org.json.JSONArray
import org.json.JSONObject
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test
import uniffi.braird_core.CryptoException
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
            )
        }
    }
}
