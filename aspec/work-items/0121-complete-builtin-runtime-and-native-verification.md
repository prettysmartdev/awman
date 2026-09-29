# Work Item: Task

Title: Complete the builtin Microsandbox runtime and close all WI 0119 verification gaps
Issue: n/a

Current distribution policy (2026-09-28): signing and notarization are out of
scope by user instruction. No signing identity, Apple distribution account,
notarization credentials or explicit ad-hoc signing step is required. Native
boot and final-artifact checks still apply; failures must be reported honestly.

Final verification (2026-09-28): **NOT ACCEPTED**. All acceptance boxes remain
unchecked. The [final evidence register](../review-notes/0121-evidence-register.md)
is authoritative for current verdicts; local hermetic passes do not close the
missing implementation, native/service/distribution or Git-identity gates.
The latest continuation adds cancellable production acquisition, explicit strict
SNI enforcement, shared image-tag leases, genuine cross-driver transactions and
registered guest/service/corpus scenarios. The user authorized follow-up items
for work requiring unavailable resources: [WI 0122](completed/0122-native-apple-image-store-bridge.md)
owns the missing native Apple-store bridge; [WI 0123](0123-builtin-native-verification-and-release-closure.md)
owns remaining native scenario implementation and full verification/release closure.
These assignments await resource-qualified agents; creating them is not execution
evidence and does not complete the original acceptance criteria below.

Resolution scope (2026-09-29, user decision): this item is resolved when
[WI 0123](0123-builtin-native-verification-and-release-closure.md) completes.
WI 0123 ships the runtime as `builtin-experimental` on Apple Silicon macOS
only, ad-hoc signed, and validates the Apple store, archive and normal-case
registry sources. At that point each acceptance criterion below is either PASS
for that scope, with linked evidence, or **Deferred (WI 0124, optional)**:
Linux KVM targets, Docker Engine sources and registry edge cases. Deferred
criteria do not keep this item open;
[WI 0124](0124-builtin-linux-and-docker-gates.md) tracks them as optional
future work.

## Summary:

- Close all outstanding implementation, compatibility, security, build and
  evidence gaps from [WI 0119](0119-strict-embedded-microsandbox-runtime.md).
  Its final review failed acceptance: **2 FAIL, 5 BLOCKED, 2 PASS**, with the
  passing documentation/regression criterion limited to hermetic checks.
- The [final adversarial review](../review-notes/0119-final-adversarial-verification.md)
  records the 2026-09-24 baseline, fixes already made, prior-finding dispositions
  and verification limits. The deficiency register below also carries forward
  unresolved risks from the workflow's blocker and coverage notes. It is
  self-contained; implementation must not require `/awman/context/workflow`.
- Deliver the missing functionality and native evidence on Apple Silicon,
  Linux ARM64/KVM and Linux x86_64/KVM. Preserve existing Windows/Intel Mac
  builds and explicit Docker/Apple backend behavior. Creating this follow-up
  does **not** close WI 0119 or convert blocked tests into passes.
- Retain the strict packaging contract: one host executable embedding the VM,
  target kernel and guest agent, with same-executable worker processes. No host
  agent, extracted executable/firmware DSO/helper script, installed runtime,
  silent fallback, SBX execution/config model or Smolvm integration.
- [WI 0120](0120-upstream-embedded-kernel-support.md) owns upstream API work and
  eventual patch removal. This item owns downstream completion and validation;
  upstream acceptance remains unnecessary for delivery. Record any additional
  local glue patch separately and extend 0120's removal inventory.

## User Stories

### User Story 1:
As a: user

I want to: acquire existing images from registries, Docker Engine stores and
the Apple Containers store, then run cached agents with only awman installed

So I can: use the builtin runtime on every required host without a separate
execution runtime or hidden helper dependency.

### User Story 2:
As a: user of CLI, TUI, API, workflows and squads

