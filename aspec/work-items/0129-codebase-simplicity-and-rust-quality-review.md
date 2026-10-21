# Work Item: Task

Title: Codebase simplicity, modularity, idiomatic Rust, and duplication remediation
Issue: Not linked
Status: Proposed — review complete; implementation not started

## Summary:

Review the findings below and implement the bounded remediations as an agent team. Prioritize failures hidden by defaults, resource ownership, and duplicated behavioral policy before structural cleanup. Preserve the four-layer architecture and existing CLI/TUI/API contracts.

This item records a repository-wide static review on 2026-10-21 against HEAD `98e111766256cd8048d1784cb6d9efad37dfe97d` **plus the existing working-tree edits**. It does not attribute findings to that commit. No implementation changes were made by this review.

The request included “preventing code reuse.” Clarification was requested but was not available when this item was written. The working interpretation is to prevent duplicated policy and unnecessary abstraction: share a proven invariant at its owning layer, retain separate code for genuinely different runtime or presentation behavior. Do not interpret this item as a mandate to prohibit ordinary reuse or to introduce a general framework.

“Slop” means concrete maintenance problems here: inert dependencies and parameters, fabricated success/defaults, repeated policy, stale factual comments, and tests that constrain spelling instead of behavior. It does not mean code was necessarily AI-generated, that long files are automatically wrong, or that all comments and explicit adapters should be removed.

### Review coverage and limitations

The inventory contains 433 Rust files under `src/` (208,896 lines), 11 under `crates/` (1,486 lines), and 102 under `tests/` (40,644 lines). These counts include tests, comments, and blanks; they are not production-complexity scores. The additional 466 vendored Rust files were inventoried, with review focused on carried patches and integration boundaries rather than an exhaustive audit of upstream implementations.

| Area | Review performed |
| --- | --- |
| Data/config/persistence | Configuration schema, coercion, layering, filesystem saves, API SQLite lifecycle, session/workflow contracts |
| Commands/dispatch | Engine construction, parsed input, command construction, launch preparation, workflow orchestration, API queue and squad coordination |
| Engines | Agent option preparation, Docker listing, builtin driver/blocking boundary, workflow launch paths, scheduler, OCI acquisition/retry/archive boundaries |
| Frontends | CLI/TUI/API adapters, API output transport, TUI shared state and application orchestration, unattended frontend |
| Auth/ACP/native code | Credential lease ownership, keychain boundary, ACP transport and filesystem refusal, worker/XPC boundary and safe adapters |
| Tests/tooling | Integration-target registration, process-environment mutation, architecture checks, isolation runner, manifests, build scripts and CI |
| Templates/vendor/docs | Template and release-script sampling, dependency patch ownership/removal gates, current specifications versus code |

The review combined repository-wide searches and duplicate-block screening with manual inspection of the cited implementations and their callers. It is not a claim that every line was manually inspected or every runtime path executed. Similar-looking blocks were checked before inclusion; broad searches alone are not findings.

Validation performed: `bash tools/architecture-lint.sh` passed. `cargo` and `rustc` were not available on this session's PATH, so compilation, Clippy, unit/integration tests, and native runtime execution were not performed. No dependencies were installed and no external services were exercised. Findings describing behavior are source-derived unless stated otherwise.

Line numbers below are review-time locators. Use the named functions/types when edits shift them. Several config, workflow, builtin, and TUI files already had uncommitted changes; revalidate those findings before implementation and preserve the existing work.

## User Stories

### User Story 1:
As a: maintainer

I want to: make one change to each behavioral rule, understand resource ownership, and navigate modules by responsibility.

So I can: extend awman without introducing frontend/runtime drift or accumulating unnecessary infrastructure.

### User Story 2:
As a: user running long-lived API, TUI, or squad sessions

I want to: receive accurate errors, retain valid configuration and workflow state, and release resources when a run finishes.

So I can: trust unattended execution and recover from failures without misleading success or repeated work.

### User Story 3:
As a: implementing agent team

I want to: work from evidence, explicit ownership, and behavior-based acceptance criteria.

So I can: deliver small, reviewable changes without competing rewrites of shared files.

