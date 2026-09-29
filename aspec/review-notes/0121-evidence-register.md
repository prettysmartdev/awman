# WI 0121 evidence register (sections J.1 and J.3)

**Scope amendment (2026-09-28):** the user removed all signing and notarization
requirements. This includes release credentials and explicit ad-hoc signing
steps. Earlier reports/logs retain their historical wording; those requirements
are superseded, not outstanding blockers. Native boot, distribution artifact,
tracing, dependency and measurement requirements remain.

**Audit date: 2026-09-28. Overall runtime acceptance: NOT ACCEPTED.** Mandatory implementation is
missing or defective, and native/service/distribution verification is blocked.
All nine WI 0119 boxes and all WI 0121 acceptance boxes remain unchecked.
A fixture, capability declaration, configured CI job, skip, or preflight is
never evidence of guest/service/distribution execution.

The latest user instruction authorizes resource-qualified follow-ups: [WI 0122](../work-items/completed/0122-native-apple-image-store-bridge.md) owns the missing Apple-store implementation; [WI 0123](../work-items/0123-builtin-native-verification-and-release-closure.md) owns remaining native scenarios, full-diff/clean-build verification, services and release closure. These are open assignments awaiting capable runner access, not executed jobs. Their creation does not satisfy an original mandatory gate. The latest final verification section below supersedes earlier snapshot descriptions.

Scope split and resolution (2026-09-29): [WI 0123](../work-items/0123-builtin-native-verification-and-release-closure.md) ships `builtin-experimental` on Apple Silicon macOS only, ad-hoc signed, validates the Apple store, archive and normal-case registry sources, and closes this work stream, resolving WI 0119 and WI 0121. When it completes, each row here must be PASS with linked evidence or **Deferred (WI 0124, optional)**. [WI 0124](../work-items/0124-builtin-linux-and-docker-gates.md) holds Linux KVM, Docker Engine and registry edge-case work as optional future work.


## Reviewed identity and evidence rules

**Awman revision and complete diff: UNKNOWN / BLOCKED.** The baseline records
no accessible base or HEAD. `.git` points to the absent
`/Users/cohix-studio/Workspaces/prettysmart/awman/.git/worktrees/0121`.
Fresh `rev-parse`, `status` and `diff` attempts exit 128 in
[identity-and-prerequisites.log](0121-evidence/raw/final/identity-and-prerequisites.log).
No repository was recreated and no upstream hash is presented as an awman commit.
[baseline-comparison.log](0121-evidence/raw/final/baseline-comparison.log)
compares current files to the planning hashes; it is **not** a Git diff and
cannot establish tracking, historical additions/deletions, or a clean checkout.
[source.sha256](0121-evidence/raw/unsigned-policy/source.sha256) identifies the latest
reviewed present source/config/test/document files. Native target matrix:
Linux ARM64/KVM, Linux x86_64/KVM, Apple Silicon/HVF. Only Linux ARM64 is
available here, without `/dev/kvm`.

Available handoffs were inspected, not trusted. The
[Claude-area review](0121-evidence/raw/final/review-claude-work.md) has 16
findings; each is dispositioned in
[finding-dispositions.md](0121-evidence/raw/final/finding-dispositions.md).
The [latest finding dispositions](0121-evidence/raw/closure/finding-dispositions.md)
supersede earlier implementation descriptions. The
[shared request dispositions](0121-evidence/raw/closure/shared-edit-requests.md)
record every applied change or resource-qualified follow-up; ownership transfer
is not mandatory acceptance.
The [Codex-area fixes](0121-evidence/raw/final/fixes-codex-area.md) explicitly
remain interim. `review-codex-work.md` and `fixes-claude-area.md` were absent;
there is no complete paired review/fix audit. Every shared request has a
[final disposition](0121-evidence/raw/final/shared-request-dispositions.md),
including explicit unresolved FAIL/BLOCKED outcomes. None is silently closed.

Earlier raw reports remain under `0121-evidence/raw/`. The
[pre-final register](0121-evidence/raw/final/register-before.md) is a historical
snapshot, superseded by this file. Previously reported PASS counts have been
withdrawn where the test was not witnessed in this verification or its claimed
artifact no longer exists. In particular, the old SQLite transcript names
`/tmp/awsq.qmx2az`, whose probe/results tree is absent. It remains a historical
report, not a current full-spike PASS. Earlier 36-test OCI reports included
four external skip paths, not 36 executed service checks.

## Verification tiers

