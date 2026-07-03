# Pinning braird-core artifacts

How downstream repos (starting with **braird-android**, SUR-762) depend on a released braird-core
build, and how a core version is bumped. This is the supply-chain contract for the crypto core —
it exists because the binding and the native are checksum-coupled and must move together.

## What a release publishes

`release.yml` fires on a `v*` tag and attaches three files to the GitHub Release for that tag:

| Artifact | What it is | Consumer |
|---|---|---|
| `braird-core-<version>.aar` | Android AAR: the committed UniFFI Kotlin binding + `libbraird_core.so` for **arm64-v8a + x86_64**, every LOAD segment 16 KB-aligned | braird-android app + instrumented tests |
| `braird-core-desktop-<version>.jar` | Self-contained JVM jar: the same binding + a bundled **linux-x86-64** `libbraird_core.so` at JNA's classpath-resource path — resolves with no `jna.library.path` | braird-android **JVM unit tests** (run on Linux CI) |
| `SHA256SUMS.txt` | `sha256sum` of the two artifacts | integrity verification |

Neither artifact bundles JNA. The consumer adds it alongside — pinned to the **exact** version the
core built against (**`5.17.0`**, not a range): `@aar` for the AAR path (ships the 16 KB-aligned
per-ABI `libjnidispatch.so`), the plain jar for the desktop path.

> **Alignment coverage boundary.** braird-core's release CI 16 KB-aligns and gates **its own**
> `libbraird_core.so` (both ABIs) — that is the only native in the AAR. JNA's `libjnidispatch.so`
> is *not* in this repo's artifact; it merges into the app at braird-android's APK build. So the
> consumer's build (SUR-762) **must** run the 16 KB-alignment check (`zipalign -c -P 16` or the
> NDK `check_elf_alignment.sh`) over the **merged** APK native libs, JNA included, and pin JNA to
> the exact `5.17.0` — the version is the only thing guaranteeing `libjnidispatch.so` is aligned.

## Why pin a tag **and** a checksum

- **Atomicity.** UniFFI verifies a contract version + per-function checksums between the binding and
  the `.so` at load. A binding paired with the wrong `.so` is not a compile error — it throws at the
  first crypto call, i.e. a silent on-device lockout. Pinning the exact released bytes (not a
  floating range) guarantees the pair a release shipped stays the pair the app ships.
- **Supply chain.** A tag can be moved; a re-uploaded asset can differ. The SHA-256 is the thing
  that can't lie. The consumer's fetch **verifies the checksum and fails the build on mismatch** —
  fail-closed, no fallback to an unverified download.

**No floating `latest`. No tag-only pin.** Both the tag and the per-artifact SHA-256 live in one
place in the consumer repo.

## Consumer pin (illustrative — the real wiring lands in braird-android, SUR-762)

Pin the tag + checksums in one file, and make the download verify:

```kotlin
// gradle/braird-core.lock  (or a version-catalog block) — the single source of truth
val brairdCoreTag = "v0.1.0"
val brairdCoreSums = mapOf(
    "braird-core-0.1.0.aar"          to "…64 hex chars…",
    "braird-core-desktop-0.1.0.jar"  to "…64 hex chars…",
)

// A download that fails closed on a checksum mismatch or a missing checksum. Download to a temp
// file and rename only after it verifies, so a crashed/partial download can never be re-used.
fun fetchPinned(name: String): File {
    val out = layout.buildDirectory.file("braird-core/$name").get().asFile
    val want = brairdCoreSums.getValue(name) // throws (fail-closed) if the pin has no checksum
    fun sha256(f: File) = f.inputStream().use { java.security.MessageDigest.getInstance("SHA-256")
        .digest(it.readBytes()).joinToString("") { b -> "%02x".format(b) } }
    if (out.exists() && sha256(out) == want) return out
    out.parentFile.mkdirs()
    val tmp = File.createTempFile(name, ".part", out.parentFile)
    uri("https://github.com/<org>/braird-core/releases/download/$brairdCoreTag/$name")
        .toURL().openStream().use { tmp.outputStream().use { o -> it.copyTo(o) } }
    val got = sha256(tmp)
    check(got == want) { tmp.delete(); "braird-core $name checksum mismatch: got $got, want $want" }
    tmp.renameTo(out)
    return out
}
```

This is illustrative; the reviewed, production fail-closed wiring lands in braird-android (SUR-762),
where `release-integrity-reviewer` verifies it against real releases.

The AAR consumer additionally declares `implementation("net.java.dev.jna:jna:5.17.0@aar")`; the
desktop-jar (JVM test) consumer declares `testImplementation("net.java.dev.jna:jna:5.17.0")`.

## Bumping the core

The bump is **one hand-made PR in the app repo** — and that PR *is* the integration gate:

1. Cut the core release: bump `version` in `Cargo.toml`, add a `[X.Y.Z]` section to `CHANGELOG.md`,
   tag `vX.Y.Z`. `release.yml` refuses to publish unless the tag, `Cargo.toml` version, and a
   matching CHANGELOG section all agree, then builds, checks 16 KB alignment, runs the consumer
   self-containment round-trip, and publishes the AAR + jar + `SHA256SUMS.txt`.
2. In the app repo, open `chore(core): pin braird-core vX.Y.Z`: update the tag **and both
   checksums together**, and let the app's JVM-against-desktop-jar suite run against the new core.
   Green means the new binding+native pair works end to end. That PR is where a core upgrade is
   reviewed and gated — nothing auto-updates.

## Scope

Android AAR + desktop jar today. The release/pin shape (one tag, one `SHA256SUMS.txt`, checksum-
verified fetch) is deliberately artifact-agnostic: the future iOS xcframework (SUR-716 follow-up)
attaches to the same release and pins the same way, no protocol change.