## Implementation Details:

### Priorities

- **P1:** correctness/resource risks to address before cosmetic restructuring.
- **P2:** concrete maintainability problems, drift risks, and narrower behavior defects.
- **P3:** dependency and documentation cleanup after behavior is protected.

All findings are in scope, but land them in coherent batches. Do not combine all of this into one mechanical rewrite.

### F01 — P1: Queue completion failures are discarded

**Evidence:** `src/command/commands/api_server/queue_worker.rs`, `execute_command` at lines 139–375: argument JSON uses `unwrap_or_default` at 143; early error branches discard `complete_command` failures; line 347 discards `completion.persist`; metadata writes are also ignored. `src/data/fs/api_db.rs:588` owns completion, and `recover_stale_commands` at 707 can return old running rows to queued status.

**Impact:** a command can finish while its database row still says running. Later recovery can requeue work whose side effects already happened. Corrupt persisted arguments are also normalized to an empty metadata argument vector instead of producing a clear queue-record error. The frontend factory separately receives the original record, so do not assume the decoded local vector is the actual dispatch input.

**Remediation:** make terminal persistence a checked part of execution completion, with one shared completion path for normal/error/early-return cases. Distinguish authoritative DB failure from best-effort metadata/log failure. Record command/session identity in diagnostics, retain a recoverable failure state, and define bounded persistence retry/reconciliation without blindly executing the command again. Validate stored argument JSON once at the queue boundary and pass the validated representation onward. Audit the related discarded lifecycle writes in `api_server/runtime.rs:138–195` under the same policy.

**Acceptance:** inject completion-write failure after a side-effecting fake command; the failure is observable and cannot silently trigger successful completion or immediate redispatch. Cover directory/setup errors, malformed stored args, and closing-session handling. Preserve the existing exit-code and partial-failure behavior.

### F02 — P1: System-prompt files have engine lifetime instead of run lifetime

**Evidence:** `src/engine/agent/mod.rs:92–114` stores `Arc<Mutex<Vec<NamedTempFile>>>`; `write_prompt_temp_file` at 909–928 only appends. There is no per-execution removal. `Engines` shares the `AgentEngine` across daemon commands. If that mutex is poisoned, the `if let Ok` drops the new file while still returning its path.

**Impact:** completed runs retain open file handles and prompt files until the shared engine is dropped. Repeated daemon/API invocations grow the collection indefinitely; failed option preparation can retain files too. Poisoning produces a path to a file already removed.

**Remediation:** carry prompt-file guards with the prepared launch and transfer them to the instance/execution that uses them, following the existing execution-owned credential-lease pattern. Release on preparation failure, spawn failure, completion, cancellation, and dropped wait/execution. Do not replace this with a global cleanup task or filename-based deletion registry. Eliminate the silent poisoned-lock success path.

**Acceptance:** many sequential runs through one engine return prompt-file/handle counts to baseline; two concurrent runs retain only their own live files. Exercise failure and cancellation, including dropping an unlaunched plan. Files must remain present for the entire period the guest can read them.

### F03 — P1: Config and workflow saves truncate the live record

**Evidence:** `GlobalConfig::save_with` (`src/data/config/global.rs:187–194`), `RepoConfig::save` (`src/data/config/repo.rs:488–495`), and `WorkflowStateStore::save` (`src/data/workflow_state_store.rs:82–88`) use direct `std::fs::write`. An existing atomic-publication implementation lives in `src/data/fs/private_files.rs:230`, with stricter private-file semantics.

**Impact:** a concurrent reader can see partial JSON; interruption or a failed write can destroy the last valid config/checkpoint. This compounds F04's default-on-load-error behavior.

**Remediation:** publish complete JSON through a sibling temporary file and atomic replacement. Reuse or narrowly factor the existing data-layer primitive after checking permissions, symlinks, replacement semantics, and supported platforms. Do not apply owner-only/private-directory rules to repository files without deciding their intended contract. Atomic replacement is not read/modify/write conflict detection: preserve or explicitly define concurrent-writer behavior separately.

