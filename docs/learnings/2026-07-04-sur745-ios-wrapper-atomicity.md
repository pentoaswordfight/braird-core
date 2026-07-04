---
date: 2026-07-04
ticket: SUR-745
area: [ci, bindings, governance]
gate: GCE
verdict: HOLD → PASS (one founder-accepted residual)
artefacts_updated:
  - .github/workflows/release.yml
  - scripts/build-xcframework.sh
  - docs/pinning.md
  - docs/learnings/2026-07-04-sur745-ios-wrapper-atomicity.md
---

# The iOS Swift wrapper is a second half of the checksum-coupled pair — Android's bundled binding hides that it has to be pinned too

## What happened

SUR-745 added the iOS `BrairdCore.xcframework` release leg to `release.yml`, mirroring the
Android AAR/jar pipeline (SUR-760): tag → build → checksummed GitHub Release asset. The first cut
pinned the xcframework zip by `url` + SHA-256 (SwiftPM's `.binaryTarget`) but told the consumer to
get the generated `BrairdCore.swift` wrapper by **vendoring it from the same git tag**. The
`release-integrity-reviewer` gate returned **HOLD** on exactly that, plus a second blocker
(untested shipped ABI) I had not seen.

## What surprised me

- **Android's packaging hides an iOS-only atomicity problem.** The AAR *bundles* the Kotlin binding
  next to `libbraird_core.so`, so "the binding and native are one artifact" is true for free. The
  iOS xcframework carries **only** the C FFI + native `.a` slices — the ~2000-line Swift wrapper is
  *not* in it. So iOS has two things to pin, not one, and it's easy to protect the binary
  (SwiftPM checksum) while leaving the wrapper — the other half of a UniFFI checksum-coupled pair —
  pinned to a **mutable git tag**. A moved tag → a wrapper from a different generation run → UniFFI
  throws at the first crypto call (silent on-device lockout). `docs/pinning.md` literally already
  said "a tag can be moved; the SHA-256 is the thing that can't lie" — and I'd re-introduced a
  tag-only pin two sections later. The doc's own rule caught the doc's own violation.
- **`swift package compute-checksum <zip>` == `sha256sum <zip>` (bare hex).** The SwiftPM
  `.binaryTarget(checksum:)` value is just the SHA-256 of the zip bytes, so one `SHA256SUMS.txt`
  line serves both the human integrity check and SwiftPM's resolver — no separate tooling, no
  second manifest. Verified byte-for-byte locally.
- **`swift test` on a package silently tests only the host arch.** It builds for the machine it
  runs on — the xcframework's **macOS-host** slice — so it proves nothing about the `ios-arm64` or
  `ios-arm64-simulator` slices that are actually shipped. The simulator slice *is* CI-testable
  (an arm64 `macos-14` runner can `xcodebuild test -destination 'platform=iOS Simulator'`), so
  "we can't test iOS in CI" is only true for the physical-device slice.

## What the gate caught

- **BLOCKER 1 — wrapper pinned by a mutable tag.** Fixed by publishing `BrairdCore.swift` as its
  own checksummed release asset, staged from the same `build-ios` job that regenerated it and ran
  the round-trips against the xcframework. Consumer now pins both SHA-256s, fetch-and-verify from
  the immutable release.
- **BLOCKER 2 — an ABI shipped without a test.** The persona's "an ABI shipped that isn't built
  *and tested*" is unconditional. The first cut shipped `ios-arm64` + `ios-arm64-sim` but tested
  neither. Fixed the simulator half (added `xcodebuild test` on a real sim); the **device** slice
  can't run on hosted CI and became a **founder-accepted residual**, closure tracked to SUR-660's
  on-device verification wave (gated on SUR-134). The persona is explicit that disclosure alone
  doesn't clear a BLOCKER — only the founder can, in writing on the PR.
- **CONCERN — an unpinned pre-compile action** (`Swatinem/rust-cache@v2`) in the same injection
  window the workflow already SHA-pins `dtolnay/rust-toolchain` for. SHA-pinned in both legs.
- **CONCERN — a self-contradicting doc.** The rewritten iOS section said "fetch + verify from the
  release," but the *Scope* summary still said "vendors from the tag." A reader skimming only the
  summary would take away the discouraged behavior. Fixed.

## What to compound

- **`docs/pinning.md` now states the iOS rule as: pin BOTH the zip and the wrapper by SHA-256, fetch
  both from the immutable release, never vendor the wrapper from the tag.** This is the pattern the
  braird-ios consumer PR (SUR-660) must implement and the `release-integrity-reviewer` will check on
  that consumer-side pin PR.
- **A release that ships a platform slice must FFI-test every slice it can, and name the ones it
  can't.** `build-ios` now tests macOS-host + iOS-sim; the device residual is written into
  `docs/pinning.md` (*Architecture + test coverage*) rather than implied to be covered.
- **Pre-compile actions in a release lane get SHA-pinned, not just the toolchain.** Anything that
  runs before the artifact is checksummed can poison the bytes the checksum then faithfully covers.
- **A generic release pipeline pays off but hides platform-specific gaps.** SUR-760 wrote
  `release.yml` to be artifact-agnostic and it slotted the iOS leg in cleanly — but "same protocol"
  masked that iOS distributes its binding differently from Android. When extending a generic
  pipeline to a new platform, re-ask every atomicity/coverage question from scratch for that
  platform; don't assume the generic shape carried them.

## References

- PR / commit: braird-core #22 (impl `acfcc7c`, gate fixes `0b6269a`, doc fix `61f80fd`), merged `a29ec73`; cut in `v0.2.0`
- Linear ticket: SUR-745 (M0 prerequisite for SUR-660; replaces SUR-748's interim pinned-SHA script)
- Files most affected: `.github/workflows/release.yml`, `scripts/build-xcframework.sh`, `docs/pinning.md`
- Persona: `release-integrity-reviewer` (gce `a8d7e95`)
