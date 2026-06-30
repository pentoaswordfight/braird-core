import org.jetbrains.kotlin.gradle.dsl.JvmTarget

// Kotlin/JVM round-trip over the UniFFI binding. The generated binding
// (src/main/kotlin/uniffi/braird_core/braird_core.kt) is COMMITTED — wiring a
// generate-into-srcDir task proved unreliable (compileKotlin snapshotted the source
// before the task populated it → NO-SOURCE). CI builds the cdylib and runs the
// JNA-backed test against it; UniFFI's runtime checksum guard fails loudly if the
// committed binding and the freshly-built lib ever diverge.
//
// Regenerate after any crate FFI change:
//   cargo build --release && cargo run --bin uniffi-bindgen -- generate \
//     --library target/release/libbraird_core.<dylib|so> --language kotlin \
//     --out-dir bindings/kotlin/src/main/kotlin
plugins {
    kotlin("jvm") version "2.4.0"
}

repositories { mavenCentral() }

dependencies {
    implementation("net.java.dev.jna:jna:5.14.0")
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

val repoRoot = layout.projectDirectory.dir("../..").asFile
val cargoTargetDir = repoRoot.resolve("target/release")

// Build the cdylib that JNA loads at test time (libbraird_core.{so,dylib}).
val cargoBuild by tasks.registering(Exec::class) {
    workingDir = repoRoot
    commandLine("cargo", "build", "--release")
}

tasks.test {
    dependsOn(cargoBuild)
    useJUnitPlatform()
    // JNA resolves the `braird_core` component to libbraird_core.{so,dylib} here.
    systemProperty("jna.library.path", cargoTargetDir.absolutePath)
}