**Acceptance:** injected failure before publication leaves the original readable record intact; readers observe complete old/new JSON, never a partial record. Check temp cleanup, existing permissions, first creation, and workflow resume after a failed save. Validate replacement on supported platforms, not only one Unix host.

### F04 — P1: Scheduler reload errors silently change policy

**Evidence:** `src/engine/squad/scheduler.rs:223–230`, `tick`, calls `GlobalConfig::load_with(&self.env).unwrap_or_default()`. The comment explicitly describes this fallback. Task-specific config errors later fail the run, so global and task errors have inconsistent handling.

**Impact:** malformed or unreadable global config can discard the configured concurrency limit, guidance, and leader/model policy while the scheduler continues admitting work. This is an observable behavior risk, not just a stylistic dislike of `unwrap_or_default`.

**Remediation:** reject new admissions for a failed reload, report the named config error without log flooding, and continue observing existing runs. On a subsequent valid reload, resume admission automatically. If a last-known-good policy is selected instead, document and test exactly which policy remains active; never silently substitute defaults for a read/parse failure. A genuinely absent file can keep its documented default behavior.

**Acceptance:** valid → malformed/unreadable → valid transitions do not launch with default policy, do not lose in-flight accounting, and recover without restart. Retain the current live-reload behavior for valid edits.

### F05 — P2: Duplicated API output pumps corrupt split UTF-8 and copy buffers repeatedly

**Evidence:** `AgentFrontend::take_io` implementations in `src/frontend/api/command_frontend.rs:441–499` and `603–650` duplicate stdout/stderr line pumps. Each calls `String::from_utf8_lossy` on individual byte chunks and replaces the remaining string after every newline. Status mapping is also repeated at 416 and 579; a related adapter exists in `src/command/commands/api_server/session_setup.rs:248`.

**Impact:** a valid multibyte character divided across two chunks becomes replacement characters. Many lines in a chunk repeatedly copy the remaining buffer; an arbitrarily long unterminated line grows memory without a defined bound. Fixes must currently be copied into several paths.

**Remediation:** introduce one small byte-oriented line framer with explicit invalid-UTF-8, EOF, and overlong-line behavior, and delegate API presentation pumps to it. Keep stream/event labels and host-specific grace timeouts explicit. Own/drain pump tasks so final buffered output is ordered before the command's terminal event; verify the present lifecycle before changing it. Lower-layer session setup may use a data/engine framing primitive but must not import the API frontend. Prefer local composition over an all-frontends output framework.

**Acceptance:** test every split point of multibyte UTF-8, multiple lines per chunk, empty lines, invalid bytes, EOF without newline, and bounded handling of a long line. Verify actual event order for a short-lived execution and preserve stdout/stderr distinction and stdin EOF behavior.

### F06 — P2: TUI input discards types already parsed by Layer 2

**Evidence:** `ParsedArgs::into_command_box_input` (`src/command/dispatch/projections/raw_args.rs:290–333`) converts typed numbers and paths into `FlagValue::String`. `TuiCommandFrontend` reparses them at `src/frontend/tui/command_frontend.rs:391–488`, including its own positive-number check. API already delegates to `ParsedArgs` accessors. API parsing additionally keeps `parsed` plus `parse_error` and manually reconstructs error variants in `clone_parse_error` at `api/command_frontend.rs:371`.

**Impact:** two representations and repeated accessors make parity harder to maintain; new value/error variants require synchronized edits. The TUI applies validation policy in a presentation layer despite successful catalogue parsing.

**Remediation:** retain one typed parsed representation through TUI/API dispatch, adding only the canonical command path needed by the command box. Keep caller-profile defaults in Layer 2. Make API frontend construction fallible where callers can propagate the error; if deferred construction is necessary, represent success/error as a `Result` rather than unrelated fields and avoid hand-maintained error cloning. Retain the CLI's clap adapter where it is useful; do not replace clap for symmetry.

**Acceptance:** shared cases cover omitted/explicit false flags, default/forced flags, aliases, numeric bounds, path positionals, trailing args, repeated flags, and hyphen-leading agent args. Existing CLI/API/TUI differences that are declared policy remain intact. Errors retain structured classifications.

