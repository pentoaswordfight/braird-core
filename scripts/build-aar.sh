#!/usr/bin/env bash
#
# Build the pinned Android AAR (braird-core, arm64-v8a + x86_64) — the packaging mirror of
# scripts/build-xcframework.sh. It refreshes the committed binding, cross-compiles the crate for
# both the device (arm64-v8a) and emulator (x86_64) ABIs via cargo-ndk (16 KB page-aligned), then
# has AGP assemble the AAR that bundles those .so + the committed Kotlin binding. The AAR is a
# build artifact (gitignored, like the xcframework): the .so ships ONLY inside the released AAR,
# atomically coupled to the binding by UniFFI's contract-version + per-function checksums.
#
# The consumer (braird-android) adds JNA's @aar (>= 5.17.0, ships the 16 KB-aligned per-ABI
# libjnidispatch.so) alongside the pinned AAR — see docs/pinning.md.
#
# Requires: Rust + the aarch64/x86_64-linux-android targets, cargo-ndk, a JDK, the Android SDK,
# and a pinned NDK r28+ (16 KB-aligns .so by default). Point ANDROID_NDK_HOME at it and
# ANDROID_HOME / ANDROID_SDK_ROOT at the SDK.
#
# Usage:  scripts/build-aar.sh [version]      (default version: 0.0.0-dev)
set -euo pipefail

cd "$(dirname "$0")/.."
VERSION="${1:-${CORE_VERSION:-0.0.0-dev}}"
NDK_PIN=28.2.13676358 # first stable NDK line that 16 KB-aligns .so by default (targetSdk 35)

: "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to a pinned NDK r28+ (e.g. \$ANDROID_HOME/ndk/${NDK_PIN})}"
command -v cargo-ndk >/dev/null || {
  echo "cargo-ndk missing — run: cargo install cargo-ndk" >&2
  exit 1
}

# Fail loudly if any LOAD segment of any bundled .so is under 16 KB (0x4000). Prefer the NDK's
# llvm-readelf; fall back to binutils readelf. `$(( hex ))` parses 0x-prefixed alignments in both
# bash builds we run under (git-bash + CI ubuntu), so no gawk/strtonum dependency.
check_alignment() {
  local aar="$1" tmp so aligns a bad=0 so_bad found=0
  local readelf
  readelf=$(ls "${ANDROID_NDK_HOME}"/toolchains/llvm/prebuilt/*/bin/llvm-readelf* 2>/dev/null | head -1 || true)
  [ -n "$readelf" ] || readelf=$(command -v readelf || command -v llvm-readelf || true)
  [ -n "$readelf" ] || { echo "no readelf available for the alignment check" >&2; exit 1; }

  tmp=$(mktemp -d)
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" RETURN
  unzip -q "$aar" -d "$tmp" # extract all — a `jni/*` filter only grabs the dir entry on some unzips
  while IFS= read -r so; do
    found=1
    so_bad=0
    aligns=$("$readelf" -lW "$so" | awk '/LOAD/ {print $NF}')
    for a in $aligns; do
      if [ "$(( a ))" -lt 16384 ]; then
        echo "::error::UNALIGNED (<16 KB): ${so#"$tmp"/} — LOAD p_align $a"
        so_bad=1
        bad=1
      fi
    done
    [ "$so_bad" = 0 ] && echo "  ✓ 16 KB-aligned: ${so#"$tmp"/}"
  done < <(find "$tmp/jni" -name '*.so')
  [ "$found" = 1 ] || { echo "::error::no .so found inside $aar"; exit 1; }
  [ "$bad" = 0 ] || { echo "::error::AAR has an unaligned native lib — see above"; exit 1; }
}

echo "▸ refreshing the committed binding (atomic with the .so built below)"
# DEBUG build, not release: profile.release has `strip = true`, and UniFFI library-mode bindgen
# reads its metadata from the cdylib's symbols — a stripped .so yields no bindings on Linux ELF
# (works on Windows PE, which is why local build succeeds; the bindings-drift CI job uses this same
# debug path and passes). The binding TEXT is profile-independent (derived from FFI metadata), so it
# matches the shipped release .so's UniFFI checksums exactly.
scripts/gen-bindings.sh

# 16 KB page alignment is mandatory (targetSdk 35 Play requirement). NDK r28+ links aligned by
# default; force the linker flag too so alignment holds regardless of the NDK version in use.
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-z,max-page-size=16384"

echo "▸ cross-compiling libbraird_core.so (arm64-v8a + x86_64, release, 16 KB-aligned)"
rm -rf bindings/android/src/main/jniLibs
cargo ndk -t arm64-v8a -t x86_64 -o bindings/android/src/main/jniLibs build --release

echo "▸ assembling the AAR (AGP: committed binding + per-ABI .so)"
(cd bindings/android && ./gradlew assembleRelease --no-daemon -PcoreVersion="${VERSION}")

SRC_AAR=$(ls bindings/android/build/outputs/aar/*-release.aar | head -1)
mkdir -p dist
OUT_AAR="dist/braird-core-${VERSION}.aar"
cp "${SRC_AAR}" "${OUT_AAR}"

echo "▸ verifying 16 KB alignment of every native lib in the AAR"
check_alignment "${OUT_AAR}"

echo "✓ ${OUT_AAR}"
(cd dist && sha256sum "braird-core-${VERSION}.aar")