| Tier | Executed scope / fixture | Current result and raw evidence |
|---|---|---|
| Ordinary hermetic | Default build on Linux ARM64; pure builtin plans, resource validation, cache/archive invariants, process/socket tests, generated OCI archives and loopback transport doubles | See final commands below. Service/guest skip paths are excluded from acceptance. Real-store tests are opt-in; removal of the broad Docker fast filter restores hermetic adapter discovery. |
| Actual SDK without guest boot | Feature Linux ARM64 build, isolated SDK config, inherited descriptors, debug executable metadata/dependencies, SDK catalog materialization | [builtin-hermetic.log](0121-evidence/raw/final/builtin-hermetic.log), [sdk-and-builtin-lib.log](0121-evidence/raw/final/sdk-and-builtin-lib.log). These checks do not establish final artifacts, guest boot, native coexistence, or full old/new SQLite probe transactions. |
| Real store and corpus | Disposable Unix/TLS/mTLS Docker, authenticated private-CA registry/proxy/keychain, format × template corpus | **BLOCKED execution**, and missing scenarios are **FAIL**. [services-required.log](0121-evidence/raw/final/services-required.log), [corpus-required.log](0121-evidence/raw/final/corpus-required.log) enforce missing prerequisites. No anonymous endpoint or synthetic archive is promoted to a real-service PASS. |
| Native guest | Current provider, non-root strategies, refresh/writeback, DNS/auth/proxy/CA/egress, CPU/memory/OOM, source-free cached execution | **BLOCKED**: [required hardware log](0121-evidence/raw/final/hardware-required.log); preflights for [ARM64 Linux](0121-evidence/raw/final/native-aarch64-unknown-linux-gnu.log), [x86_64 Linux](0121-evidence/raw/final/native-x86_64-unknown-linux-gnu.log), [Apple Silicon](0121-evidence/raw/final/native-aarch64-apple-darwin.log) each exit 2. Registration and driver builds are not native execution. |
| Frontend end-to-end | Actual awman CLI/TUI/API, workflow/squad, PTY/ACP, cancellation/reattach/crash/upgrade/multi-VM | **FAIL**: only an actual CLI smoke body exists; full required matrix absent. That smoke is now registered but hardware-blocked. Genuine old/new awman artifacts are absent. |
| Distribution | Optimized/LTO/stripped artifact guest boot, provider retention, helper/dependency/native access traces, Mac distribution artifact, ABI/licenses/measurements | **BLOCKED**: [distribution-required.log](0121-evidence/raw/final/distribution-required.log). No final artifact, workload trace or measurement exists. Debug metadata and hardcoded `host_helpers: false` are not proof. |

## D-01 through D-15

Every row refers to the unavailable awman commit and the current source manifest
above, unless a concrete artifact is explicitly identified. A row's test names
identify the assertions to run, not an automatic PASS.