### F07 — P2: Config coercion guesses the value type instead of using the field schema

**Evidence:** `src/data/config/fields.rs:36–64` describes each field's kind; `validate_and_coerce` in `src/command/commands/config.rs:103–258` nevertheless uses string-keyed special cases and a generic bool → number → string fallback. `validate_default_leader_value` at 265 separately implements validation also owned by config loading.

**Impact:** a string field such as `work_items.dir` given `123` or `true` becomes a JSON number/bool and is rejected by the later typed-config deserialization. A valid textual value cannot be saved through the editor. Validation and hints can drift between editing and loading.

**Remediation:** dispatch coercion by explicit scalar/list/map-entry shape and field kind, sharing domain validators with config loading. Keep dynamic guidance/model-map rules explicit. Extend the current field table only as needed; do not introduce schema generation or a new config framework. String values must stay strings regardless of their contents. Also change the global write path at `config.rs:925` to honor the session's environment snapshot via the existing `save_with` API, so the file being edited is the file the session loaded.

**Acceptance:** string values `123`, `true`, and `false` round-trip through CLI and TUI; invalid booleans/numbers fail before writing. Cover list/map deletion, guidance containing commas, leader validation parity, field scope, sensitive-value masking, and a session config root different from the process root.

### F08 — P2: Command and workflow launch policy still has parallel copies

**Evidence:** ACP-explicitness/config resolution is repeated in `chat.rs:118–139` and `exec_prompt.rs:195–216`, alongside duplicated overlay parsing and separate `command_effective_config` functions at 396 and 454. Workflow single and parallel launch preparation repeat runtime-context construction, Running transitions, persistence, and factory invocation in `src/engine/workflow/single_step.rs:141–196` and `parallel.rs:160–212`.

**Impact:** updating a launch rule or pre-spawn failure path requires matching edits across entrypoints. Existing `LaunchPolicy`, agent matrix, and workflow factory already provide appropriate owners; another generic command hierarchy would make the situation worse.

**Remediation:** move common ACP decision inputs and overlay-list parsing into small existing Layer 2 helpers. Factor the common workflow preparation/spawn/state-recording step within the workflow engine, returning a concrete launch result; keep single-step waiting, parallel fan-in, retry, UI callbacks, and cancellation scheduling distinct. Preserve callback and persistence order and define what happens if the factory fails after Running was persisted.

**Acceptance:** add focused cross-entrypoint launch cases and single/parallel spawn-failure cases; retain plan/yolo/ACP rules, non-interactive prompt delivery, worktree handling, and parallel recovery tests. Avoid a “runner” abstraction controlled by many booleans.

### F09 — P2: Docker listing has duplicated queries and treats failure as an empty result

**Evidence:** `src/engine/container/docker.rs:74–180`, `list_running` and `list_running_all`, repeat queries, deduplication, and row parsing. Both skip command-launch/nonzero-exit errors. The latter alone checks empty id/name rows.

**Impact:** daemon failure is indistinguishable from no running agents, and duplicated parsers have already diverged. This can make status/cleanup callers reason from an incomplete snapshot.

**Remediation:** use one backend-local query/parser for the shared ownership-filtered listing. Explicitly distinguish no rows, failed listing, and partially successful filter queries. Return contextual errors instead of fabricated emptiness; preserve deliberate legacy ownership compatibility until separately retired. Do not generalize Docker argv into a common runtime contract.

**Acceptance:** fake host CLI covers empty success, unavailable daemon, nonzero exit, partial query failure, malformed rows, and deduplication. Confirm callers render unavailable state rather than “zero agents.” Coordinate with WI 0128, which owns the new agents/status commands; implement this backend fix once and consume it there.

### F10 — P2: API lifecycle states are mostly unrestricted strings

**Evidence:** `SessionRecord` and `CommandRecord` at `src/data/fs/api_db.rs:19–87`, plus `update_setup_status`, `complete_command`, and `update_session_status` at 270, 588, and 757. `NewSessionRow` already uses typed `SessionSetupStatus`/`SessionKind`, but read records and mutation methods revert to strings. Queue completion uses another `&'static str` status field.

