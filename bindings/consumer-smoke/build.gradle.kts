import org.jetbrains.kotlin.gradle.dsl.JvmTarget

// SUR-760 self-containment proof. Depends on ONLY the desktop jar (+ JNA) and sets NO
// `jna.library.path` — so the sole way the native resolves is JNA extracting it from the jar's
// `<RESOURCE_PREFIX>/` entry. Green here == a consumer can pin the jar and exercise the real core
// with zero native setup (no cargo build, no LD_LIBRARY_PATH). This is the executable form of the
// SUR-760 acceptance criterion "a vault round-trip runs against it from a consumer-style project".
plugins {
    kotlin("jvm") version "2.4.0"
}

repositories { mavenCentral() }

// The jar under test. In CI's release job this is the artifact fetched from the GitHub Release
// (SHA-256 verified) and passed via -PcoreJar=<abs path>; locally it defaults to the freshly
// built bindings/kotlin jar. `coreVersion` lets the release job name the jar by tag.
val coreVersion = (findProperty("coreVersion") as String?) ?: "0.0.0-dev"
val coreJar: String =
    (findProperty("coreJar") as String?)
        ?: file("../kotlin/build/libs/braird-core-desktop-$coreVersion.jar").absolutePath

dependencies {
    testImplementation(files(coreJar))
    testImplementation("net.java.dev.jna:jna:5.17.0")
    testImplementation("org.junit.jupiter:junit-jupiter:5.10.2")
    testRuntimeOnly("org.junit.platform:junit-platform-launcher")
}

kotlin {
    compilerOptions { jvmTarget = JvmTarget.JVM_17 }
}

java {
    sourceCompatibility = JavaVersion.VERSION_17
    targetCompatibility = JavaVersion.VERSION_17
}

tasks.test {
    useJUnitPlatform()
    // Deliberately NO systemProperty("jna.library.path", ...) — see the class doc.
    testLogging { events("passed", "failed", "skipped") }
    doFirst { logger.lifecycle("consumer-smoke: core jar under test = $coreJar") }
}