| ID | Code and test/command | Final evidence, remaining gap and verdict |
|---|---|---|
| D-01 | `src/engine/oci/apple_store.rs` (safe protocol, release pins, selection, output adoption), `src/engine/oci/apple_xpc.rs` (binary-only libxpc transport); `apple_store::tests`, `apple_store_is_version_gated_and_blocked_without_a_bridge` (`tests/oci_import`), native `apple_xpc::tests::apple_native_*`, `builtin_hw_actual_awman_apple_store_import_runs_offline_after_service_stops` (WI 0122) | The in-process, version-gated adapter ran natively on Apple Silicon, with all logs from one frozen tree ([WI 0122 evidence](0122-evidence/notes.md)). 0.12.0 was the installed release; disposable 1.4.1 and 1.5.0 pass and 1.3.1 is refused. Covered: cancellation, concurrency, a helper crash with retry, and a stopped service. All nine agent templates plus the project image export and validate. A `sandbox-exec` run proves no exec or private-store access. Actual awman imports from the Apple store, the service stops, and the cached image runs in a builtin guest with network `none`. **PASS**. The guest runs used a locally ad-hoc-signed artifact (hypervisor entitlement); distribution signing stays with WI 0123. |
| D-02 | `oci/{docker_engine,registry,retry,transport,sources}.rs`; `tests/oci_import/{cancellation,real_stores}.rs`; production Ready import | Caller/drop/Ctrl-C cancellation now reaches acquisition, projection and import locks; HTTP futures close peer sockets during header/body/token waits. [Acquisition notes](0121-evidence/raw/closure/acquisition/notes.md), [OCI execution](0121-evidence/raw/closure/focused-oci.log). Real Unix/TLS/mTLS and required auth/private-CA/observed-proxy scenarios exist, but did not execute. Real expiry/disconnect/retry and source-stop cached guest scenarios still need completion in WI 0123. **FAIL** for incomplete mandatory scenarios; real-service execution **BLOCKED**. SDK mutation finishes coherently before cancellation is reported; no immediate-abort claim. |
| D-03 | `builtin/embedded/`, `build.rs`, `tools/msb-payloads/`, `tools/native-builtin-ci.sh`; `lifecycle::builtin_hw_actual_awman_cli_synthetic_agent_exit_37` | Feature build/tests use available ARM64 payloads. Prior [payload verification](0121-evidence/raw/build-tooling/payload-linux-arm64-verify.log) is historical preparation, not boot. Corrected stale native selector; actual entrypoint test now discovered in [inventory](0121-evidence/raw/final/builtin-inventory.log). All native preflights and complete-diff/clean-checkout identity unavailable. **BLOCKED**. | WI 0122 addendum: the first native Apple Silicon guest boots (actual-awman exit-37 smoke, offline Apple-store import, offline denial) passed after fixing four latent macOS defects ([notes](0122-evidence/notes.md)). They used a locally entitlement-signed debug artifact, so the D-03 verdict is unchanged.
| D-04 | `tests/builtin_runtime/guest_compat.rs`, `builtin/backend/matrix_tests.rs` | Registered actual-awman non-root nine-agent × named/all-skills scenario covers shipped settings/prompt families, HOME, overlays and writeback. [Matrix notes](0121-evidence/raw/closure/matrix/notes.md). Its native run fails prerequisites: zero guest executions. Artificial descriptor strategy combinations, live atomic refresh with multiple consumers and complete defaults/overrides still need WI 0123 implementation. **FAIL**, native execution **BLOCKED**. No strategy exempted. |
| D-05 | `builtin/{network,msb_driver,resources}.rs`; patched network/types SDK; `tests/builtin_runtime/network_resources.rs` | Explicit strict SNI now passes actual proxy TLS handshake/application data; wrong/missing/unbound/sibling SNI is refused before dial. Exact DNS-name/IP binding and TCP-only name rules retain UDP/QUIC denial. [Executed network tests and limits](0121-evidence/raw/closure/network/notes.md). Controlled endpoint probe classification is fixed. Full guest auth/DNS/MCP/proxy/CA/network/OOM/CPU scenarios still need WI 0123 completion and execution. **FAIL** for remaining scenario code; native enforcement **BLOCKED**. Encrypted HTTP authority/ECH inner-name inspection is not claimed. |
| D-06 | `builtin/{backend,exec_bridge}.rs`, `tests/builtin_runtime/lifecycle.rs` | Fake-driver cancellation/cleanup and socket framing tests execute in [SDK/lib log](0121-evidence/raw/final/sdk-and-builtin-lib.log). Actual CLI smoke registered; shared production API/squad TLS now selects ring per server, preserves ALPN, and has an actual trusted/untrusted HTTPS handshake regression; TUI/API/workflow/squad, cross-session crash/recovery and genuine two-build upgrade matrix absent. **FAIL**. Native owner-exit/reattach and multi-VM contracts remain unexecuted. |
| D-07 | `builtin/msb_driver.rs`, `builtin/embedded/mod.rs`, vendored `microsandbox 0.7.2` isolated builder | `sdk_isolated_config_ignores_installed_msb_config` executes in [SDK/lib log](0121-evidence/raw/final/sdk-and-builtin-lib.log); worker non-UTF-8 overrides/refusal checks in [hermetic log](0121-evidence/raw/final/builtin-hermetic.log). One-method patch exists and is selected by Cargo/lock. Layer 0 `host_var_os` preserves non-UTF-8 and overlay precedence. Native installed-runtime coexistence/boot not run. **BLOCKED**. |
| D-08 | `builtin/worker.rs`; `builtin_worker::tests::*`, `binary::worker_unopened_fd_is_sanitized_error_not_abort`, `builtin_final_artifact_retains_worker_provider` | Pure descriptor open/type/access and fixed SDK slots plus current feature child-process refusal execute in local suites. [Hermetic log](0121-evidence/raw/final/builtin-hermetic.log). Valid guest worker handshake and optimized section retention not run; metadata strings are declarations only. **BLOCKED**. |
| D-09 | `oci/{mod,cache}.rs`, `builtin/{paths,backend,instance}.rs`, attach/exec bridge | Atomic archive leases survive cloned handoff and cross-process prune. New shared image leases protect launch plans; exclusive import/remove refuses competing use, then SDK rootfs references protect the installed image. Cross-process lock and planned-image removal regressions execute in [focused local log](0121-evidence/raw/closure/focused-local.log). Broader native multi-process crash/stress scenarios remain incomplete in WI 0123. **FAIL**, native execution **BLOCKED**. |
| D-10 | `Makefile`, isolated runner, Cargo module registry, `.github/workflows/test.yml` | Final local commands below. Pure builtin modules execute without payload gating; fast Docker-source filtering removed; all supplied guest modules discovered. Feature crypto-provider fixture panic corrected by an explicit per-server provider. No blanket guest/service success from harness return counts. Enforced native matrix/CI jobs unavailable. **BLOCKED** once local gates pass. |
| D-11 | Cargo/config/lock, third_party build inputs, `tools/reproducible-build-audit.sh`, native/Windows/Intel Mac CI | [Git attempts and host evidence](0121-evidence/raw/final/identity-and-prerequisites.log); no accessible base/HEAD, tracking, clean checkout or unsupported-target build artifact. Windows/Intel Mac and real Docker/Apple regression runs absent. **BLOCKED**. |
| D-12 | `tests/builtin_runtime/sqlite.rs`, SQLx 0.9.0/rusqlite 0.40.2 and bundled libsqlite3-sys 0.38.2 | Exact resolution/migration/debug DSO checks plus genuine SQLx/rusqlite bidirectional commit, rollback, uncommitted visibility and SQLITE_BUSY contention execute against one SDK catalog on Linux ARM64. Actual awman reopens it with 27 migrations. [Executed SQLite test](0121-evidence/raw/closure/sqlite-transactions.log), [hermetic suite](0121-evidence/raw/closure/hermetic-builtin.log). Historical full-spike artifacts remain absent; genuine old/new builds and every supported native target remain **BLOCKED**, assigned WI 0123. No database-contract or SQLite DSO exception. |
| D-13 | `tools/release-artifact-check.sh`, release workflow, `builtin_final_artifact_retains_worker_provider` | [Enforced distribution prerequisite](0121-evidence/raw/final/distribution-required.log) fails without exact artifact. No optimized/stripped guest boot, final native access/dependency/helper trace, Mac distribution artifact. No native release job executed. **BLOCKED**. |
| D-14 | `tools/measure-release-artifact.sh`, `NOTICE.third-party.md`, release artifact | Harness/notices exist, but no artifact measurement, Linux ABI audit, complete licensing/relink review or maintainer release disposition. **BLOCKED**. |
| D-15 | `tests/oci_import/corpus.rs`, corpus builder, guest/onboarding scenarios | Full required format × shipped-template corpus builder/consumer now exists with production size limits; shell syntax and synthetic formats execute. [Matrix notes](0121-evidence/raw/closure/matrix/notes.md). Actual corpus production is **BLOCKED** without Docker/Apple services. Complete onboarding and remaining guest strategy scenarios need WI 0123 implementation: **FAIL**. No parity exemption created. |

