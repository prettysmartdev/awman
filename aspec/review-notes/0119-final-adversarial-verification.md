# WI 0119 final adversarial verification — 2026-09-24

Historical review snapshot preserved in the repository on 2026-09-25.
[WI 0121](../work-items/0121-complete-builtin-runtime-and-native-verification.md)
owns completion of the outstanding deficiencies. Log filenames below refer to
the original workflow artifacts; their availability is not assumed by the
follow-up, which requires durable, revision-identified evidence for closure.

**Result: FAILED acceptance; do not mark WI 0119 complete or release this as a
verified builtin runtime.** The mandatory Apple-store adapter is an explicit
stub. Required network/OOM/workflow/squad coverage is missing. No production
guest boot, optimized provider boot, or signed-artifact boot was executed here.
The earlier Apple Silicon spike is not evidence for the current typed loader.

## Scope and evidence limits

Reviewed the workflow plan, change notes, both finding lists, resolutions,
blockers, test-coverage notes, and current implementation/tests/specs. Audited
prior log results separately in `final-prior-log-audit.txt`. Re-downloaded the
three pinned patched crates, verified their published archive checksums, applied
each checked-in diff, and compared its result with the vendored source.

**Full Git diff and clean-checkout verification are BLOCKED.** `/workspace/.git`
points to `/Users/cohix-studio/Workspaces/prettysmart/awman/.git/worktrees/0119`,
which is not mounted. There is no accessible base object database. Current-file
review and regression tests cannot establish an exact old/new diff or prove
that all required files will be committed. No commit was made. In particular,
confirm `.cargo/config.toml` is tracked when the real worktree is restored.

Host: Linux ARM64, Rust 1.94.0. No `/dev/kvm`, macOS, x86_64 host, Docker Engine,
Apple store, or production signing credentials. Hardware-gated tests returning
success after an internal SKIP/BLOCKED are **not** execution passes.

## Acceptance criteria

| # | Criterion | Verdict | Evidence and remaining requirement |
|---|---|---|---|
| 1 | Single awman executable on all required targets; unsupported builds preserved | **BLOCKED** | ARM64 feature binary and actual SDK image materialization link/run in `make test-builtin`; `builtin_version_output_is_unchanged`, worker-route, dependency-listing and helper-scan tests. No current guest boot. Mac/x86_64 payloads unverified and disabled in release matrix; unsupported-platform native builds not run. |
| 2 | Owned reproducible patches/payloads/native dependencies; clean checkout | **BLOCKED** | `verify-vendor.sh` / `final-vendor.log`: all three sources exactly match pinned crate plus recorded patch. `final-payload-verification.log`: Linux ARM64 kernel and agent checksums OK. Fixed `.cargo` ignore rule. Other payloads and clean checkout unverified. |
| 3 | One bundled SQLite, unchanged rusqlite, full recorded spike checks on supported targets | **BLOCKED** | `final-sqlite-tree.txt`: exactly `libsqlite3-sys 0.38.2`, `rusqlite 0.40.2`, patched `sqlx-sqlite 0.9.0`; combined feature link and in-tree persistence/catalog/migration tests pass. Original old probe/fixture checkout is absent; current rusqlite-written-row test does not prove SQLx readback. Complete historical cross-driver/old-catalog checks and other targets remain unrun. |
| 4 | Native guest execution on Apple Silicon and both Linux architectures | **BLOCKED** | No KVM or Mac hardware; typed provider has never booted in this workflow. Build, archive import, config parsing and spike logs are insufficient. |
| 5 | Every mandatory source adapter and real store round trips/offline reuse | **FAIL** | `src/engine/oci/apple_store.rs` only returns `ImageSourceBlocked`; `apple_store_is_blocked_without_running_anything` tests refusal. Real Docker Unix/TLS and authenticated registry round trips separately BLOCKED. Wire doubles and cached-source tests pass but cannot replace these. |
| 6 | All required config/overlay/prompt/runtime/frontend/squad contracts tested | **FAIL** | Hermetic matrix, lifecycle and transport tests pass, including added skill/env and partial-frame cases. No guest network-policy contract, positive denied-egress/OOM tests, or real workflow/squad integration test. Existing guest tests separately BLOCKED on hardware; CPU/MemTotal checks do not prove memory OOM or fractional CPU semantics. |
| 7 | Optimized/LTO/signed provider artifacts boot and distribution/security checks pass | **BLOCKED** | CI contains artifact inspection and explicit hardware jobs, but neither is execution evidence. No final optimized/signed boot, notarization check, release measurement or guest-path executable/firmware trace here. |
| 8 | Accurate docs/specs, existing Docker/Apple regressions, no SBX adoption | **PASS** (hermetic scope) | Existing guides describe preview status, missing Apple adapter and parity limits. Corrected release/cache/security wording. `make pre-push` includes Docker/Apple command/model regressions and generated command-reference freshness. Real Docker/Apple behavior and exact old/new diff remain BLOCKED; no claim of native regression certification. |
| 9 | WI 0120 precise inventory/removal path; no upstream dependency | **PASS** | `aspec/work-items/0120-upstream-embedded-kernel-support.md` and each vendored `PATCH.md` separately inventory SQLx bound backport, typed msb_krun loader, guest-agent build-input patch and awman worker glue with replacement/removal gates. Reproducible diffs verified independently. |

