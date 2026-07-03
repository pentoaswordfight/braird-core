// SUR-760 consumer smoke: a standalone Gradle project that consumes the braird-core desktop jar
// exactly as an external repo (e.g. braird-android's JVM unit tests) would — nothing from this
// repo's build is on its classpath except the jar under test.
rootProject.name = "braird-core-consumer-smoke"
