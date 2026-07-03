import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Test
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
}
