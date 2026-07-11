# Pinning braird-core artifacts

How downstream repos (**braird-android**, SUR-762; **braird-ios**, SUR-660) depend on a released
braird-core build, and how a core version is bumped. This is the supply-chain contract for the
crypto core — it exists because the binding and the native are checksum-coupled and must move
together.

## What a release publishes

`release.yml` fires on a `v*` tag and attaches five files to the GitHub Release for that tag:

| Artifact | What it is | Consumer |
|---|---|---|
| `braird-core-<version>.aar` | Android AAR: the committed UniFFI Kotlin binding + `libbraird_core.so` for **arm64-v8a + x86_64**, every LOAD segment 16 KB-aligned | braird-android app + instrumented tests |
| `braird-core-desktop-<version>.jar` | Self-contained JVM jar: the same binding + a bundled **linux-x86-64** `libbraird_core.so` at JNA's classpath-resource path — resolves with no `jna.library.path` | braird-android **JVM unit tests** (run on Linux CI) |
| `braird-core-<version>.xcframework.zip` | `BrairdCore.xcframework` (the C FFI + `libbraird_core.a` static-lib slices for **iOS device, iOS simulator, and macOS host — all arm64**), zipped for SwiftPM's remote binary target. Does **not** carry the Swift wrapper — that ships separately (next row) | braird-ios app + Swift round-trip tests |
| `BrairdCore.swift` | The generated UniFFI Swift wrapper, **from the same generation run** as the xcframework it is published beside (`gen-bindings.sh` regenerates it from the compiled lib the xcframework packages, and both the macOS and iOS-simulator round-trips ran against that pair before publish). Ships as its own checksummed asset because the xcframework can't carry Swift source — the iOS analog of the AAR bundling the Kotlin binding | braird-ios app (vendored into `Sources/`) |
| `SHA256SUMS.txt` | `sha256sum` of the **four** artifacts above (the AAR, jar, xcframework zip, and `BrairdCore.swift`) — computed from the exact published bytes | integrity verification |

The Android artifacts don't bundle JNA. That consumer adds it alongside — pinned to the **exact**
version the core built against (**`5.17.0`**, not a range): `@aar` for the AAR path (ships the
16 KB-aligned per-ABI `libjnidispatch.so`), the plain jar for the desktop path.

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

## Consumer pin — Android (illustrative — the real wiring lands in braird-android, SUR-762)

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

## Consumer pin — iOS (illustrative — the real wiring lands in braird-ios, SUR-660)

iOS has one extra moving part Android doesn't: the AAR bundles the Kotlin binding, but the
**xcframework carries only the C FFI + native `.a` slices — not** the ~2000-line generated Swift
wrapper (`BrairdCore.swift`). The release publishes that wrapper as its own asset so the consumer
can pin it the same way it pins the binary. An iOS consumer pins **two checksummed release assets**:

1. The **xcframework zip** as a SwiftPM remote binary target — `url` at the release asset,
   `checksum` = the zip's SHA-256 (the value in `SHA256SUMS.txt`; identical to what
   `swift package compute-checksum <zip>` prints).
2. The **`BrairdCore.swift` wrapper**, fetched **from the release** and verified against its own
   `SHA256SUMS.txt` entry, then vendored into `Sources/`. It is **not** copied from a git tag:
   a tag is mutable (see *Why pin a tag and a checksum* above), so vendoring "from the tag" would
   leave the wrapper — half of a checksum-coupled pair — pinned only to a movable ref. Pin its
   SHA-256, not the tag. The pair is coupled by UniFFI's contract-version + per-function checksums
   exactly as on Android: a wrapper and native from **different generation runs** throw at the
   first crypto call (a silent on-device lockout), so both halves must be pinned by checksum to the
   one release that built them together.

```swift
// braird-ios/Package.swift  (illustrative — SUR-660 lands the reviewed version)
import PackageDescription

let brairdCoreTag = "v0.1.0"
let package = Package(
    name: "App",
    targets: [
        // The xcframework, pinned by URL + checksum. checksum = the SHA256SUMS.txt entry for the zip.
        .binaryTarget(
            name: "braird_coreFFI",
            url: "https://github.com/pentoaswordfight/braird-core/releases/download/\(brairdCoreTag)/braird-core-0.1.0.xcframework.zip",
            checksum: "…64 hex chars from SHA256SUMS.txt…"
        ),
        // The wrapper source, fetched from the release + checksum-verified before vendoring into
        // Sources/BrairdCore/ (a fail-closed fetch step in the app's tooling — NOT copied from the tag).
        .target(name: "BrairdCore", dependencies: ["braird_coreFFI"]),
    ]
)
```