The WI 0119 acceptance checkboxes match: only 8 and 9 checked; 5 and 6 explicitly
FAIL, all other unchecked criteria BLOCKED.

## Revalidation of every previous finding

FIX means the identified code defect is fixed; it does not imply native parity.

| Finding | Final disposition | Verification |
|---|---|---|
| R1 | FIX confirmed | Optional startup fd, required distinct attached descriptors; `attached_sdk_descriptor_shape_is_accepted`, `sdk_attached_argv_parses_and_validates`, real-binary worker tests. |
| R2 | FIX confirmed | `msb_driver::create_error` matches typed duplicate/image/I/O/database/config/startup errors without forwarding SDK config text. |
| R3 | REJECT upheld | SDK 0.7.2 `backend/local/sandbox/mod.rs` get/list call reconciliation (around 494/546/717); `pid_from_run` filters dead PIDs. Added awman summary hardening is present. The original assertion that get/list never reconcile is false. Actual crash recovery remains a hardware gate. |
| R4 | FIX confirmed | Create/finish/remove do not hold the global catalog lock; bounded `try_lock`, separate import lock. `busy_lock_times_out_with_a_clear_error` and lock/concurrency tests. |
| R5 | FIX extended | Named passwd/group resolution and mount owner override present. Fixed numeric UID without explicit GID skipping lookup and account size checked only after full allocation. Added numeric-primary-GID assertion plus real EROFS normal/oversized/invalid-upper-file tests; lookup now streams with a cap after checking inode size. Guest mode-0600 access still BLOCKED. |
| R6 | FIX confirmed | Interrupt, up to Docker-equivalent 10 s grace, early exit handling, then kill/finish with `remove_on_exit`. Bridge cancellation/keep/stale-owner tests. Original `Duration::ZERO` was start delay, not cancel grace. |
| R7 | FIX confirmed | Explicit stop accepts the record's owner/generation across sessions; automatic cleanup remains launch-owned. `explicit_stop_works_across_sessions_but_not_for_a_reused_name`. |
| R8 | Documentation FIX; boot BLOCKED | Patch README explicitly distinguishes the new typed loader from the booted spike. First native boot remains required. |
| R9 | FIX confirmed; boot BLOCKED | x86_64 uses a writable aligned process-lifetime copy. `writable_copy_is_aligned_and_identical`; no x86_64 KVM boot. |
| R10 | FIX confirmed | Ambient msb config refusal names its path and gives actionable move/retry/explicit runtime guidance. Coexistence isolation remains a limitation. |
| R11 | FIX confirmed | Summary requires owner label; attach uses checked lookup, not indexing. Missing-label/protocol refusal tests. |
| R12 | FIX confirmed | Seeded prompt includes trailing newline; `noninteractive_seeded_prompt_is_queued_then_stdin_closes`. |
| R13 | FIX confirmed | Existing parent canonicalized, symlinked leaf refused; `symlinked_parent_is_resolved_but_symlinked_leaf_is_refused`. |
| R14 | FIX confirmed | Imported USER wins over Dockerfile; background uses image user. `background_sandbox_env_lives_in_memory_and_exec_runs_as_the_image_user`. |
| R15 | FIX confirmed | Only `machine` plus config-fd selects private worker route; `builtin_worker_route_is_hidden_from_humans`. Private info protocol remains separate from public version. |
| R16 | FIX confirmed with limit | Deleted/missing executable produces restart guidance; `detects_a_replaced_executable`. Not evidence of all Mac/in-place upgrade variants or live cross-version reattachment. |
| R17 | FIX extended | Unverified Mac/x86 payload targets disabled, non-hardware CI named honestly. Fixed Windows release Bash shell and hardware test build target selection. Native jobs remain unexecuted. |
| R18 | FIX confirmed and related defect fixed | Broadcast lag no longer claims agent failure; `lagging_attach_client_still_receives_the_real_exit`. Separately fixed cancellation-unsafe partial frame reads in both attach directions. Lag may still drop output; no lossless overloaded-stream guarantee. |
| R19 | FIX confirmed | `nix::unistd::geteuid()` replaces temporary-file UID inference. |
| O1 | FIX confirmed | Credentials sent only to trusted token origin; foreign realm receives anonymous request. `foreign_token_realm_never_receives_credentials`, realm/auth tests. |
| O2 | REOPENED → FIX | Previous fix missed endpoint/path-only changes. Configured sources now always reach full-key acquisition cache; unchanged installed content avoids SDK reimport. `import_runtime_rechecks_changed_locators_with_the_same_kind_and_reference` covers two archive paths, Docker endpoints and registry hosts. |
| O3 | FIX confirmed | Lookup checks archive SHA-256 as well as size; `a_same_length_corruption_is_a_miss_not_a_hit`. |
| O4 | FIX confirmed | Commit/prune share exclusive cache lock, lookup shared. Added `prune_waits_for_publication_of_an_in_flight_archive`: prune cannot remove an archive in the rename-to-metadata publication window. Broader in-use import/prune behavior still needs multi-process stress. |
| O5 | FIX confirmed | Effective cross-layer symlink graph/whiteout handling; `a_lower_layer_symlink_cannot_be_written_through`. Found separate hardlink-parent replacement gap and closed it. |
| O6 | FIX confirmed, missing test added | Absolute entry/hardlink target rejected; added actual absolute-hardlink case to `layer_attacks_are_rejected`. |
| O7 | FIX confirmed | Per-layer cap then checked total bounded by archive cap; overflow/limit tests. |
| O8 | FIX confirmed | Synthetic manifest hashes config and ordered layer content/size; `legacy_save_identity_depends_on_content_not_entry_names`. Corrected stale source comment. |
| O9 | FIX confirmed | Indexed identity requires config plus ordered layers; `modern_save_manifest_digest_must_match_config_and_layers`. Separate SDK selection bypass fixed below. |
| O10 | FIX confirmed | Modern Engine export verifies exported platform rather than rejecting unrelated default inspect variant; `a_default_variant_mismatch_still_exports_the_native_platform`. Real engines BLOCKED. |
| O11 | FIX confirmed | Import ready provisions Dockerfile before external-build hint and never calls builder; ready tests. |
| O12 | FIX extended | Directory replacement removes regular target. Added rejection of hardlinks through a target's replaced symlink parent; `layer_attacks_are_rejected`. |
| O13 | FIX confirmed (stated scope) | Pre-existing symlinked cache directories refused before chmod; `a_symlinked_cache_directory_is_refused_not_followed`. Not a complete hostile same-user race audit. |
| O14 | REOPENED → FIX | Earlier reservation checked too late for a chunk crossing the 16 MiB window. Added pre-crossing check and changing-disk double: `a_write_crossing_the_reserved_window_checks_disk_before_writing`. |
| O15 | FIX confirmed, missing test added | Detailed `availability()` reason reaches ready; `import_runtime_preserves_the_concrete_availability_error`. |

