# Work Item: Task

Title: Codebase simplicity, modularity, idiomatic Rust, duplication, and architecture conformance remediation
Issue: Not linked
Status: Proposed — findings revalidated 2026-10-30 against `6128a50f`; architecture audit added; implementation not started. One maintainer decision (D1, under A05) is open; it blocks A05 only.

## Summary:

Implement the bounded remediations below as an agent team. Prioritize failures hidden by defaults, resource ownership, and duplicated behavioral policy before structural cleanup. Preserve the four-layer architecture and existing CLI/TUI/API contracts.

This item began as a repository-wide static review on 2026-10-21 (`98e11176` plus working-tree edits). Between then and this revalidation, main took 0120, 0130–0137, 0149, and 0150, which changed 414 files under `src/`, `crates/`, and `tests/` (about 83,000 lines added). Of the original 17 findings, 8 were narrowed by that work and 9 are unchanged. None were fully closed. This revision also adds a brief audit against `aspec/architecture/2026-grand-architecture.md` (A01–A08). Read that document before implementing anything here.

The request included “preventing code reuse.” The working interpretation is to prevent duplicated policy and unnecessary abstraction: share a proven invariant at its owning layer, and keep separate code for genuinely different runtime or presentation behavior. This item is not a mandate to prohibit ordinary reuse or to introduce a general framework.

“Slop” means concrete maintenance problems here: inert dependencies and parameters, fabricated success/defaults, repeated policy, stale factual comments, and tests that constrain spelling instead of behavior. It does not mean code was necessarily AI-generated, that long files are automatically wrong, or that all comments and explicit adapters should be removed.

### Finding status at `6128a50f`

| Finding | Priority | Status | What changed since 2026-10-21 |
| --- | --- | --- | --- |
| F01 queue completion | P1 | Narrowed | Shared completion path exists; a failed write still leaves the row `running`, and startup then requeues it |
| F02 prompt temp files | P1 | Open | Unchanged |
| F03 non-atomic saves | P1 | Narrowed | Global/repo config are atomic; workflow state and squad task config are not |
| F04 scheduler reload | P1 | Open | Unchanged |
| F05 API output pumps | P2 | Narrowed | UTF-8 splitting fixed and API pumps shared; quadratic copying, unbounded lines, and three framers remain |
| F06 TUI input types | P2 | Open | Unchanged |
| F07 config coercion | P2 | Narrowed | Session-environment write fixed; guessed value types remain |
| F08 launch policy copies | P2 | Open | Unchanged |
| F09 Docker listing | P2 | Narrowed | Errors now propagate; four parsers now exist, and some callers still discard errors |
| F10 lifecycle strings | P2 | Narrowed | A `CommandStatus` enum was added, but the store still uses strings and overlapping types now exist |
| F11 OCI retry | P2 | Open | Unchanged |
| F12 weak boundaries | P2 | Narrowed | Dispatch partly split; `ConfigCommand` still takes `Engines`, for one field |
| F13 TUI shared state | P2 | Open | Unchanged |
| F14 test environment | P2 | Narrowed | `git_engine.rs` has one lock; cross-file and restoration problems remain |
| F15 lint scanner failure | P2 | Open | Unchanged |
| F16 binary dependencies | P3 | Open | Unchanged |
| F17 stale prose | P3 | Open | Unchanged; more instances identified |

### Review coverage and limitations

The inventory at `6128a50f` contains 515 Rust files under `src/` (272,091 lines), 12 under `crates/` (2,707 lines), and 125 under `tests/` (52,084 lines). These counts include tests, comments, and blanks; they are not production-complexity scores. Vendored code was not re-reviewed in this revision.

The 2026-10-30 revalidation re-read the cited code for every finding, using file searches to find what moved and to look for new instances. The architecture audit is deliberately brief: it swept layer imports, frontend engine/persistence use, the largest frontend files, and the areas added since the last audit (`aspec/review-notes/0113-architecture-audit.md`). It is not a replacement for that 54-finding audit and does not claim every line was inspected.

Validation performed on 2026-10-30:

- `bash tools/architecture-lint.sh`: passed (see F15 and A05 for what that does and does not prove).
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: passed with default features.
- Not run: the test suite, the `builtin-runtime` feature, Docker/native runtime paths, macOS. Findings describing behavior are source-derived; none was reproduced with a failing test.

Line numbers are review-time locators. Use the named functions and types when edits shift them.

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

### User Story 4:
As a: future frontend author

I want to: get API admission, event replay, and command routing from Layer 2.

So I can: add a frontend without reimplementing business logic, as the grand architecture requires.

## Implementation Details:

### Priorities

- **P1:** correctness/resource risks to address before cosmetic restructuring.
- **P2:** concrete maintainability problems, architecture violations, drift risks, and narrower behavior defects.
- **P3:** dependency, documentation, and low-severity conformance cleanup after behavior is protected.

All findings are in scope, but land them in coherent batches. Do not combine all of this into one mechanical rewrite.

### F01 — P1: Failed queue completion leaves a finished command eligible for re-execution

**Already done:** `QueuedCommandResult::publish` (`src/command/commands/api_server/queue_worker.rs:330`) is now the shared completion path, including the early directory-failure branch at 655–679.

**Evidence:** when `persist` fails, `publish` logs the error and sends subscribers an in-memory terminal event, leaving the row `running`. At the next startup, `recover_stale_commands(0)` (`runtime.rs:272`, `src/data/fs/api_db.rs:1415`) returns every `running` row to `queued` and increments its attempt. Separately, `execute_command` decodes stored args with `unwrap_or_default` at line 634, ignores metadata writes at 695 and 1050, and `ApiServerRuntime` bootstrap discards lifecycle writes at `runtime.rs:163, 204, 213, 222–235`, including `finalize_unowned_command`.