## All nine WI 0119 acceptance criteria

| Criterion | Evidence mapping | Final verdict |
|---|---|---|
| 1 single executable / required targets, existing unsupported builds | D-03/D-08/D-11/D-13; only current debug ARM64 artifact exercised | **BLOCKED** |
| 2 reproducible patches/payloads/native dependencies and clean checkout | D-03/D-07/D-11; hashes/patch files exist, Git and native clean builds absent | **BLOCKED** |
| 3 exactly one bundled SQLite and full supported-target contract | D-12; local exact dependency checks separate from unverified full spike | **BLOCKED** |
| 4 native guest execution on all three required hosts | D-03 required hardware failures, no guest run | **BLOCKED** |
| 5 every mandatory source, real-store round trip and offline reuse | D-01 PASS (WI 0122: Apple store round trip and offline builtin reuse). D-02 required Docker/registry service scenarios are still missing | **FAIL** |
| 6 every strategy, runtime/frontend/workflow/squad contract | D-04/D-05/D-06/D-09: remaining native strategy, network and lifecycle scenarios | **FAIL** |
| 7 optimized/distributed artifact, scans and measurements | D-13/D-14, no final artifact | **BLOCKED** |
| 8 accurate docs/specs, real existing Docker/Apple regressions, no SBX adoption | D-11/D-15; docs corrected, real backend regressions and full diff unavailable | **BLOCKED** |
| 9 precise WI 0120 inventory/removal path, no unrecorded upstream dependency | Six patched vendor crates and awman glue recorded in WI 0120; SDK patch is applied, not proposed. Full tracked-input/dependency-diff audit unavailable | **BLOCKED** |

## Regression carry-forward and evidence limits

The [WI 0119 historical review](0119-final-adversarial-verification.md) retains
R1–R19 and O1–O15. Do not reopen R3 based on a claim that SDK get/list lacks
reconciliation. Preserve bounded account reads/numeric primary GID, selected
image-only projection, per-reference archive identity, full ready source keys,
private cache paths, worker sanitized errors, optional startup descriptors,
and explicit owner-exit reattachment refusal. Current local tests cover parts
of these regressions; no historical native PASS is inferred.

The original B-60–B-70 mapping was not supplied. Known anchors remain B-67 Git,
B-68 Apple and B-69 network/OOM. No invented mappings or blanket closure.
Reqwest remains the explicit registry transport for TLS/auth/proxy/retry;
helper-only credentials remain refused. Synthetic PAX/OCI/docker-save and
gzip checks do not establish Apple acquisition or a real template corpus.
Native keychain tests require disposable credentials outside isolation.
The four authorized HostAgentPinger triggers and Docker-only socket exception
are unchanged; no host agent or silent backend fallback is authorized.

## Final adversarial verification — initial snapshot

The following commands describe the earlier snapshot. The completion follow-up
below supersedes its source identity and local gate results.

**Date:** 2026-09-28. **Reviewed revision:** UNKNOWN / BLOCKED (missing Git
metadata); current source manifest linked above. **Disposition: NOT ACCEPTED.**

Final commands and exact exit results are recorded below after completion.

Commands ran on Linux aarch64 with Rust 1.94.0, `/usr/local/cargo/bin` on PATH
and `CARGO_BUILD_JOBS=2` for the feature/final builds. Test commands use
`tools/isolated-test.sh` (private HOME/state, no host credentials/services).
[command-exits.tsv](0121-evidence/raw/final/command-exits.tsv) records the
focused/final invocations; [runner](0121-evidence/raw/final/run-final-followups.sh)
contains their exact arguments. No external skip counts as successful execution.

