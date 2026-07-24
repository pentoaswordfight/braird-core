# SUR-661 Decomposition — Phase 4: Android app (Jetpack Compose)

## Context

SUR-661 is Phase 4 of ADR 0001: the native Android client (`braird-android`, new repo) of the shared Rust/UniFFI core. All crypto/sync/store logic lives in `braird-core` and is consumed as a pinned AAR over its Kotlin binding; the app is a Compose shell around the `Vault`/`SyncEngine` handles, mirroring the PWA feature surface. Both blockers are **Done**: SUR-716 (crypto core + bindings, 2026-06-30) and SUR-659/SUR-726 (sync engine + full 8-store coexistence, 2026-07-02).

Scope updates from ticket comments:
- **De-scoped:** `ACTION_SEND` share intent → SUR-722. Do not build here.
- **Re-homed in:** three PWA app-layer post-sync behaviors (`supabase.js` `fetchAllCloud` 2b–2d + 3): image download+cache, backfill missing books / rehome stranded notes, dropped-tag → custom-idea conversion.
- **Repo standard:** CHANGELOG.md + CI changelog-check (mirror `gce/.github/workflows/changelog-check.yml`) at scaffold time.

Founder decisions taken during this decomposition (2026-07-02):
1. **Split into sub-issues** under SUR-661 (as SUR-659 was sliced 659a–d).
2. **AAR packaging = new braird-core ticket** (blocks SUR-661), not folded in here.
3. **MVP slice first**; MVP includes Commonplace/Index, Idea Tree, and Lexicon (see M8a); importers deferred; **Seams dropped from scope** (not yet implemented in the PWA — remove from ticket text).
4. **Onboarding bridge = PIN transfer** (same as iOS SUR-660). Direct reuse of a GPM-synced braird.app passkey is a bonus path when available, not the onboarding dependency — so the web-RP-ID migration question stops blocking this ticket.
5. **Key-at-rest = device-secret design approved** (finding #5; parallels the iOS HKDF decision). Crypto-reviewer gate still applies at M4.
6. **Ticket text will be amended** (not just PR-description deviations): force-GPM → `allowCredentials` scoping; EncryptedSharedPreferences → DataStore; Seams removed; minSdk pending the week-1 experiment result.
7. **API-33 PRF experiment approved** — scoped below.

---

## Output contract

- **complexity:** 5
- **is_spine:** true
- **affected_paths:**
  - `braird-android/**` (new repo — everything)
  - `braird-core/scripts/build-aar.sh` (new), `braird-core/.github/workflows/parity.yml` (AAR lane), `braird-core/bindings/kotlin/build.gradle.kts` (JNA bump + packaging) — via the new M0 ticket
  - `surfc-web` — one-line `assetlinks.json` addition (`com.braird.app` + shared debug-keystore SHA-256; later the Play App-Signing SHA)
- **reviewer_personas (advisory):** spine → `crypto-reviewer` + `security-reviewer`; CE surface (Compose screens/theme) → `regression-reviewer` + `ux-reviewer`; `sync-reviewer` for the re-homed post-sync behaviors (they mutate data the PWA also reads); `naming-reviewer` for the new repo/public API names.

Complexity 5 because: new repo + cross-repo packaging + five spine subsystems (PRF unlock, key-at-rest, core binding, sync wiring, capture pipeline) + a full app's UI surface + device-bound testing. Firmly above the fast-path; every spine sub-issue takes the full GCE gate.

---

## Pressure-test findings (the reason this ticket can't be executed as written)

1. **The pinned Android AAR does not exist.** braird-core has committed Kotlin bindings (JNA) and a per-PR `aarch64-linux-android` cross-compile lane in `parity.yml`, but no AAR assembly, no publish (`publish = false`), no x86_64-for-emulator build. The bindings and `.so` are checksum-coupled (UniFFI verifies per-function checksums at load), so vendoring a loose `.so` is fragile — the AAR must bundle both atomically. **→ file the M0 braird-core ticket (founder-approved).** M0 must also produce a **desktop JVM jar** (bindings + desktop natives), or the app repo can never JVM-unit-test against the real core.
2. **minSdk 28 is an unproven claim — the project's own spike contradicts it.** SUR-700's harness was built with **minSdk 34**, and provider-based Credential Manager exists only on API 34+; on 28–33 androidx.credentials falls back to Play-Services FIDO2 where PRF has never been demonstrated by this project. Nobody has evidence the unlock ceremony works below API 34. Options: raise minSdk to 34, or keep 28 and capability-gate (pre-34 unlocks via PIN transfer only). **Open question + week-1 experiment** (API-33 Play-image emulator, one day).
3. **"Force the GPM provider" is not implementable as specified.** The spike's own comment: "There is no API to force a provider for a GET". `CredentialOption.allowedProviders` is API 34+ and advisory. The correct mechanism: populate **`allowCredentials`** with the user's enrolled credential IDs from `wrapped_key_blobs` — the picker then only offers enrolled passkeys regardless of provider — and verify the returned credentialId against the blob row. **Ticket text should be rewritten** to this requirement.
4. **16 KB page alignment is mandatory** (targetSdk 35 required for new Play apps since 2025-08-31; 16 KB since 2025-11-01). Applies to `libbraird_core.so` (NDK r28+/link-arg) **and** JNA's `libjnidispatch.so` — needs **JNA ≥ 5.17.0** (core currently pins 5.14.0, fine for desktop, wrong for the AAR). M0 CI must verify alignment.
5. **Key-at-rest: challenge "persist the PRF bytes".** Equal-effort better design: random 32-byte **device secret** → `vault.wrap_with_prf(device_secret)` → blob stored in `noBackupFilesDir`; Keystore wraps only the device secret (biometric-gated). PRF output is never persisted, biometric-enrollment invalidation recovery = delete + redo ceremony, and — decisive given founder answer #4 — **it works identically for PIN-transfer-onboarded devices, which have no PRF**. Within the ticket's letter ("PRF *or derived wrap key* — never the MK"); needs crypto-reviewer + founder sign-off at the unlock milestone.
6. **`EncryptedSharedPreferences` is deprecated.** Use Preferences DataStore under `noBackupFilesDir` + `dataExtractionRules` excluding the core SQLite and the device-secret blob (a Keystore-wrapped blob restored to a different device via Auto Backup is pure breakage). FBE covers the at-rest threat model for the session token.
7. **SyncEngine concurrency seam is unspecified.** The FFI is synchronous (`block_on` inside); overlapping foreground-resume sync and a WorkManager run risks double `SyncEngine::open` on one `db_path` + races on `set_access_token`. Policy belongs in the wrapper layer from day one: one engine singleton, one mutex, refresh JWT before every sync entry.
8. **assetlinks has two easy-to-miss entries:** a **shared committed debug keystore** (else every dev/CI box has a different SHA and passkeys silently fail) week 1, and the **Play App-Signing key SHA** (not the upload key) at release — an external-lead-time surfc-web change to initiate when the Play Console record is created (SUR-702), not at release wiring.
9. **Smaller but real:** process death kills the Rust-heap MK — root-level `Locked/Unlocked` gate with biometric re-unlock overlay (not a nav destination); `FLAG_SECURE` on note-content screens; offline cold-start must not need the network (cache the `wrapped_key_blobs` row); "expedited-on-connectivity" isn't a WorkManager primitive — periodic (15-min floor) + `NetworkType.CONNECTED` + foreground sync on launch/resume; entitlements need an offline grace policy; Google sign-in via Credential Manager `GetGoogleIdOption` + `signInWithIdToken` (skips browser OAuth redirect + deep links entirely).

---

## Implementation plan (sub-issues to file under SUR-661)

**Week-1 experiment — PRF below API 34 (decides minSdk)** *(timeboxed: 1 day; runs alongside M0/M1)*
- **Question:** does `GetPublicKeyCredentialOption` + PRF extension return PRF output below API 34, where androidx.credentials routes to Play-Services FIDO2 instead of the Android-14 provider framework?
- **Setup:** rebuild the SUR-700 spike harness (`PrfHarness.kt`) with minSdk 28. AVDs with **Google Play** system images at API 33 + API 30 (Play services, signed-in Google account with GPM, screen lock set); API 34 AVD as control. Verify the assetlinks entry from SUR-698 covers the harness applicationId; if not, reuse the spike's registered ID.
- **Procedure:** enroll a PRF passkey for the test account on the S25U → let GPM sync → on each AVD run the GET with eval salt `SHA-256("surfc-prf-eval-v1")` → inspect the response for `prf.results.first` → compare against the SUR-700 known-answer vector.
- **Pass:** PRF bytes present and match → minSdk 28 viable (capability-gate devices without Play services).
- **Fail / passkey not offered / emulator can't exercise GPM at all:** minSdk 34, or keep 28 with pre-34 = PIN-transfer-only unlock — founder picks using install-base data once the Play Console record exists. If the emulator path is inconclusive, one physical API-33 device test before concluding.
- **Output:** comment on SUR-661 + the minSdk value baked into M1's scaffold + ticket-text amendment.

**M0 — braird-core: Android AAR + desktop JVM jar packaging** *(new braird-core ticket; blocks all below; spine, GCE)*
`scripts/build-aar.sh` (mirror `build-xcframework.sh`): cargo-ndk arm64-v8a + x86_64, 16 KB-aligned (NDK r28+), bundle committed Kotlin bindings + JNA ≥5.17.0 `@aar`; desktop jar from the existing `bindings/kotlin/` project; **publish both to a braird-core GitHub Release tag** (founder decision, 2026-07-02). CI: alignment check (`zipalign -c -P 16`) in the existing Linux lane.
**Pin-update protocol (explicit coupling — nobody trusts a bare version string):** the app repo pins release tag **+ SHA-256 of each artifact** in one place (`gradle/libs.versions.toml` or a `core-artifact.lock` read by the Gradle download task, which verifies the checksum on fetch and fails the build on mismatch). Bumping the core = one hand-made app-repo PR that updates tag + checksums together, titled `chore(core): pin braird-core vX.Y.Z`, and runs the full JVM-against-desktop-jar suite — that PR *is* the integration gate for a core upgrade. No floating `latest`, no tag-only pin.
Touches: `braird-core/scripts/`, `bindings/kotlin/build.gradle.kts`, `.github/workflows/parity.yml`.

**M1 — Repo scaffold + core binding layer** *(spine)*
Reset the SUR-700 spike (tag it, clear `main`), scaffold `braird-android`: Gradle/Kotlin DSL with **committed wrapper incl. jar**, Compose BOM, `applicationId com.braird.app`, targetSdk 35, minSdk per open question #1; pinned AAR dep; thin wrappers `VaultManager`/`SyncManager` (process singletons, lifecycle + single-engine mutex + token-refresh-before-sync policy); manual DI (Hilt is unrequested abstraction at this size); shared debug keystore committed; `GATING.md` specialised from `gce/docs/methodology/GATING-baseline.md`; `CHANGELOG.md` + changelog-check CI; CI = assemble + JVM tests against the desktop jar + lint. Week-1 side PRs: surfc-web assetlinks debug-SHA entry; the API-33 PRF experiment.

**M2 — Theme + navigation shell** *(surface, CE)*
Port `surfc/src/design/tokens.css` + `DESIGN.md` to a Material3 theme (bundled Plex Sans/Lora/Sometype Mono, forest/paper palette, AA-safe text-green, 8px grid, radii/shadows); nav scaffold + root `Locked/Unlocked` gate; edge-to-edge + predictive-back. **R8-minified release variant with JNA/UniFFI keep rules lands here, not at release** (a proguard-broken JNA fails only at runtime on device).

**M3 — Auth + entitlements** *(spine: session handling)*
supabase-kt; email OTP + Google via `GetGoogleIdOption`/`signInWithIdToken`; session in DataStore under `noBackupFilesDir` (not deprecated EncryptedSharedPreferences); `dataExtractionRules`; `me-entitlements` client + offline grace; upgrade CTA → web checkout (no Play Billing; confirm Play policy at submission per SUR-704).

**M4 — Unlock ceremony + key-at-rest** *(spine; the walking skeleton lands mid-M4)*
Port SUR-700 `PrfHarness.kt` construction: `GetPublicKeyCredentialOption` + PRF ext, RP `braird.app`, eval salt `SHA-256("surfc-prf-eval-v1")` verbatim, `allowCredentials` from the user's `wrapped_key_blobs` rows (the pressure-tested replacement for "force GPM") → `vault.unlock(prf, blob)`. Key-at-rest per finding #5 (device secret + Keystore/StrongBox-with-fallback + BiometricPrompt; `setUserAuthenticationParameters` API 30+ / validity-duration on 28–29; `KeyPermanentlyInvalidatedException` → redo ceremony). Cold-start + process-death re-unlock overlay. Cache the blob row for offline cold-start.
**Walking-skeleton PR:** scaffold + AAR + OTP sign-in → blob fetch → PRF → unlock → `SyncEngine.pull()` → decrypt + render one real PWA-authored `enc:v2` note, plus R8-variant boot smoke. Retires every scary integration at once (coexistence AC #3 proven here).

**M5 — Multi-device** *(spine)*
PIN transfer **receive** (the onboarding bridge — founder answer #4) and send; enroll a new Android passkey (`CreatePublicKeyCredentialRequest` + PRF) → `vault.rewrap(new_prf)` → upsert blob row.

**M6 — Sync plumbing** *(spine)*
WorkManager periodic (`NetworkType.CONNECTED`) + foreground sync on launch/resume through the M1 singleton; JWT refresh before every entry; know whether core flush is per-row or per-batch transactional before writing retry logic.

**M7 — Re-homed post-sync behaviors** *(spine — mutates synced data; sync-reviewer)*
(a) image blob download+cache for `imagePath` notes; (b) backfill missing books / rehome stranded notes; (c) dropped-tag → custom-idea. Fixtures extracted from the PWA's `supabase.js` implementations as the oracle.

**M8a/M8b — Screen set by flow** *(surface, CE; MVP cut per founder decision #3)*
M8a (MVP browse/organise): note list/review + idea tagging (Add-Idea sheet), Library/Sources, search, **Commonplace/Index, Idea Tree, Lexicon**, settings-critical. M8b: capture flow — CameraX → `image-upload` → `anthropic-proxy` OCR/discover (existing endpoints/prompts), PII pre-check sheet. Deferred to later sub-issues: importers (Readwise/Kindle), duplicate-resolution screen. **Seams: out of scope** (not implemented in the PWA). `FLAG_SECURE` on note-content screens.

**M9 — Settings/account/compliance batch** *(mixed)*
LinkedDevices, account deletion via `delete-account`, export, camera/biometric rationale strings.

**M10 — Play release wiring**
Play Console record (SUR-702), Play App-Signing SHA → assetlinks (initiated at M4 time), internal testing track, data-safety behaviors (SUR-704), 16 KB-image boot test.

## Files to read (implementer's seed set)

- `braird-core/src/vault.rs`, `braird-core/src/sync/mod.rs` — the real API surface; `bindings/kotlin/.../braird_core.kt` (committed binding); `scripts/gen-bindings.sh`; `.github/workflows/parity.yml`; `GATING.md`, `CLAUDE.md`
- `braird-android/app/src/main/java/app/braird/prfspike/PrfHarness.kt` — proven PRF request + the no-provider-forcing finding
- `surfc/src/design/tokens.css`, `src/design/DESIGN.md` — theme spec; `src/App.jsx` — screen inventory
- `surfc/src/hooks/useKeyManagement.js`, `src/crypto/passkeyEnrollment.js` — unlock ceremony semantics (eval salt, blob shapes)
- `surfc/src/supabase.js` (`fetchAllCloud` 2b–2d + 3) — oracle for M7 behaviors
- `surfc/supabase/functions/{me-entitlements,image-upload,anthropic-proxy,fetch-link-metadata,delete-account}/` — endpoint contracts
- `gce/docs/methodology/GATING-baseline.md` + `gce/.github/workflows/changelog-check.yml` — repo-standard templates
- ADRs: `surfc/docs/architecture/0001-native-clients-shared-core.md`; `braird-core/docs/adr/0003` (seal-at-write), `0004` (HTTP client)

## Test cases

| Behavior to assert | Layer |
|---|---|
| Vault round-trip through the **pinned** artifact (catches contract/checksum drift) | JVM unit + desktop jar (CI) |
| Unlock-state machine incl. process-death restore w/ faked PRF bytes | JVM unit |
| M7 behaviors (b)(c) vs PWA-derived fixtures | JVM unit |
| Sync wiring, token refresh mid-flush, outbox survives failed push | JVM + local Supabase (CI) |
| Theme/screens (design-system port) | Paparazzi screenshots (CLI, no emulator) |
| Keystore wrap/unwrap (TEE), BiometricPrompt (`adb emu finger`), re-unlock after `am kill`, WorkManager, CameraX virtual scene, R8 boot smoke, 16 KB image boot | Instrumented (emulator, Play image) |
| GPM passkey PRF ceremony; StrongBox path; PWA↔Android note decrypt both directions (coexistence AC) | Physical device (S25U) — manual |
| PIN transfer (two devices); web-checkout CTA; account deletion e2e; **API-33 PRF experiment (week 1)** | Manual |

## Verification

Walking skeleton (M4) is the end-to-end proof: on-device sign-in → PRF unlock → pull → render a note the PWA wrote (`enc:v2`, AAD=noteId). Reverse direction (note written on Android decrypts on web) closes coexistence AC. Every spine sub-issue: GCE gate (pressure-test → persona pass → founder sign-off); CE screens: multi-persona report.

## Dev environment (the Android Studio question)

**No, Android Studio is not required.** Fully CLI-buildable on this Windows box: JDK 17 + SDK cmdline-tools (`sdkmanager` for platform-tools/platform-35/build-tools/emulator/system-images) + committed Gradle wrapper **including the jar** (the spike omitted it — that's why it "needed" Studio). `gradlew assembleDebug / testDebugUnitTest / lint / connectedDebugAndroidTest`; emulator via `avdmanager`/`emulator` (WHPX/AEHD on Windows); `adb install` to the S25 Ultra over USB. Studio buys rendered `@Preview`s, Layout Inspector, profiler — conveniences, not blockers (Paparazzi covers previews in CI). Genuinely device-only regardless of IDE: GPM passkey PRF, StrongBox. The Mac requirement is iOS/core-Swift only; Android app dev stays on Windows.

## Assumptions, gaps, risks, dependencies

- **Assumptions:** Supabase project/anon key shared with the PWA; `wrapped_key_blobs` REST access works from supabase-kt with existing RLS; SUR-700 spike code is sound seed for M4; core `SyncEngine` API is stable post-SUR-726.
- **Gaps:** AAR + desktop jar (M0, unowned until filed); braird-android `GATING.md` doesn't exist (M1); no Android CI anywhere yet.
- **Risks:** PRF below API 34 unproven (finding #2); JNA/R8/16 KB runtime failures only visible on device (mitigated: walking skeleton + M2 R8 variant); biometric-enrollment invalidation UX; Play external-checkout policy drift (re-confirm at submission, SUR-704); GPM-passkey e2e only proven on one device model.
- **Dependencies:** M0 ticket (blocks M1); SUR-698 assetlinks (done) + new debug-SHA entry (week 1, surfc-web); SUR-702 Play Console (blocks M10); SUR-722 share intent (parallel, consumes M1's scaffold); SUR-704 compliance (parallel).

## Open questions (founder)

None remaining.

*(All resolved 2026-07-02: sub-issue split ✓; AAR as braird-core ticket ✓; AAR via GitHub Release with hand-bumped tag+SHA-256 pin ✓; MVP cut incl. Commonplace/Idea Tree/Lexicon, Seams out ✓; PIN-transfer onboarding bridge ✓; device-secret key-at-rest ✓; ticket text to be amended ✓; minSdk experiment scoped ✓.)*

## Next step after approval

1. File the sub-issues in Linear: M0 in braird-core; the experiment + M1–M10 under SUR-661.
2. Amend SUR-661's description: force-GPM → `allowCredentials` scoping; EncryptedSharedPreferences → DataStore under `noBackupFilesDir`; remove Seams from §8; mark minSdk as pending the experiment.
3. Post this decomposition as a SUR-661 comment.
