#!/usr/bin/env bash
#
# Build BrairdCore.xcframework (macOS + iOS device + iOS simulator, arm64) and refresh
# the committed Swift bindings. macOS-only (xcodebuild). Run from anywhere; resolves the
# repo root itself. The xcframework is a build artifact (gitignored) — the nightly-macos
# CI job runs this, then `swift test` in bindings/swift.
#
# Usage:  scripts/build-xcframework.sh [version]
#   No version (nightly's call): build the xcframework only — behaviour unchanged.
#   With a version (release.yml's call): additionally stage the pinned, SPM-consumable
#   dist/braird-core-<version>.xcframework.zip and print its SwiftPM checksum. Mirrors
#   scripts/build-aar.sh's version-argument shape (SUR-745).
set -euo pipefail

cd "$(dirname "$0")/.."
LIB=libbraird_core.a
NAME=braird_core
VERSION="${1:-${CORE_VERSION:-}}"

echo "▸ building static libs (macOS host + iOS device + iOS sim, arm64)"
cargo build --release
cargo build --release --target aarch64-apple-ios
cargo build --release --target aarch64-apple-ios-sim

# Refresh the COMMITTED Kotlin + Swift bindings via the single canonical generator (DRY,
# --no-format) — same script the `bindings-drift` CI guard runs, so this can never drift
# from CI. The binding text is target-independent, so generating from the release host lib
# here matches CI's debug-host generation byte-for-byte.
echo "▸ refreshing committed bindings (scripts/gen-bindings.sh)"
scripts/gen-bindings.sh release

# The xcframework additionally needs the C shim header + modulemap (NOT committed — only
# live inside the gitignored xcframework), generated here from the iOS device lib.
echo "▸ generating FFI header + modulemap for the xcframework"
GEN=$(mktemp -d)
cargo run --quiet --bin uniffi-bindgen -- generate \
  --library "target/aarch64-apple-ios/release/${LIB}" \
  --language swift --no-format --out-dir "${GEN}"
HDRS=$(mktemp -d)
cp "${GEN}/${NAME}FFI.h" "${HDRS}/"
cp "${GEN}/${NAME}FFI.modulemap" "${HDRS}/module.modulemap"

echo "▸ assembling BrairdCore.xcframework"
rm -rf BrairdCore.xcframework
xcodebuild -create-xcframework \
  -library "target/release/${LIB}" -headers "${HDRS}" \
  -library "target/aarch64-apple-ios/release/${LIB}" -headers "${HDRS}" \
  -library "target/aarch64-apple-ios-sim/release/${LIB}" -headers "${HDRS}" \
  -output BrairdCore.xcframework >/dev/null

echo "✓ BrairdCore.xcframework + bindings/swift/Sources/BrairdCore/BrairdCore.swift"

# Release path (SUR-745): stage the pinned SPM binary artifact. SPM's remote binaryTarget
# consumes a ZIP whose checksum is the SHA-256 of the zip bytes — so we zip with `ditto
# --keepParent` (Apple's archiver; the .xcframework dir lands at the archive root, which SPM
# requires) and report the checksum via `swift package compute-checksum` (the canonical
# value a consumer pastes into `.binaryTarget(checksum:)`; identical to the zip's SHA-256
# hex). The release job re-hashes the exact published bytes into SHA256SUMS.txt — this print
# is for local verification. No version → skip entirely, so nightly's bare call is unchanged.
if [ -n "${VERSION}" ]; then
  echo "▸ staging dist/braird-core-${VERSION}.xcframework.zip (SPM binary artifact)"
  mkdir -p dist
  ZIP="dist/braird-core-${VERSION}.xcframework.zip"
  rm -f "${ZIP}"
  ditto -c -k --sequesterRsrc --keepParent BrairdCore.xcframework "${ZIP}"
  CHECKSUM=$(cd bindings/swift && swift package compute-checksum "../../${ZIP}")
  echo "✓ ${ZIP}"
  echo "  swift checksum: ${CHECKSUM}"
fi
