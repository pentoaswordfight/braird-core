// SUR-760: the Android AAR module. Standalone Gradle build (its own wrapper) so it can be
// assembled independently in CI (scripts/build-aar.sh) without pulling in the desktop JVM
// project. google() supplies the Android Gradle Plugin + jna@aar.
pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}
dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
    }
}
rootProject.name = "braird-core-android"
