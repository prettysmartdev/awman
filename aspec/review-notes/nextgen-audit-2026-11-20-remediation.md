# Nextgen audit remediation — 2026-11-20

Work item: #78; workflow specification: `/awman/context/workflow/work-item.md`.
Audit: [nextgen audit](nextgen-audit-2026-11-20.md).

This is a developer traceability and qualification record. Implementation and
adversarial review were followed by cross-file integration. No High finding is
closed by a disposition or by local tests alone. A16 remains a High release
readiness blocker. Windows, Apple Silicon, protected-release provenance,
publication and authenticated rehearsal require their own evidence.

The mounted workspace's `.git` points at an unavailable host gitdir. Therefore
there is no verified upstream HEAD or index diff here. Qualification snapshots
were copied into new private directories and committed only in those directories;
their synthetic commit IDs identify tested bytes, not upstream/main provenance.
No shared checkout was reset, stashed, cleaned or reverted. `docs/` belongs to the
parallel documentation step and was not edited by integration.

Full handoff/review history and raw evidence are under
`/awman/context/workflow/`; final integration evidence is in
`evidence/integration/`. `context-read-index.json` inventories every prior artifact
read with its SHA-256 and final result lines. Earlier failing runs remain diagnostic
history; only the final command results below qualify the integrated inputs.

## Finding traceability

Test names below refer to registered suites in `ci/gates.toml`. A suite's coverage
registration is distinct from an observed platform pass.

