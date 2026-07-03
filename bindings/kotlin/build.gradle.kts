import org.jetbrains.kotlin.gradle.dsl.JvmTarget

// Kotlin/JVM round-trip over the UniFFI binding + the SELF-CONTAINED desktop jar (SUR-760).
// The generated binding (src/main/kotlin/uniffi/braird_core/braird_core.kt) is COMMITTED —
// wiring a generate-into-srcDir task proved unreliable (compileKotlin snapshotted the source
// before the task populated it → NO-SOURCE). CI builds the cdylib and runs the JNA-backed test
// against it; UniFFI's runtime checksum guard fails loudly if the committed binding and the
// freshly-built lib ever diverge.
//
// Regenerate after any crate FFI change (incl. a docstring on an exported item) via the
// single canonical script — do NOT hand-run uniffi-bindgen; the `bindings-drift` CI job
// (parity.yml) regenerates through this same script and fails the PR on any diff:
//   scripts/gen-bindings.sh   # emits both Kotlin + Swift, --no-format, from repo root
plugins {
    kotlin("jvm") version "2.4.0"
}

group = "com.braird"
// The release tag drives the artifact version (release.yml passes -PcoreVersion=<tag>);
// local/dev builds fall back to a dev version so `./gradlew jar` works standalone.
version = (findProperty("coreVersion") as String?) ?: "0.0.0-dev"

repositories { mavenCentral() }

dependencies {
    // SUR-760: JNA 5.17.0. The @aar variant (bindings/android) ships the 16 KB-aligned per-ABI
    // libjnidispatch.so required by targetSdk 35; the desktop jar tracks the same JNA version
    // so the binding is exercised against one JNA across both artifacts.
    implementation("net.java.dev.jna:jna:5.17.0")
    testImplementation("org.json:json:20240303")
    testImplementation("org.junit.jupiter:junit-jupiter:5.10.2")
    testRuntimeOnly("org.junit.platform:junit-platform-launcher")
}

kotlin {
    compilerOptions { jvmTarget = JvmTarget.JVM_17 }
}

// Keep the Java compile tasks on the same JVM target as Kotlin. Without this,
// compileTestJava defaults to the running JDK (21 in CI) while Kotlin targets 17,
// and Gradle fails the build on the inconsistency. 17 bytecode runs on CI's JDK 21
// and a local JDK 26 alike, so no toolchain download is needed.
java {
    sourceCompatibility = JavaVersion.VERSION_17
    targetCompatibility = JavaVersion.VERSION_17
}

base { archivesName.set("braird-core-desktop") }

val repoRoot = layout.projectDirectory.dir("../..").asFile
val cargoTargetDir = repoRoot.resolve("target/release")

// Build the cdylib that JNA loads (libbraird_core.{so,dylib} / braird_core.dll).
val cargoBuild by tasks.registering(Exec::class) {
    workingDir = repoRoot
    commandLine("cargo", "build", "--release")
}

// SUR-760 self-contained desktop jar: bundle the HOST native at JNA's classpath-resource
// layout (`<RESOURCE_PREFIX>/<mapped-lib-name>`). A consumer that puts ONLY this jar (+ JNA)
// on the classpath then resolves the core with NO `jna.library.path` and NO local cargo build —
// JNA extracts the native straight from the jar. The release jar is built on Linux CI, so it
// carries `linux-x86-64/libbraird_core.so` (the platform braird-android's JVM unit tests run
// on, per SUR-760's Linux-x86_64-only decision); a local build carries the dev box's native,
// which is enough to prove the mechanism (see bindings/consumer-smoke).
val hostOs = System.getProperty("os.name").lowercase()
val hostArch = System.getProperty("os.arch").lowercase()
val (jnaPrefix, coreLibName) =
    when {
        hostOs.contains("win") -> "win32-x86-64" to "braird_core.dll"
        hostOs.contains("mac") || hostOs.contains("darwin") ->
            (if (hostArch.contains("aarch64") || hostArch.contains("arm")) "darwin-aarch64" else "darwin-x86-64") to
                "libbraird_core.dylib"
        else -> "linux-x86-64" to "libbraird_core.so"
    }

val jnaResourcesDir = layout.buildDirectory.dir("jna-resources")
val bundleNativeForJar by tasks.registering(Copy::class) {
    dependsOn(cargoBuild)
    from(cargoTargetDir.resolve(coreLibName))
    into(jnaResourcesDir.map { it.dir(jnaPrefix) })
}

tasks.named<Jar>("jar") {
    dependsOn(bundleNativeForJar)
    from(jnaResourcesDir) // <prefix>/<lib> lands at the jar root — JNA's resource-extraction path
}

tasks.test {
    dependsOn(cargoBuild)
    useJUnitPlatform()
    // JNA resolves the `braird_core` component to libbraird_core.{so,dylib} here (the test uses
    // the on-disk lib; the self-contained-jar path is proven by bindings/consumer-smoke instead).
    systemProperty("jna.library.path", cargoTargetDir.absolutePath)
}