**Impact:** clients are told a command finished while the database disagrees. A restart can then run the agent's side effects a second time. Corrupt stored args become an empty metadata vector instead of a queue-record error.

**Remediation:** retry the row write a small fixed number of times. Before the first attempt, have Layer 0 write a terminal sidecar in the command directory holding the completion's status, exit code, and result JSON. During startup recovery, a `running` row with a sidecar is finalized from it instead of requeued; the sidecar is removed once the row is terminal. A `running` row with no sidecar keeps today's requeue behavior, because that process really did die mid-run. Decode stored args once at the queue boundary and fail the command with a named queue-record error when they are invalid. Check the bootstrap writes at `runtime.rs:163–235`: failures of authoritative row writes are logged with session and command identity, and metadata remains best effort but logged.

**Acceptance:** with an injected completion-write failure after a side-effecting fake command, a restart finalizes the row and does not redispatch. A crash before completion still requeues. Cover directory/setup errors, malformed stored args, closing-session handling, and sidecar removal. Preserve exit-code and partial-failure behavior.

### F02 — P1: System-prompt files have engine lifetime instead of run lifetime

**Evidence:** `AgentEngine` (`src/engine/agent/mod.rs:235`) stores `Arc<Mutex<Vec<NamedTempFile>>>`; `write_prompt_temp_file` (1109–1128) only appends. There is no per-execution removal. `Engines` shares the `AgentEngine` across daemon commands. If the mutex is poisoned, the `if let Ok` at 1124 drops the new file while still returning its path.

**Impact:** completed runs retain open file handles and prompt files until the shared engine is dropped. Repeated daemon/API invocations grow the collection indefinitely; failed option preparation can retain files too. Poisoning produces a path to a file already removed.

**Remediation:** carry prompt-file guards with the prepared launch and transfer them to the instance/execution that uses them, following the existing execution-owned credential-lease pattern. Release on preparation failure, spawn failure, completion, cancellation, and dropped wait/execution. Do not replace this with a global cleanup task or filename-based deletion registry. Eliminate the silent poisoned-lock path.

**Acceptance:** many sequential runs through one engine return prompt-file/handle counts to baseline; two concurrent runs retain only their own live files. Exercise failure and cancellation, including dropping an unlaunched plan. Files must remain present for the entire period the guest can read them. Check custom agents (`src/engine/agent/custom.rs`), added since the original review, on the same path.

### F03 — P1: Workflow state and squad task config saves still truncate the live record

**Already done:** `GlobalConfig::save_with` and `RepoConfig::save` publish through `src/data/fs/config_file.rs` (`write_config_file_atomically`). Do not rework them.

**Evidence:** `WorkflowStateStore::save` (`src/data/workflow_state_store.rs:88`) and the squad task config writer (`src/data/fs/task_store.rs:389`) still use direct `std::fs::write`. Direct writes also remain in `src/data/fs/api_paths.rs:276` (session setup state) and `:324` (command metadata), `daemon_process.rs:223` (server metadata), `skill_library.rs:56`, and `log_dirs.rs:77`.

**Impact:** a concurrent reader can see partial JSON, and an interruption or failed write can destroy the last valid checkpoint or task config. The squad scheduler re-reads task config on every tick, so it can observe a partial file.

**Remediation:** publish workflow state and squad task config through a sibling temporary file and atomic rename. Reuse `write_config_file_atomically` if its symlink and mode behavior fits; otherwise factor its rename step and share that. Classify each remaining direct write listed above: convert it if a reader depends on the file being complete, and otherwise record why not. Atomic replacement is not read/modify/write conflict detection; preserve concurrent-writer behavior.

**Acceptance:** injected failure before publication leaves the original readable record intact; readers observe complete old/new JSON, never a partial record. Check temp cleanup, existing permissions, first creation, and workflow resume after a failed save. Validate replacement on supported platforms, not only one Unix host.

### F04 — P1: Scheduler reload errors silently change policy

**Evidence:** `tick` (`src/engine/squad/scheduler.rs:228`) calls `GlobalConfig::load_with(&self.env).unwrap_or_default()`, and its comment explicitly describes the fallback. Task-specific config errors later fail the run, so global and task errors are handled inconsistently. `Engines::for_daemon` already refuses a malformed file at startup, so only the live reload has this gap.

**Impact:** malformed or unreadable global config discards the configured concurrency limit, guidance, and leader/model policy while the scheduler continues admitting work.

**Remediation:** on a failed reload, admit no new work for that tick, report the named config error once per distinct error rather than every tick, and continue observing existing runs. Resume admission automatically on the next valid reload. Do not keep a last-known-good policy. A genuinely absent file keeps its documented default behavior.

**Acceptance:** valid → malformed/unreadable → valid transitions do not launch with default policy, do not lose in-flight accounting, and recover without restart. Retain live reload for valid edits.

### F05 — P2: Output line framing is copied, quadratic, and unbounded

**Already done:** both API `take_io` implementations delegate to `spawn_output_drains` (`src/frontend/api/command_frontend.rs:566`), which buffers bytes and decodes whole lines, so split multibyte characters no longer corrupt. Drains are tracked by the event bus's completion barrier (`event_bus.rs:161`).

**Evidence:** the same find-newline/`drain` loop appears for stdout (595) and stderr (619) in that function, and again in `forward_container_stream_to_tracing` (`src/command/commands/api_server/session_setup.rs:315`), whose task is not tracked by any barrier. `report_status` mapping is repeated at `command_frontend.rs:487` and `:802`. ACP has its own framer (`src/engine/acp/protocol.rs:381–391`); leave it, because it is protocol framing.