**Impact:** invalid states and spelling drift compile successfully, despite nearby enums demonstrating the intended domain boundaries. Database, command, and frontend code independently compare state spellings.

**Remediation:** use narrow enums for persisted session/command states and existing setup/kind types where semantics match. Convert to/from the existing SQLite/wire spellings at persistence/serialization boundaries. Report unknown stored values explicitly; do not map them to a successful/default state. Avoid a generic typestate framework or a database schema migration solely for this refactor.

**Acceptance:** retain existing database and API compatibility fixtures; cover every persisted spelling, invalid stored values, and lifecycle transitions. Preserve the distinction between queued cancellation, execution error, session closing, and setup failure.

### F11 — P2: OCI retry classification depends on diagnostic text

**Evidence:** `src/engine/oci/retry.rs:124–165` treats `EngineError::Network(String)` as retryable unless text contains selected TLS/setup phrases. `registry.rs:1085–1115` maps malformed manifest/config JSON to that same Network variant; Docker metadata decoding has similar mapping at `docker_engine.rs:312–320`.

**Impact:** deterministic malformed content is retried despite the documented transport-only policy. Changes to error wording or contextual names can change retry decisions. The tokenizer added to avoid accidental TLS matches is complexity caused by losing the error category.

**Remediation:** retain a small typed transient/permanent acquisition classification at source boundaries, including content/schema failures, TLS/auth failures, and actual disconnect/timeout/5xx cases. Preserve useful redacted diagnostics separately. Use existing error variants where possible; keep cancellation and deadlines authoritative. Do not replace the full project error hierarchy.

**Acceptance:** assert attempt counts with local fixtures for malformed JSON, digest/platform mismatch, TLS/auth refusal, 5xx, and truncation. Changing a diagnostic's text or a reference name containing `tls` must not change retryability. Retain cancellation, private staging, and provenance checks.

### F12 — P2: Module/dependency boundaries are weaker than their signatures suggest

**Evidence:** `src/command/dispatch/mod.rs` combines engine factories, admission, frontend contracts, outcomes, execution dispatch, and extensive inline tests. `Engines::for_daemon` accepts `DataPaths` but only reads `paths.root()` into unused `_shared_data_root` at 219 while deriving actual paths independently. `ConfigCommand` takes/clones `Engines` (`config.rs:680–712`) and discards it at 739.

**Impact:** dependencies advertised by the API are not real dependencies, and unrelated responsibilities share a central file. This increases setup/test burden and makes ownership unclear. Raw line count alone is not the finding: much of these files is useful test coverage.

**Remediation:** remove unused engine dependencies from pure commands and either use the supplied daemon path context or remove the misleading parameter and update its docs. Split dispatch engine assembly, contracts/outcomes, and execution routing into responsibility-named modules with compatible reexports as needed. Move tests by responsibility only where it helps navigation. Reduce visibility of internal helpers opportunistically after checking consumers; do not split every layer into a new crate or hide the command enum in macros.

**Acceptance:** constructing/running config operations does not require runtime engines. Daemon path tests prove which supplied context determines storage. Catalogue completeness, runtime startup/fallback, exit-code, and command dispatch tests continue to pass without broad new mocks.

### F13 — P2: TUI state is bundled at construction but fragmented during updates

**Evidence:** `TabSharedState` at `src/frontend/tui/tabs.rs:632–698` contains separate shared slots for container identity/title/exit, I/O channels, workflow identity/view/path, and countdown state. `TuiCommandFrontend::new` (`command_frontend.rs:147–210`) unpacks these into a large parallel field list. `App::refresh_tabs_from_sessions` (`app.rs:711`) and related refresh/dialog methods coordinate the snapshots.

**Impact:** one logical lifecycle transition requires several independent updates and matching reset code. The existing tests for exit plus group-start/PTY-reset in one tick show that ordering is important; the current constructor bundle alone does not enforce those invariants.

**Remediation:** group state by actual lifecycle ownership (for example workflow identity/view metadata and active container I/O identity) and expose focused transition methods. Extend the existing container-slot event mechanism where it simplifies ordering. Keep presentation state in Layer 3. Do not put all TUI state behind one global mutex, rewrite the UI into an actor framework, or remove proven per-slot parallel behavior.