I want to: retain my settings, overlays, credentials, prompts and lifecycle
behavior, with tested and explicit runtime differences

So I can: switch runtimes without losing access, corrupting streams or files,
or exposing another session's data.

### User Story 3:
As a: maintainer

I want to: reproduce builds and verify the actual distributed artifacts on
native hardware, with durable evidence for each acceptance criterion

So I can: ship the feature without relying on simulated drivers, an older
spike, missing development artifacts or undocumented distribution assumptions.

## Implementation Details:

### Deficiency register and ownership

Blocker IDs below refer to the WI 0119 workflow. Some early IDs were reused by
different steps; descriptions and this register's D-IDs disambiguate them.

| ID | Outstanding deficiency | Implementation section / required result |
|---|---|---|
| D-01 | Apple-store adapter only returns `ImageSourceBlocked` (B-03/B-23/B-68; acceptance 5 FAIL) | A: native in-process adapter and real store round trip; archive workaround is insufficient |
| D-02 | Real Docker Unix/TLS/mTLS and authenticated registry/CA/proxy/keychain acquisition untested (B-04/B-24) | A: disposable real services and exported-image corpus, failure/retry tests, cached execution with services stopped |
| D-03 | Mac ARM64/x86_64 Linux payloads unverified; production typed loader has never booted anywhere (B-01/B-02/B-15/B-18/B-31/B-32/B-34) | B: verified target inputs and actual awman boot/exec on all three hosts, including writable x86 kernel registration |
| D-04 | Guest settings, filesystem and prompt parity unverified (B-29/B-30/B-50; acceptance 6) | C: all mandatory strategies and non-root live-sharing behavior executed in guests |
| D-05 | Guest network policy, denied egress and OOM coverage missing (B-13/B-55/B-69; acceptance 6 FAIL) | D: explicit enforceable network/resource contracts and positive/negative guest tests |
| D-06 | Frontend/workflow/squad, crash, cancellation, reattachment and upgrades only partly simulated (B-10/B-18/B-21/B-33/B-50) | E: real awman dispatch with synthetic guest agents; documented and tested lifecycle limits |
| D-07 | SDK ambient config prevents coexistence; environment discovery remains a risk (B-09/B-19) | F: explicit isolated SDK configuration, fail-closed overrides, no ambient helper discovery |
| D-08 | Private route accepts unopened descriptor numbers and SDK aborts; reserved argv/version boundary needs durable tests (B-08/B-20/B-56) | F: validate inherited descriptor ownership/open state, clean sanitized errors; preserve public CLI and optimized metadata |
| D-09 | Lagging attach can drop output; import/prune and cache-path races need broader stress (R18/O4/O13 limits in final review) | E/G: explicit lag semantics, no silently corrupted ACP, concurrent import/use/prune and filesystem-race validation |
| D-10 | Pure builtin tests require verified payload/full SDK; test links have exhausted RAM/disk (B-26/B-51/B-53) | G: SDK-independent fast tests and reliable bounded feature/native test tiers |
| D-11 | Full Git diff, tracked build inputs and clean checkout unverified (B-11/B-16/B-67) | B/H: restored base/head audit, tracked `.cargo/config.toml`/lockfile/patches, native clean-checkout and unsupported-target regressions |
| D-12 | Historical SQLite cross-driver and genuine old/new catalog checks absent (B-14 deps-foundation/B-54) | H: reproducible fixtures/probes and actual SQLx/rusqlite bidirectional checks on supported targets |
| D-13 | Final LTO/stripped boot and executable/firmware tracing absent (B-05/B-20/B-57/B-58) | I: exact distributed artifacts boot and satisfy dependency, helper and distribution checks |
| D-14 | Optimized size/performance/workload evidence and distribution obligations unresolved (B-07/B-12/B-55) | I: measurements, matching Linux ABI, source/notices/relink materials and recorded distribution review |
| D-15 | Real-image compatibility and documented restrictions need closure; prior risk notes contain stale statements (B-22/B-25/B-27/B-28/B-42) | A/C/J: validate shipped templates, source/format policy, onboarding and every remaining bounded difference; reconcile the ledger against final code |