| ID | Change and review resolution | Executable proof | Remaining acceptance |
| --- | --- | --- | --- |
| **A01 High** | Typed mount authority crosses admission, setup, persistence and restart; exact child/pointer workdirs refuse before allocation. Git mounts are judged by declared purpose. Engine pins source identity and repeats checks after preparation. Integration carries those pins to Docker/Apple spawn and builtin create. High-Claude H1/H2 corrected; no disposition. | `security::mount_scope::authority::{whole_repo_grant_mounts_only_its_root_at_launch_and_after_restart,exact_subdirectory_of_a_checkout_is_refused_before_setup_and_never_widens,workspace_swapped_while_the_launch_is_prepared_never_reaches_the_runtime,declared_git_mount_redirected_to_a_foreign_target_is_refused}`; `engine::conformance::runtime_create_rechecks_the_mount_identity_after_build`; `data::mount_authority::tests::*` (now unit-tier). | Native mount/reparse/case-alias evidence remains required. Path-based daemon bind APIs retain a final check-to-bind interval; source hardening is not a directory-handle capability or proof of atomic filesystem confinement. |
| **A02 High** | Sole HostAgentPinger exception retained with unchanged triggers/greeting/scratch policy. Installed first-party programs and child interpreter PATH exclude checkout/Git/worktree roots. Monitor fails closed without context; pre-step refresh now registers real session roots. Custom approval pins definition + canonical program, and review uses the same snapshot. H1–H4 fixed in source except unqualified native aliases; no disposition. | `engine::ready::{installed_ping_skips_relative_empty_and_absolute_checkout_entries,installed_script_cannot_run_checkout_interpreter,relative_linked_worktree_backlinks_remain_excluded,non_repository_home_does_not_exclude_installed_programs,custom_ping_approval_binds_definition_and_canonical_program_separately}`; `engine::credential_refresh::synchronous_refresh_and_live_tick_use_only_installed_snapshot_programs`; `command::prepare_only::workflow_pre_step_and_post_exit_recovery_keep_the_sanctioned_ping`. All host executables in these tests are owned fakes. | Native APFS/NTFS alias qualification and final corrected-commit security review remain open. No real provider was run on a host. |
| **A03 High** | Runtime/state-dir/refresh process settings resolve before engine/Core/monitor creation. One-shot invocation includes its repo; daemon/TUI keep immutable startup binding. Commands refuse conflicting overrides before allocation; client env conflicts remain visible, changed global values warn about restart. Native-plan fake now binds its actual config. H5/H6 resolved under the documented one-shot/startup precedence; no disposition. | `daemon::process_config::{startup_docker_to_builtin_binds_actual_backend_and_disables_refresh,startup_builtin_to_docker_binds_actual_backend_and_disables_refresh,invalid_config_unavailable_or_mismatched_backend_allocates_no_daemon,a_client_environment_conflict_survives_inheritance_and_is_refused}`; `command::config::{api_process_overrides_are_refused_before_session_and_queue_allocation,one_shot_startup_binds_repository_refresh_settings_before_engines}`; `command::native_plan::*`. | Native backend/platform parity and corrected-commit review remain required. No backend fallback added. |
| **A04 High** | Common migration-aware opener holds OS lock over migration + open; journal hashes verify every present destination even after Published. Conflicts preserve both stores/sidecars/journal; backups do not overwrite unrelated bytes. All six previously unowned production openers now use that opener. High-Claude H3/H4 source fixes integrated; no disposition. | `data::store_migration::{independently_populated_stores_conflict_and_every_file_is_preserved,interruption_at_every_step_resumes_to_one_complete_store,a_published_journal_does_not_bless_a_replaced_destination,a_direct_open_never_creates_the_destination_during_a_migration,concurrent_opens_migrate_once_and_share_one_database}`; `daemon::store_unification::{legacy_then_foreground_then_daemon_keeps_every_record,independent_stores_are_refused_by_foreground_and_daemon_alike}`. | Native filesystem/power-loss and TUI-entry race execution remain open; source review verifies every production direct open was removed. Unix lock/fault tests do not qualify Windows rename/ACL behavior. |
| **A05 High** | Fallible delivery/observation/CAS writes, atomic native result + execution/projection/pending-batch commits, flush before publication, immutable recovery without task redelivery. Separate durable profile-cleanup journal prevents success before export or after abandoned cleanup. Bus progress spills privately. Final result redaction precedes commit. Integration adds unnumbered permanent-storage error transport without fabricated Done/cursor. H7–H11/H16/H17 resolved as recorded below; no High disposition. | `command::automation_observers::{completion_write_faults_return_storage_error_without_publishing_success,publication_faults_and_crashes_replay_the_committed_batch_once,oversized_native_answer_is_a_small_durable_failure,bus_secrets_are_redacted_before_the_immutable_native_answer_commit}`; `data::automation_store::{terminal_commit_is_atomic_and_cancellation_is_serialized_with_it,post_profile_cleanup_is_durable_and_never_upgraded_by_reopen,old_ended_command_without_result_is_repaired_only_on_store_open}`; worker `verified_chat_and_failed_cleanup_finalize_through_worker_without_restart`; `command::settings_profiles::automation_command_finalization_waits_for_profile_export_and_records_failure`; `daemon::automation_routes::restart_publishes_committed_automation_once_and_never_requeues_input`; stream `permanent_storage_failure_is_unnumbered_and_never_fabricates_done`; worker persistence-trigger test also checks recovery health. | Permanent unavailable storage truthfully remains recovery-required; repair/restart is necessary. Native crash/filesystem qualification and corrected-commit independent review remain required. Pending-clear errors after acknowledged completion preserve the true completion and are still errors by contract. |
| A06 | Streaming union of overlaps equals whole-buffer redaction, including partitions/UTF-8/non-UTF-8; retirement synchronously scrubs idle held buffers. Medium-Claude #8 corrected. | `engine::auth::redaction::stream_tests::{audit_example_unions_overlapping_secrets,streaming_equals_whole_buffer_for_arbitrary_partitions,multibyte_utf8_secrets_split_inside_characters,overlap_chains_stay_bounded_and_equal,retirement_clears_pending_and_fails_closed}`; `tests::retirement_scrubs_idle_streams_on_every_clone_immediately`; A18 overlap seed. | Registered tests/property evidence only; released caller buffers and allocator/OS copies are outside stream-owned retirement. |
| A07 | Windows native shim enforces protected owner/DACL, no-follow/reparse/ancestor pinning, link count, handle-based replacement; absent provider refuses. Unix guarantees retained; ELOOP-only symlink error wording. Review found no additional bypass. | `data::private_files::{replacement_is_atomic_and_absent_records_read_as_none,replacement_never_writes_through_a_hard_link,unix::created_objects_are_owner_only_and_broad_or_linked_ones_are_refused_unchanged}`; Windows-specific inherited ACL, wrong owner, junction and concurrent replacement tests registered in component-data. Prior cross-compile passed. | **BLOCKED[Windows host]**: no native Windows run, ACL privileges/share modes/junction behavior not qualified by Linux or cross-compilation. |
| A08 | Backend-neutral grace/full transcript semantics; private unnamed spool with 256 MiB cap and explicit incomplete warning, bounded tail/write buffer/readers. Integration supplies session env to interactive phase, streams full redacted failure log and full remediation file, returns bounded excerpt/path. M3/M4/M10 resolved. | `engine::container::builtin::runtime_contracts::*`; `engine::conformance::cli_backends_keep_ordered_phase_transcripts_and_propagate_failed_stop`; `frontend::api::command_frontend::phase_capture_tests::failing_interactive_phase_keeps_a_complete_private_redacted_transcript_above_one_mib`; output-tail cap/fault tests; guest `phase_runtime` scenarios. | Linux quiet-phase capture/cancellation passed in the verified packet; other supported native platforms and repeated/contention measurements remain unqualified. Failure-log errors warn without changing the real step exit. |
| A09 | Explicit stop preserves stopped state; remove is separate/idempotent and uncertain shutdown keeps profile Closing. Checked removal carries observed generation + execution to builtin same-handle SDK removal; Docker deletes full immutable IDs. Stable Apple missing timestamp. M11/M12 addressed conservatively. | `engine::container::builtin::runtime_contracts::explicit_stop_preserves_get_and_remove_checks_current_owner` includes old-generation checked-delete refusal; `command::settings_profiles::{checked_removal_refuses_a_stopped_same_name_replacement,orphan_close_rechecks_shutdown_absence_and_same_name_generation,reused_resource_identity_is_left_alone_and_close_delete_launch_contend}` and existing profile recovery cases; Apple `a_stopped_row_without_timestamp_has_stable_identity`. | Apple name-based deletion cannot prove atomic identity: checked profile removal refuses and remains Closing. Native Apple JSON/runtime qualification and an identity-safe Apple deletion API remain open. The native checked-profile replacement race has not been executed; live Linux stop/list/remove and controlled generation-fencing tests do not establish that separate race. |
| A10 | Typed fallible Foreground/Shared hosting; API injects daemon Arc store, no `.ok()?` reopen or fallback. Common opener also used by TUI/clean/unlink. | `daemon::store_unification::{api_automation_is_hosted_on_the_injected_shared_store,api_automation_without_a_bound_store_fails_and_never_runs_in_the_foreground}` plus A04 migration journeys. | Platform/store acceptance follows A04. |
| A11 | One typed ProfileResultProjection feeds CLI JSON/text, API data/stdout and TUI status lines; required presenter replaces default no-op. Origin/owner checks unchanged. | `command::settings_profiles::{profile_results_project_identically_to_cli_api_and_tui,typed_profile_close_failure_is_visible_with_failure_status,profile_commands_refuse_origins_without_owner_authority}` and parity suites. | Actual Ratatui rendered-frame assertion not obtained; projection semantics are exercised. |
| A12 | Layer 2 SessionLifecycle owns admission handoff, setup/cancel tasks, compensation and finalization; HTTP only adapts DTO/presentation. Stop reservation fenced before writes, panic caught, transient finalize retried; joined shutdown releases store. H12–H15/H18 addressed as below. | `daemon::session_lifecycle::{typed_and_http_setup_have_identical_authority_state_and_events,http_disconnect_and_idempotent_retry_keep_one_setup_owner,durable_cancel_before_token_registration_is_reconciled_and_finalized_once,shutdown_joins_setup_finalization_even_when_presentation_is_blocked,stopping_worker_refuses_new_setup_before_any_durable_write,context_key_write_failure_compensates_the_session_without_scheduling,presenter_panic_closes_setup_and_finalizes_the_run,transient_run_finalization_failure_is_retried_by_the_setup_owner}`; `daemon::git_results::remote_run_clone_auth_failure_reports_git_auth_failed`. | Permanent persistence failure still requires recovery. Lower-level public admission remains for admission-only tests; production callers use SessionLifecycle. No native cancellation claim inferred. |
| A13 | Bounded byte SSE decoder frames before UTF-8, LF/CRLF/multiline/id/comment handling, malformed counts/cursor continuation. Exact-cap split delimiter fixed (#7). | `engine::remote::sse::tests::*`, especially every partition of terminators; `engine::remote::follow::{unicode_output_split_across_writes_arrives_intact,an_oversized_frame_is_a_protocol_failure_that_is_not_retried}`. | Maximum frame 16 MiB; oversize fails terminally. |
| A14 | Cancellation selected across connect/read/reconnect; no surviving signal task; command detach returns without uncancellable status GET (#1). | `engine::remote::follow::{cancel_on_a_comment_only_stream_detaches_without_contacting_the_server,cancel_during_the_reconnect_delay_detaches_promptly,run_cancellable_drops_the_signal_listener_when_the_work_ends}`; `engine::remote::command_detach::an_interrupted_follow_returns_without_waiting_for_a_status_lookup`. | Separate `remote session start` poll-loop signal handling unchanged/outside stream finding. |
| A15 | Explicit Done/Detached/Incomplete/protocol-failure states; 5 bounded no-progress reconnects with Last-Event-ID and duplicate suppression; no command resubmission. | `engine::remote::follow::{eof_mid_frame_resumes_from_the_last_delivered_cursor_without_duplicates,eof_after_done_ends_the_follow_without_reconnecting,a_replay_restarted_at_a_new_attempt_is_delivered_in_full,repeated_eof_without_progress_exhausts_the_budget_as_incomplete,a_connection_that_delivers_progress_resets_the_reconnect_budget}`. | Existing server replay semantics unchanged. |
| **A16 High readiness blocker** | Deterministic fixture provenance + verify-build, attested immutable publication workflow, strict shared pin validator and candidate input identity. Pin bypasses/inconsistencies and tag race guards corrected (M5/M18/M19). No fabricated production pins. | `xtask-pipeline`: reproduced/tampered archive/provenance, absent/corrupt base, preflight/dry-run pins, changed fixture identity, fetch/verify and unrelated pending tier tests; release-contract publication test. | **BLOCKED[maintainer publication/protections]**: both production guest pins remain pending; no fixture publication, public attestation verification, immutable release setting or authenticated candidate/rehearsal evidence obtained. Local run-specific fixture preparation cannot close this finding. No disposition. |
| A17 | Candidate group is repository/version/dry_run; strict boolean journal-only runs are independent. No run-ID escape for normal candidates. | Release-contract `candidate_keys_serialize_same_repository_version_and_mode_across_runs`, `only_boolean_journals_get_independent_run_specific_keys`; pipeline fresh allocation/collision tests. | **BLOCKED[authenticated Actions rehearsal]**. Actions pending replacement/order is not a durable FIFO. |
| A18 | Two production source defects; isolated clean baseline, fresh product-failure JUnit, reversal/restored baseline. Empty/stale/surviving/compiler/blocked/skipped outcomes nonpassing; test hunks/stray patches refused. Integration refreshed hunk offsets to final source. | Real source qualification results below; `xtask-pipeline::source_defects::*`, inline classification tests; actual pipeline fault injection. | Upstream final-commit nightly timing/provenance still separate from synthetic snapshot qualification. |
| A19 | Caller-owned conformance refs/names and RAII cleanup replace fixed-name preclean/prefix sweep. All background requests can label at create; fixtures reserve names before launch and only remove recorded IDs or verified labels. | Real Docker `concurrent_fixture_cleanup_preserves_peer_and_preexisting_resources`, `failed_background_creation_is_cleaned_without_a_returned_id`, `conformance::real_docker_cli_conforms`; template tests use unique guarded tags. | Real boundary results below; only owned resources may be removed. |
| A20 | Resize queries wait on observed dimensions with deadline and full backlog draining, then explicitly exit. Refresh guests prove v1 readiness before file replacement. Driver-local deadline helper removes bin/dev-dependency mismatch. | `exec::pty_resize_is_observed_in_the_guest` and refresh-multi guest scenarios; both real cfg(awman_builtin) driver bins must compile. | PTY and two-consumer refresh passed in the verified Linux packet. Repetition/contention statistics and p95 are not established by one run. |
| A21 | 16 s shared readiness budget inside unchanged 20 s component deadline; bounded cancellation/join reserve and diagnostic real clone/helper fault. Final repeated local qualification exposed a Git cancellation race: reaping the leader could precede killed helpers exiting. Linux transport now waits for its exact killed group to become quiescent, with a bounded failure if it cannot. | `engine::git_credentials::{running_git_and_helper_processes_carry_no_secret_in_argv_or_environment,missing_authenticated_readiness_reports_diagnostics_and_joins_worker}`; `worker::push::fencing_during_remote_request_kills_git_and_never_reports_success`; local head/config cancellation tests; support wait/budget tests. Corrected worker suite: 59/59. | No timeout increases, disabled leak checks or passing skips. |
| A22 | Active authoritative specs use runtime.kind docker/apple/sbx/builtin, nested builtin keys, true platform gates, process/session scope, builder/source policy. Review M9 short builder key corrected. | `command::config::{active_runtime_config_examples_parse_and_retired_names_hint_current_syntax,builtin_init_config_journey_uses_the_nested_runtime_contract}`. | User-docs owns published guide changes and their final example recheck. Historical migration/release text retained. |
| A23 | Squad recovery resolves connected listener, preserves explicit configured features, and falls back to inspect-only when that metadata is missing; M7 fixed. | Web screen recovery tests including `does not replace configured features using the effective feature list`; complete web check below. | No configuration mutation or authority widening from UI. |
| A24 | Catalogue-backed accepted automation null/auto/require_full projects into admission/idempotency response only for accepting commands. M8 fixed unsupported flag field. | `command::admission::{automation_flag_projection_matches_accepted_modes_and_replay,agents_list_does_not_project_an_unsupported_automation_flag}`. | Original owner/origin restrictions retained. |
| A25 | End-to-end item/byte gates, bounded decode-ahead, ingress stderr count/drop, control-priority dispatch, unread response credits, safe coalescing, line/outbound cap and cancellable teardown. Medium-Claude #2–#6 corrected, bridges forwarded/qualified. | `engine::acp::bounds_tests::{permission_and_response_arrive_while_bulk_stays_saturated,stderr_is_bounded_at_ingress_while_its_pump_cannot_run,unread_responses_are_charged_to_the_control_budget,a_producer_that_cannot_wait_fails_the_session_instead_of_queueing,an_outbound_message_over_the_line_limit_is_refused_before_it_is_queued}`; budget/framer tests; `engine::acp::{a_stalled_consumer_holds_the_docker_bridge_within_the_acp_budgets,maximum_size_valid_lines_are_delivered_whole_within_the_budgets,the_docker_bridge_bounds_acp_stderr_at_ingress}`. | Prior production-budget stress observed stdout 4,194,304 B/1024 items, bulk 8,343,773 B/127 items and 6.8 ms cancel; exact maximum line reached 8,388,608 B bulk. Native binary-clean/EOF ACP passed; builtin ACP memory-pressure stress remains unqualified. Bounded decode window cannot bypass arbitrary output; stdin-blocked protocol replies may delay later control. |

## Smaller observations

| Observation | Action / evidence / remaining limit |
| --- | --- |
| README badge / removed workflow reference | Native HTML badge/link target `ci.yml`; architecture-lint spec uses `ci.yml`. Read-only source inspection confirms both contain the current workflow and no `test.yml` reference (`smaller-observations.json`); historical release docs retained. |
| Vendored SQLx executable modes | Read-only filesystem enumeration of all 58 vendored files finds no executable bits (`smaller-observations.json`); content/provenance retained. Snapshot Git index records captured modes. Original worktree index unavailable: maintainer must confirm/commit mode-only diff in the actual repository. |
| Mutable Cursor/Cline installers | Existing in-image upstream routes retained under the explicitly permitted smaller-observation residual-risk disposition. `rg -n 'https\|latest\|install' templates/Dockerfile.cursor templates/Dockerfile.cline` records the current source (`mutable-installer-inspection.log`). Prior implementation inspected official provider instructions: no immutable Cursor checksum route selected; Cline remains mutable latest. No version/checksum invented; no host installation. Reproducibility/supply-chain risk remains, and the observed template boundary pass is separate build evidence. This is not a High disposition. |
| Large modules | Extracted migration/lifecycle/projection responsibilities where required; no cosmetic repository-wide split. Existing large API/agent modules remain maintainability debt. |
| Raw DTOs / `.ok()?` / no-op hooks | Typed fallible hosting and required profile projection, Layer 2 lifecycle and catalogue-backed automation contract at touched boundaries. No broad unrelated abstraction rewrite. Low-level admission-only tests do not prove lifecycle ownership; composed lifecycle tests do. |

## Review resolution boundaries

All four High-Claude defects were reproduced before fixing. Mount/source-target
and Published-journal cases have permanent regression tests; integration removes
remaining production opener bypasses and adds the runtime-create recheck.
High-Codex's confirmed interpreter-PATH, worker chat result and profile-cleanup
success hazards have fixes with composed tests. Permanent storage is an explicit
recovery-required transport failure, never a false durable terminal result.

Lower-severity review decisions retained: startup/global override precedence is
intentional with restart warning; pending-clear failure after acknowledged
publication is still an error by the completion contract; low-level public
admission is used only by admission tests, while production uses SessionLifecycle.
Neither decision disposes of a High finding. Medium-Codex's mutable installer
risk remains explicit. Native alias, Windows, Apple, publication and Actions
claims are not inferred from inspection or fake-backed tests.

## Commands and final qualification

Local commands used the pinned toolchain, `CARGO_BUILD_JOBS=1`, private
`CARGO_TARGET_DIR=/tmp/wi0078-integration-target`, `CARGO_HOME=/usr/local/cargo`,
`RUSTUP_HOME=/usr/local/rustup`, and the verified manifest tools on PATH. Final
local qualification was serialized to fit the instance's memory. No selected
test, assertion, leak probe, deadline or required gate was removed or relaxed.

| Command | Observed result / evidence in `evidence/integration/` |
| --- | --- |
| `cargo xtask gates render` | PASS; mount-authority unit registration and generated nextest configuration synchronized (`gates-render.log`). |
| `cargo xtask gates check` | PASS, zero gaps; repeated on final inputs using the locally built xtask (`gates-final.log`, exit 0). |
| `cargo xtask matrix check` | PASS; repeated on final inputs (`matrix-final.log`, exit 0). |
| `make check` | **PASS**, lint all (architecture, Clippy workspace/all targets/locked with warnings denied, licenses/bans/sources, formatter, hygiene, notice, shell, workflows, matrix, gates), then **387/387** unit tests. Git-unavailable changed selection correctly ran every selected unit suite. `make-check-passed.log`, exit 0. |
| `make test` | **PASS**, manifest local gate: **2,112/2,112** tests in 30 nextest invocations. `make-test-passed.log`, exit 0; `local-results.json`. Nextest filter deselections are not ignored selected tests. |
| `npm --prefix web run check` | **PASS**, 47 files / **702 tests**, typecheck, lint, matrix, build, audits and 43 edge assets (`web-check.log`). Subsequent integration edits affected Rust/specs only. |
| `cargo xtask ci seeded-defects pipeline` | **PASS**, missing target, renamed required job and silently skipped hardware faults all detected (`pipeline-defects.log`, exit 0). |
| `cargo xtask test run --suite component-worker --test-threads 2` | **PASS**, **59/59**, including cancellation/leak regression (`worker-cancellation-fixed.log`, exit 0); repeated successfully by the final full local gate. |
| `cargo deny --locked check` | **FAIL**, separate full current advisory scan (`deny.log`, `advisory-status.md`). The configured lint selects licenses/bans/sources, not advisories. No ignore or dependency exception was added. |

The full advisory scan reports RUSTSEC-2026-0440 (astral-tokio-tar),
RUSTSEC-2025-0141 (bincode), RUSTSEC-2026-0543 (chrono),
RUSTSEC-2026-0459 / CVE-2026-119375 (bundled libsqlite3-sys 0.38.2), and
RUSTSEC-2024-0449 (unsafe-libyaml). Dependency remediation/review remains an open
release prerequisite; the narrower configured gate's pass is not an advisory pass.

Diagnostic attempts are retained. `make-test-complete.log` exposed the real Git
helper cancellation leak; the production cleanup was fixed, not the test.
Overlapping large Rust compilers caused SIGKILL in some other attempts, recorded
as failures/inconclusive compilation, never defect detection. Earlier green runs
precede that final cleanup change and are not substituted for the final runs above.
An explicit `--test-threads 1` support-suite run passed 73 tests but evidence export
refused the missing original Git provenance (exit 2); that entire invocation is
not reported green. Source qualification uses its own clean synthetic repository.

### Isolated source-defect qualification

The actual production runner was executed from the clean private snapshot at
`dfb05ac2d7c507f2cf88d42df33a0b8414a7a8b1`:

```sh
CARGO_MANIFEST_DIR=/tmp/wi0078-integration-snapshot/xtask taskset -c 3 \
  /tmp/wi0078-integration-target/debug/xtask ci seeded-defects source \
  --dir ci/seeded-defects \
  --report /awman/context/workflow/evidence/integration/source-defects-serialized.md
```

**PASS, exit 0.** Initial clean baselines passed **563/563** core tests and
**204/204** xtask unit tests. Both patches applied with `git apply --check`, changed
production source, produced fresh product-failure JUnit, reversed with
`git apply -R`, and left a clean restored source tree. The same suites then passed
again (**563/563**, **204/204**). Final patch digests match the reviewed files.

- `redaction-overlap.patch`: six actual redaction assertion failures, including
  `audit_example_unions_overlapping_secrets`, `overlap_chains_stay_bounded_and_equal`
  and `streaming_equals_whole_buffer_for_arbitrary_partitions`. These demonstrate
  escaped overlap bytes, not a compiler/startup error.
- `fixture-layer-digest.patch`: three actual fixture assertions failed:
  `amd64_fixture_retains_its_platform_and_unsupported_bases_are_rejected`,
  `corrupt_base_cannot_create_a_fixture`, and
  `repeatable_image_contains_digest_verified_identity_and_whiteout_layers`.
  Valid/corrupt layer handling was inverted as intended; compilation succeeded.

The report, all 35 raw baseline/injection/restoration evidence files, and
`source-defects-results.json` retain the commit/tree, inventories, commands,
JUnit, patch digests and actual failing names. `seed-worktree-cleanup.log` confirms
only the owning snapshot remains; the runner removed its detached checkout.
`source-defects-qualified.*` also passed before the final Git-cleanup correction;
`source-defects.*` and `source-defects-final.*` are earlier inconclusive compiler
attempts and are not used as qualification. No upstream/protected-commit nightly
provenance is inferred from the synthetic snapshot.

### Linux boundary and native qualification

The granted host was Linux x86_64/KVM at `debian@148.113.136.44`. Only the private
directory `/home/debian/wi0078-integration.lKQULc` was created for checkout, build,
logs and fixtures. Synthetic commit
`7cc50bde4cbe14ba02412b31828ec2ef9e5d0296` was clean; all **1,897** recorded software
inputs match the corrected workspace and the independent local review checkout
(`native-final-source-check.*`, `native-review-source-check.*`). The source bundle
retains that exact commit for later review after remote cleanup.

| Command / cell | Observed result |
| --- | --- |
| `make test-guest`, including explicit KVM/payload grants | **BLOCKED**, exit 2: production `guest-docker-base` and `guest-oci-base` pins remain pending. No passing skip or fabricated production pin. `native/operator/guest-final.log`. |
| `make test-boundary AWMAN_TEST_HOST_RESOURCES=docker=unix:///var/run/docker.sock,local-registry=127.0.0.1:32778,git-transports,offline` | **PASS**, **43/43**: conformance 3, Docker 10, Git 2, offline 1, registry 10, templates 17. Registry was created on an allocated loopback port and retained by exact ID. `native/operator/boundary-final.log`. |
| `CARGO_TARGET_DIR=target/native-validation-build cargo check --locked -p awman-cli --features guest-drivers --test guest --bin awman-guest-driver --bin awman-guest-net-driver` | **PASS**, real cfg(awman_builtin) SDK/driver compilation, exit 0, 11m31s. No new awman-test-support binary dependency required. `native/operator/drivers.log`. |
| `make linux-prerelease` | **PASS**, exit 0. The committed runner prepared real run-local fixtures, verified KVM/payload, ran the private-memory prerequisite and all **90/90** selected nextest tests in **19 suites**: **41 boundary + 37 guest + 12 artifact**. The candidate used the normal optimized/thin-LTO/stripped release profile. `native/operator/linux-prerelease.log`, packet raw records and `native/packet-summary.json`. |
| `cargo xtask native-validation verify /awman/context/workflow/evidence/integration/native/extracted/evidence` from the clean exact-commit local review checkout | **PASS**, exit 0. Source/tree/input hashes, every packet file digest, candidate/fixture digests, required command records, complete native selection and independently enumerated tests equal passing JUnit. Run-local config changes are restricted to the prepared fixture hashes and removing their pending markers. `native/packet-verify.log`. |
| `AWMAN_TEST_HOST_RESOURCES=git-transports xtask test run --suite boundary-git --build linux-x64-native-builtin --evidence-dir .../boundary-git-final` | **PASS**, **2/2**, exit 0, on the final source after the Git helper cleanup change. Fresh compiled inventory, JUnit and run.json retained in `native/operator/boundary-git-final/`; SSH 1.130s, smart HTTP 1.136s. |
| `xtask fixtures verify-build --base .../work/base.tar --sha256 7c6caacadbb377711479d89900f8bacbadf8709c9c7e74011a1eb4984043f0c7 --out .../work/fixture-build` | **PASS**, exit 0: both archives and full recipe/contract/platform provenance independently reproduce. `native/operator/fixture-reproduction.log`, `recipe-provenance.json`. No publication or production pin change. |

The ordinary boundary run initially exposed a fixture-name error: a supposedly
foreign container used the production legacy `awman-*` prefix. The corrected
foreign name leaves the assertion intact; the complete rerun passed. Failed
create/no-returned-ID cleanup, concurrent peer ownership and the template guards
all ran against the real Docker Engine. That boundary rerun preceded the final
Git helper-quiescence fix; the native packet reran the
Docker/registry/offline/template cells on the final source. Both Git transport
tests then passed separately on that final source. Thus all 43 applicable boundary
tests have observed final-source passes, in addition to the packet's guest/artifact
evidence.

The packet is
`native/20261120T212429Z-331680.tar.gz`, SHA-256
`c72128c4dc0ecff3133125fe315e51edfb9e0ee25a9de8b361ebf501d381e715`.
Its separately captured remote digest matches the runner output and downloaded
bytes. Before extraction, member inspection found **206** regular-file/directory
members, **147,691,982** expanded bytes, no absolute/traversal paths, links or
special files. It was extracted into a fresh private directory. No received
candidate, helper binary or script was executed locally; only the locally built,
reviewed verifier ran.

Reviewed native facts: Linux x86_64, Debian kernel 6.12.100, Rust **1.94.0**,
Docker **26.1.5**, KVM probe `1`, `umask 0002`. Verified payload archive
`86f9f72dc3e639c7175bc07909b4b63ce412517c1ef8a2e1921171af5682fded`, kernel
`c09aba16346d3a72f0aa1728d444ce09a7d82f296716d2955ffe0012e2fc8864`, and agent
`1a911b6cb85e306100b102ab4bac2702acfcdaf929ea7ee3ea473486b05ac349`.
The separate upstream private-memory prerequisite passed all **5** selected
contracts (immutable siblings/file, sparse unlinked private backing, slot coverage,
and malformed descriptor/alignment/overflow refusal).

The exact optimized candidate SHA-256 is
`76e927f99282bf92c154172397f6676a02b9e387068659777f8235767f4dc6d0`.
Guest OCI fixture SHA-256 is
`c86291072784402551a5a223eaca257e3affd7ef37b6bb5bd2402f494f353f6f`;
Docker-save fixture SHA-256 is
`2dde9194d583e4fe853aa7a1089cc78a30388b61faf7b78e31d2373b910a3faf`.
Fixture provenance records real base image
`alpine@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f`,
base archive `7c6caacadbb377711479d89900f8bacbadf8709c9c7e74011a1eb4984043f0c7`
and the exact recipe commit. These are run-local inputs, not published production
pins or public attestations.

Important actual assertions were reviewed: the quiet phase checks all **20,000**
ordered lines above 1 MiB, real exit 37, disabled first-byte grace and explicit
cancellation; PTY checks observed dimensions; refresh checks both consumers after
v1 readiness; stop keeps a listable stopped instance until explicit removal;
workspace/read-only and vCPU/RAM checks execute inside real guests. The exact
candidate runs offline guests and cached replacement, scans allowed dependencies
and embedded callbacks/sections, and leaves no extracted helpers. Synthetic
provider scripts execute only in guests; host readiness pings use owned fakes.

Operator packet wall time was **5,950.543 s** (99m10.543s), including a cold
optimized build of **4,035.386 s** (67m15.386s), with one compiler job on the 8 GiB
host. These are observed durations, not a CI budget pass, flake rate or p95 claim.
The Linux replacement checks use the same candidate inode/cache boundary; no
published Linux predecessor was available, so they do not prove historical
protocol migration. Windows/Apple Silicon execution, Apple Containers/sbx,
native alias/reparse qualification, a native profile replacement race, builtin
ACP memory-pressure stress, public fixture publication/attestation, protections
and authenticated candidate rehearsal remain **OPEN/BLOCKED** as applicable.
No High finding is closed by the operator packet.

After collecting the final logs/packet/provenance and preserving both exact source
bundles, the remote checkout was confirmed clean and all 1,897 software hashes
were rechecked unchanged (`native/remote-final-source-check.log`). Both recorded
registry IDs were independently confirmed absent against the running Docker
Engine (`native/registry-cleanup-check.*`). The private directory was removed,
and its absence was checked (`native/remote-cleanup.*`, exit 0). No broad image,
container-name or process-prefix sweep was used. The pre-existing checkout,
payloads, shared Cargo/Docker caches and unrelated resources were left in place.
The isolated source-defect worktree also removed itself. No real provider agent
was executed on either host.

Final integration status: manifest render/check and acceptance matrix pass;
`make check`, `make test`, web check, both actual source defects, applicable Linux
boundaries, run-local fixture reproduction and the independently reviewed Linux
native packet pass. Remaining acceptance is explicitly listed per finding above:
protected upstream provenance/review, production fixture publication and immutable
pins, authenticated Actions/repository protections and timing rehearsal, native
Windows/Apple/alias/race/stress cases, and current dependency advisory remediation.
Those remain open; no High disposition or release readiness approval is granted.