**Acceptance:** preserve same-tick exit/new-launch ordering, per-slot stats and cancellation, stale-event rejection, tab closing, reset behavior, and dialog input preservation. Review each moved field for a single clear writer/transition owner; file splitting alone does not close this finding. Coordinate status-related fields with WI 0128 and current TUI edits.

### F14 — P2: Integration tests still share process environment under unrelated locks

**Evidence:** `tests/engine/main.rs` compiles daemon, git, and overlay tests into one test binary. `daemon_supervisor.rs:27,122–153` protects PATH and HOME using its own `PATH_LOCK`; `git_engine.rs:244–284` mutates HOME/XDG/Git variables under another `IDENTITY_ENV_LOCK`. Several helpers restore variables only after assertions/callbacks return (`overlay_engine.rs:14–26`, `git_engine.rs:272–280`).

**Impact:** unrelated mutexes do not serialize the same process-global HOME; a panic can leave changed state for later tests. Separate top-level test binaries do not share a process, so do not “fix” all similarly named locks indiscriminately. The existing library test PATH guard is not automatically shared with integration binaries.

**Remediation:** prefer injected `EnvSnapshot`/path context or child-specific `Command::env`. Where a true process-global probe is unavoidable, isolate it in a child test process or use a shared guard within that executable, including affected readers. Restore via RAII with `Option<OsString>` so absent, empty, and non-UTF-8 values remain distinct. A writer mutex alone is insufficient if unguarded tests read the same environment.

**Acceptance:** run the affected integration target with normal/high test concurrency and demonstrate panic-safe restoration in a focused guard test. Cover the macOS HOME collision specifically. Do not solve this by globally forcing all project tests to one thread.

### F15 — P2: Architecture checks can mistake scanner failure for success

**Evidence:** `tools/architecture-lint.sh:37,73,132` suppress scanner stderr and use `|| true`; line 132 relies on a `grep -P` negative lookahead. Numerous later checks use the same no-match/error collapse. The current successful run proves the local scan result, not that all required scanners work on every supported host.

**Impact:** an unavailable regex capability or failed input scan can disable an enforcement rule without failing CI. A maintenance guard must distinguish “no violation” from “could not check.”

**Remediation:** distinguish grep's no-match exit status from execution errors, check required inputs/tools, and use the script's common portable regex capabilities where practical. Add small rule fixtures for allowed/disallowed imports and scanner failure. Keep the existing script if it remains adequate; a new `syn` tool or whole-project parser is not required by this item.

**Acceptance:** unsupported scanner behavior and missing/unreadable inputs fail with an actionable diagnostic; allowed fixtures pass; direct/grouped forbidden imports fail. Exercise the script on the Linux/macOS CI environments already used by the project. Preserve security-oriented guards while refactoring.

### F16 — P3: The binary package repeats library dependencies and build responsibilities

**Evidence:** `crates/awman-cli/Cargo.toml:18–102` repeats most root library normal dependencies and builtin optional SDK entries. Its production Rust is the small entrypoint plus safe Apple transport adapter; integration tests and guest examples also belong to this package. All three packages include the same root build script, which verifies payloads and generates `msb_kernel.rs`; the kernel consumer is in host shims.

**Impact:** the manifest obscures production versus test/example requirements and repeats feature wiring that must stay synchronized. This review has not run an unused-dependency tool and does not claim every repeated dependency is unused or that deleting it will reduce the final binary size.

**Remediation:** audit actual normal/build/dev/example use per package and feature. Move test-only dependencies to dev-dependencies, remove proven unused entries, and forward builtin features through the crates that own them. Separate genuinely shared build verification from kernel-source generation only if the ownership improvement justifies it. Keep centralized workspace versions, exact SDK pins, and the host-shim unsafe boundary.

**Acceptance:** record the usage reason for retained feature/dependency exceptions. Build workspace libs, binary, tests, and both guest examples under default/builtin configurations and supported target gates. Verify the single-binary worker path and payload/artifact checks still work. No opportunistic dependency upgrades or patch removals; coordinate those with WI 0120/0124.