Preserve the fixes already present: R1/R2 and O1, full source-key ready lookup,
single-image SDK projection, per-reference cache metadata, bounded account-file
reads, numeric primary-GID lookup, archive/link/space checks, partial-frame
retention and build configuration corrections. R3's rejection was supported by
SDK reconciliation code; do not reintroduce redundant lifecycle maintenance on
the assumption that get/list never reconcile. Reopen a fix only with evidence.

### A. Complete image acquisition

1. Resolve Apple-store feasibility early on Apple Silicon against real supported
   service versions. Implement a versioned in-process API/XPC bridge that can
   be linked into awman. No executed `container` helper, separately distributed
   bridge executable/DSO, or scraping of undocumented private storage. Validate
   actual exported bytes, platform/config/layer identity and error handling.
   If no strict bridge is feasible, retain an explicit blocker and leave this
   item open; manual export is not completion or permission to reduce scope.
2. Exercise Docker Engine export through selected local Unix and remote TLS/
   mTLS endpoints, including multi-platform stores, older supported API versions,
   missing images, expired certificates, disconnects and truncated transfers.
   Cover registry auth challenges, trusted token realms, native credentials,
   private CAs, proxy/no-proxy and local registries with real disposable services.
   Keychain tests, where supported, must use disposable test entries and explicit
   opt-in; ordinary tests never contact a real credential store.
3. Define bounded retry/cancellation behavior for idempotent acquisition and test
   it without leaking secrets. Never silently switch source types/endpoints or
   invoke a credential helper; preserve an explicit executable-free auth path.
4. Import real Docker-save, OCI and Apple-export archives, including images built
   from every shipped agent Dockerfile. Preserve image defaults, modes, ownership,
   whiteouts, hardlinks and platform identity. Validate restrictive symlink rules
   against a real-image corpus; any relaxation requires root-confined/no-follow
   materialization tests. Resolve zstd/non-distributable-layer support explicitly
   in the format contract with tests and actionable refusals for excluded formats.
5. Stop acquisition services, deny network access and empty host PATH; execute
   cached images with actual awman. Test changed endpoint/path/reference/platform,
   source deletion, replaced archives, shared bytes with distinct selections,
   cache corruption and pre-metadata cache migration. Unselected images/tags must
   never enter the SDK store or overwrite unrelated tags.

### B. Establish reproducible native builds and first boots

1. Restore access to the actual Git base/head; review the full WI 0119 diff,
   including files outside the original ownership lists. Verify required inputs
   are tracked and no build consumes developer Cargo-cache edits or `/tmp` files.
   Preserve a commit/artifact-identified baseline of final-review fixes and logs.
2. Run target-native payload extraction/verification for all required triples.
   Record architecture, provenance, hashes, alignment, payload/address bounds,
   kernel/agent/VMM compatibility and reproducible native-library build inputs.
   Change manifest status to verified only from matching evidence.
3. Build actual awman from clean checkouts on Apple Silicon, Linux ARM64 and
   Linux x86_64. Boot the **current typed provider**, not the old symbol-loader
   probe. Assert same-executable worker launch, provider selection, guest identity,
   exit 37 and no host helper/library extraction. Verify x86 writable kernel RAM.
4. Provision and run the existing native CI jobs with accessible KVM or HVF and
   entitlements. Hardware-required jobs must fail missing prerequisites; local
   missing hardware remains SKIP/BLOCKED and contributes zero execution passes.
   Enable release targets only after their payload, build and boot gates.
5. Build/test existing backends on Windows and Intel Mac with builtin disabled;
   selecting builtin must give a precise unsupported-backend error. Retain the
   existing default runtime and independent Docker/Apple selections.

### C. Prove guest filesystem, settings and prompt compatibility