## Additional defects fixed in this step

1. **Critical: validator/SDK image selection mismatch.** SDK imports all archive
   images and assigns the requested tag to the first. Awman validated only the
   selected image. Added a private single-image tar projection before SDK import:
   one selected manifest/tag, original config/layer bytes, no unrelated images
   or aliases. Both Docker-save and OCI multi-image regressions put an unsafe
   image first and a valid selected image second. Feature branch uses the actual
   SDK importer/materializer without a VM.
2. **Critical: attach frames corrupted by simultaneous I/O.** `read_frame` was
   cancelled by the opposite branch of `tokio::select!`, discarding partial
   headers/payloads. Client and server retain a pinned read until completion.
   `partial_attach_frames_survive_simultaneous_output_and_input` exercises both.
3. **Cache provenance collision.** Identical archive bytes shared one metadata
   record, overwriting each source's identity, selected config and fingerprint.
   References now retain their own metadata; prune retains bytes used by any
   retained selection. `shared_archive_keeps_each_references_identity_config_and_fingerprint`.
   Older reference records require one reacquisition; documented.
4. **Build portability.** `.cargo/config.toml` was ignored despite owning payload
   locations/static cap-ng flags. It is now excepted from `.cargo/*`. Windows
   release command explicitly uses Bash. Hardware feature build uses the bounded
   set of test targets, matching `make test-builtin`, avoiding known linker OOM.