| Command / exact scope | Exit and final result |
|---|---|
| `make pre-push` | **0, PASS** for architecture lint, formatting, default all-target Clippy and isolated local suite. [Final log](0121-evidence/raw/final/final-pre-push.log). Does not execute real Docker/Apple services or guest hardware. |
| `cargo clippy --all-targets --features builtin-runtime -- -D warnings` | **0, PASS**. [Final log](0121-evidence/raw/final/final-feature-clippy.log). The vendored SQLx dependency emits 40 deprecation warnings under Cargo's dependency warning cap; no awman warning suppression added. |
| `make test-builtin` | **0, local checks executed; native acceptance BLOCKED.** [Log](0121-evidence/raw/final/test-builtin.log). Harness return counts include opt-out paths, so this is not an all-required-tests PASS. Lib: 3229 passed/1 intentional ignored; bin: 14; builtin target: 78 including 19 guest opt-outs; data: 113; OCI: 36 including four external opt-outs; examples: 9 each. |
| Feature `builtin_runtime -- --skip builtin_hw --nocapture` | **0**; 59 harness returns; final-artifact, missing-strace and measurement paths explicitly skipped. Feature/platform early-return branches are not counted as acceptance evidence. [Log](0121-evidence/raw/final/builtin-hermetic.log). |
| Feature lib `engine::container::builtin:: -- --nocapture` | **0, PASS** for **102 executed** pure/SDK/fake-driver/socket tests. Includes isolated SDK config, socket-bridge refusal, explicit lag failure, partial frames and bounded account reads. [Log](0121-evidence/raw/final/sdk-and-builtin-lib.log). |
| Feature `oci_import -- --nocapture` | **0**; **32 executed** fixture/loopback checks, four external skip paths. [Log](0121-evidence/raw/final/oci-current.log). No real-service PASS. |
| `AWMAN_TEST_BUILTIN=1 AWMAN_TEST_BUILTIN_REQUIRE_HW=1 AWMAN_TEST_BUILTIN_NETWORK=1 AWMAN_TEST_BUILTIN_PRESSURE=1 tools/isolated-test.sh --features builtin-runtime --test builtin_runtime builtin_hw -- --nocapture` | **101, BLOCKED**: all 19 required guest cases fail the missing-KVM prerequisite. [Log](0121-evidence/raw/final/hardware-required.log). Zero guest executions. |
| `tools/native-builtin-ci.sh` for each required triple | **2 each, BLOCKED**: missing KVM on ARM64; wrong native architecture for x86_64; no Apple Silicon/macOS host. Three preflight logs linked above; no CI execution inferred. |
| `AWMAN_TEST_REQUIRE_EXTERNAL=1` with `real_stores::` / required real corpus selector | **101 each, BLOCKED**: three real-service cases and one corpus case lack opt-ins/disposable inputs. [Services](0121-evidence/raw/final/services-required.log), [corpus](0121-evidence/raw/final/corpus-required.log). Missing scenario implementations remain FAIL independently. |
| `AWMAN_TEST_DISTRIBUTION=1` with `builtin_final_artifact_retains_worker_provider` | **101, BLOCKED**: `AWMAN_TEST_BUILTIN_ARTIFACT` absent. [Log](0121-evidence/raw/final/distribution-required.log). |
| Default lib `host_var_os_preserves_non_utf8_and_overlay_precedence` | **0, PASS**, one executed test. [Log](0121-evidence/raw/final/non-utf8-env-helper.log). |

Earlier failed attempts are retained: [initial architecture lint](0121-evidence/raw/final/pre-push-initial.log),
[feature driver missing network field](0121-evidence/raw/final/feature-clippy-missing-network.log),
[feature TLS fixture panic](0121-evidence/raw/final/test-builtin-crypto-provider-failure.log),
and [probe assertion update](0121-evidence/raw/final/pre-push-probe-regression.log).
One intermediate runner was edited while its shell was executing and exited
127 after its tests; [that invalidated run](0121-evidence/raw/final/pre-push-interrupted-script-edit.log)
is not PASS evidence. The final pre-push ran against the stable runner.

At this earlier snapshot the Rustls edge was dev-only and resolved the already-locked 0.23.45:
[lock diff](0121-evidence/raw/final/rustls-lock-diff.txt). Fixture servers select
a provider per server; no process-global provider default or production TLS
validation was weakened. The later completion follow-up below moves that edge to normal dependencies
and fixes/tests the production API/squad TLS constructor too.

[Ignored-test and external-tool audit](0121-evidence/raw/final/ignored-and-external-tools.log)
identifies the intentional documentation generator and subprocess-helper test
bodies. It also confirms Docker, Apple `container`, `codesign`, `xcrun`, and
`strace` are unavailable. Real existing Docker/Apple backend regressions,
Windows/Intel Mac builds, native CI, signing, final scans and the full SQLite
spike are **BLOCKED**, not successful local skips. No linker or disk failure
occurred in the completed final local commands.

Final mandatory verdicts: **FAIL D-01/D-02/D-04/D-05/D-06/D-09/D-15**;
**BLOCKED D-03/D-07/D-08/D-10/D-11/D-12/D-13/D-14**. WI 0119 AC5/AC6
are FAIL; its other seven criteria are BLOCKED. WI 0119/0121 remain incomplete.

`make test-fast` also exits **0, PASS for its hermetic scope**:
[final log](0121-evidence/raw/final/final-test-fast.log). The actual
[fast-filtered OCI inventory](0121-evidence/raw/final/fast-oci-inventory.log)
retains all **13 `docker_store::` cases**. The
[builtin inventory](0121-evidence/raw/final/builtin-inventory.log) discovers
all **19 `builtin_hw_*` cases**; their required-hardware run above is BLOCKED.

The final runner additionally preserves proxy configuration only when **both**
disposable-registry gates are enabled. Its four-case regression executes in
[builtin-hermetic.log](0121-evidence/raw/final/builtin-hermetic.log).
Pre-push was rerun successfully after that last change; feature Clippy,
`make test-builtin`, the named hermetic suite, inventory and fast tier were
also rerun successfully. Exact [recheck arguments](0121-evidence/raw/final/run-final-registry-recheck.sh)
and [exit codes](0121-evidence/raw/final/registry-recheck-exits.tsv) supersede
the earlier local results where applicable. Native/service/distribution
prerequisites remain unchanged and blocked.