`.binaryTarget(url:checksum:)` is fail-closed by construction: SwiftPM re-downloads and re-hashes
the zip on `swift package resolve`, and a checksum mismatch fails the build — no fallback to an
unverified download. The **wrapper** has no equivalent built-in SwiftPM check (it's plain source),
so the app's pin/bump tooling must fetch `BrairdCore.swift` from the release, verify its
`SHA256SUMS.txt` hash, and **fail closed** on mismatch or a missing checksum before it lands in
`Sources/` — the same fail-closed shape the Android fetch uses (see the Kotlin example above). Do
**not** float the URL to `latest`; pin the exact tag **and** both SHA-256s. The `module.modulemap`
+ C header live **inside** the xcframework (generated at build time, never committed), so the binary
target resolves the `braird_coreFFI` module the wrapper's `import braird_coreFFI` expects.

> **Architecture + test coverage.** The zip carries **arm64** slices for iOS device, iOS simulator,
> and the macOS host. Before publish, the release leg drives the FFI through **two** of them: `swift
> test` on the macOS-host slice, and `xcodebuild test` on a real iOS **simulator** (arm64-sim). The
> iOS **device** slice (`aarch64-apple-ios`) is built and shipped but **cannot** run in CI (no
> device on the runner) — it is the one slice pinned without a functional round-trip behind it,
> covered instead by the on-device verification wave planned in SUR-660. The **simulator** slice is
> arm64 only — Apple-Silicon Macs for the simulator; an Intel-Mac slice (`x86_64-apple-ios`) is out
> of scope (the dev fleet is Apple Silicon). The macOS-host slice is inert baggage for an iOS-only
> consumer but is a shipped, tested slice.

## Toolchain & AGP compatibility

The AAR is built by an AGP `com.android.library` module (`bindings/android`) pinned to
**AGP 8.13.0 / Gradle 9.5.0** (Gradle held below 9.6.0 — AGP 8.x relies on a Gradle internal
removed there). The desktop jar (`bindings/kotlin`) and its `:core-roundtrip` self-containment
check (`bindings/consumer-smoke`) are plain `kotlin("jvm")` on a **separate** Gradle 9.6.1
wrapper — no AGP at all, so an app-side AGP bump can't reach them.

**Consumer AGP baseline: braird-android is on AGP 9.2.1 / Gradle 9.5.0** (SUR-853). The next
AAR cut targets this consumer.

**Forward-compat verified (SUR-854, 2026-07-11).** The pinned AAR (`v0.4.1`, sha256
`a5dc38d4…`, the exact bytes in the app's `braird-core.lock`) declares
`minAndroidGradlePluginVersion=1.0.0` and `minCompileSdk=1` in its `aar-metadata.properties`
— the module sets no `aarMetadata { }` override, so both sit at AGP's default floor. Any
AGP ≥ 1.0.0 consumer satisfies them, so the AGP 9.2.1 app resolves and links the AAR with
**no metadata warning or error**. (The breaking direction — an AGP-9-built AAR consumed by an
AGP-8 app — has no consumer.) The desktop-jar `:core-roundtrip` path is JVM-only and
untouched (JNA still `5.17.0`).

**AGP-alignment decision: leave the AAR-build module on AGP 8.13.0** — do *not* align at the
next cut. Forward-compat holds, and moving the producer into AGP 9.x would churn the
checksum-coupled release pipeline (16 KB alignment, parity round-trips) for zero consumer
benefit. Revisit only when AGP ships a 9.6-clean release (which lifts the Gradle 9.5 cap) or
a library feature forces a higher `minAndroidGradlePluginVersion`.

## Bumping the core

The bump is **one hand-made PR in the app repo** — and that PR *is* the integration gate:

1. Cut the core release: bump `version` in `Cargo.toml`, add a `[X.Y.Z]` section to `CHANGELOG.md`,
   tag `vX.Y.Z`. `release.yml` refuses to publish unless the tag, `Cargo.toml` version, and a
   matching CHANGELOG section all agree (on both build legs), then builds in parallel — Android:
   checks 16 KB alignment + runs the desktop-jar self-containment round-trip; iOS: builds the
   xcframework, runs the FFI round-trip on the macOS-host slice (`swift test`) **and** the
   iOS-simulator slice (`xcodebuild test`) against the exact bytes being zipped — and publishes the
   AAR + jar + xcframework zip + `BrairdCore.swift` + `SHA256SUMS.txt` in one create-only release.
2. In the app repo, open `chore(core): pin braird-core vX.Y.Z`: update the tag **and both
   checksums together**, and let the app's JVM-against-desktop-jar suite run against the new core.
   Green means the new binding+native pair works end to end. That PR is where a core upgrade is
   reviewed and gated — nothing auto-updates.

## Scope

Android AAR + desktop jar (SUR-760) and the iOS xcframework (SUR-745) today. The release/pin shape
(one tag, one `SHA256SUMS.txt`, checksum-verified fetch) is deliberately artifact-agnostic: the
iOS xcframework attaches to the same release and pins the same way, no protocol change — the only
iOS-specific wrinkle is that the Swift wrapper ships as its own **checksummed release asset**
(pinned by SHA-256 and fetched-and-verified from the release, exactly like the binary — **never**
vendored from the tag) rather than riding inside the binary artifact (see *Consumer pin — iOS*). A
future Android/desktop or additional Apple-platform slice attaches the same way.