5. Added missing skill named/all and env passthrough/literal-precedence tests;
   corrected coverage claims instead of treating a per-agent matrix as every
   Cartesian combination. No test touched real credential stores or paid agents.
6. Added a concurrent prune regression for the incomplete publication window;
   prior commit/lookup/prune tests had only been sequential.
7. Account-file size was checked after an unbounded SDK allocation. Lookup now
   checks inode size and reads through a bounded stream. Real EROFS test
   `image_accounts_are_bounded_and_an_invalid_upper_file_never_falls_back` covers
   ordinary accounts and oversized/invalid UTF-8 upper files without fallback.

## Security/docs spot-check

Worker routing precedes clap/Tokio. VMM executes in the re-executed child, not a
parent thread. Runtime selection has no agent-execution fallback; the existing
non-runtime-command fallback is only inert orchestration. HostAgentPinger's
authorization boundary was not broadened. SDK control logs are suppressed;
exec secrets travel IPC, not worker argv/config labels. Existing secret-sentinel,
worker refusal and binary framing tests pass. File mounts remain individual
files; socket mounts and `--allow-docker` are explicitly refused. No parent-dir
exposure or installed msb/container helper was introduced.

Updated existing user guides only, with no WI-specific guide. Release wording
now describes configuration, not an established published/boot-tested asset.
Cache documentation describes full source identity and selected-image projection.
Removed the builtin claim that host escape necessarily requires a hypervisor exploit;
shared-filesystem/device implementations are also security boundaries. Security
spec records the SDK import projection. Command-reference freshness is checked
by `markdown_reference_matches_committed_docs`; no command catalogue changed.

## Verification commands

Final command results are recorded below. All commands use
`PATH=/usr/local/cargo/bin:$PATH`; Cargo concurrency is limited to two jobs.
Logs are evidence for this workspace, not clean-checkout or native-boot proof.

`make docs-reference` regenerated the command reference successfully (one
generator test passed). Before/after SHA-256 files are identical, so no generated
documentation change was needed (`final-docs-reference.log`,
`final-docs-before.txt`, `final-docs-after.txt`).

| Command | Final result | Evidence |
|---|---|---|
| `make pre-push` | PASS, exit 0: architecture lint, fmt, Clippy `-D warnings`, full isolated default suite; library 3,062 passed, 1 ignored, no failures | `final-pre-push.log` |
| `make test-builtin` | PASS, exit 0 for hermetic checks: library 3,147 passed, 1 ignored; binary 12, builtin integration 59, data 107, OCI 2, example 7. Hardware-gated returns inside these counts do not prove execution | `final-test-builtin.log` |
| `cargo clippy --all-targets --features builtin-runtime -- -D warnings` | PASS, exit 0; pinned upstream SQLx emits 40 dependency deprecation warnings (not a warning-free dependency graph) | `final-clippy-feature.log` |
| `make docs-reference` | PASS, generator 1 passed; before/after SHA-256 identical | `final-docs-reference.log`, hash files |
| `bash /awman/context/workflow/verify-vendor.sh` | PASS, checksum-verified published crates plus diffs match each of the three vendored sources | `final-vendor.log` |
| Payload verifier, Linux ARM64 | PASS, kernel and agent verified | `final-payload-verification.log` |
| `cargo tree --locked --features builtin-runtime -i libsqlite3-sys` | PASS, one native SQLite package, exact required versions | `final-sqlite-tree.txt` |
| Existing `builtin_runtime` test executable, exact `hardware::builtin_hw_image_defaults_hold_and_the_guest_is_not_root`, with `AWMAN_TEST_BUILTIN=1 AWMAN_TEST_BUILTIN_REQUIRE_HW=1` in an empty environment | **BLOCKED**, expected test-process exit 101: `/dev/kvm is not usable (No such file or directory)`; zero execution passes | `final-hardware-gate.log` |
| `git diff --stat`, `git status --short` | **BLOCKED**, exit 128: missing host worktree path | B-67; scope statement above |