## Final adversarial verification — earlier completion snapshot

**Completion follow-up date:** 2026-09-28. **Reviewed revision: UNKNOWN / BLOCKED**;
Git still exits 128 and the baseline has no available base commit. The latest
[source manifest](0121-evidence/raw/completion/source.sha256) records present
files, not a commit, complete diff or clean-checkout proof. The
[identity/prerequisite log](0121-evidence/raw/completion/identity-and-prerequisites.log)
confirms the unavailable Git metadata, `/dev/kvm`, Docker socket and missing
review/fix handoffs. No runner/service/signing access was supplied in response
to the request for those prerequisites.

The user requested completion of the remaining work. This follow-up implements
and tests the fixes below; it **does not achieve WI 0119/0121 acceptance**.
Detailed [finding dispositions](0121-evidence/raw/completion/finding-dispositions.md)
retain missing code as FAIL and unavailable execution as BLOCKED.

| Executed regression | Result and bounded claim |
|---|---|
| `acquisition_holds_lease_before_publication_callback_and_through_cloned_handoff` | **PASS** in the [OCI log](0121-evidence/raw/completion/focused-oci.log): a child process prunes while the real acquirer is paused before returning, for fresh and cache-only acquisitions; cloned result protects actual archive projection. Native SDK import and installed-image use/removal are not proven. |
| `injected_environment_ignores_ambient_credentials_and_proxies`, `retry_uses_credentials_captured_before_the_first_request` | **PASS** in the [OCI log](0121-evidence/raw/completion/focused-oci.log): upper/lower proxy and valid inherited Docker config cannot override absent snapshot inputs; source-free cached hit succeeds; deleting credentials after the first 503 does not change the retried request's credentials. Loopback services only. |
| `late_success_and_cancelled_success_are_rejected`, `controlled_reader_rejects_bytes_returned_after_expiry`, `expired_validation_progress_never_publishes_a_reference`, `acquisition_budget_expires_while_waiting_for_cache_lock`, `cancelled_reference_write_keeps_previous_reference` | **PASS** in the [focused lib log](0121-evidence/raw/completion/focused-fixes.log). Cooperative local/validation/hash/publication checks added. Full phase-specific deadline tests and production/header cancellation remain open; filesystem syscalls are not preemptible. |
| `sdk_hostname_dns_binding_cannot_authorize_udp_or_quic` | **PASS** in the [focused lib log](0121-evidence/raw/completion/focused-fixes.log): the actual pinned SDK evaluator denies UDP/443 despite an allowed DNS binding, allows TCP and allowed DNS, and refuses unrelated TCP/unlisted DNS. Native packet enforcement is BLOCKED; strict allowed HTTPS remains FAIL. |
| `production_https_server_handshakes_with_both_crypto_providers` | **PASS** in the [focused lib log](0121-evidence/raw/completion/focused-fixes.log): the shared production API/squad server completes a trusted HTTPS request and rejects an untrusted client connection with builtin features enabled. Explicit per-server ring preserves h2/http/1.1 ALPN. This is not the missing frontend/guest workflow matrix. |

Exact final commands are in the [runner](0121-evidence/raw/completion/run-gates.sh)
and [exit records](0121-evidence/raw/completion/command-exits.tsv). PATH includes
`/usr/local/cargo/bin`; `CARGO_BUILD_JOBS=2`; tests use the isolated runner.

| Command / scope | Exit / observed result |
|---|---|
| `make pre-push` | **0, PASS** for lint/format/default Clippy and local tests. [Log](0121-evidence/raw/completion/pre-push-reviewed.log). |
| `cargo clippy --all-targets --features builtin-runtime -- -D warnings` | **0, PASS**. [Log](0121-evidence/raw/completion/feature-clippy-reviewed.log). The same 40 vendored SQLx dependency deprecations remain under Cargo's dependency cap; no awman lint suppression added. |
| `make test-builtin` | **0, local checks pass; mandatory native acceptance BLOCKED**. [Log](0121-evidence/raw/completion/test-builtin-reviewed.log). Lib 3236 passed/1 intentional ignored; bin 14; builtin 78 including 19 hardware opt-outs; data 113; OCI 39 including four external opt-outs; examples 9 each. |
| Feature OCI suite with `--nocapture` | **0**, **35 executed** fixture/loopback regressions, four explicit external opt-outs. [Log](0121-evidence/raw/completion/focused-oci.log). |
| Feature focused OCI/TLS/SDK lib tests | **0**, **110 executed**, no ignored tests. [Log](0121-evidence/raw/completion/focused-fixes.log). |
| Enforced hardware suite | **101, BLOCKED**: all 19 fail for missing `/dev/kvm`; zero guest executions. [Log](0121-evidence/raw/completion/hardware-required.log). |
| Enforced real stores and real corpus | **101 each, BLOCKED**: three service cases and one corpus case lack disposable service inputs/opt-ins. [Services](0121-evidence/raw/completion/services-required.log), [corpus](0121-evidence/raw/completion/corpus-required.log). |
| Enforced final-artifact check | **101, BLOCKED**: no `AWMAN_TEST_BUILTIN_ARTIFACT`. [Log](0121-evidence/raw/completion/distribution-required.log). |

