import org.jetbrains.kotlin.gradle.dsl.JvmTarget

// Kotlin/JVM round-trip over the UniFFI bindings. This build is self-contained for CI:
// it compiles the Rust cdylib and regenerates the Kotlin binding (Rust is on PATH in
// the parity workflow), then runs the JNA-backed test against the vendored vectors.
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

// Repo root (two levels up from bindings/kotlin) and the cargo output dir.
val repoRoot = layout.projectDirectory.dir("../..").asFile
val cargoTargetDir = repoRoot.resolve("target/release")
// Generate the binding straight into the conventional main source root so it is always
// on compileKotlin's source set (a build/generated srcDir registered NO-SOURCE). The
// generated `uniffi/` subtree is gitignored.
val generatedSrcDir = layout.projectDirectory.dir("src/main/kotlin").asFile

val nativeLibName =
    if (System.getProperty("os.name").startsWith("Mac")) "libbraird_core.dylib" else "libbraird_core.so"

val cargoBuild by tasks.registering(Exec::class) {
    workingDir = repoRoot
    commandLine("cargo", "build", "--release")
}

val uniffiBindgen by tasks.registering(Exec::class) {
    dependsOn(cargoBuild)
    workingDir = repoRoot
    doFirst { generatedSrcDir.mkdirs() }
    commandLine(
        "cargo", "run", "--quiet", "--bin", "uniffi-bindgen", "--",
        "generate",
        "--library", "target/release/$nativeLibName",
        "--language", "kotlin",
        "--out-dir", generatedSrcDir.absolutePath,
    )
}

tasks.named("compileKotlin") { dependsOn(uniffiBindgen) }

tasks.test {
    dependsOn(cargoBuild)
    useJUnitPlatform()
    // JNA resolves the `braird_core` component to libbraird_core.{dylib,so} here.
    systemProperty("jna.library.path", cargoTargetDir.absolutePath)
}