The hardware-required test was deliberately run separately to verify that an
enforced gate fails rather than converting missing hardware to a passing run.

Repository files changed during final review (existing workflow changes retained):

- `.gitignore`, `.github/workflows/release.yml`, `.github/workflows/test.yml`
- `src/engine/ready/mod.rs`
- `src/engine/oci/archive.rs`, `cache.rs`, `verify.rs`
- `src/engine/container/builtin/backend.rs`, `backend/matrix_tests.rs`,
  `msb_driver.rs`, `exec_bridge.rs`
- `docs/00-getting-started.md`, `docs/11-runtimes.md`
- `aspec/architecture/security.md`,
  `aspec/work-items/0119-strict-embedded-microsandbox-runtime.md`

Workflow artifacts: this report, README index, appended blocker/coverage notes,
`verify-vendor.sh`, `final-prior-log-audit.txt`, dependency/payload/vendor logs,
test/lint/reference-generation logs and command-reference before/after hashes.

## Remaining blockers and native manual test plan

- Implement and validate a versioned in-process Apple-store adapter; manual
  `container image save` plus archive import does not satisfy it (B-03/B-23/B-68).
- Supply guest network/auth/DNS/host-local MCP/proxy/CA/denied-egress contracts
  and tests, OOM enforcement and complete real frontend/workflow/squad tests.
- Verify target payload provenance/checksums natively for Mac ARM64 and Linux
  x86_64 before enabling those builds. Boot the current typed loader on all
  three required targets (B-01/B-02/B-15/B-18/B-31/B-32).
- Restore Git worktree/base, inspect the actual full diff, confirm required
  inputs are tracked, and perform clean-checkout builds including Windows and
  Intel Mac regressions (B-11/B-67).
- Run original SQLite migration, cross-driver and old/new catalog probes on
  all supported targets; retain native dependency listings (B-14/B-54).
- Produce final optimized/LTO/stripped artifacts and Mac signing/distribution
  evidence; artifact version/section scans are insufficient boot evidence.

Manual plan for disposable native hosts:

1. From a clean checkout, build pinned payloads/static libraries with Rust
   1.94.0. Linux ARM64 and x86_64 must have readable/writable `/dev/kvm`; on Mac
   verify Apple Silicon/HVF and sign the **actual optimized awman** with the
   hypervisor entitlement. Preserve checksums and exact dependency listings.
2. Build the promoted OCI fixture with `tools/oci-runtime-spike/fixture` and set
   `AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE` to its absolute path. Run `make pre-push`,
   then `AWMAN_TEST_BUILTIN_REQUIRE_HW=1 make test-builtin` on each host. Use
   isolated HOME/config/state and synthetic agents/credentials. Fail missing
   prerequisites; never count a skipped test.
3. Repeat all nine image and 32 mount/config fixture assertions against actual
   awman guest sessions: defaults/overrides, named/numeric non-root UID/GID,
   supplementary groups, mode-0600 settings, every overlay/settings/prompt family,
   nested ro/rw mounts, atomic refresh with two consumers, writeback, watchers,
   hardlinks/xattrs and Mac case behavior.
4. Exercise actual CLI, TUI, API, ACP, workflows and squads: binary stdin/EOF,
   separate stdout/stderr, exit 37, simultaneous partial frames, PTY resize,
   interrupt/grace/keep, detach/reattach, owner/worker crash, interrupted boot,
   OOM, name collisions, cross-session stop, concurrent VMs and live upgrades
   using two awman builds. Verify other sessions survive every cleanup.
5. Acquire from disposable authenticated/private-CA/local registries, local
   Docker Unix sockets, remote TLS Docker stores and the implemented Apple
   adapter. Test retry/failure/truncation, wrong platform, malicious multi-image
   selection and credentials. Stop stores, block network and empty PATH; run
   cached sessions without any source/runtime helper.
6. Verify CPU/memory allocation and OOM, DNS/API auth/proxy/CA/host MCP and denied
   egress; ensure socket bridge remains rejected unless a separately reviewed
   explicit bridge is implemented. Run realistic git/build workloads and record
   cold/warm startup, RSS, disk growth and optimized size without invented targets.
7. Trace executable/firmware access during boot and sessions; inspect `ldd` or
   `otool -L`, state files and helper-extraction scans. Repeat with final LTO,
   stripped/signed/distributed artifact and minimal PATH. Run full SQLite spike
   and existing Docker/Apple opt-in regressions. Archive raw logs with hashes.
