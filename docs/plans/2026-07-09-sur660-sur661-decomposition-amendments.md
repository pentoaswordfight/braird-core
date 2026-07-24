# SUR-660 / SUR-661 decomposition amendments (as of 2026-07-09)

The two decomposition reports of 2026-07-02 ("shimmering-lemur" for SUR-660/iOS, "breezy-spindle" for SUR-661/Android) are **point-in-time records**. This addendum lists everything that has superseded them since. **If you are an agent handed one of those plans, read this first — Linear tickets are canonical.** Source: the 2026-07-09 cross-platform parity audit (findings on SUR-660/SUR-661 comments) plus the week's founder decisions.

## Superseding both plans

- **Home screen reinstated (2026-07-07).** Both plans silently dropped the front door (conflated with Seams). Core reads: SUR-806 (Done). Clients: SUR-807 (iOS, start destination of the shipped M3 shell), SUR-808 (Android, `startDestination` reserved in 661c). Rule going forward: walk the app from cold launch; name the start destination.
- **Test environments (2026-07-03, SUR-307).** Device/e2e tests run against the `braird-staging` Supabase project — **never prod**. Supersedes the Android plan's "Supabase project/anon key shared with the PWA" assumption. Local Supabase = Windows box + CI only (no Docker on the Mac). PWA-written `enc:v2` coexistence fixtures live on staging.
- **Wrapper selection contract (SUR-812, Done 2026-07-08).** Unlock and transfer-create must select the `prf-v1` wrapper by trial-decrypt, never positionally. Shipped in braird-core **v0.4.0** as `Vault::unlock_from_blobs` (Swift `unlockFromBlobs(prf:blobs:)`, Kotlin `unlockFromBlobs(prf, blobs)`). Any plan/ticket text reading `unlock_any` means this symbol. Enrollment must stamp `credential_id` (+`rp_id`).
- **Post-sync behaviors re-homed to the core (2026-07-09, SUR-820).** Book backfill/rehome + dropped-tag→custom-idea run as an in-core post-pull reconciliation pass (one implementation, outbox-routed mutations, PWA fixtures as oracle). Image download/cache stays host-side: SUR-768 (Android, re-scoped) + SUR-821 (iOS, new — the plan never had an iOS leg).
- **v1 browse cut aligned (2026-07-09).** Android M8a trimmed to the iOS M6 shape (SUR-769 re-cut): flat Commonplace, Library, note review + tagging, search, settings-critical. Idea Tree / Lexicon / Commonplace tree / export → the Android fast-follow bucket **SUR-819** (mirror of iOS M12 / SUR-759). Reason: lockstep + the shipped read API covers exactly the M6 subset; the extra screens' reads (collections/lenses, per-idea counts, hierarchy) need a future SUR-806-style core extension first.
- **PII pre-check:** both platforms port `surfc/src/safety/piiRegex.js` **and its test suite 1:1** (iOS SUR-752 Done; Android SUR-770 amended to match).
- **Deep links split (2026-07-09).** SUR-691 = server-side association files + host/path map (⚠ SUR-697's AASA shipped `paths: ["*"]` on the apex — must be scoped); clients: SUR-825 (iOS), SUR-826 (Android), both blocked by 691.

## Superseding the iOS plan (shimmering-lemur)

- **Apple chain cleared:** SUR-134 Done 2026-07-02; SUR-697 + SUR-699 Done 2026-07-03. Track B is no longer externally gated; M9 (SUR-756) is **Done** via `unlockFromBlobs`.
- **Core pin:** SUR-784 (Done 2026-07-09) retired the M0 interim pinned-SHA script — checksum-pinned release artifacts, first pin v0.4.0.
- **PIN-transfer send added:** SUR-817 (Done) — the plan (and M-set) only had receive; iOS can now act as the linking sender.
- **Entitlements offline-grace:** SUR-822 (new) applies the founder-locked policy (72h cached, then fail-closed to Free) that Android ratified on 2026-07-08, after iOS M2 had shipped without one.
- **Lock-lifecycle verification:** SUR-823 (new, High) — the SUR-781 crypto-reviewer concern (SyncEngine retaining a decrypt-capable Vault past lock) was never checked on iOS.
- **M3 fonts/tokens:** SUR-751 as built consumes the generated `braird-tokens` artifact (SUR-642), not a hand-port of tokens.css.
- **iOS M12 bucket** (SUR-759) unchanged; SUR-819 is its Android mirror.

## Superseding the Android plan (breezy-spindle)

- **minSdk resolved (2026-07-03, SUR-761):** **28 with capability-gating** — PRF proven on API 33 (byte-identical to 34); no-provider devices fall back to PIN transfer.
- **Ingest funnel:** there is **no core `ingest()`**. SUR-770 (661j) now owns the host-side Kotlin `IngestService` (SUR-537 adapter semantics), the parallel of iOS's Swift IngestService (SUR-752). SUR-722 (share intent) corrected accordingly and blockedBy SUR-770.
- **Export moved out of v1:** 661k (SUR-771) re-cut 2026-07-09; export → SUR-819, pairing with iOS M12's export/import.
- **Auth amendments (2026-07-08, SUR-764):** offline grace = 72h then fail-closed to Free; `allowBackup="false"` for 661d with granular backup rules in SUR-811 (Done).
- **Lock-lifecycle design ratified:** SUR-781 (Done) — engine teardown/re-init on lock/sign-out.
- **gce repo-profile:** SUR-824 (new) — braird-android had no repo-profile/classifier coverage (747 = ios, 777 = core).

## Standing process rule (from four recurrences)

The platform decompositions ran independently and single-sided decisions didn't propagate (Home, offline grace, SUR-781, the re-homing comment, the 653-vs-722 correction). Every founder decision on a platform ticket now triggers the check: **"does the sibling platform ticket need this too?"** — and screen inventories are walked from cold launch.