**Impact:** each line drains the remaining buffer, so a large chunk with many lines copies repeatedly. An unterminated line grows memory without a defined limit. A fix must be applied in three places.

**Remediation:** add one small byte-oriented line framer below the API frontend, with explicit invalid-UTF-8, EOF, and overlong-line behavior (emit the bounded prefix and mark it truncated). Use it from all three pumps. Lower-layer session setup must not import the API frontend. Do not build an all-frontends output framework.

**Acceptance:** test every split point of multibyte UTF-8, multiple lines per chunk, empty lines, invalid bytes, EOF without newline, and a long line's bounded handling. Verify event order for a short-lived execution, stdout/stderr distinction, stdin EOF, and WI 0137 translator behavior.

### F06 — P2: TUI input discards types already parsed by Layer 2

**Evidence:** `ParsedArgs::into_command_box_input` (`src/command/dispatch/projections/raw_args.rs:290–305`) converts typed numbers and paths into `FlagValue::String`. `TuiCommandFrontend` reparses them at `src/frontend/tui/command_frontend.rs:457–487`. API parsing keeps `parsed` plus `parse_error` (`api/command_frontend.rs:91, 190`) and reconstructs error variants by hand in `clone_parse_error` (442).

**Impact:** two representations and repeated accessors make parity harder to maintain. New value/error variants require synchronized edits. The TUI applies validation policy in a presentation layer, against Tenet 2.

**Remediation:** retain one typed parsed representation through TUI/API dispatch, adding only the canonical command path the command box needs. Keep caller-profile defaults in Layer 2. Store the API result as `Result<ParsedArgs, CommandError>` behind a shared handle instead of two fields, so `clone_parse_error` can be deleted. Retain the CLI's clap adapter.

**Acceptance:** shared cases cover omitted/explicit false flags, default/forced flags, aliases, numeric bounds, path positionals, trailing args, repeated flags, and hyphen-leading agent args. Declared per-frontend differences remain intact. Errors retain structured classifications.

### F07 — P2: Config coercion guesses the value type instead of using the field schema

**Already done:** global writes now use `save_with(session.env())` (`src/command/commands/config.rs:1309`). Do not rework it.

**Evidence:** `src/data/config/fields.rs` describes each field's kind and shape, but `validate_and_coerce_scalar` (`config.rs:194`) still uses string-keyed special cases and ends in a generic bool → number → string fallback (about line 277). `validate_default_leader_value` (292) separately implements validation also owned by config loading.

**Impact:** a string field such as `work_items.dir` given `123` or `true` becomes a JSON number/bool and is then rejected by typed-config validation. A valid textual value cannot be saved through the editor. Validation and hints can drift between editing and loading.

**Remediation:** dispatch coercion by explicit field kind and shape, sharing domain validators with config loading. Keep dynamic guidance/model-map rules explicit. Extend the field table only as needed. String values stay strings regardless of their contents. Cover the fields added by WI 0150's dynamic config migration.

**Acceptance:** string values `123`, `true`, and `false` round-trip through CLI and TUI; invalid booleans/numbers fail before writing. Cover list/map deletion, guidance containing commas, leader validation parity, field scope, and sensitive-value masking.

### F08 — P2: Command and workflow launch policy still has parallel copies

**Evidence:** the ACP-explicitness expression is repeated at `src/command/commands/chat.rs:129` and `exec_prompt.rs:244`, the overlay parsing loops at `chat.rs:172` and `exec_prompt.rs:272`, and `command_effective_config` at `chat.rs:407` and `exec_prompt.rs:589`. Workflow `launch_step` (`src/engine/workflow/single_step.rs:160`) and `launch_parallel_step` (`parallel.rs:187`) repeat resolution, runtime-context construction, Running transitions, persistence, and factory invocation almost line for line.

**Impact:** updating a launch rule or pre-spawn failure path requires matching edits across entrypoints. If the factory fails after Running was persisted, the step is left recorded as Running with no container.

**Remediation:** move ACP decision inputs and overlay-list parsing into small Layer 2 helpers beside `LaunchPolicy`. Factor the common workflow preparation/spawn/state-recording step within the workflow engine, returning a concrete launch result; keep single-step waiting, parallel fan-in, retry, UI callbacks, and cancellation scheduling distinct. On factory failure, move the step out of Running before returning the error, and persist that.

**Acceptance:** add focused cross-entrypoint launch cases and single/parallel spawn-failure cases. Retain plan/yolo/ACP rules, non-interactive prompt delivery, worktree handling, and parallel recovery tests. Avoid a “runner” abstraction controlled by many booleans.

### F09 — P2: Docker listing has four parsers, and some callers still discard failures

**Already done:** `list_running` and `list_running_all` now propagate errors through `checked_output`, and WI 0128 has landed. A failed query now fails the listing as a whole, which is the defined behavior for partial failure.

**Evidence:** `src/engine/container/docker.rs` repeats row parsing in `list_running` (74), `list_running_all` (140), `list_stopped` (204), and `list_running_with_name_prefix` (361). They have diverged on empty rows: the first skips nothing, the second and fourth skip only when id and name are both empty, and the third skips on empty id. Unparseable timestamps become `now()` in all four. `list_running` ignores its `_session` parameter. Callers still convert failure into absence at `src/command/commands/agents/resolve.rs:55`, `src/command/commands/squad/attach.rs:567`, and `src/command/stats_sampler.rs:132`.

**Impact:** divergence continues, and a daemon failure can still read as “not found” in those paths.

**Remediation:** use one backend-local query/parser for the shared ownership-filtered listing. Classify each listed caller: the stats sampler may stay best effort, and that gets a comment; resolve and attach return the contextual error. Preserve deliberate legacy-name ownership compatibility. Do not generalize Docker argv into a common runtime contract, and do not touch the Apple backend's own listing.