1. Run all nine image assertions and 32 mount/config assertions from the promoted
   fixture against actual awman, then extend them to the required agent matrix.
   A per-agent pairing is not evidence for unexercised strategy variants.
2. Cover Directory, Skill (named/all), Env and Context overlays in global/repo/
   workflow scopes; files/directories, ro/rw, nested destinations, conflicts,
   precedence and environment/argv boundaries. Never expose a file's parent or
   replace live sharing with a one-time copy.
3. Cover every SettingsMount family (None, Direct, Claude, Antigravity) and every
   SystemPromptMode (Append/file, AppendInline, Replace, AgentsMd, EnvFile, AddDir,
   deliberately Unsupported). Derive destinations from effective image HOME.
4. Exercise named and numeric UID/GID, primary/supplementary groups, executable
   modes and host/guest ownership differences. Verify Claude sanitized staging,
   separate config and refresh-token-free mode-0600 credentials; preserve
   Antigravity/keychain rules and HostAgentPinger's existing authorization.
5. Test atomic replacement and guest writeback with at least two simultaneous
   credential consumers, file watchers, upper-layer account replacements and
   whiteouts, hardlinks/xattrs and Mac case sensitivity. No real paid agents or
   developer credentials are necessary: use synthetic guest executables/files.

### D. Define and enforce network and resource behavior

1. Put persisted network options in data types and policy enforcement in the
   engine/runtime. Specify DNS, authenticated APIs, proxy/CA handling and how a
   guest reaches an explicitly authorized host-local MCP endpoint. Do not equate
   guest loopback with host loopback or silently inherit unenforceable policy.
2. Implement an enforceable denied-egress configuration and test both permitted
   traffic and refusal, including DNS/proxy bypass attempts and isolation between
   concurrent VMs. Test offline cached execution with actual network denial.
3. Test memory allocation **and guest OOM**, host survival and cleanup under
   pressure. Verify CPU allocation under load; vCPU count is not a fractional
   CPU quota. Keep unsupported fractional/invalid/out-of-range requests explicit
   errors with tests rather than rounding or ignoring them.
4. Keep Docker-socket access disabled unless a separately specified opt-in bridge
   is implemented with an updated security boundary and positive/negative tests.
   An ordinary Unix-socket file mount is never a bridge. Preserve agent-internal
   namespaces/seccomp/sandbox behavior; do not disable safeguards to pass tests.

### E. Complete lifecycle, transport and frontend integration

1. Exercise actual CLI, TUI and API orchestration, workflows (setup/teardown,
   headless and failure paths) and squads (dispatch/discovery/concurrency) through
   synthetic agents running in guests. Add missing end-to-end scenarios; the
   SDK driver and fake engine are not substitutes for the awman entrypoint.
2. Validate PTY resize/interrupt/terminal restoration, noninteractive stdin/EOF,
   seeded newline, argv boundaries, stdout/stderr separation, exit codes and
   binary-clean ACP with fragmented simultaneous I/O. Test slow/disconnected
   clients and full buffers. Define attach lag behavior and ensure protocol data
   cannot be silently dropped and then presented as an intact ACP stream.
3. Cover grace/keep semantics, cancellation during startup/import/exec, parent
   and worker crash, guest failure/OOM, stale locks/sockets, orphan cleanup,
   reused names, cross-session explicit stop and multi-VM ownership isolation.
   Neither delayed cleanup nor a cancelled losing start may kill another session.
4. Test live upgrade/replacement with two actual awman builds: compatible and
   incompatible protocols/catalogs, surviving workers, Linux deleted executables
   and Mac replacement/translocation behavior. Provide precise restart/recovery
   outcomes without corrupting another version's catalog.
5. Validate launcher-alive detach/reattach. Owner-exit reattachment and checkpoint
   restore currently remain unsupported: record and test the intended bounded
   capability contract or implement them if required by the agreed frontend flow.
   Do not substitute a new shell for reattachment or weaken parent watchdogs.