### F17 — P3: Stale migration prose obscures current contracts

**Evidence:** `crates/awman-cli/Cargo.toml:10–13` still says oldsrc tests await WI 0072 while the same manifest registers current integration suites. `aspec/devops/localdev.md` says builtin acquisition/lifecycle/network test modules are not registered, but `tests/builtin_runtime/main.rs:23–37` registers them. It also describes a Criterion benchmark target absent from the current manifests/tree. Historical explanations dominate some small APIs, for example `src/engine/mod.rs`'s PATH guard and the opening of `command/dispatch/mod.rs`. `ResolvedAgentOptions::sandbox` still mentions a Result signature although it returns `Self`.

**Impact:** agents and maintainers can follow obsolete instructions, misjudge coverage, or preserve scaffolding because its comment claims a need that no longer exists.

**Remediation:** correct current factual guidance, remove completed-migration instructions, and shorten history-heavy comments to the invariant and reason a future edit must preserve. Keep substantial safety/FFI, lifecycle, cancellation, and compatibility rationale. Preserve historical work items and evidence archives as history rather than mass-editing them. Identify actual test coverage through registered targets; textual inventory/source assertions may guard wiring but must not stand in for execution tests.

**Acceptance:** development commands and test-registration statements match the final manifests. Changed comments describe current behavior and do not claim stronger isolation, atomicity, cancellation, or native verification than tests establish. No arbitrary comment-deletion quota or churn renaming all historic test files.

### Existing designs to preserve

- The catalogue, `LaunchPolicy`, agent matrix, and headless profiles are useful owners of shared policy. Extend them narrowly instead of making a competing universal registry.
- Credential leases are intentionally execution-owned; the safe-library/native-shim split and explicit runtime backend operations are intentional boundaries.
- Docker, Apple, builtin, and sandbox differences are real. Identical-looking option fields do not justify collapsing different launch, mount, image, or credential semantics.
- OCI provenance, bounded acquisition, archive validation, network policy checks, and vendored patch removal gates are not cleanup targets merely because they are verbose.
- `#[cfg(test)]` fakes with deliberate panic-on-unexpected-use are not unfinished production implementations. Likewise, feature/platform-specific dead-code allowances require context rather than blanket removal.

### Agent-team execution plan

Use a coordinating integrator and bounded implementers. These are future assignments, not agents started by this review. Each implementer should first reproduce or characterize its findings against the then-current tree and record any finding invalidated by intervening work.

| Owner | Findings / owned area | Dependencies and handoff |
| --- | --- | --- |
| Integrator | Baseline, shared interface decisions, final review, F12 | Reserve `dispatch/mod.rs`, manifests, module exports, and common error variants; approve small interface changes before consumers branch |
| Persistence/config implementer | F01, F03, F04, F07, F10; data stores/config and queue/scheduler | Land F03/F04 first; coordinate typed DB statuses with queue completion; integrator supplies shared signatures |
| Runtime implementer | F02, F08, F09, F11; agent/workflow/container/OCI | Agree launch-resource ownership first; hand typed launch inputs to command changes; coordinate F09 with WI 0128 |
| Frontend implementer | F05, F06, F13; API/TUI and parsed-input adapter | Agree typed input boundary with integrator; API output framing must respect Layer 2 setup adapter dependencies |
| Verification/cleanup implementer | F14–F17; tests, lint, dependency audit, current docs | Run after or alongside independent fixes; integrator owns final manifest edits; never change other agents' source to make a check pass silently |

Run only as many assignments concurrently as the available team supports. Test changes beside owned source remain owned by that implementer; the verification owner integrates cross-cutting fixtures and reports failures rather than racing to edit shared files.

Suggested landing sequence:

1. Establish baseline and focused failing regressions; agree DB status/input/resource contracts.
2. Land F01–F04 correctness fixes with their tests. F03/F04 should be reviewed together for reload behavior.
3. Land F05–F11 in independently reviewable behavior/duplication batches.
4. Land F12/F13 structural work after the affected contracts settle, preserving behavior.
5. Finish F14–F17 and run the integrated validation matrix.

