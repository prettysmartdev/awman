# Work Item: Feature

Title: Headless Mode Part 2
Issue: issuelink

## Summary:
- Add `amux exec prompt <prompt>` and `amux exec workflow <path>` subcommands, sharing codepaths with `chat` and `implement --workflow` respectively
- Add `-n` as a short-form alias for `--non-interactive` on all commands that support it
- Restructure global config to place all headless settings under a top-level `headless` key, and add `headless.alwaysNonInteractive` option
- Add `--json` flag to the `ready` subcommand for machine-readable output


## User Stories

### User Story 1:
As a: developer running amux from a CI pipeline or script

I want to: run `amux exec prompt "Fix the failing tests"` and have it launch an agent container with that prompt, just like `chat` but with the initial prompt pre-supplied

So I can: invoke a one-shot agent task non-interactively without any manual input, using the same flags I know from `chat`

### User Story 2:
As a: developer automating multi-step agent workflows

I want to: run `amux exec workflow ./path/to/workflow.md` (optionally with `--work-item 0053`) and have it behave identically to `amux implement --workflow`

So I can: reuse workflow files across projects without always needing a paired work item, and eventually transition away from the `implement` command entirely

### User Story 3:
As a: headless server operator

I want to: set `headless.alwaysNonInteractive: true` in `~/.amux/config.json` and have all dispatched commands automatically run in non-interactive mode

So I can: guarantee that no command blocks waiting for TTY input when executing from the headless HTTP server


## Implementation Details:

### 1. `amux exec prompt <prompt>`

**CLI (`src/cli.rs`)**
- Add a new top-level `Exec` variant to the `Command` enum, with a nested `ExecAction` subcommand enum containing `Prompt` and `Workflow` (with alias `wf`)
- `ExecAction::Prompt` fields: positional `prompt: String`, then the exact same flags as `Chat` (`non_interactive`, `plan`, `allow_docker`, `mount_ssh`, `yolo`, `auto`, `agent`, `model`)
- Add `-n` as a clap short alias for `--non-interactive` on every command variant that carries that flag (`Implement`, `Chat`, `Ready`, `ExecAction::Prompt`, `ExecAction::Workflow`, `SpecsAction::Amend`). Use `#[arg(long = "non-interactive", short = 'n')]` in each struct field declaration.

**Dispatch (`src/commands/mod.rs`)**
- Add `Command::Exec { action }` match arm dispatching to `commands::exec::run_prompt(...)` or `commands::exec::run_workflow(...)`

**Implementation (`src/commands/exec.rs`)** — new file
- `run_prompt(prompt, non_interactive, plan, allow_docker, mount_ssh, yolo, auto, agent_override, model_override, runtime)`: identical to `chat::run_with_sink` but injects the `prompt` string as the initial input to the agent container. The entrypoint should be built using `agent_entrypoint_with_prompt(agent, prompt, plan)` (new helper in `agent.rs`, analogous to `agent_entrypoint_non_interactive` but without the non-interactive print flag). In interactive mode, the prompt is passed as the initial stdin string/argument to the agent; in non-interactive mode, it follows the same path as `agent_entrypoint_non_interactive`. The key behavioural difference from `chat`: the container is always started with the prompt baked into the launch args, so the user is not dropped into a blank session.
- `chat::run` and `exec::run_prompt` must share the same underlying sink/dispatch function with the only varying parameter being the optional initial prompt.

**Specs (`src/commands/spec.rs`)**
- Add `EXEC_PROMPT_FLAGS` and `EXEC_WORKFLOW_FLAGS` constants and include them in `ALL_COMMANDS` so the TUI autocomplete and headless server both know about the new subcommands.

**Headless server (`src/commands/headless/server.rs`)**
- Add `"exec"` to `KNOWN_SUBCOMMANDS` so the headless API can dispatch it.

### 2. `amux exec workflow <path>` / `amux exec wf <path>`

**CLI (`src/cli.rs`)**
- `ExecAction::Workflow` (with `#[command(alias = "wf")]`) fields:
  - Positional required: `workflow: PathBuf`
  - Optional flag: `--work-item <N>` / `-w <N>` (string, validated with `parse_work_item`): `#[arg(long = "work-item", short = 'w', value_name = "N")]`
  - All other flags identical to `Implement` minus the positional `work_item`: `non_interactive`, `plan`, `allow_docker`, `worktree`, `mount_ssh`, `yolo`, `auto`, `agent`, `model`