**Acceptance:** a fake host CLI covers empty success, unavailable daemon, nonzero exit, failure of one of the two queries, malformed rows, and deduplication. `agents status`, `agents kill`, and `squad attach` report an unavailable runtime instead of “not found.”

### F10 — P2: API lifecycle states are mostly unrestricted strings, and status types now overlap

**Already done:** `src/data/command_status.rs` names command status spellings, and its doc comment records that the column stays a free `String`.

**Evidence:** `SessionRecord` and `CommandRecord` (`src/data/fs/api_db.rs:26, 32, 105`), plus `update_setup_status` (685), `complete_command` (1329), and `update_session_status` (1466), still take strings. `QueuedCommandResult.status` is `&'static str` (`queue_worker.rs:46`). Session status has no enum, so literals are compared in `runtime.rs:158, 475–476, 493, 569–570, 633–637`, `queue_worker.rs:432–434, 1005, 1081`, and `routes.rs:372, 1238, 1246, 2299`. There are now three command-status types: `data::command_status::CommandStatus`, `data::session::CommandStatus` (`session.rs:120`), and `data::execution_event::CommandStatusKind`.

**Impact:** invalid states and spelling drift compile. Two public types with one name and different variants invite the wrong import.

**Remediation:** add a narrow `SessionStatus` enum. Use it, `CommandStatus`, and the existing `SessionSetupStatus` in records and mutation methods, converting to the existing SQLite/wire spellings at the store boundary. Report unknown stored values explicitly. Rename or remove `data::session::CommandStatus` after checking its users, and keep `CommandStatusKind` only as the wire event type with an explicit conversion. No schema migration and no typestate framework.

**Acceptance:** retain database and API compatibility fixtures. Cover every persisted spelling, the legacy `pending`, invalid stored values, and lifecycle transitions. Preserve the distinction between queued cancellation, execution error, session closing, and setup failure.

### F11 — P2: OCI retry classification depends on diagnostic text

**Evidence:** `is_retryable` (`src/engine/oci/retry.rs:124–126`) treats `EngineError::Network(String)` as retryable unless `is_final_network_failure` finds selected TLS/setup phrases in the text (141–163). `registry.rs:824, 1044, 1086, 1115` map malformed manifest/index/config JSON to that same variant; Docker metadata decoding does the same at `docker_engine.rs:317`.

**Impact:** deterministic malformed content is retried despite the documented transport-only policy. Changes to error wording or contextual names can change retry decisions. The tokenizer added to avoid accidental TLS matches is complexity caused by losing the error category.

**Remediation:** retain a small typed transient/permanent classification at source boundaries, covering content/schema failures, TLS/auth failures, and actual disconnect/timeout/5xx cases. Keep redacted diagnostics separately. Keep cancellation and deadlines authoritative. Do not replace the project error hierarchy.

**Acceptance:** assert attempt counts with local fixtures for malformed JSON, digest/platform mismatch, TLS/auth refusal, 5xx, and truncation. Changing a diagnostic's text or a reference name containing `tls` must not change retryability. Retain cancellation, private staging, and provenance checks.

### F12 — P2: Module/dependency boundaries are weaker than their signatures suggest

**Already done:** `dispatch/` now has `build.rs`, `parsed_input.rs`, `resolved.rs`, `runtime_context.rs`, and `frontend_action.rs`.

**Evidence:** `src/command/dispatch/mod.rs` (2,557 lines) still combines engine factories, admission, outcomes, and execution dispatch with extensive inline tests. `Engines::for_daemon` (186) accepts `DataPaths` but only reads `paths.root()` into unused `_shared_data_root` (231) while deriving actual paths from the process environment. `ConfigCommand` stores `Engines` (`config.rs:872`) and uses only `config_notices` (955).

**Impact:** dependencies advertised by the API are not real dependencies, which adds setup burden in tests and hides ownership.

**Remediation:** remove the `paths` parameter from `for_daemon` and update its one production caller (`squad/daemon.rs:209`) and its tests. Give `ConfigCommand` only the notices handle. Move engine assembly and outcomes out of `dispatch/mod.rs` into responsibility-named modules with compatible reexports. Move tests only where it helps navigation. Do not split every layer into a new crate or hide the command enum in macros.

**Acceptance:** running config operations does not require runtime engines. Catalogue completeness, runtime startup/fallback, exit-code, and command dispatch tests continue to pass without broad new mocks.

### F13 — P2: TUI state is bundled at construction but fragmented during updates

**Evidence:** `TabSharedState` (`src/frontend/tui/tabs.rs:635`) has 19 fields, mostly independent shared slots for container identity/title/exit, I/O channels, workflow identity/view/path, and countdown state. `TuiCommandFrontend::new` (`command_frontend.rs:153`) unpacks them into a parallel field list. `App::refresh_tabs_from_sessions` (`app.rs:742`) and related methods coordinate the snapshots.

**Impact:** one logical lifecycle transition requires several independent updates and matching reset code. Existing tests for exit plus group-start/PTY-reset in one tick show that ordering matters; the constructor bundle does not enforce it.

**Remediation:** group state by lifecycle ownership (workflow identity/view metadata; active container I/O identity) and expose focused transition methods. Extend the existing container-slot event mechanism where it simplifies ordering. Keep presentation state in Layer 3. Do not put all TUI state behind one global mutex, rewrite the UI as an actor framework, or remove per-slot parallel behavior.

**Acceptance:** preserve same-tick exit/new-launch ordering, per-slot stats and cancellation, stale-event rejection, tab closing, reset behavior, and dialog input preservation. Each moved field has one clear writer. File splitting alone does not close this finding.

### F14 — P2: Integration tests still share process environment under unrelated locks