### F. Remove avoidable SDK/worker integration limitations

1. Supply explicit isolated SDK configuration so an installed Microsandbox's
   config does not block builtin use or alter its runtime paths. Carry the
   smallest reproducible local glue patch if needed; coordinate removal with
   WI 0120. Never solve coexistence by accepting ambient executable discovery.
2. Keep private worker argv/version/capability transport separate from public
   CLI/version behavior. Validate that inherited descriptors are distinct, open
   and appropriate before transferring ownership to the SDK; unopened fds must
   produce a clean sanitized diagnostic, not an IO-safety abort.
3. Retain early worker dispatch before normal Tokio/TUI startup, private config
   transport, lifecycle locks/watchdogs and secrets outside argv/logs. Review the
   narrow version-section unsafe-attribute exception and prove retention under
   optimization/stripping. No host pointers in worker messages.

### G. Close cache/concurrency and test-infrastructure gaps

1. Stress concurrent acquisition, publication, SDK materialization, cached
   execution and pruning across processes. Protect in-use archives/staging and
   installed identities, and recover interrupted imports without partial tags or
   deleting another session's data. Keep the existing publication-window test.
2. Exercise symlink/hardlink substitution, permissions and ownership races at
   cache/state roots and locks. Validate long/non-ASCII socket paths, concurrent
   starts and stale resources on native hosts. Bound disk and memory use before
   allocation/write; retain malicious/truncated/oversized archive regressions.
3. Make pure planning/path/naming/resource/catalog/framing tests run without
   verified payloads or the full VM SDK; restrict feature/target gating to the
   actual runtime boundary. Ensure fast-test name filters cannot hide hermetic
   source-adapter tests. Add an explicit coverage inventory for each test tier.
4. Keep builtin test targets/concurrency bounded and measure build RAM/disk use;
   fix reproducible linker OOM or disk exhaustion without silently dropping
   required coverage. Ordinary tests remain hermetic and never reuse real HOME,
   CODEX_HOME, credential stores, paid agents or source daemons.

### H. Complete SQLite and existing-backend verification

1. Make the inputs for
   `tools/oci-runtime-spike/sqlite-resolution/checks.sh` reproducibly obtainable:
   pinned source, old probe/catalog and fixture images. Preserve genuine old
   formats instead of renaming a fresh catalog. Archive sanitized evidence.
2. On all required targets verify one bundled `libsqlite3-sys 0.38.2`, unchanged
   `rusqlite 0.40.2`, the manifest-only SQLx backport, combined awman/runtime link,
   existing persistence regressions and all 27 msb migrations. Any replacement
   dependency needs the same validation and WI 0120 inventory update.
3. Exercise actual SQLx-written/rusqlite-read and rusqlite-written/SQLx-read
   transactions, rollback/locking/crash recovery and old/new catalog round trips.
   Reopening the SDK before rusqlite reads its own row is not cross-driver proof.
   Preserve database paths, schemas, migrations and pool policies; reject foreign
   catalogs without silently adopting or changing them. Confirm no SQLite DSO.
4. Run real Docker and Apple backend opt-in regression suites and compare their
   defaults, CLI argv, overlays/settings, lifecycle and frontend behavior against
   the restored base. Include error/unsupported paths and shared-contract changes.

### I. Validate distribution artifacts and costs

1. Build final release/LTO/stripped awman artifacts on each required target.
   Distribute without signing or notarization and boot the exact artifact.
   A fixture driver or old probe does not establish final-artifact boot.
2. Inspect `ldd`/`otool -L`, embedded version/provider retention and helper scans;
   trace executable and firmware access throughout boot and realistic sessions.
   Provision `strace`/allowed tracing on native runners and a suitable Mac trace.
   Missing tracing is BLOCKED, not replaced by a version-only loader listing.