For each finding, record: commit/change location, behavior before/after, focused evidence, retained intentional duplication, and any blocked platform check. A finding may be withdrawn only with concrete current-tree evidence; file movement, fewer lines, or a green lint alone is not remediation.

## Edge Case Considerations:

- Preserve existing dirty-tree work; do not revert another task's config, builtin, workflow, or TUI changes to simplify merges.
- Preserve CLI exit codes, API payload spellings, old SQLite records, config precedence, aliases, and declared per-frontend defaults. Any deliberate behavior correction must be named in the review.
- Keep command completion failures distinct from harmless receiver shutdown or best-effort cleanup. Do not mechanically replace every `let _ =`, `.ok()`, `unwrap`, or `unwrap_or_default`.
- A dropped/cancelled operation must release its own resources without deleting another live run's files or terminating an attached agent it does not own.
- Shared state snapshots must not mix prior and current workflow/container identities, especially across parallel retries and same-tick events.
- Avoid widening mounts, network policy, credentials, or host execution while consolidating code. Keep native and sandbox capability refusals explicit.
- Keep private-file versus repo-file permissions intentional; atomic publication must not silently change established symlink or concurrent-writer behavior.
- Coordinate agents/status changes with WI 0128, upstream runtime work with WI 0120, and Linux/native gates with WI 0124. Do not duplicate their feature scope here.

## Test Considerations:

Use each finding's acceptance tests and the repository's isolation runner. Add tests for observable contracts and failure modes, not one assertion per trivial delegate. Preserve existing behavioral coverage while removing only demonstrably redundant scaffolding.

Required integrated checks on a machine with the toolchain:

```sh
bash tools/architecture-lint.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
make test-fast AWMAN_TEST_TMPROOT=/tmp/awman-0129-tests
```

Run focused tests through `tools/isolated-test.sh` with a writable private fixture root. Use the correct package/target registrations in `crates/awman-cli/Cargo.toml`; listing or source-presence tests alone do not prove execution. Do not opt into real runtimes or services for a hermetic regression.

On provisioned supported hosts, also compile/lint the builtin feature, exercise affected Docker/native runtime cases, and validate macOS environment/lint behavior. Follow `aspec/devops/localdev.md` and the CI gates for required payloads, signing, fixtures, and hardware. Default-feature success does not establish builtin, Apple XPC, KVM/HVF, or release-artifact correctness. Record missing prerequisites as BLOCKED, with the specific check still outstanding.

Completion criteria:

- [ ] F01–F17 each have implementation/test evidence or a justified current-tree withdrawal.
- [ ] Shared policy has one owning implementation; retained differences are explicit.
- [ ] No new general framework, global service locator, blanket lint suppression, or fallback-to-success was introduced to satisfy a cleanup metric.
- [ ] Targeted behavior and compatibility tests pass; required platform checks have results.
- [ ] Existing work was preserved, conflicts were resolved by the integrator, and final changes were reviewed across team boundaries.
- [ ] Current docs describe the resulting behavior and verification limits accurately.

## Codebase Integration:

Follow `aspec/architecture/design.md`, `aspec/architecture/2026-grand-architecture.md`, `aspec/architecture/security.md`, and existing command/catalogue/test conventions. Data owns persistence and config contracts; engines own runtime mechanics; commands own orchestration/policy; frontends own interaction/presentation. Keep dependencies directed downward.

Prefer a small concrete struct, enum, or local helper when it removes an actual repeated invariant. Retain straight-line Rust when extracting a helper would only hide a few expressions or force unrelated callers through flags, callbacks, or generic parameters. Separate production and test concerns based on ownership rather than arbitrary file-size targets.

## Documentation

After implementation, update existing user-facing docs only for changed observable behavior (config errors, daemon recovery, output handling, runtime availability). Regenerate the command reference if catalogue changes require it. Put implementation rationale and evidence in this item or review notes, not in a new user-facing “WI 0129 guide.” Correct current development/spec guidance identified by F17 while preserving historical evidence archives. No documentation or source beyond this proposed work item was changed during the review.