Intermediate failures are preserved in the same bundle (`check-1.log`,
`feature-clippy.log`, `pre-push.log`, `pre-push-2.log`); these caught typed lease
helpers, enum size, an unused import and ambiguous inference. Final commands
above ran after the corrections. No linker or disk failure occurred in them.

The [checklist audit](0121-evidence/raw/completion/checklist-audit.log) confirms
**0 checked / 9 open** for WI 0119 and **0 checked / 12 open** for WI 0121.
WI 0120 still records four vendor patches; normal Rustls and the direct pinned
network evaluator dependency are manifest/integration changes, not new patches.
Docs/specs now describe TCP-only hostname allows, the remaining HTTPS defect,
and the limits of acquisition cancellation/deadlines.

Mandatory verdicts remain **7 FAIL, 8 BLOCKED**. The Apple adapter, allowed HTTPS,
production cancellation, strategy/frontend/lifecycle matrices and real-service
scenario gaps still require implementation. Native hosts, actual services,
clean-checkout Git identity, full native SQLite contracts and signed final
artifacts still require external evidence. Neither work item is complete.

## Final adversarial verification

**Date:** 2026-09-28. **Reviewed revision:** UNKNOWN / BLOCKED. The Git object
store/worktree pointer is still inaccessible, and baseline.md records no
available base. The latest [identity log](0121-evidence/raw/closure/identity-and-prerequisites.log)
and [present-source manifest](0121-evidence/raw/closure/source.sha256) do not
substitute for a complete diff or clean checkout. The earlier final sections
are historical snapshots; this section and the current D-rows supersede them.

The user authorized resource-qualified follow-up work. [WI 0122](../work-items/completed/0122-native-apple-image-store-bridge.md)
now owns native Apple-store implementation and validation; [WI 0123](../work-items/0123-builtin-native-verification-and-release-closure.md)
owns remaining native scenario code, services, full-diff/clean builds and
release closure. Both specify capable-agent assignments and mandatory resource
preflights. No connected external runner/signing/service agent was available
or executed here. **Original WI 0119/0121 acceptance remains incomplete.**

The local continuation closes concrete implementation gaps: cancellable HTTP
and Ready propagation, strict visible-SNI enforcement with exact DNS binding,
shared planned-image leases/exclusive mutation, genuine SQLx/rusqlite
transactions, and additional guest/service/corpus scenarios. Exact finding and
remaining-code dispositions are in the [review audit](0121-evidence/raw/closure/finding-dispositions.md).
The [matrix handoff](0121-evidence/raw/closure/matrix/notes.md) explicitly lists
missing native scenarios; none is reclassified as a parity exception.

Commands used `/usr/local/cargo/bin` on PATH and `CARGO_BUILD_JOBS=2`. Test gates
use the isolated runner. Exact [runner](0121-evidence/raw/closure/run-gates.sh),
[stable recheck](0121-evidence/raw/closure/run-stable-recheck.sh), and
[exit records](0121-evidence/raw/closure/command-exits.tsv) retain all outcomes.

| Command / observed scope | Result and evidence |
|---|---|
| `make pre-push` | **0, PASS** for architecture lint, formatting, default all-target Clippy and isolated local tests. [Log](0121-evidence/raw/closure/pre-push.log). A stable-tree recheck is recorded separately below. |
| `cargo clippy --all-targets --features builtin-runtime -- -D warnings` | **0, PASS**. [Log](0121-evidence/raw/closure/feature-clippy.log). The vendored SQLx dependency still emits 40 capped deprecation warnings; no awman lint suppression added. |
| `make test-builtin` | **0, local gates PASS; native acceptance BLOCKED**. [Log](0121-evidence/raw/closure/test-builtin.log). Lib 3239 passed/1 intentional documentation-generator ignore; bin14; builtin82 harness returns including20 hardware opt-outs; data113; OCI46 including6 external opt-outs; examples9 each. The newly enforced isolated SDK policy/SNI subset executes **86 passed**, no ignores. |
| Focused feature lib: paths/lifecycle, Ready, OCI, production TLS | **0, 151 executed, PASS**. [Log](0121-evidence/raw/closure/focused-local.log). Includes cross-process tag lease exclusion, planned-image removal refusal, cancellable import lock, Ready caller/drop cancellation and production trusted/untrusted TLS. |
| Feature OCI suite with `--nocapture` | **0, 40 executed fixture/loopback checks and six explicit external opt-outs**. [Log](0121-evidence/raw/closure/focused-oci.log). Header/body/token cancellation closes actual peer sockets; late-stage report failure never writes PASS; corpus pair inventory and archive projection execute. |
| Feature builtin suite excluding `builtin_hw` | **0, 62 harness returns; three explicit final-artifact/strace/measurement skips excluded from evidence**. [Log](0121-evidence/raw/closure/hermetic-builtin.log). Genuine two-driver transactions, rollback, uncommitted visibility, write contention, catalog reopen and 27 migrations execute on this ARM64 host. |
| Full patched network crate tests and patch reconstruction | **562 executed, PASS** in [SDK log](0121-evidence/raw/closure/network/sdk-network-all-final.log); [patch reconstruction](0121-evidence/raw/closure/network/patch-reconstruction.log) succeeds against published archives. Native guest enforcement remains unproven. Final Makefile enforces only the reviewed pure/loopback subset. |
| Required hardware/network/pressure `builtin_hw` suite | **101, BLOCKED**:20 prerequisite failures, **zero guest executions**, absent `/dev/kvm`. [Log](0121-evidence/raw/closure/hardware-required.log). |
| Required real stores | **101, BLOCKED**:5 service/keychain cases lack disposable prerequisites; one local late-report regression passes. [Log](0121-evidence/raw/closure/services-required.log). No actual Docker/registry/keychain acceptance. |
| Required real corpus and producer | Consumer **101, BLOCKED**, missing corpus; producer **2, BLOCKED**, no Docker. [Consumer](0121-evidence/raw/closure/corpus-required.log), [producer](0121-evidence/raw/closure/matrix/corpus-build-blocked.log). |
| Required distributed artifact | **101, BLOCKED**, missing exact optimized/stripped artifact. [Log](0121-evidence/raw/closure/distribution-required.log). No signing/boot/trace/measurement PASS. |
| Native CI preflights ARM64/Linux, x86_64/Linux, Apple Silicon | **2 each, BLOCKED**: absent KVM, wrong architecture, absent Apple host respectively. [ARM64](0121-evidence/raw/closure/native-arm.log), [x86_64](0121-evidence/raw/closure/native-x86.log), [Apple](0121-evidence/raw/closure/native-apple.log). No configured CI job is execution evidence. |