3. Test minimal PATH and enforced-offline cached use, with no dependency on tools
   scripts, Cargo caches, source services or extracted host executables/DSOs.
   Document and validate the existing Linux release ABI/system-library baseline;
   keep non-system cap-ng linkage static or eliminate that dependency.
4. Finish distribution review for embedded kernel/native code: notices, matching
   source/provenance, source-offer and any required object/relink materials and
   reproducible instructions. Record the disposition in developer/release material.
5. Measure optimized size, build time, cold/warm startup, host/guest RSS and disk
   growth using a fixture with realistic git/build tools. Preserve raw inputs and
   results for all targets; do not invent performance thresholds or reuse debug
   probe size as a release estimate.

### J. Reconcile evidence, documentation and acceptance

1. Maintain a durable register mapping D-01–D-15 and every WI 0119 acceptance
   criterion to code, test name/command, target, artifact/commit hash, raw evidence
   and PASS/FAIL/BLOCKED verdict. Resolve historical stale notes explicitly:
   ready source wiring, archive identity and account lookup already changed.
   Carry R/O and B-60–B-70 fixed cases forward as regressions, not new unfinished
   implementation. Record any bounded exception and its tested user-visible error.
   Record the rationale for the existing reqwest acquisition transport, supported
   archive formats and keychain/native credential paths; do not leave the old
   plan-deviation notes as unresolved implied approvals. Inject environment and
   source settings in tests rather than depending on ambient developer state.
2. Fix builtin onboarding gaps found by actual `init`/`ready`/workflow runs:
   external-build/import guidance must refer to real provisioned Dockerfiles;
   no automatic builder or source-daemon start to run cached images. Surface
   unsupported operations as shared outcomes without losing unrelated setup/audit
   results. Keep orchestration in commands and runtime policy in the engine.
3. Re-run final adversarial review against the complete diff. A fixture-only run,
   hardcoded capability declaration or CI job that never ran is not evidence.
   Keep missing hardware/services as BLOCKED and missing required code
   as FAIL. Neither status satisfies a mandatory checkbox.

## Acceptance criteria

- [ ] D-01/D-02: every mandatory source adapter works through actual services,
  including strict Apple-store and local/remote Docker; real private-registry
  auth/CA/proxy and failure/retry behavior pass; cached execution needs no source.
- [ ] D-03/D-11: clean-checkout actual awman builds and boots through the current
  embedded provider on Apple Silicon and both Linux KVM architectures; payloads,
  local patches, tracked build inputs and native dependencies are reproducible.
- [ ] D-04: all required overlay/settings/prompt strategies and image defaults/
  overrides pass in non-root guests, including live atomic refresh/writeback with
  multiple consumers; no missing strategy is declared a parity exception.
- [ ] D-05: tested guest network/auth/DNS/MCP/proxy/CA/denied-egress and memory/CPU/
  OOM contracts exist; unsupported limits/socket-bridge requests fail explicitly.
- [ ] D-06/D-09: actual frontend/workflow/squad, PTY/ACP, cancellation/reattachment,
  crash/upgrade/multi-VM and cache concurrency scenarios pass without affecting
  another session or silently truncating protocol streams.
- [ ] D-07/D-08: isolated SDK coexistence and worker descriptor validation pass;
  no ambient runtime discovery, host execution, secret leakage or silent fallback.
- [ ] D-10: pure builtin tests run in the fast hermetic tier; required feature and
  hardware tiers complete without known linker/disk failures or hidden test skips.
- [ ] D-12: exact required bundled SQLite resolution, migrations, genuine
  bidirectional driver transactions and old/new catalog checks pass on every
  supported native target; no database-contract change or SQLite DSO.
- [ ] D-13/D-14: the final optimized/stripped/distributed artifacts boot, retain
  the provider, pass native access/dependency/helper scans and Mac release distribution checks; ABI, licensing materials and measurements are recorded.