**Already done:** the two locks inside `tests/engine/git_engine.rs` are now one `HOME_ENV_LOCK` (105).

**Evidence:** `tests/engine/main.rs` compiles daemon, git, and overlay tests into one binary. `daemon_supervisor.rs:27, 140` changes `HOME` under its own `PATH_LOCK`, so it does not exclude `git_engine.rs`, which changes `HOME` at 403. `overlay_engine.rs:12–24` has a third lock. `restore_env` (`git_engine.rs:415`) saves through `unwrap_or_default`, so an empty value and an absent one are treated alike, and restoration in these helpers runs only if the test does not panic. `set_var` now appears in 14 test files.

**Impact:** unrelated mutexes do not serialize the same process-global `HOME`; a panic can leave changed state for later tests. Separate top-level test binaries do not share a process, so do not “fix” all similarly named locks indiscriminately.

**Remediation:** prefer injected `EnvSnapshot`/path context or child-specific `Command::env`. Where a true process-global probe is unavoidable, use one guard for that test executable, including affected readers. Restore via RAII with `Option<OsString>`.

**Acceptance:** run the affected target with normal and high test concurrency, and demonstrate panic-safe restoration in a focused guard test. Cover the macOS `HOME` collision. Do not force all project tests to one thread.

### F15 — P2: Architecture checks can mistake scanner failure for success

**Evidence:** `tools/architecture-lint.sh:37, 132, 184, 214` suppress scanner stderr and use `|| true`; line 132 relies on a `grep -P` negative lookahead. Other checks use the same no-match/error collapse. A successful run proves the local scan result, not that all required scanners work on every supported host.

**Impact:** an unavailable regex capability or failed input scan can disable an enforcement rule without failing CI.

**Remediation:** distinguish grep's no-match exit status from execution errors, check required inputs/tools, and use portable regexes. Add small rule fixtures for allowed/disallowed imports and scanner failure. Keep the existing script; no new `syn` tool is required.

**Acceptance:** unsupported scanner behavior and missing/unreadable inputs fail with an actionable diagnostic; allowed fixtures pass; direct/grouped forbidden imports fail. Exercise the script on the Linux/macOS CI environments. Preserve the security-oriented guards.

### F16 — P3: The binary package repeats library dependencies and build responsibilities

**Evidence:** `crates/awman-cli/Cargo.toml` repeats most root library normal dependencies and builtin optional SDK entries. `src/main.rs` and `crates/awman-cli/src/` directly name only about ten crates (`anyhow`, `chrono`, `clap`, `tokio`, `tracing`, `tracing_subscriber`, and the awman crates). Integration tests and guest examples also belong to this package. All three packages include the same root build script.

**Impact:** the manifest obscures production versus test/example requirements and repeats feature wiring. This review has not run an unused-dependency tool and does not claim every repeated dependency is unused or that deleting it will reduce binary size.

**Remediation:** audit actual normal/build/dev/example use per package and feature. Move test-only dependencies to dev-dependencies, remove proven unused entries, and forward builtin features through the crates that own them. Keep centralized workspace versions, exact SDK pins, and the host-shim unsafe boundary. Separate shared build verification from kernel-source generation only if the audit shows a package needs one and not the other.

**Acceptance:** record the reason for retained exceptions. Build workspace libs, binary, tests, and both guest examples under default/builtin configurations and supported target gates. Verify the single-binary worker path and payload checks. No opportunistic upgrades or patch removals; coordinate those with WI 0120/0124.

### F17 — P3: Stale migration prose obscures current contracts

**Evidence:**

- `crates/awman-cli/Cargo.toml:10–13` says top-level tests await WI 0072, and line 223 refers to the `oldsrc` surface. The same manifest registers current suites.
- `aspec/devops/localdev.md:35` says `tests/builtin_runtime/{network_resources,lifecycle,acquisition}.rs` are not registered, but `tests/builtin_runtime/main.rs` registers all three. Lines 47–66 describe a `benches/performance.rs` Criterion target that does not exist.
- `oldsrc` is still named in 14 files. `src/command/dispatch/catalogue/mod.rs:6` says the catalogue enumerates `oldsrc/cli.rs` exactly, and `src/frontend/api/routes.rs:3` says the router is wire-identical to it. Neither is a meaningful claim now.
- `ResolvedAgentOptions::sandbox` (`src/engine/agent_runtime/mod.rs:78–80`) mentions a `Result` signature although it returns `Self`.

**Impact:** agents and maintainers can follow obsolete instructions, misjudge coverage, or preserve scaffolding whose stated need is gone.

**Remediation:** correct current factual guidance, remove completed-migration instructions, and shorten history-heavy comments to the invariant a future edit must preserve. Keep safety/FFI, lifecycle, cancellation, and compatibility rationale. Do not mass-edit work item references out of source (about 250 files have them) or edit historical work items and evidence archives.

**Acceptance:** development commands and test-registration statements match the final manifests. Changed comments describe current behavior and do not claim stronger isolation, atomicity, cancellation, or native verification than tests establish. No comment-deletion quota.

## Architecture audit against the grand architecture

Audited 2026-10-30 at `6128a50f`. Rules: T1–T3 are the document's tenets; L0–L4 are its layers.

### What conforms

These were checked and need no work. Do not reopen them under this item:

- No upward imports: `architecture-lint` passes, and a separate search of `src/data` and `src/engine` found no `crate::command` or `crate::frontend` use.
- The critical and high findings of the 0113 audit that were re-checked stay fixed: `squad attach` is a Layer 2 command (`src/command/commands/squad/attach.rs`) with presentation-only frontends and no `src/frontend/attach.rs`; the squad daemon frontend's `mod.rs` is 53 lines; API bootstrap is in `ApiServerRuntime`; `QueueWorker` is in Layer 2; session close goes through `state.lifecycle().close`.
- `src/frontend/squad/unattended.rs` delegates its answers to `HeadlessDefaults::squad` and writes logs through Layer 0's `SquadRunLog`.
- `dispatch/build.rs` is the catalogue's function-pointer table. `command/prompts.rs` holds prompt constructors. Their raw `pub fn`s are within T3's exceptions.
- No `Arc::try_unwrap` in `src/command`, and no `runtime_name()` comparison or downcast of runtimes in non-test `src/command` or `src/frontend` code.
- No non-test `std::fs`, process-spawn, or `rusqlite` use in `src/frontend` was found. The one hit outside `#[cfg(test)]`-named files, `cli/per_command/helpers.rs:207`, is inside that file's test module.

### A01 — P2: API admission and idempotency are implemented in the frontend

**Rule:** T2; L3 “no specific persistence logic,” “only job is to translate.” **Kind:** violation.

**Evidence:** in `src/frontend/api/routes.rs`, `create_submission` (866) inserts the session row (954), records remote fields (972) and labels, records the run (1008), enqueues the command, compensates by closing the session on failure, and creates the setup bus. `enqueue_command` (1581) performs the command insert. `AdmissionJournal` and `with_idempotency` (494–700) implement the idempotency reservation state machine, including its `Drop` release (560).

**Impact:** the ordering of these writes is crash-consistency logic, as the file's own comments say. Any other HTTP host, including the daemon proposed by WI 0145, would have to reproduce it. It is also not testable without HTTP.

**Remediation:** add a Layer 2 admission type under `src/command/commands/api_server/` that takes typed session/command submissions and an idempotency key, owns the writes, compensation, and reservation lifecycle, and returns a typed outcome. Routes parse requests, call it, and map outcomes to status codes, following the `CloseOutcome` pattern already in `runtime.rs`.

**Acceptance:** existing API/idempotency tests pass unchanged. New Layer 2 tests cover crash-order and compensation without HTTP. `routes.rs` has no store mutation calls for sessions, commands, runs, labels, or idempotency.

### A02 — P2: API routes own command log replay and gap repair

**Rule:** T2. **Kind:** violation.

**Evidence:** `CommandLogCursor` (`routes.rs:1679`), `ReplayFrom` use, `command_log_events` (1825), `finish_terminal_log` (1840), `recover_command_log_gap` (1909), and `wait_for_worker` (1943) implement resume, deduplication, gap repair, and terminal-event rules. `handle_stream_command_logs` (1983) runs them inline in the SSE stream.

**Impact:** event delivery guarantees are business rules in a transport adapter. WI 0146's live terminals and any non-SSE consumer would need the same rules.

**Remediation:** move the cursor and replay logic to a Layer 2 stream type next to the event bus, yielding `ExecutionEvent`s. The route keeps only SSE encoding, the query mapping, and shutdown wiring.

**Acceptance:** the WI 0136 resume, dedupe, and step-filter tests still pass, and cursor logic gains tests that do not start a server. Wire output is byte-compatible.

### A03 — P2: API routes make lifecycle and read-model decisions

**Rule:** T2; L3 “no validation of requests.” **Kind:** violation.

**Evidence:** `command_is_terminal` (`routes.rs:278`); session status validation with literals in `ListQuery` (372); queue assembly and positions in `handle_get_session_queue` (2232–2310), including filtering on `"queued"` and decoding stored args with an empty-array fallback; `fallback_status_from_db` (1335).

**Remediation:** after F10, move these to Layer 2/Layer 0 read-model methods returning typed views: `SessionStatus::parse`, `CommandStatus::is_terminal`, a queue-view method, and the status fallback. Share F01's stored-args decoding. Field-level request shape validation remains acceptable where serde does it. WI 0138 owns the error-code and resource-layout work; do not duplicate it.

**Acceptance:** status codes and bodies are unchanged except where WI 0138 deliberately changes them. No lifecycle literal comparisons remain in `routes.rs`.

### A04 — P2: The CLI routes `exec workflow` itself

**Rule:** T2; L3 CLI “calls `run...` on whatever `Command` `Dispatch` provides.” **Kind:** violation.

**Evidence:** `src/frontend/cli/mod.rs:44` compares the path to `["exec", "workflow"]`, builds the command, matches `BuiltCommand::ExecWorkflow`, swaps in `CliParallelFrontend`, and rebuilds `CommandOutcome::ExecWorkflow` by hand. This is the last command path named in the CLI's `run`.

**Impact:** this path can bypass what `Dispatch::run_command` does for other commands, so admission and start recording must be kept equivalent by hand.

**Remediation:** let the dispatch path obtain the workflow frontend from a frontend-trait method, with the CLI's implementation returning `CliParallelFrontend`, and delete the branch. Before deleting it, list what `run_command` does that this branch skips, and record any behavior change in this item.

**Acceptance:** `exec workflow`, alias `wf`, JSON output, and exit codes behave as before in `cli_parity` and binary smoke tests. `cli::run` contains no command-specific branch.

### A05 — P2: Tenet 1 and the lint disagree on layer skipping (needs a maintainer decision)

**Rule:** T1. **Kind:** spec ambiguity.

**Evidence:** Tenet 1 says each layer exposes its API “ONLY to the layer ABOVE,” while the lint header explicitly allows Layer 3 to import `data` and `engine`, and Layer 2 to import `data`. Non-test frontend code has 124 `use crate::engine::` and 150 `use crate::data::` lines. Most are types and engine-defined frontend traits (`engine::workflow`, `data::message`). Some are behavior: `Tab::new_with_git_engine` and `start_git_diff_poll_task` (`tui/git_sidebar.rs:63`) poll `GitEngine` from the TUI; `routes.rs:890, 897` pass `engines.git_engine` into request validation; `tui/app.rs:1123` drains notices from the concrete `ContainerRuntime`. The same document also tells the TUI and API to use `SessionManager` directly.

