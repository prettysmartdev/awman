# Work Item: Feature

Title: model selection flag
Issue: issuelink

## Summary:
- Add `--model <NAME>` flag to `chat` and `implement` subcommands so users can override which LLM model the launched agent uses.
- Add a `Model:` field to workflow step definitions so individual steps can target specific models independently.
- The `--model` flag passed to `implement` acts as a default for all workflow steps that do not define their own `Model:` field; steps with `Model:` always use their explicit value regardless of the flag.
- Each agent CLI has its own model-selection syntax; amux translates the single `--model` value to the correct per-agent flag at launch time.


## User Stories

### User Story 1:
As a: user

I want to:
pass `--model claude-opus-4-6` (or any valid model name) to `amux chat` or `amux implement`

So I can:
run a one-off session against a specific model without changing my repo or global config, and without having to know each agent's native model flag syntax.

### User Story 2:
As a: user

I want to:
specify `Model: claude-haiku-4-5` on individual steps in a workflow file

So I can:
run expensive reasoning steps on a large model while routing cheaper, high-volume steps to a smaller model — all within a single `amux implement` invocation.

### User Story 3:
As a: user

I want to:
combine `--model`, `--agent`, `--yolo`, and `--auto` freely on the same command

So I can:
fully control the agent identity, model, and permission posture for any session without flag conflicts or silent overrides.


## Implementation Details:

### 1. Per-agent model flag translation

Before implementing, confirm the exact model-selection flag for each supported agent. Based on documented CLIs:

| Agent      | Model flag syntax                        | Notes                                           |
|------------|------------------------------------------|-------------------------------------------------|
| `claude`   | `--model <name>`                         | Claude Code CLI supports `--model` directly     |
| `codex`    | `--model <name>`                         | OpenAI Codex CLI supports `--model`             |
| `gemini`   | `--model <name>`                         | Gemini CLI supports `--model`                   |
| `opencode` | `--model <name>`                         | Confirm; fallback to warning if unsupported     |
| `maki`     | `--model <name>`                         | Confirm; fallback to warning if unsupported     |

Add a new function `append_model_flag(args: &mut Vec<String>, agent: &str, model: &str)` in `src/commands/agent.rs`, following the same structure as `append_autonomous_flags`. The flag-appending behaviour differs by agent tier:

- **Confirmed agents** (`claude`, `codex`, `gemini`): append `--model <name>` silently.
- **Unconfirmed agents** (`opencode`, `maki`): emit a `WARNING:` to stderr that `--model` support is unconfirmed, then append the flag anyway. The agent will surface its own error if the flag is unsupported. Do not abort the session.
- **Unknown agents**: emit a `WARNING:` to stderr and **do not** append the flag. An unrecognised agent may use entirely different flag syntax, so passing `--model` would likely corrupt its argument list.

### 2. CLI struct changes (`src/cli.rs`)

Add `model: Option<String>` to both `Chat` and `Implement` structs. Position it after `agent` in the struct field list for consistency.

```rust
/// Override the model used by the launched agent (e.g. claude-opus-4-6).
#[arg(long, value_name = "NAME")]
pub model: Option<String>,
```

### 3. Spec table parity (`src/commands/spec.rs`)

Add one entry to both `CHAT_FLAGS` and `IMPLEMENT_FLAGS`:

```rust
FlagSpec { name: "model", takes_value: true, value_name: "NAME", hint: "override agent model (e.g. claude-opus-4-6)" },
```

The existing CLI/spec parity tests in `src/cli.rs` will enforce this automatically.

### 4. Entrypoint builder changes

The five entrypoint builder functions (`chat_entrypoint`, `chat_entrypoint_non_interactive`, `agent_entrypoint`, `agent_entrypoint_non_interactive`, `workflow_step_entrypoint`) in `src/commands/chat.rs` and `src/commands/implement.rs` are **not** modified. They continue to return a base `Vec<String>` that does not include model flags. Model flag appending is handled downstream (see steps 5 and 9c), which keeps the builders simple and single-purpose.