- [ ] D-11/D-15: Windows/Intel Mac existing builds and real Docker/Apple backend
  regressions pass; onboarding, image-format policy and bounded parity limits are
  implemented or explicitly tested/documented within WI 0119's allowed scope.
- [ ] `make pre-push`, builtin-feature Clippy `-D warnings`, `make test-builtin`,
  enforced hardware jobs and real Docker/Apple/store suites pass at the reviewed
  revision. Required-hardware skips are not counted as successful execution.
- [ ] D-01–D-15 and all nine WI 0119 acceptance criteria have final linked
  evidence with no unresolved mandatory FAIL/BLOCKED verdict. WI 0119 checkboxes,
  this item, docs/specs and WI 0120's patch inventory agree with that evidence.

## Edge Case Considerations:

- Preserve rejection of wrong architecture, digest mismatch, unsafe unselected
  images, traversal/absolute paths, symlink/hardlink escapes, malformed whiteouts,
  truncated transfers and resource exhaustion before partial publication.
- Source selection and credential scope remain explicit even when names/bytes
  coincide. Redact secrets in errors, probes, traces, logs and support artifacts;
  use synthetic sentinels to test these paths.
- Existing per-source cache references, retained stopped VMs and catalogs from
  older awman builds need tested migration/refusal/recovery behavior.
- A slow frontend, replaced executable, locked catalog, entitlement failure or
  denied KVM access must produce a bounded useful outcome without host execution,
  terminal corruption, runtime downloads or weakening another session's isolation.

## Test Considerations:

- Keep a table separating ordinary hermetic, actual SDK materialization, real
  store, native guest, frontend end-to-end and final distribution-artifact tests.
  Each row names the assertion, fixture, target and raw result, not only a suite.
- Run `make pre-push` and builtin-feature Clippy after implementation. On native
  runners supply the promoted fixture and run
  `AWMAN_TEST_BUILTIN_REQUIRE_HW=1 make test-builtin`; add explicit opt-in gates
  for new destructive store/network tests consistent with existing test isolation.
- Run Docker via `make test-full` and Apple via `AWMAN_TEST_APPLE_CONTAINER=1`
  with the isolated test runner. Keep service credentials/disks disposable.
- Re-run the original SQLite spike and preserved final-review regressions.
  Fresh implementation tests must exercise failures as well as success, including
  fragmented streams, repeated/concurrent refresh, ENOSPC and interrupted commit.
- Native host access, KVM/HVF, fixture images, distribution artifacts
  and disposable stores are prerequisites. Arrange them early; if unavailable,
  report the exact block and leave the affected criteria open.

## Codebase Integration:

- Follow `aspec/architecture/design.md`, `aspec/architecture/security.md`,
  `aspec/devops/localdev.md` and `aspec/devops/cicd.md`.
- Core paths: `src/data/config/`, `src/engine/agent_runtime/`,
  `src/engine/container/builtin/`, `src/engine/oci/`, `src/engine/ready/`,
  `src/engine/overlay/agent_settings.rs`, agent matrix, command orchestration,
  `tests/builtin_runtime/`, `tests/oci_import/`, native CI/release workflows,
  `third_party/`, and payload/SQLite tools.
- Data owns persisted configuration; engine owns import/runtime/overlay policy;
  commands orchestrate; frontends render shared outcomes. The entrypoint retains
  only narrow private-worker routing. Preserve the local patch boundary and
  original licenses; no mutable forks or installed-runtime dependencies.

## Documentation

- Update existing installation/runtime/image/overlay/settings/headless/workflow/
  squad/cleanup/troubleshooting guides after behavior is verified. Remove preview
  limits only when the associated native or distribution evidence exists.
- Update architecture/security/CLI/local-test/CI/release specs for implemented
  network policy, lifecycle bounds, native gates and any new local patch. Regenerate
  `docs/14-command-reference.md` if the command catalogue changes.
- Keep technical provenance, blocker decisions, measurements and evidence in
  work-item/review/developer material. Do not create a work-item-specific user guide.
