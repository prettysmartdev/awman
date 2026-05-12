# Work Item: Task

Title: grand architecture refactor — Layer 1/2 business-logic completion + full CLI completion
Issue: n/a — fifth-of-eight work item implementing `aspec/architecture/2026-grand-architecture.md`

> **Scope note (post-rewrite):** the original 0070 was scoped as "TUI parity only" on the assumption that work item 0067 had landed real engine bodies and 0068 had landed real command bodies. In practice, 0067/0068/0069 shipped only the **structural skeleton** of Layers 1–3: typed traits, phase-machine enums, command structs, frontend trait dispatch — but every container backend is a no-op (`docker.rs:103: "Until full subprocess wiring lands, hand back a finished execution representing a no-op success"`), every multi-phase engine body sets `StepStatus::Done` without doing the work, every interactive command body returns `Ok(...Outcome { … })` without invoking an agent or writing a file, and the CLI shipped in 0069 is therefore a working clap front door over a no-op backend.
>
> This rewritten 0070 carries the entire "structural skeleton → working CLI" gap. After this work item the CLI is fully functional and matches old-amux behavior; it is the validation surface that proves the engines and command bodies are real. The remaining frontends (TUI, headless) follow in 0071/0072 once the engines are real and easy to build on top of.
>
> The remaining work is partitioned across the work items:
>
> - `0070-…` (this work item) — Real container execution + real engine phase bodies + real per-command Layer 2 bodies for **every** command (`init`, `ready`, `chat`, `specs new`, `specs amend`, `new spec/workflow/skill`, `status`, `config show/get/set`, `claws init/ready/chat`, `implement`, `exec prompt`, `exec workflow`, `download`, the interactive half of `auth`) + real `AgentEngine::ensure_available` (download + build) + real `OverlayEngine` Claude transformations + full CLI completion (every `*Outcome` and `*Error` variant rendered, every flag honored end-to-end including `--json`, every Q&A frontend method TTY-aware).
> - `0071-grand-architecture-tui-frontend.md` — TUI frontend on top of the now-real engines and commands (no business logic; pure presentation per the four tenets).
> - `0072-grand-architecture-headless-frontend.md` — Headless frontend + the still-stub Layer 2 command bodies that exist only to talk to the headless server (`headless start/kill/logs/status`, `remote run/session start/session kill`) + the headless-side persistence half of `auth` + `AuthEngine::ensure_self_signed_tls` real wiring.
> - `0073-grand-architecture-finalize.md` — Cross-frontend parity validation, `tests/` rebuild, docs/aspec refresh.

## Required reading before starting

This work item is the fifth of eight implementing the grand architecture refactor described in `aspec/architecture/2026-grand-architecture.md`. The implementing agent **MUST** read that document, the four prior work items (`0066-…` through `0069-…`), and the current state of `src/data/`, `src/engine/`, `src/command/`, and `src/frontend/cli/` end-to-end before writing any code.

The four tenets, again:

1. **Frontends contain NO business logic.** Any `if`, `match`, or computed-default behavior whose output depends on the *meaning* of a command, flag, or response is wrong and lives in Layer 2.
2. **Lower layers never call upward.** Layer 1 cannot call into Layer 2; Layer 2 cannot call into Layer 3. When a layer needs work done by a higher layer (e.g. an engine needs the user's confirmation), it accepts a frontend trait at construction time and calls methods on that trait.
3. **Typed objects over `pub fn`.** Every stateful concern is a struct with methods, not a free function with eight parameters.
4. **When uncertain, ASK THE DEVELOPER.**

The companion work items are:

- `0066-grand-architecture-foundation-and-layer-0-data.md` (merged)
- `0067-grand-architecture-layer-1-engines.md` (merged — but note: shipped only the structural traits/types; bodies in `src/engine/container/{docker,apple}.rs`, `src/engine/{init,ready,claws}/mod.rs`, `src/engine/agent/{download,mod}.rs` are no-op placeholders that this work item replaces with real implementations)
- `0068-grand-architecture-layer-2-command-and-dispatch.md` (merged — but note: shipped only the typed command structs and dispatch wiring; the `run_with_frontend` bodies for `chat`, `specs`, `new`, `download`, `auth`, `headless`, and `remote` are pass-through stubs that this work item replaces, except `headless`/`remote` which stay stubbed until 0072)
- `0069-grand-architecture-layer-3-frontends-and-binary.md` (merged — CLI shell exists; this work item completes the CLI rendering / flag-handling / TTY-detection paths so it is fully functional once the engines underneath go real)
- `0071-grand-architecture-tui-frontend.md`
- `0072-grand-architecture-headless-frontend.md`
- `0073-grand-architecture-finalize.md`

## Summary

Three deliverables, in dependency order:

1. **Real container execution.** Replace the no-op `DockerContainerInstance::run_with_frontend` and `AppleContainerInstance::run_with_frontend` with real subprocess wiring that spawns the container, allocates a PTY when interactive, streams stdin/stdout/stderr through the supplied `ContainerFrontend`, propagates resize, captures exit code, and supports cancel. Every higher-level engine and command underpins on this. (§1a)

2. **Complete every Layer 1 / Layer 2 stub** so every CLI command works end-to-end: real engine phase bodies (`InitEngine`, `ReadyEngine`, `ClawsEngine`), real per-command bodies (`chat`, `specs new`, `specs amend`, `new spec/workflow/skill`, `status`, `config show/get/set`, `download`, the interactive half of `auth`, `chat`, `exec prompt`), real `AgentEngine::ensure_available` build/download path, real `OverlayEngine` Claude transformations, real network helper (aspec download). The plumbing for `implement` and `exec workflow` already exists in 0068; once §1a is real these commands start working. (§1b–§1q)

3. **Full CLI completion.** Every `*Outcome` variant gets a final `render_outcome_for_cli` branch (no `todo!()`s); every `*Error` variant gets a `render_error_for_cli` branch with the right exit code; every command's flag set is fully threaded through (`ready --json` actually emits JSON; `status --watch` actually loops; every `Q&A` frontend method falls back to safe defaults when stdin is not a TTY). (§2)

After this work item, every CLI command in `aspec/uxui/cli.md` MUST behave identically (or better, with developer sign-off) to the legacy CLI. The TUI and headless server stay stubbed for one more WI each; that's `0071-…` and `0072-…`.

## User Stories

### User Story 1:
As a: existing amux user

I want to:
run every CLI command from `aspec/uxui/cli.md` against the new binary

So I can:
get the same behavior I got from the pre-refactor binary, with no commands silently no-oping or returning before doing the work I asked for.

### User Story 2:
As a: existing amux user piping `amux ready --json` into a script

I want to:
machine-readable JSON output that matches the legacy schema

So I can:
keep my CI / scripting workflows working without rewriting parsers.

### User Story 3:
As a: maintainer

I want to:
each command body live entirely in Layer 2 (or call into Layer 1), with no business logic in `src/frontend/cli/`

So I can:
add a new frontend (TUI in 0071, headless in 0072, future desktop / extension frontends) without re-porting per-command business logic, and trust that command behavior stays consistent across CLI / TUI / headless by construction.

## Implementation Details

### 1. Layer 1/2 business-logic completion — exhaustive stub list

Every entry below MUST be replaced with a real implementation that matches old-amux behavior. The order roughly tracks dependencies — container execution underpins everything else, so do it first.

#### 1a. Container execution — real Docker + Apple subprocess wiring

Files: `src/engine/container/docker.rs`, `src/engine/container/apple.rs`, `src/engine/container/instance.rs`, `src/engine/container/backend.rs`.

Today `DockerContainerInstance::run_with_frontend` returns `ContainerExecution::finished(handle, info)` with `exit_code = 0` and never spawns a subprocess. `AppleContainerInstance::run_with_frontend` does the same. `DockerBackend::list_running` returns `Vec::new()`; `stats` and `stop` return `EngineError::NotImplemented`. Every command that runs an agent (chat, exec, implement, ready audit, init audit, claws audit, claws controller, specs amend) silently no-ops as a result.

Replace with full subprocess wiring derived from `oldsrc/runtime/docker.rs` and `oldsrc/runtime/apple.rs`:

- **`DockerContainerInstance::run_with_frontend`** — translate `ResolvedContainerOptions` into a `docker run` argv, spawn the subprocess, allocate a PTY when `Interactive(true)` and the frontend supports it, wire stdin/stdout/stderr through the supplied `Box<dyn ContainerFrontend>`, propagate PTY resize via `ContainerFrontend::resize_pty`, capture exit info, and return a `ContainerExecution::new(handle, Box::new(DockerExecution { … }))`. The `ExecutionBackend` impl on `DockerExecution` MUST own the spawned `tokio::process::Child` (or `Command` + `JoinHandle` pair) and implement `wait_blocking` against it. `cancel` MUST send SIGTERM (then SIGKILL after a short grace period) and remove the container if it persists.
- **Seeded prompt** — when `ContainerOption::SeededPrompt(s)` is present, the container's stdin gets `s` written ahead of any user stdin (matches old-amux `seed_prompt_file` flow); the wired-up `ContainerFrontend::read_stdin` then takes over.
- **Overlays** — every `ContainerOption::Overlay(spec)` becomes a `-v <host_path>:<container_path>:<mode>` flag. `OverlayPermission::ReadOnly` → `:ro`; `OverlayPermission::ReadWrite` → no suffix.
- **Env passthrough** — `ContainerOption::EnvPassthrough(EnvVar(name))` becomes `-e NAME=value` when `name` is set in the host environment; missing env vars are silently skipped (matches old-amux).
- **Allow Docker** — `ContainerOption::AllowDocker(true)` mounts `/var/run/docker.sock` and adds the host docker group GID so the container's user can talk to the daemon. Same socket logic as `oldsrc/runtime/docker.rs`.
- **Mount SSH** — `ContainerOption::MountSsh { source }` mounts `source` read-only at `/root/.ssh` (or the agent's container-home as configured).
- **Working dir** — `ContainerOption::WorkingDir(p)` becomes `-w <p>`.
- **Container name** — `ContainerOption::Name(name)` becomes `--name <name>`. When omitted, the random name from `naming::generate_container_name()` (already present) is used.
- **Image tag** — `ContainerOption::Image(ImageRef(tag))` is the final positional arg.
- **Entrypoint** — `ContainerOption::Entrypoint(e)` becomes `--entrypoint <e>` (or appended args, depending on agent matrix). Match `agent_matrix::entrypoint_for` semantics.
- **Yolo / Auto / Plan** — these are *agent argv* knobs, not Docker flags. They are encoded into the entrypoint argv assembled by `AgentEngine::build_options` (already partially present); `DockerContainerInstance` just forwards the argv verbatim. Confirm the existing `ContainerOption` variants are sufficient or extend per `oldsrc/runtime/docker.rs::run_*` callers.
- **Container labels** — apply the legacy `amux=true` and `amux.session=<id>` labels so `list_running` can filter (mirror `oldsrc/runtime/docker.rs::AMUX_LABEL`).
- **`DockerBackend::list_running`** — shell `docker ps --filter label=amux=true --format '{{json .}}'`, parse one `ContainerHandle` per row. Use the shared image-tag/started-at format from `oldsrc/runtime/docker.rs::list_amux_containers`.
- **`DockerBackend::stats`** — shell `docker stats --no-stream --format '{{json .}}' <name>` (one row), parse into `ContainerStats { name, cpu_percent, memory_mb }`. Match `oldsrc/runtime/docker.rs::container_stats`.
- **`DockerBackend::stop`** — shell `docker stop <name>` then `docker rm <name>`. Best-effort; missing container is not an error.
- **Apple Containers** — mirror the same surface against the Apple Containers CLI (`container run`, `container ps`, `container stats`, `container stop`). Keep behind `cfg(target_os = "macos")` where the Apple CLI is unavailable; the existing `BackendUnsupportedOnPlatform` error already gates this.
- **Image existence check** — `crate::engine::agent::image_exists_locally` already shells `docker image inspect`; keep the existing implementation but extend with an Apple variant gated by `cfg`.

`ContainerFrontend::read_stdin` is finally usable here; the existing `Err(EngineError::NotImplemented(...))` stubs in `src/command/commands/exec_workflow.rs:180-185` and `src/command/commands/implement.rs:173-175` MUST be replaced with the real implementation that reads from the underlying frontend (CLI: `tokio::io::stdin()` with a small async buffer; TUI: deferred to 0071; headless: deferred to 0072).

#### 1b. Real `ReadyEngine` phase bodies

File: `src/engine/ready/mod.rs`. Every phase body currently sets `summary.* = StepStatus::Done` and advances. Replace with real work derived from `oldsrc/commands/ready.rs` + `oldsrc/commands/ready_flow.rs`:

- `ReadyPhase::CreatingDockerfile` — write the embedded project base template (`oldsrc/commands/init_flow::project_dockerfile_embedded`) to `<git_root>/Dockerfile.dev`. Set `summary.dockerfile = StepStatus::Done`. Move the template into `src/data/templates/dockerfile_dev.template` (or a `data::templates::project_dockerfile_dev() -> &'static str` Layer-0 helper).
- `ReadyPhase::AwaitingLegacyMigrationDecision` — only enter when `<git_root>/Dockerfile.dev` exists AND `<git_root>/.amux/Dockerfile.<agent>` does NOT (the bug fix landed alongside this work item already gates this). Otherwise jump straight to `ReadyPhase::BuildingBaseImage` with `summary.legacy_migration = StepStatus::Skipped`.
- `ReadyPhase::MigratingLegacyLayout` — execute `oldsrc/commands/ready::perform_legacy_migration` semantics: copy `Dockerfile.dev` to `Dockerfile.dev.bak`, overwrite `Dockerfile.dev` with the embedded project base. Emit two `UserMessage::info` messages identical to legacy ("Backed up existing Dockerfile.dev to …", "Dockerfile.dev recreated with project base template."). Set `summary.legacy_migration = StepStatus::Done`.
- `ReadyPhase::BuildingBaseImage` — call the real `ContainerRuntime::build` + `run_with_frontend` to execute `docker build -t <project_image_tag(git_root)> -f Dockerfile.dev <git_root>`. Honor `options.no_cache` → `--no-cache` and `options.build` (force flag in old-amux means build even when cache is fresh). Stream output through the supplied `ContainerFrontend`. Set `summary.base_image = StepStatus::Done` on success; `Failed(err)` on non-zero exit.
- `ReadyPhase::BuildingAgentImage` — analogous: `docker build -t <agent_image_tag(git_root, agent)> -f .amux/Dockerfile.<agent> <git_root>`. Per-agent Dockerfile MUST exist on disk; if absent and the agent is known, call `AgentEngine::ensure_available` (see §1e) to download it first.
- `ReadyPhase::CheckingLocalAgent` — `image_exists_locally(<agent_image_tag>)`. Set `summary.local_agent = StepStatus::Done` when true, `Failed(...)` when false.
- `ReadyPhase::RunningAudit` — when the user accepts via `frontend.ask_run_audit_on_template()`, build `AgentRunOptions` with the audit prompt seeded (`READY_AUDIT_PROMPT` from `oldsrc/commands/ready.rs:14-19` — move to `src/command/commands/audit_prompts.rs::ready_audit_prompt() -> &'static str`), call `AgentEngine::build_options`, then `ContainerRuntime::build` + `run_with_frontend` with the supplied `ContainerFrontend`. Wait for exit; nonzero exit → `summary.audit = Failed`, the rebuild step is skipped. Zero exit → `summary.audit = Done`.
- `ReadyPhase::RebuildingAfterAudit` — when audit ran successfully AND the audit modified `Dockerfile.dev` (compare its hash against the pre-audit hash captured in `BuildingBaseImage`), rebuild the base + agent images. Otherwise no-op. Set `summary.image_build` accordingly.
- The `_suppress` helper at the bottom of the file goes away — `git_engine` and `overlay_engine` now have real callers.

#### 1c. Real `InitEngine` phase bodies

File: `src/engine/init/mod.rs`. Today every phase sets `summary.* = StepStatus::Done` and advances. Replace per `oldsrc/commands/init.rs` + `oldsrc/commands/init_flow.rs`:

- `InitPhase::CreatingAspecFolder` — when `options.run_aspec_setup` is false (i.e. `--aspec` flag absent), copy the bundled aspec template tree into `<git_root>/aspec/`. When true, attempt `download_aspec_tarball` (see §1g for the real network helper); on failure emit `UserMessage::warning("aspec download failed — using bundled template")` and fall back to bundled. Bundled template lives at `src/data/templates/aspec/` and is included via `include_dir!` (or equivalent). When `aspec/` already exists and the user declined replacement (`ask_replace_aspec` returned false), skip — preserve existing behavior.
- `InitPhase::SettingUpDockerfile` — write `<git_root>/Dockerfile.dev` from the embedded project base template (same template as `ReadyPhase::CreatingDockerfile`). Skip if it already exists. Set `summary.dockerfile = StepStatus::Done`.
- `InitPhase::WritingConfig` — write `<git_root>/aspec/.amux.json` using the `RepoConfig` Layer-0 type. The default config matches `oldsrc/config::default_repo_config(agent)` — preserve the chosen agent, default model, and any other defaults the legacy code set. Idempotent: when the file already exists, leave it alone and emit `UserMessage::info("aspec/.amux.json already present — preserving existing config")`.
- `InitPhase::BuildingImage` — execute the project base build (same as `ReadyPhase::BuildingBaseImage`). Wire through the supplied `ContainerFrontend` to stream build output. Failure → `summary.image_build = Failed`.
- `InitPhase::RunningAudit` — same as `ReadyPhase::RunningAudit` but with the init audit prompt (`oldsrc/commands/init_flow::INIT_AUDIT_PROMPT` → `src/command/commands/audit_prompts::init_audit_prompt()`).
- `InitPhase::WritingWorkItemsConfig` — when `ask_work_items_setup` returned `Some(WorkItemsConfig)`, persist the config to `<git_root>/aspec/.amux.json` under the `work_items` key. Add a `RepoConfig::set_work_items_config` Layer-0 helper.

#### 1d. Real `ClawsEngine` phase bodies

File: `src/engine/claws/mod.rs`. Today every claws phase is a stub. The legacy implementation in `oldsrc/commands/claws.rs` is the reference. The three modes (`ClawsMode::Init`, `Ready`, `Chat`) take different paths:

- **`ClawsMode::Init`** — full lifecycle: `Preflight` → `CloningRepo` (fork + clone nanoclaw to `<HOME>/.amux/claws/<repo-hash>`; or use existing if `ask_replace_existing_clone` returned false) → `CheckingPermissions` (verify the host user can write to the clone dir; assemble sudo prompts where needed; show the legacy `SudoConfirm` dialog through `frontend.confirm_sudo_actions(...)`) → `BuildingImage` (real `docker build` for the nanoclaw image) → `RunningAudit` (optional, via `ask_run_audit`) → `Configuring` (write `<HOME>/.amux/claws/<repo-hash>/config.json`) → `LaunchingController` (real `docker run -d --label amux-claws=true …` for the controller container) → `Complete`.
- **`ClawsMode::Ready`** — short path: `Preflight` checks whether the controller container is running (`docker ps --filter label=amux-claws=true …`); if running, jump to `Complete` with `summary.controller = Done`. If stopped, prompt `frontend.confirm_restart_stopped` → `LaunchingController` → `Complete`. If absent, prompt `frontend.confirm_offer_init` → if accepted, transition to `ClawsMode::Init` flow; else `summary.controller = Skipped`, `Complete`.
- **`ClawsMode::Chat`** — short path: `Preflight` requires the controller running, else fail with a structured error pointing at `amux claws ready`. When running, transition to `AttachingChat` (new phase) which uses `ContainerRuntime::build` + `run_with_frontend` to `docker exec -it <controller_name> /amux/claws-chat` (or whatever the legacy entrypoint is — confirm against `oldsrc/commands/claws.rs::launch_chat_session`) with the supplied `ContainerFrontend` bound to its PTY.

All `frontend.container_frontend()` calls in the current scaffold get replaced with real `ContainerRuntime::build(...)` + `instance.run_with_frontend(frontend.container_frontend())` round trips that wait for exit. The `ClawsFailure` enum gets new variants for cloning errors, sudo errors, image-build errors, and chat-attach errors.

The Layer-0 helpers (clone-path resolution, controller-name resolution, image-tag resolution) live in `src/data/claws_paths.rs` (new) — pull from `oldsrc/commands/claws.rs::*_path` helpers verbatim.

#### 1e. Real `AgentEngine::ensure_available` body + agent download

Files: `src/engine/agent/mod.rs`, `src/engine/agent/download.rs`.

`download_agent_dockerfile` returns `EngineError::NotImplemented`. Replace with a real HTTP fetch from the canonical per-agent Dockerfile URL (matrix in `oldsrc/commands/download.rs::AGENT_DOCKERFILE_URLS` → move to `src/engine/agent/download.rs::DOCKERFILE_URL_FOR_AGENT`). Use `reqwest` (already a transitive dependency via TLS); fall back to `ureq` if simpler. Write the response body to `<git_root>/.amux/Dockerfile.<agent>` atomically (write to `<path>.tmp`, rename). On download failure, return `EngineError::AgentDockerfileDownloadFailed { agent, source }` and let the caller surface it.

`AgentEngine::ensure_available`'s "build image" branch currently does `let _container = frontend.container_frontend(); … StepStatus::Done` without invoking the runtime. Replace with: build a `ResolvedContainerOptions` for `docker build -t <agent_image_tag> -f .amux/Dockerfile.<agent> <git_root>`, call `self.container_runtime.build(...)` then `instance.run_with_frontend(frontend.container_frontend())`, await, propagate exit info. On nonzero exit, return `EngineError::AgentImageBuildFailed { agent, exit_code }`.

#### 1f. Real `OverlayEngine` Claude transformations

File: `src/engine/overlay/mod.rs`. Per `0067-…` §9a parity addenda, `agent_settings_overlays(claude)` MUST:

1. Strip `oauthAccount` from `~/.claude.json` before mounting (write the sanitized copy to a temp dir and mount that, not the original).
2. Apply the legacy denylist filter (a fixed list of MCP server keys + sensitive settings — copy verbatim from `oldsrc/passthrough/claude.rs::CLAUDE_SETTINGS_DENYLIST`).
3. When `OverlayRequest::yolo == true`, inject the yolo-mode settings overlay (`{ "permissionMode": "bypassPermissions", … }`) into the mounted settings dir.
4. Suppress the LSP recommendation banner (set the appropriate flag in the sanitized settings file).
5. Detect a non-root `USER` directive in `Dockerfile.<claude>` and rewrite `container_path` from `/root/.claude*` to `/home/<user>/.claude*` (matches `oldsrc/passthrough/claude.rs::adjust_for_user_directive`).

The current implementation only maps host paths to container paths verbatim. Add a private helper `sanitize_claude_settings(input_dir: &Path, output_dir: &Path, yolo: bool) -> Result<(), EngineError>` that produces the sanitized copy in a temp directory owned by `OverlayEngine` (lifetime tied to the overlay engine instance — temp dir cleanup on Drop). Tests live colocated.

For non-Claude agents, the existing per-agent branches (`codex`, `gemini`, `opencode`, `crush`) already work — leave them alone unless 0067 §9a flagged a gap.

#### 1g. Real network helpers — aspec download

Today there is no Layer-0 network module. Add `src/data/network/aspec_tarball.rs`:

```rust
pub async fn download_aspec_tarball() -> Result<Vec<u8>, NetworkError>;
pub async fn extract_aspec_tarball(bytes: &[u8], dest: &Path) -> Result<(), NetworkError>;
```

URL constant: ports verbatim from `oldsrc/commands/init_flow::ASPEC_TARBALL_URL`. Return `NetworkError::DownloadFailed` / `NetworkError::ExtractFailed` on failure. Used by `InitEngine::CreatingAspecFolder` when `--aspec` is set; falls back silently to bundled template.

#### 1h. Real `ChatCommand::run_with_frontend` body

File: `src/command/commands/chat.rs`. Today the body is `let _ = self.engines; frontend.replay_queued(); Ok(ChatOutcome { … })`.

Replace with: resolve the agent (flag → repo config → fallback), call `self.engines.agent_engine.ensure_available` through the `AgentSetupFrontend` portion of the per-command frontend (existing supertrait already has it), call `self.engines.auth_engine.resolve_agent_auth` through `AgentAuthFrontend`, build `AgentRunOptions` from `flags`, call `agent_engine.build_options(session, &agent, &run_opts)`, build the `ContainerInstance` via `self.engines.runtime.build(options)`, hand the supplied `ContainerFrontend` to `instance.run_with_frontend(...)`, wait for exit, return `ChatOutcome { agent: Some(agent.as_str().to_string()), exit_code: Some(exit.exit_code) }`.

The `frontend.set_pty_active(true)` / `set_pty_active(false)` lifecycle around `instance.run_with_frontend` is already established for `exec_workflow`; mirror it here. Add `set_pty_active` to the `ChatCommandFrontend` supertrait to match.

#### 1i. Real `ExecPromptCommand::run_with_frontend` body

File: `src/command/commands/exec_prompt.rs`. Same shape as `ChatCommand` but seeds the prompt via `AgentRunOptions::initial_prompt = Some(self.flags.prompt.clone())` and forces `non_interactive: true`. The container runs to completion non-interactively; output streams through the supplied `ContainerFrontend`.

#### 1j. Real `SpecsNew` + `SpecsAmend` command bodies

File: `src/command/commands/specs.rs`. Today both subcommands return immediately.

- **`SpecsNew`** — port `oldsrc/commands/new.rs::run_new_spec`:
  - Resolve `aspec/work-items/0000-template.md` (or the configured `work_items.template`) → read into a `String`.
  - Determine the next work-item number by scanning `aspec/work-items/` for the highest `NNNN-*.md` and adding 1 (or `aspec/work-items/<configured_dir>/` if `RepoConfig::work_items.dir` is set).
  - Q&A via `SpecsCommandFrontend` (extend the trait): `ask_kind() -> SpecKind` (`Issue | Refactor | Feature | Spike` etc — the legacy enum), `ask_title() -> String`, `ask_summary() -> String` (multiline). When `--interview`, also `ask_interview_summary() -> String`. The CLI implementation prompts on stdin (TTY-gated per §2); the TUI implementation drives the `NewSpecDialog` tree (deferred to 0071).
  - Substitute placeholders in the template (`{{kind}}`, `{{title}}`, `{{summary}}`, `{{number}}`).
  - Write to `aspec/work-items/<NNNN>-<slug>.md`.
  - When `--interview` is set, after writing the bare file, hand it to an agent for completion: build an `AgentRunOptions` with `initial_prompt = render_interview_prompt(<path>, <summary>)`, run via `AgentEngine` + `ContainerRuntime`. The agent rewrites the file in-place inside the container; the container has `<git_root>` mounted RW.
  - Return `SpecsNewOutcome { interview, created_path: Some(<path>) }`.
- **`SpecsAmend`** — port `oldsrc/commands/spec.rs::run_amend`:
  - Locate the work-item file by number (search `aspec/work-items/<NNNN>-*.md`). If missing → `CommandError::WorkItemNotFound { number }`.
  - Build an `AgentRunOptions` with `initial_prompt = render_amend_prompt(<path>)` (template moved to `src/command/commands/implement_prompts::amend_prompt`).
  - Run via `AgentEngine` + `ContainerRuntime` with the supplied `ContainerFrontend`. Honor `--non-interactive` and `--allow-docker`.

#### 1k. Real `New` command body — `new spec`, `new workflow`, `new skill`

File: `src/command/commands/new.rs`. Today every variant returns `path: None`.

- **`NewSubcommand::Spec`** — alias for `SpecsCommand::new(SpecsSubcommand::New(...))` per the catalogue's path-alias rule. Either delegate (preferred) or duplicate the body. Confirm with the developer which the catalogue dispatch already does — if the alias is resolved at the dispatch layer, this branch should never be reached and can be `unreachable!()` with a comment.
- **`NewSubcommand::Workflow`** — port `oldsrc/commands/new_workflow.rs::run_new_workflow`:
  - Q&A via `NewCommandFrontend` (extend trait): `ask_workflow_name`, `ask_workflow_step_count`, per-step `ask_step_name`/`ask_step_agent`/`ask_step_prompt_template`. When `--interview`, instead `ask_interview_summary` → run agent.
  - Render to TOML / YAML / Markdown depending on `--format`.
  - Resolve write path: when `--global`, `<HOME>/.amux/workflows/<name>.<ext>`; else `aspec/workflows/<name>.<ext>` under git root.
  - Return `NewWorkflowOutcome { interview, global, format, path: Some(<path>) }`.
- **`NewSubcommand::Skill`** — port `oldsrc/commands/new_skill.rs::run_new_skill`:
  - Q&A via `NewCommandFrontend`: `ask_skill_name`, `ask_skill_description`, `ask_skill_body` (multiline). When `--interview`, `ask_interview_summary` → run agent.
  - Resolve write path: when `--global`, `<HOME>/.amux/skills/<name>/SKILL.md`; else `aspec/skills/<name>/SKILL.md` under git root. Create the directory if missing.
  - Return `NewSkillOutcome { interview, global, path: Some(<path>) }`.

The `NewCommandFrontend` trait grows new methods for each Q&A above. The CLI impl prompts on stdin (TTY-gated per §2); the TUI / headless impls are deferred (0071/0072).

#### 1l. Real `StatusCommand` body

File: `src/command/commands/status.rs`. The body already calls `self.engines.runtime.list_running` (good) but `list_running` returns `Vec::new()` until §1a is done. After §1a, status begins working. Additionally:

- Append a random tip from `TIPS: &[&str]` (port verbatim from `oldsrc/commands/status.rs::TIPS`). Selection index: `(unix_seconds % TIPS.len())`. Move TIPS to `src/command/commands/status_tips.rs` per `0069-…` §7r.
- When `flags.watch == true`, the command loops with a 3-second sleep between invocations and emits `CLEAR_MARKER` (ANSI `\x1b[2J\x1b[H`) before each repaint via `frontend.write_clear_marker()`. The CLI sink writes the marker; the TUI sink swallows it (deferred to 0071). Exit when `frontend.should_continue_watching()` returns false.
- Container stats — when the runtime exposes `stats()` (newly real per §1a), enrich each row with CPU/memory.

#### 1m. Real `ConfigCommand` body

File: `src/command/commands/config.rs`. Today the show variant already reads config; the get/set variants need real plumbing:

- `ConfigSubcommand::Show` — produce a `ConfigShowOutcome { fields: Vec<ConfigFieldRow> }` where each row carries `field_name`, `global_value`, `repo_value`, `effective_value`, `kind` (string/bool/number/enum), `read_only` (true for `auto_agent_auth_accepted` per `0069-…` §7i). Enumerate by walking the `EffectiveConfig` / `RepoConfig` / `GlobalConfig` schemas via a Layer-0 reflection helper (or hand-coded list in `src/data/config/field_descriptors.rs`).
- `ConfigSubcommand::Get` — return one `ConfigFieldRow` for the requested field; error with `CommandError::UnknownConfigField { name, suggestions: Vec<&'static str> }` (Levenshtein-suggest from the descriptor table).
- `ConfigSubcommand::Set` — validate the value against the field's kind (parse u16, parse bool, parse enum), then persist. `--global` writes to `<HOME>/.amux/config.json`; default writes to `<git_root>/aspec/.amux.json`. Use the `RepoConfig::write` / `GlobalConfig::write` helpers; both already exist in Layer 0.

#### 1n. Real `DownloadCommand` body

File: `src/command/commands/download.rs`. Today `let _ = self.engines; Ok(...)`. Port `oldsrc/commands/download.rs`: download the requested asset (`AgentDockerfile { agent }` → call `agent::download::download_agent_dockerfile`; `AspecTarball` → call `data::network::download_aspec_tarball`; etc.). Return `DownloadOutcome { asset, bytes_written, dest_path }`.

#### 1o. `AuthCommand` body — interactive consent (TLS half deferred)

File: `src/command/commands/auth.rs`. Today the body is `let _ = self.engines; AuthOutcome { accepted: self.flags.accept }`.

For 0070, implement the consent prompt path so `amux auth` prompts on stdin (or in a TUI/headless dialog when those land) for `[y]/[n]/[o]` matching the legacy `AgentAuthConsent` dialog (`0069-…` §7h). Persist the choice via `GlobalConfig::set_auto_agent_auth_accepted(...)` (Layer 0 helper — add if missing). Return `AuthOutcome { accepted, persisted }`.

The headless-side persistence helpers and `AuthEngine::ensure_self_signed_tls` are deferred to 0072 since they are only exercised by the headless server.

#### 1p. `ImplementCommand` and `ExecWorkflowCommand` — degraded → working

Files: `src/command/commands/implement.rs`, `src/command/commands/exec_workflow.rs`. The Layer 2 plumbing here is already real (it loads the workflow, prepares the worktree, builds the `ContainerExecutionFactory`, runs `WorkflowEngine`). It silently no-ops today only because the underlying `ContainerInstance::run_with_frontend` returns a pre-finished execution. After §1a these commands start working.

Three small fixes needed beyond §1a:

- The `ContainerFrontendProxy::read_stdin` stub (`src/command/commands/exec_workflow.rs:180-185` and the parallel in `implement.rs:173-175`) — replace with a real implementation that delegates to the underlying `ExecWorkflowCommandFrontend`.
- `inject_prompt` returns `Ok(None)` — when the agent matrix supports prompt injection (some agents allow stdin re-injection mid-session), wire the real injection. Per `oldsrc/runtime/docker.rs::inject_prompt`. For agents without injection support, `Ok(None)` stays correct.
- The `WorkflowSummary { steps_completed: 0, steps_failed: if had_error { 1 } else { 0 } }` is a placeholder — replace with the actual completed/failed counts pulled from `WorkflowEngine::state()` (which already exists).

#### 1q. Layer 1/2 unit tests

Each Layer 1/2 implementation above gets colocated `#[cfg(test)] mod tests`:

- Phase-by-phase tests against fakes (no real Docker, no real network) covering each `*Phase` variant's transition behavior.
- Frontend-trait Q&A tests that drive each interactive method with `Yes`/`No`/`Abort` paths.
- Argv-assembly tests for `DockerContainerInstance::run_with_frontend` against a `MockSpawner` that records the constructed argv (no real subprocess).
- Image-tag stability tests (already in 0067 §9a) extended to cover the new build paths.

The full real-Docker / real-network end-to-end tests are 0073.

### 2. Full CLI completion

Files: `src/frontend/cli/mod.rs`, `src/frontend/cli/render.rs`, `src/frontend/cli/output.rs`, `src/frontend/cli/per_command/*.rs`, `src/frontend/cli/per_command/render.rs`.

The CLI shell built in 0069 dispatches every command path to Layer 2 and renders outcomes. After §1's Layer 1/2 work lands, every command path produces a real outcome — the CLI's job is to surface those outcomes correctly. The remaining gaps:

#### 2a. Outcome rendering completeness

`render_outcome_for_cli` MUST have a branch for every `*Outcome` variant in `src/command/commands/*.rs`. Audit:

- `ChatOutcome` — render exit code + agent name on a single line ("amux: chat (claude) exited 0").
- `ExecPromptOutcome` — same shape.
- `ExecWorkflowOutcome` — render workflow path + exit code + worktree-used flag.
- `ImplementOutcome` — render work item, agent, workflow used, exit code.
- `InitOutcome` — render the summary box (already implemented for the Q&A flow; confirm the final outcome rendering produces no duplicate output).
- `ReadyOutcome` — same; ALSO honor `--json`: when `flags.json == true`, suppress the human-readable summary box and emit the documented JSON schema (`{"runtime": "...", "base_image": "Done", "agent_image": "Done", ...}`) on stdout. Schema MUST match `oldsrc/commands/ready.rs` JSON output exactly.
- `ClawsOutcome` — render mode + summary rows.
- `StatusOutcome` — render the legacy ASCII table (`oldsrc/commands/status.rs::render_table`); each row shows id/name/image/started_at/optional tab annotation. Append the random tip line.
- `ConfigShowOutcome` — render a 4-column table (field / global / repo / effective). Read-only fields rendered with `(read-only)` suffix.
- `ConfigGetOutcome` — render a single block of `field=...; global=...; repo=...; effective=...`.
- `ConfigSetOutcome` — render `set <field> = <value> in <scope>`.
- `SpecsOutcome::New { created_path }` — render `created <path>`.
- `SpecsOutcome::Amend` — render `amended <path>` (or just exit code when --non-interactive).
- `NewOutcome::{Spec, Workflow, Skill}` — render `created <path>` per variant.
- `DownloadOutcome` — render `downloaded <asset> -> <path> (<bytes_written> bytes)`.
- `AuthOutcome` — render `auth: <accepted? "accepted" : "declined">; persisted=<bool>`.
- `HeadlessOutcome` and `RemoteOutcome` variants — placeholder rendering OK in 0070; real rendering ships in 0072 alongside the real command bodies.

When a new outcome variant is added later, the build MUST fail until the renderer covers it. Use an exhaustive `match` (no `_ =>` arm).

#### 2b. Error rendering completeness

`render_error_for_cli` MUST have a branch for every `CommandError` variant (and indirectly every `EngineError` and `DataError` variant the command layer surfaces). The branches map each variant to:

1. A user-friendly error message on stderr (no Rust types, no debug formatting).
2. A specific exit code per the table in `aspec/uxui/cli.md` (e.g. `2` for invalid usage, `3` for missing-Docker, `4` for missing-work-item, etc.).

Each branch SHOULD include a "next step" hint where actionable — for example, `EngineError::ContainerRuntimeUnavailable` → `"amux requires Docker. Install Docker Desktop / docker-engine and retry."`; `CommandError::WorkItemNotFound { number }` → `"work item {number} not found. Run \`amux specs new\` to create one, or \`amux status\` to list current items."`.

Use exhaustive matches; no fallback `_ =>` arm. New error variants force the build to fail until handled.

#### 2c. Flag plumbing completeness

For every flag in `CommandCatalogue`, audit the per-command CLI frontend impl to confirm the flag value is read and threaded through to the command struct (via `*CommandFlags`). Specifically:

- `ready --refresh / --build / --no-cache / --non-interactive / --allow-docker / --json` — every one drives a path in 0069's `ReadyCommandFlags` *and* must be honored by `ReadyEngine` (most are; `--json` is the new addition above).
- `chat / exec prompt / exec workflow / implement` — `--non-interactive / --plan / --allow-docker / --mount-ssh / --yolo / --auto / --agent / --model / --overlay` plus per-command flags. Confirm each is read into the appropriate `*CommandFlags` field. The CLI's `--overlay` is repeatable; ensure it's collected as `Vec<String>` and parsed (`HOST:CONTAINER:MODE`) per `oldsrc/cli.rs::parse_overlay_spec` (move the parser into `src/data/overlay_spec.rs`).
- `status --watch` — ensure the CLI's `should_continue_watching` returns true on each tick (probably needs a `Ctrl+C` handler that toggles the flag).
- `init --agent / --aspec` — already works; add a confirmation test.
- `specs amend / new <kind>` — every flag wired.
- `headless start --port / --workdirs / --background / --refresh-key / --dangerously-skip-auth` — flags read into `HeadlessCommandFlags`. The command body itself stays stubbed until 0072; only the flag-reading half lands in 0070.
- `remote run / session start / session kill --remote-addr / --session / --follow / --api-key` — same: flags read; bodies stubbed until 0072.

#### 2d. TTY-aware Q&A defaults

Every CLI per-command frontend that reads from stdin MUST gate the read on `stdin_is_tty()`:

- `helpers::yes_no(prompt, default) -> bool` — when `!stdin_is_tty()`, return `default` immediately without reading. Confirm against the current implementation; fix any regressed.
- `init.rs::ask_work_items_setup` — when `!stdin_is_tty()`, return `Ok(None)`.
- New CLI per-command files for `specs new`, `new workflow`, `new skill`, `auth`, `claws init` — every Q&A method MUST follow the same pattern.
- `--non-interactive` is implicit when stdin is not a TTY (no separate flag check needed at the CLI layer; the engines already accept the safe defaults).

When a Q&A method has no safe default (e.g. `ask_workflow_name` which has no fallback), it MUST surface a structured `CommandError::InteractiveInputUnavailable { prompt }` rather than block. The renderer translates this to `amux: stdin is not a TTY; provide --workflow-name on the command line or run from an interactive shell`.

#### 2e. Color, hyperlinks, TTY width

Move `oldsrc/commands/output.rs` color/no-color/hyperlink helpers into `src/frontend/cli/output.rs`. Detect:

- `NO_COLOR` env var → disable color.
- `--color=always|never|auto` flag (top-level; add to catalogue if absent — ASK THE DEVELOPER if a new flag is needed or whether the env var alone suffices).
- `stdout_is_tty()` for hyperlink emission (OSC 8 sequences only when stdout is a TTY).
- `terminal_width()` (via `crossterm::terminal::size`) for table-width-aware rendering.

#### 2f. Unit tests for CLI completion

Each per-command renderer gets snapshot tests using `insta` (already a dependency) or a hand-rolled string-equality test:

- `render_outcome_for_cli` snapshot per `*Outcome` variant.
- `render_error_for_cli` snapshot per `CommandError` variant including exit code mapping.
- TTY-vs-pipe rendering decision snapshots (color on, hyperlinks on/off, table width adjustments).
- `ready --json` JSON schema snapshot against a frozen fixture.
- TTY-gated Q&A: piped-stdin tests that confirm safe defaults are returned and no read attempted.

### 3. Test layout and philosophy

Same philosophy as `0069-…` §"Test Considerations" and `0067-…` §"Test Considerations": **only Layer 0/1/2 colocated unit tests and Layer 3 (CLI) unit tests**. The cross-layer integration tests, real-Docker / real-network end-to-end tests, parity tests against the pre-refactor binary, and the `tests/` directory rebuild are 0073's responsibility. **Do not create files under `tests/` in this work item.**

## Manual sign-off checklist (gating 0071)

The PR description MUST include:

- **CLI command parity table** — every command and subcommand documented in `aspec/uxui/cli.md`, each marked PASS / MINOR-DRIFT (one-sentence justification) / REGRESSION (block). The expected coverage:
  - `amux init [--agent X] [--aspec]` — for each agent in `AGENT_VALUES` plus `--aspec` on/off.
  - `amux ready [--refresh] [--build] [--no-cache] [--non-interactive] [--allow-docker] [--json]` — exercise every flag combination at least once.
  - `amux implement 0001 [--workflow] [--worktree] [--yolo] [--auto] [--plan] [--agent] [--model] [--non-interactive] [--allow-docker] [--mount-ssh] [--overlay]` — exercise the implication rule (`--yolo + --workflow ⇒ --worktree`).
  - `amux chat [flags]` interactive (PTY) and `amux chat -n` non-interactive.
  - `amux specs new [--interview]` and `amux specs amend 0042 [-n] [--allow-docker]`.
  - `amux new spec` (alias check), `amux new workflow [--interview] [--global] [--format toml|yaml|md]`, `amux new skill [--interview] [--global]`.
  - `amux claws init / claws ready / claws chat`.
  - `amux status [--watch]`.
  - `amux config show / get FIELD / set FIELD VALUE [--global]`.
  - `amux exec prompt "..."` and `amux exec workflow PATH [...]`.
  - `amux download` for each asset.
  - `amux auth`.
  - `amux headless` and `amux remote` — confirm flag parsing works; the command bodies are deferred to 0072 (PASS = "stubbed cleanly with `command not yet implemented` exit code").
- A confirmation that `oldsrc/` was NOT touched (other than possibly `oldsrc/README.md`).

A REGRESSION blocks the PR. The implementing agent MUST fix or escalate.

## What must NOT happen in this work item

- No business logic in `src/frontend/cli/`. Every decision that affects behavior lives in Layer 2.
- No `src/frontend/tui/` work. That is `0071-…`.
- No `src/frontend/headless/` work. That is `0072-…`.
- No completion of `headless start/kill/logs/status` or `remote run/session start/session kill` command bodies — those depend on the headless frontend and ship in `0072-…`.
- No completion of `AuthEngine::ensure_self_signed_tls` — only exercised by the headless server; `0072-…`.
- No deletion of `oldsrc/`. That is `0073-…`.
- No new commands, no new flags, no new user-visible behavior. This work item closes the gap between the structural skeleton and the documented surface; it does not add to the surface.
- No edits inside `oldsrc/` other than possibly the `oldsrc/README.md` note.
- No tests under `tests/`. 0073 owns that tree.

## Edge Case Considerations

- **`docker` binary missing on host** — every backend method that shells out to Docker MUST translate "binary not found" into a structured `EngineError::ContainerRuntimeUnavailable` rather than crashing with `ENOENT`. The CLI surfaces this as a friendly message via `render_error_for_cli`.
- **Apple Containers on macOS** — same as Docker but for the `container` CLI; the existing `BackendUnsupportedOnPlatform` error already covers wrong-OS configuration.
- **PTY allocation failure** — when the host can't allocate a PTY (rare; common in some CI containers), fall back to non-PTY mode and emit `UserMessage::warning("PTY unavailable — running in non-interactive mode")`. The container still runs.
- **Workflow file at non-existent path** — surface via `CommandError::WorkflowFileNotFound { path }`.
- **Concurrent ready/init builds** — if two `amux ready` invocations race in the same repo, the second sees the first's image already built (idempotent). If they race on writing `Dockerfile.dev`, the loser silently no-ops (the file exists). No inter-process locking is required.
- **Aspec download fallback** — `--aspec` with no network access falls back to bundled silently with a warning.
- **Per-agent Dockerfile download fallback** — when `download_agent_dockerfile` fails (no network, 404), surface as `EngineError::AgentDockerfileDownloadFailed` and let the caller decide whether to abort the build or continue. Old-amux aborts the build; preserve.
- **Config field validation** — `config set agent something-bogus` MUST fail with a structured error listing valid agents.
- **Spec template missing** — `specs new` when `aspec/work-items/0000-template.md` doesn't exist MUST surface `CommandError::SpecTemplateMissing` with a hint to run `amux init --aspec`.
- **Worktree merge conflict** — already covered in `0069-…`; reaffirm that the `WorktreeLifecycle` engine surfaces conflicts and the frontend prompts the user.
- **Stdin-piped CLI input** — every Q&A frontend method must safely return a default when stdin is not a TTY rather than blocking.
- **`--non-interactive` AND `--yolo`** — already legal per the catalogue. Behavior: yolo enabled but no workflow control dialog; agent advances autonomously.
- **`--overlay` parsing errors** — invalid overlay spec → `CommandError::InvalidOverlaySpec { spec, reason }`.
- **`amux config set` of an unknown field** — Levenshtein-suggest and reject.

## Test Considerations

### Test philosophy (read first)

Layer 0/1/2 implementations get colocated `#[cfg(test)] mod tests`. Each engine phase, each command body, each new helper has at least one happy-path test plus failure-path tests for every distinct error variant it can return.

Layer 3 CLI tests cover renderers (snapshot tests), per-command Q&A frontend impls (TTY-vs-pipe behavior), and the flag-plumbing audit table.

**Do NOT create files under `tests/`.** That tree is rebuilt from scratch in 0073.

### Tests added in this work item

Per the inventory in §1 + §2, every replaced stub gets colocated tests. Notable additions:

- `src/engine/container/docker.rs` — argv-assembly tests against a `MockSpawner` covering: image-only, image+entrypoint, image+overlays (RW + RO), image+env, image+yolo, image+seeded-prompt, image+working-dir, image+container-name, image+allow-docker, image+mount-ssh.
- `src/engine/ready/mod.rs` — phase-by-phase tests for `CreatingDockerfile` (file written), `MigratingLegacyLayout` (backup + overwrite happens), `BuildingBaseImage` (real `ContainerRuntime` call against a fake), `RunningAudit` accepted vs declined paths, `RebuildingAfterAudit` (rebuild iff Dockerfile changed).
- `src/engine/init/mod.rs` — phase-by-phase tests for `CreatingAspecFolder` bundled vs downloaded, `SettingUpDockerfile` (idempotent), `WritingConfig` (idempotent), `WritingWorkItemsConfig` (writes when Some).
- `src/engine/claws/mod.rs` — per-mode happy paths plus the `Init`-from-`Ready` transition.
- `src/engine/agent/mod.rs` — `ensure_available` happy path, missing-Dockerfile-download path, build-failure path.
- `src/engine/overlay/mod.rs` — claude sanitization (oauthAccount stripped, denylist applied, yolo-mode injected, LSP suppressed, USER directive rewrite).
- `src/command/commands/chat.rs` — argv assembly + frontend lifecycle (`set_pty_active` invoked in correct order).
- `src/command/commands/specs.rs` — `SpecsNew` interactive path writes a file, `--interview` triggers the agent run, `SpecsAmend` looks up the file and runs the agent.
- `src/command/commands/new.rs` — workflow (toml/yaml/md) + skill paths.
- `src/command/commands/status.rs` — TIPS deterministic-by-second selection, CLEAR_MARKER emitted only on watch ticks 2+, list_running rows enriched with stats.
- `src/command/commands/config.rs` — set with invalid value rejected, get of unknown field returns suggestions, show enumerates every documented field.
- `src/frontend/cli/render.rs` — snapshot per `*Outcome` and per `*Error`.
- `src/frontend/cli/per_command/*.rs` — TTY-piped tests per Q&A method, flag-plumbing tests per command.

### Build & CI

- `cargo build --release` produces a single statically-linked `amux`.
- `cargo test` passes including the new colocated tests added by this work item.
- `cargo clippy --all-targets -- -D warnings` passes.
- `make all`, `make install`, `make test` work.
- The existing `tests/` directory continues to compile (nothing under it is updated yet — that's 0073) — if 0066–0069 left a `tests/` tree that no longer compiles after these changes, document each failure in `aspec/review-notes/0070-followups.md` for 0073 to address rather than fixing in this WI.

## Codebase Integration

- Follow `aspec/architecture/2026-grand-architecture.md` as the source of truth.
- Follow `aspec/uxui/cli.md` for user-facing behavior; nothing in this work item changes that surface.
- Follow `0067-…` §9a engine parity addenda for the engine-body implementations.
- Follow `0069-…` §1 for the CLI's structural surface; this work item completes the rendering / flag-plumbing / TTY paths inside that surface.
- Do not edit `oldsrc/` (other than the README note).
- Do not delete `oldsrc/` — that is `0073-…`.
- Do not introduce upward calls — engines accept frontend traits; commands accept frontend traits; the CLI implements those traits and never reaches into Layer 2 internals.
- The PR description MUST link to `aspec/architecture/2026-grand-architecture.md` and to this work item, MUST include the CLI parity smoke-test checklist, and MUST list every developer-clarification question raised.
- After this work item lands, the next agent picks up `0071-grand-architecture-tui-frontend.md`.