Earlier compile/reactor/probe attempts are retained in the acquisition/matrix/
network subdirectories; corrected reruns above supersede those failures.
No linker/disk failure occurred in the completed local gates. Resource skips
are disclosed, never acceptance evidence. The [checklist audit](0121-evidence/raw/closure/checklist-audit.log)
keeps WI 0119 **0 checked/9 open** and WI 0121 **0 checked/12 open**; both new
follow-ups are open. WI 0120 and notices record **six** carried dependency
patches, their exact provenance and independent removal gates. Runtime/security
and design docs describe current boundaries and cancellation safe points.

Mandatory aggregate verdicts remain **7 FAIL, 8 BLOCKED**: D-01/D-02/D-04/D-05/
D-06/D-09/D-15 retain missing mandatory implementation/scenario work; other
D-rows lack native/build/distribution evidence. The nature of several FAILs
changed: allowed HTTPS and production cancellation are now implemented and
locally tested, while remaining native scenario code is explicitly assigned
WI 0123. Neither work-item transfer nor local gate success closes those rows.

The final stable-tree recheck also passed: [`make pre-push`](0121-evidence/raw/closure/pre-push-stable.log)
**0**, [`make test-fast`](0121-evidence/raw/closure/test-fast-stable.log) **0**,
and the [complete builtin SDK/lib subset](0121-evidence/raw/closure/builtin-sdk-local.log)
**0, 105 executed**. This includes installed-config isolation, SNI configuration
roundtrip, UDP denial and image leases. The [registered inventory](0121-evidence/raw/closure/builtin-inventory.log)
contains82 tests including20 required guest cases. The [link audit](0121-evidence/raw/closure/register-links.log)
checks that every cited local evidence artifact exists; it does not promote
unexecuted tests to PASS.

## User scope amendment — unsigned distribution

On 2026-09-28 the user removed signing and notarization from **all** requirements.
WI 0119–0123, the current D-13/AC7 rows, runtime/CI docs and follow-up access
requirements now reflect that decision. CI and native/spike scripts no longer
require signing/notarization credentials or run explicit signing, signature
checks, notarization, stapling or signed-installer packaging. The Mac job
uploads the binary directly and boots/traces that exact distribution file.
Native tests check host availability and then attempt actual guest boot;
removing the signature preflight does not turn an OS permission failure into
PASS. No native Mac execution is claimed by this amendment.

Historical evidence and its source hashes remain unchanged. Their references
to required signing are superseded; missing signing credentials are no longer
an outstanding acceptance blocker. Other native/service/artifact prerequisites
and missing implementations remain open, so WI 0119/0121 stay incomplete.
The amendment's [source manifest](0121-evidence/raw/unsigned-policy/source.sha256)
identifies the updated present tree; Git revision/full diff remains BLOCKED.

Validation: both workflow files parse as YAML, job dependencies resolve, shell
blocks pass syntax checks and contain no explicit signing operations. The
[validation log](0121-evidence/raw/unsigned-policy/yaml-check.log) records these
checks. Parsing found an existing unquoted colon in a test step name; quoting
it fixes the workflow syntax without changing the command.

`make pre-push` passes after the amendment: **exit 0**, including formatting,
architecture lint, default all-target Clippy and local tests ([final log](0121-evidence/raw/unsigned-policy/pre-push-final.log)).
The [initial failure](0121-evidence/raw/unsigned-policy/pre-push.log) exposed an
existing test that still required the removed signature prerequisite; its list
now checks the retained KVM/HVF host prerequisites. Modified native/spike shell
scripts pass `bash -n`. [Notes](0121-evidence/raw/unsigned-policy/notes.md) and
[command exits](0121-evidence/raw/unsigned-policy/command-exits.tsv) preserve
scope and validation details. Earlier feature/native results remain historical;
no new native execution or full-diff approval is inferred.