**Implementation (`src/commands/exec.rs`)**
- `run_workflow(workflow_path, work_item_str, non_interactive, plan, allow_docker, worktree, mount_ssh, yolo, auto, agent_override, model_override, runtime)`:
  - Calls `implement::run(...)` with the workflow path as the `workflow_path` parameter
  - If `work_item_str` is `Some`, passes it through as-is. If `None`, passes a sentinel (`None`) indicating "no work item"
- Refactor `implement::run` to accept `work_item_str: Option<&str>` instead of `&str`. When `None`:
  - Skip `find_work_item()` lookup
  - Skip work item number parsing
  - Skip work item content loading for prompt substitution
  - `WorkflowState.work_item` becomes `Option<u32>` (or use `0` as sentinel and adjust state path to use workflow name only when no work item is set)
  - Prefer making `WorkflowState.work_item` an `Option<u32>` to keep semantics clear
  - Workflow state file path: when no work item, use `~/.amux/headless/<workflow_name>.state.json` or similar, keyed by workflow content hash only
- `implement` command continues to work identically by always providing `Some(work_item_str)`.
- The `--work-item` / `-w` flag on `exec workflow` is optional; when absent no work item context is injected.

**Headless server**
- `exec workflow` and `exec wf` are dispatched via the `"exec"` subcommand path.

### 3. Config restructuring — `headless.*` namespace

**`src/config/mod.rs`**
- Add a new nested struct:
  ```rust
  #[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
  pub struct HeadlessConfig {
      #[serde(rename = "workDirs", skip_serializing_if = "Option::is_none")]
      pub work_dirs: Option<Vec<String>>,
      #[serde(rename = "alwaysNonInteractive", skip_serializing_if = "Option::is_none")]
      pub always_non_interactive: Option<bool>,
  }
  ```
- In `GlobalConfig`, replace the flat `headless_work_dirs: Option<Vec<String>>` field with:
  ```rust
  #[serde(skip_serializing_if = "Option::is_none")]
  pub headless: Option<HeadlessConfig>,
  ```
- Add a convenience accessor `effective_headless_work_dirs()` that reads from `config.headless.work_dirs`
- Add a convenience accessor `effective_always_non_interactive()` that reads `config.headless.always_non_interactive`, defaulting to `false`
- No automatic migration from the old `headlessWorkDirs` flat key (headless mode is unreleased)

**`src/commands/config.rs`**
- `config get headless.alwaysNonInteractive` and `config get headless.workDirs` must work — field paths use dot notation to traverse nested structs
- `config set headless.alwaysNonInteractive true --global` must serialize correctly into the nested `headless` object without destroying other sibling keys
- `config show` must render the `headless` block with its sub-fields

**Applying `headless.alwaysNonInteractive`**
- In `src/commands/mod.rs` (or at the dispatch layer), after parsing the CLI args and before calling the command handler, check `effective_always_non_interactive()`. If `true`, forcibly set `non_interactive = true` on any `Command` variant that carries that flag (`Implement`, `Chat`, `Ready`, `ExecAction::Prompt`, `ExecAction::Workflow`, `SpecsAction::Amend`).
- In headless server dispatch (`server.rs`), inject `--non-interactive` into the args vector when `effective_always_non_interactive()` is true and the dispatched subcommand supports it.

**Headless server start**
- Update the workdir loading in `headless/mod.rs` to read from `config.headless.work_dirs` instead of the old flat field. The `--workdirs` CLI flag on `headless start` still overrides config.

### 4. `ready --json`

**CLI (`src/cli.rs`)**
- Add `#[arg(long)] json: bool` to the `Ready` variant

**`src/commands/ready.rs`**
- When `--json` is set, suppress all human-readable output and instead collect status into a structured type, then print `serde_json::to_string_pretty(...)` at the end
- JSON shape (at minimum):
  ```json
  {
    "docker": { "available": true },
    "dockerfile": { "exists": true, "path": "/repo/Dockerfile.dev" },
    "base_image": { "built": true, "tag": "amux-base:abc123" },
    "agent_image": { "built": true, "tag": "amux-claude:abc123" },
    "audit": { "ran": false }
  }
  ```
- When `--refresh` is also set, include audit results in the JSON output once the audit completes
- The `--json` flag should imply `--non-interactive` (no interactive prompts) since it is inherently for machine consumption
- Pass `json: bool` into `ReadyOptions` and thread it through `ready_flow.rs`


## Edge Case Considerations:

- **`exec prompt` empty string**: Reject empty prompt at CLI validation time with a descriptive error ("prompt cannot be empty").
- **`exec workflow` missing work item**: The workflow file can use `{{work_item}}` or `{{work_item_content}}` template variables; when no work item is provided, leave those placeholders unexpanded (or emit a warning) rather than crashing.
- **`exec workflow` work item not found**: Return a clear error pointing to the expected path pattern, same as `implement` does today.
- **`WorkflowState` backward compatibility**: Existing state files on disk have `"work_item": <u32>`. Changing to `Option<u32>` must deserialize old state files correctly (`serde` default will treat a missing field as `None`, but existing files have an integer value that must still deserialize to `Some(n)`).
- **`headless.alwaysNonInteractive` with TTY-required commands**: Commands that cannot meaningfully run non-interactively (e.g., bare `chat` without `--non-interactive`) may still work — non-interactive mode is already a supported flag. Just ensure the auto-injection doesn't produce duplicate flags.
- **Config get/set for nested paths**: The `config get/set` implementation must handle dot-separated paths without breaking existing flat-key lookups (`default_agent`, `runtime`, etc.).
- **`ready --json` during `--refresh`**: The audit involves a streaming agent container run. Buffer output and include a summary in the final JSON rather than streaming intermediate text.
- **Headless `KNOWN_SUBCOMMANDS`**: `"exec"` must be added; the existing validation must also accept `exec prompt` and `exec workflow` as valid two-level subcommand paths (the args vector passed to the headless server includes `["exec", "prompt", "<prompt>", ...]`).
- **`implement` backward compatibility**: The refactor of `work_item_str` to `Option<&str>` in `implement::run` must not change the observable behavior of the existing `implement` command in any way.


## Test Considerations:

- **Unit tests** in `src/commands/exec.rs`: Verify that `run_prompt` builds the same container launch call as `chat::run_with_sink` when given identical flags, with only the prompt injection differing.
- **Unit tests** in `src/commands/exec.rs`: Verify `run_workflow` with `work_item = None` skips the work item lookup path and does not substitute `{{work_item}}` variables.
- **Unit tests** in `src/config/mod.rs`: Verify `HeadlessConfig` round-trips through JSON with both fields set, with only one set, and with neither set. Verify old flat `headlessWorkDirs` key is no longer deserialized into the new struct (documents the intentional breaking change).
- **Unit tests** in `src/commands/ready.rs`: When `--json` is set, the output is valid JSON containing the expected top-level keys.
- **Integration tests**: `exec prompt` → verify container launch args include the prompt and all flag-driven options (plan, yolo, model, etc.)
- **Integration tests**: `exec workflow ./wf.md` without work item → workflow executes, `{{work_item}}` placeholders in templates are left as-is or emit a clear warning.
- **Integration tests**: `exec workflow ./wf.md --work-item 0053` → identical behavior to `implement 0053 --workflow ./wf.md`.
- **Integration tests**: `exec wf` alias → same result as `exec workflow`.
- **Config tests**: `config get headless.alwaysNonInteractive` returns `None`/`false` by default; after `config set headless.alwaysNonInteractive true --global`, returns `true`; and `config get headless.workDirs` works.
- **CLI spec parity tests** (`src/cli.rs` lines ~1145–1554): Add parity tests for `exec prompt` and `exec workflow` to confirm all flags are present in both `spec.rs` definitions and the clap structs.
- **Headless server test**: Submit a command with `subcommand = "exec"` and args `["prompt", "hello"]` via `POST /v1/commands`; verify it is accepted rather than rejected by `is_valid_subcommand`.


## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- `exec.rs` should be thin: delegate to `agent::run_agent_with_sink` and `implement::run_workflow` rather than duplicating logic. The goal is shared codepaths, not new implementations.
- The `WorkflowState` struct change (`work_item: u32` → `Option<u32>`) touches the persistence layer in `src/workflow/mod.rs`. Verify that the state file path derivation still produces unique, collision-free paths when no work item is provided (use a combination of the workflow file's name and its content hash).
- Add `ExecAction::Prompt` and `ExecAction::Workflow` entries to `ALL_COMMANDS` in `src/commands/spec.rs` so TUI autocomplete stays in sync with the new commands automatically.
- For `config get/set` with dot-notation nested keys, the simplest approach is an explicit match on known dot-path strings (e.g. `"headless.alwaysNonInteractive"`, `"headless.workDirs"`) alongside the existing flat-key match, rather than a generic recursive JSON path evaluator.