**D1 — Decision needed:** what may Layer 3 import from Layers 0 and 1? The recommendation, which implementers should assume only after the maintainer confirms it: Layer 3 may import data types and engine-defined frontend traits/types, but may not call engine behavior except through Layer 2. Tenet 1 and the lint header are then amended to say so. The alternative, reexporting everything through Layer 2, is about 270 import lines of churn for little protection.

**Remediation once decided (under the recommendation):** route the three behavior cases above through Layer 2 (a small Layer 2 git-summary source for the TUI, session-create validation inside A01's admission type, runtime notices via a Layer 2 method). Add a lint rule rejecting `engines.<engine field>.` in `src/frontend`, with the fixtures from F15. Do not start this finding before D1 is answered.

**Acceptance:** the document and lint agree; the lint rule has allow and deny fixtures; the TUI sidebar behaves the same.

### A06 — P3: The binary does more than set up frontends

**Rule:** L4 “builds the `clap` command structure, then executes it. That's it.” **Kind:** violation, low severity.

**Evidence:** `frontend_main` in `src/main.rs:33` prints startup messages and runtime notices, decides by launch mode where notices go, and drains deferred notices from `ctx.engines.container_runtime` after the CLI returns. The logic that decides TUI versus CLI has its own tests there.

**Remediation:** move notice output into `cli::run` and `tui::run`, which already receive the context, with startup messages passed in. Keep clap construction, tracing setup, the native transport install, and launch-mode selection in the binary. Layer 2's `Startup` stays as it is.

**Acceptance:** notice order and stream (stderr in command mode, status area in the TUI) are unchanged. `main.rs` no longer references `container_runtime`.

### A07 — P3: Layer 0 holds traits and self-described policy

**Rule:** L0 “things that do not belong: layer-specific types such as traits or business logic concerns.” **Kind:** drift; triage required.

**Evidence:** `src/data` defines `UserMessageSink`, `GitRootResolver`, `SessionStore`, and `DaemonEnvStore`. `src/data/workflow_prerequisites.rs` (1,172 lines, new since the last audit) opens with “Workflow prerequisite data and policy.” `merge_fix_prompt.rs` holds prompt rendering, while `command/prompts.rs` is where the 0114 audit placed prompt copy.

**Remediation:** review `workflow_prerequisites.rs` and move decision logic to `src/engine/workflow/prerequisites/`, leaving serializable types and pure validation of them. Move `merge_fix_prompt.rs`'s rendering only if a Layer 1+ owner is clear. Keep the four traits: each abstracts a storage or environment seam that Layer 0 itself consumes, so moving them would invert a dependency. Instead, correct the document's wording to allow storage-seam traits.

**Acceptance:** the moved logic's tests move with it, and the review outcome for each named item is recorded here. Prerequisite behavior is unchanged.

### A08 — P3: The architecture document omits current components

**Rule:** document accuracy. **Kind:** documentation.

**Evidence:** Layer 3 lists CLI, TUI, and API, but `src/frontend/squad/` is a fourth frontend. `Engines`, `Startup`, `HeadlessDefaults`, the squad engine, and the catalogue-driven build table are not described. The list of key Layer 1 components omits `squad`, `oci`, `acp`, and `credential_refresh`. `aspec/architecture/four-layer-summary.md` and `docs/architecture.md` should be checked for the same gaps.

**Remediation:** make a short factual update after A01–A05 settle, so the document describes the final state. WI 0145 will change the daemon section, so keep daemon wording minimal. Leave the document's historic framing alone.

**Acceptance:** each top-level module in `src/engine` and each directory in `src/frontend` is either described or covered by a stated general rule.

### Existing designs to preserve

- The catalogue, `LaunchPolicy`, agent matrix, and headless profiles are useful owners of shared policy. Extend them narrowly instead of making a competing universal registry.
- Credential leases are intentionally execution-owned; the safe-library/native-shim split and explicit runtime backend operations are intentional boundaries.
- Docker, Apple, builtin, and sandbox differences are real. Identical-looking option fields do not justify collapsing different launch, mount, image, or credential semantics.
- OCI provenance, bounded acquisition, archive validation, network policy checks, and vendored patch removal gates are not cleanup targets merely because they are verbose.
- `#[cfg(test)]` fakes with deliberate panic-on-unexpected-use are not unfinished production implementations. Feature/platform-specific dead-code allowances require context rather than blanket removal.
- WI 0135's cancellation and recovery recipes, WI 0136's resumable events, WI 0137's structured results and final-message sink, and WI 0149's teardown hardening are recent behavior contracts. Moving their code (A01–A03) must not change them.

### Agent-team execution plan

Use a coordinating integrator and bounded implementers. These are future assignments, not agents started by this review. Each implementer first reproduces or characterizes its findings against the then-current tree, and records any finding invalidated by intervening work.

| Owner | Findings / owned area | Dependencies and handoff |
| --- | --- | --- |
| Integrator | Baseline, shared interface decisions, final review, F12, A05 (after D1), A06, A08 | Reserve `dispatch/mod.rs`, manifests, module exports, and common error variants; approve small interface changes before consumers branch |
| Persistence/config implementer | F01, F03, F04, F07, F10 | Land F03/F04 first; coordinate typed statuses with queue completion and with the API implementer |
| Runtime implementer | F02, F08, F09, F11, A07 | Agree launch-resource ownership first; hand typed launch inputs to command changes |
| API implementer | A01, A02, A03, F05 | A03 follows F10. Agree the admission type with the integrator. Coordinate with WI 0138 and WI 0145 owners |
| Frontend implementer | F06, F13, A04 | Agree the typed input boundary with the integrator. A04 follows F06 |
| Verification/cleanup implementer | F14–F17 | Run after or alongside independent fixes; the integrator owns final manifest edits; never change other agents' source to make a check pass silently |

Run only as many assignments concurrently as the available team supports. Test changes beside owned source remain owned by that implementer; the verification owner integrates cross-cutting fixtures and reports failures rather than racing to edit shared files.

Landing sequence:

1. Establish the baseline and focused failing regressions; agree DB status, input, resource, and admission contracts. Get the D1 decision.
2. Land F01–F04 correctness fixes with their tests. Review F03/F04 together for reload behavior.
3. Land F05–F11 in independently reviewable behavior/duplication batches.
4. Land A01–A03 before WI 0138 and WI 0145 start, because those items change the same routes and daemon code. If either has started, the integrator re-sequences with its owner.
5. Land F12, F13, A04, and A05 after the affected contracts settle, preserving behavior.
6. Finish F14–F17 and A06–A08, then run the integrated validation matrix.

For each finding, record: commit/change location, behavior before/after, focused evidence, retained intentional duplication, and any blocked platform check. A finding may be withdrawn only with concrete current-tree evidence; file movement, fewer lines, or a green lint alone is not remediation.

## Edge Case Considerations:

- Main moves quickly; revalidate each finding before changing it, and do not revert unrelated in-progress work.
- Preserve CLI exit codes, API payload spellings, old SQLite records, config precedence, aliases, and declared per-frontend defaults. Any deliberate behavior correction must be named in the review.
- Keep command completion failures distinct from harmless receiver shutdown or best-effort cleanup. Do not mechanically replace every `let _ =`, `.ok()`, `unwrap`, or `unwrap_or_default`.
- The F01 sidecar is written before the row, so a crash between the two must finalize from the sidecar exactly once and must not publish a contradictory terminal event. A sidecar for a row that is already terminal is removed without changing the row.
- A dropped/cancelled operation must release its own resources without deleting another live run's files or terminating an attached agent it does not own.
- Shared state snapshots must not mix prior and current workflow/container identities, especially across parallel retries and same-tick events.
- Moving idempotency logic (A01) must keep a retried request from creating a second session or command across a crash, and must keep the reservation released when a handler future is dropped.
- Avoid widening mounts, network policy, credentials, or host execution while consolidating code. Keep native and sandbox capability refusals explicit.
- Keep private-file versus repo-file permissions intentional; atomic publication must not silently change established symlink or concurrent-writer behavior.
- Coordinate upstream runtime work with WI 0120, Linux/native gates with WI 0124, API surface with WI 0138, daemon structure with WI 0145, and live terminals with WI 0146. Do not duplicate their feature scope here.

## Test Considerations:

Use each finding's acceptance tests and the repository's isolation runner. Add tests for observable contracts and failure modes, not one assertion per trivial delegate. Preserve existing behavioral coverage while removing only demonstrably redundant scaffolding.

Required integrated checks on a machine with the toolchain:

```sh
bash tools/architecture-lint.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
make test-fast AWMAN_TEST_TMPROOT=/tmp/awman-0129-tests
```

Run focused tests through `tools/isolated-test.sh` with a writable private fixture root. Use the correct package/target registrations in `crates/awman-cli/Cargo.toml`; listing or source-presence tests alone do not prove execution. Do not opt into real runtimes or services for a hermetic regression.

On provisioned supported hosts, also compile/lint the builtin feature, exercise affected Docker/native runtime cases, and validate macOS environment/lint behavior. Follow `aspec/devops/localdev.md` and the CI gates for required payloads, signing, fixtures, and hardware. Default-feature success does not establish builtin, Apple XPC, KVM/HVF, or release-artifact correctness. Record missing prerequisites as BLOCKED, with the specific check still outstanding.

Completion criteria:

- [ ] F01–F17 and A01–A08 each have implementation/test evidence or a justified current-tree withdrawal.
- [ ] D1 is answered and recorded here, and Tenet 1 and the lint agree.
- [ ] Shared policy has one owning implementation; retained differences are explicit.
- [ ] `src/frontend/api/routes.rs` contains HTTP translation only, as A01–A03 define it.
- [ ] No new general framework, global service locator, blanket lint suppression, or fallback-to-success was introduced to satisfy a cleanup metric.
- [ ] Targeted behavior and compatibility tests pass; required platform checks have results.
- [ ] Conflicts were resolved by the integrator, and final changes were reviewed across team boundaries.
- [ ] Current docs describe the resulting behavior and verification limits accurately.

## Codebase Integration:

Follow `aspec/architecture/design.md`, `aspec/architecture/2026-grand-architecture.md`, `aspec/architecture/security.md`, and existing command/catalogue/test conventions. Data owns persistence and config contracts; engines own runtime mechanics; commands own orchestration/policy; frontends own interaction/presentation. Keep dependencies directed downward.

Prefer a small concrete struct, enum, or local helper when it removes an actual repeated invariant. Retain straight-line Rust when extracting a helper would only hide a few expressions or force unrelated callers through flags, callbacks, or generic parameters. Separate production and test concerns based on ownership rather than arbitrary file-size targets.

## Documentation

After implementation, update existing user-facing docs only for changed observable behavior (config errors, daemon recovery, output handling, runtime availability). Regenerate the command reference if catalogue changes require it. Put implementation rationale and evidence in this item or review notes, not in a new user-facing “WI 0129 guide.” Correct the development/spec guidance identified by F17 and the architecture documents per A05 and A08, while preserving historical evidence archives.
