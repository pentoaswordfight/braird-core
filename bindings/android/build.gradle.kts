import org.jetbrains.kotlin.gradle.dsl.JvmTarget

// SUR-760: the pinned Android AAR — the braird-core crypto core packaged for braird-android.
//
// This module compiles the SINGLE committed UniFFI binding (reused from ../kotlin via a srcDir,
// kept fresh by the bindings-drift CI guard — deliberately NOT duplicated here) and packages it
// with the per-ABI libbraird_core.so (produced by cargo-ndk in scripts/build-aar.sh, dropped into
// src/main/jniLibs) plus JNA's @aar (which ships the 16 KB-aligned per-ABI libjnidispatch.so).
// The .so and the bindings ship atomically in one artifact — UniFFI couples them with a
// contract-version + per-function checksums, so a mismatched pair is a silent on-device lockout.
plugins {
    id("com.android.library") version "8.13.0"
    id("org.jetbrains.kotlin.android") version "2.4.0"
}

android {
    namespace = "com.braird.core"
    compileSdk = 35
    buildToolsVersion = "36.1.0"

    defaultConfig {
        minSdk = 28 // SUR-761 (founder-approved): PRF works below API 34 via the Play FIDO2 path.
    }

    // One binding, one jniLibs tree — no source duplication.
    sourceSets["main"].kotlin.srcDir("../kotlin/src/main/kotlin")
    sourceSets["main"].jniLibs.srcDir("src/main/jniLibs")

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    // Keep the .so uncompressed + page-aligned in the AAR/APK so the dynamic loader can mmap it
    // directly — the 16 KB page-size Play requirement (targetSdk 35) needs aligned, extractable
    // native libs. NDK r28+ links our .so 16 KB-aligned; the CI alignment check enforces it.
    packaging {
        jniLibs.useLegacyPackaging = false
    }
}

kotlin {
    compilerOptions { jvmTarget = JvmTarget.JVM_17 }
}

dependencies {
    // 5.17.0+ @aar ships the per-ABI 16 KB-aligned libjnidispatch.so.
    implementation("net.java.dev.jna:jna:5.17.0@aar")
}