### 5. `run_agent_with_sink` signature (`src/commands/agent.rs`)

Add `model: Option<&str>` to `run_agent_with_sink`. After the entrypoint is received (which already contains any autonomous and plan flags built by the caller), call `append_model_flag` when `model` is `Some`. This ensures model is appended last. The TUI launch functions (`launch_implement`, `launch_chat`) perform the equivalent call directly before spawning the container, since they manage Docker themselves without going through `run_agent_with_sink`.

### 6. Workflow parser changes (`src/workflow/parser.rs`)

Add `model: Option<String>` to `WorkflowStep`:

```rust
/// Optional model override for this step (from `Model:` field).
/// When `None`, the workflow-level --model flag (if any) is used; if that is also
/// absent the agent uses its default model.
pub model: Option<String>,
```

Parse `Model:` using the same guard pattern as `Agent:` — only recognised before `Prompt:` is encountered and only when `!in_prompt`. Maintain the same `current_model` accumulator pattern used for `current_agent`.

### 7. Workflow state persistence (`src/workflow/mod.rs`)

Add `model: Option<String>` to `WorkflowStepState` with `#[serde(default)]` so existing state files deserialise without errors.

### 8. Workflow runner model resolution (`src/commands/implement.rs`, `run_workflow`)

For each step, compute the effective model using:

```rust
let step_model: Option<&str> = step.model.as_deref().or(cli_model.as_deref());
```

- If the step defines `Model:`, use it (ignores `--model` flag).
- If the step has no `Model:` but `--model` was passed, use the flag value.
- If neither is set, pass `None` — the agent launches with its built-in default.

### 9. TUI parity

The `FlagSpec` entry added in step 3 automatically extends TUI **autocomplete** (consumed by `src/tui/input.rs`) and **flag-parser tokenization** (the `flag_parser::parse_flags` function consults the spec to know that `--model` takes a value and must consume the next token). Those two surfaces require no additional changes.

However, the TUI command dispatcher does not automatically propagate new flags — every parsed flag must be explicitly extracted, stored, and threaded. Three additional changes are required:

**a. `src/tui/state.rs` — add `model` to both `PendingCommand` variants**

Add `model: Option<String>` to `PendingCommand::Chat` and `PendingCommand::Implement`. Position the field after `agent` in each variant, matching the struct field order in the CLI structs (step 2).

**b. `src/tui/mod.rs` — command dispatcher**

In both the `"chat"` and `"implement"` branches of `execute_command`, extract the model value from the parsed flag map and include it in the `PendingCommand` construction:

```rust
let model = flag_parser::flag_string(&flags, "model").map(str::to_string);
```

Also update the `implement` usage-error hint string to include `[--model=<NAME>]` so it stays in sync with the accepted flags.

**c. `src/tui/mod.rs` — `launch_chat` and `launch_implement` and all `PendingCommand` re-enqueue sites**

Add `model: Option<String>` to the `launch_chat` and `launch_implement` function signatures and thread it through to `run_agent_with_sink` / the entrypoint builder calls (completing the chain started in steps 4–5).

`launch_implement` contains multiple sites that re-construct `PendingCommand::Implement` to save state before showing a dialog (worktree uncommitted-files check, agent Dockerfile missing for non-workflow runs, agent Dockerfile missing for workflow steps). Each of these re-enqueue sites must include the `model` field so that the model is preserved when execution is resumed after the dialog is dismissed.


## Edge Case Considerations:

- **Unknown model names**: amux passes the value verbatim to the agent without validation. The agent will surface its own error for invalid models. Do not add a model allowlist in amux — it would require constant updates as providers release new models.
- **Model + Agent mismatch**: a user may pass `--agent codex --model claude-opus-4-6`. amux passes the value through; the agent CLI will reject incompatible models on its own. Emit no amux-level warning unless the agent is known not to support `--model` at all.
- **`Model:` field combined with `Agent:` field in same step**: both are independent overrides. Resolve agent first (same existing logic), then resolve model. The resulting container is launched with both the step's agent and the step's model.
- **`--model` flag on single-step `implement` (no workflow)**: behaves identically to `chat --model`; model is passed to the single agent launch.
- **Workflow resume**: `WorkflowStepState.model` is persisted in the JSON state file. On resume, the persisted per-step model is used, not any `--model` flag that may differ on the resumed invocation. This matches the existing behaviour for `agent` field.
- **Empty string `Model:`**: treat the same as absent — `None`. A `Model:` line with no value should be a no-op, not an empty string passed to the agent.
- **`--model` with `--plan` or `--yolo`/`--auto`**: these flags are orthogonal. `append_model_flag` is called independently of `append_plan_flags` and `append_autonomous_flags`; the resulting arg order is: plan flags first (appended inside the entrypoint builder), autonomous flags next (appended in `run_with_sink` / TUI launch before calling `run_agent_with_sink`), model flag last. All supported agent CLIs parse flags in an order-independent manner, so the exact position of `--model` relative to other flags has no effect on behaviour.


## Test Considerations:

- **Unit — `append_model_flag`**: for each agent, verify the correct flag string(s) are appended; verify that a `None` model produces no additional args; verify unsupported agents emit a warning and do not abort.
- **Unit — workflow parser**: add cases for `Model:` field present, absent, empty, and appearing after `Prompt:` (should be treated as body text). Cover `Model:` combined with `Agent:` in the same step.
- **Unit — model resolution in workflow runner**: test all three resolution paths: step model wins over flag, flag used when step has none, neither yields `None`.
- **Unit — CLI/spec parity**: the existing parity test in `src/cli.rs` must continue to pass; verify `model` appears in both `CHAT_FLAGS` and `IMPLEMENT_FLAGS` and both CLI structs.
- **Unit — `WorkflowStepState` serde**: deserialising a JSON state file that lacks the `model` key must produce `model: None` (backward compatibility via `#[serde(default)]`).
- **Integration — `chat` with `--model`**: launch a `chat` session in non-interactive mode with a mocked runtime; assert the model flag appears in the constructed entrypoint args.
- **Integration — `implement` with `--model` and no workflow**: same as chat case.
- **Integration — workflow with per-step `Model:` fields**: run a two-step workflow where step A has `Model: model-a` and step B has no `Model:`. With `--model model-b` on the CLI, assert step A launches with `model-a` and step B launches with `model-b`.
- **Integration — workflow with no `Model:` fields and no `--model` flag**: assert neither step receives a model flag in its entrypoint args.
- **TUI integration — `chat --model` sets `PendingCommand`**: in a no-git `App`, execute `"chat --model claude-opus-4-6"` via `execute_command`; assert `PendingCommand::Chat { model: Some("claude-opus-4-6"), .. }`. Mirror this test for both space form and `=` form, following the pattern of `tui_chat_agent_space_form_sets_pending_command`.
- **TUI integration — `implement --model` sets `PendingCommand`**: same as above but for `"implement 0042 --model claude-haiku-4-5"`; assert `PendingCommand::Implement { model: Some("claude-haiku-4-5"), work_item: 42, .. }`. Follow the pattern of `tui_implement_agent_eq_form_sets_pending_command`.


## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Mirror the exact structure of `append_autonomous_flags` when writing `append_model_flag` — same match-on-agent-name pattern, same stderr warning convention for unsupported agents.
- The `Model:` workflow field must be parsed in the same guard style as `Agent:` in `src/workflow/parser.rs` — recognised only before `Prompt:`, ignored inside the prompt body.
- Workflow state backward-compatibility is mandatory: add `#[serde(default)]` to `model` on `WorkflowStepState`, matching the pattern used for the `agent` field.
- All new public functions and structs must have unit tests in the same module per the foundation spec.
- Do not add a model validity check in amux; delegate validation to the agent CLI to avoid coupling amux to any provider's model catalogue.
