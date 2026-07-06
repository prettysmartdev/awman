# Work Item: Enhancement

Title: Dynamic Workflow Config
Issue: issuelink

## Summary

Add a `dynamicWorkflows` section to the repo-local `.awman/config.json` that lets teams customize which agents and models are available in dynamic workflows, cap concurrent steps, and set a default leader — all without touching CLI flags. The leader prompt is updated to include this information so the leader agent can make informed scheduling decisions. A mismatch between configured agents and available Dockerfiles fails fast with a descriptive error. The TUI config view is audited and improved to handle the resulting long and nested values legibly.

## User Stories

### User Story 1
As a: user

I want to: configure which agents and models are available for dynamic workflows in `.awman/config.json`

So I can: restrict workflow steps to a known, approved set of agents and models without passing flags on every run.

### User Story 2
As a: user

I want to: set `maxConcurrentSteps` and `defaultLeader` in the repo config

So I can: control workflow parallelism and the leader agent/model as repo-level defaults that the whole team shares via version control.

### User Story 3
As a: user

I want to: view and understand `dynamicWorkflows` values in the TUI config dialog (Ctrl-,) without them being truncated or unreadable

So I can: inspect and reason about the current workflow configuration without leaving the TUI.

## Implementation Details

### 1. Extend `RepoConfig` with `DynamicWorkflowsConfig`

**File:** `src/data/config/repo.rs`

Add a new nested struct and a field to `RepoConfig`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DynamicWorkflowsConfig {
    /// agent name → list of model name strings available for that agent
    pub agents_to_models: Option<HashMap<String, Vec<String>>>,
    /// advisory cap on concurrent workflow steps (None = unlimited)
    pub max_concurrent_steps: Option<usize>,
    /// default leader spec in agent::model format; overridden by --leader flag
    pub default_leader: Option<String>,
}
```

Add to `RepoConfig`:
```rust
pub dynamic_workflows: Option<DynamicWorkflowsConfig>,
```

Use `#[serde(default)]` on the field so an absent JSON key deserializes to `None` without error. Follow the existing `WorkItemsConfig` pattern.

### 2. Validate agents before workflow start

**File:** `src/command/commands/exec_workflow.rs`

After `discover_agent_dockerfiles()` is called and `available_agents` is populated, if `repo_config.dynamic_workflows.agents_to_models` is `Some(map)`, validate every key in the map against the set of discovered agent names. Collect all missing names and fail with a single descriptive error:

```
Error: dynamicWorkflows.agentsToModels references agents that have no Dockerfile in this repo: [foo, bar].
Available agents: [claude, codex, gemini].
Add a .awman/Dockerfile.<agent> for each missing agent, or remove it from agentsToModels.
```

Fail before spawning any container. Do not warn and continue.

### 3. Build the agents section for the leader prompt

**File:** `src/command/commands/exec_workflow.rs` (near the `format_available_agents` call, lines ~1513–1523, 1740–1744)

Add a new helper `format_agents_with_models(map: &HashMap<String, Vec<String>>) -> String` that renders as:

```
- claude: claude-opus-4-8, claude-sonnet-4-6
- codex: codex-mini-latest
- gemini: gemini-2.5-pro
```

When `agents_to_models` is `Some`, pass its output to `build_leader_prompt` instead of the Dockerfile-derived list. When `None`, keep the existing `format_available_agents(&available_agents)` call.

### 4. Extend `build_leader_prompt` with a `max_concurrent_steps` advisory

**Files:** `src/data/dynamic_workflow_assets.rs`, `src/assets/dynamic/leader-prompt.md`

Add a `max_concurrent_steps: Option<usize>` parameter to `build_leader_prompt`. Add a `{{max_concurrent_steps_note}}` placeholder to the template. Render it as:

- When `Some(n)`: `"Note: the repository configuration advises a maximum of {n} concurrent steps. Plan your workflow accordingly."`
- When `None`: `""` (empty — omit the line entirely)

This is advisory only. No hard enforcement in the scheduler at this time.

### 5. Leader resolution order

**File:** `src/command/commands/exec_workflow.rs` (near `LeaderSpec::parse`, lines ~71–87)

Apply this priority order when resolving the leader for a dynamic workflow:

1. `--leader` CLI flag (existing behavior)
2. `repo_config.dynamic_workflows.default_leader` (new)
3. Existing repo-default agent/model fallback

Parse `default_leader` through `LeaderSpec::parse` on load so format errors surface as config errors before the workflow starts, not mid-run.

### 6. TUI config display — audit and improvements

**Files:** `src/frontend/tui/render.rs` (`render_config_show`), `src/command/commands/config.rs` (`collect_config_rows`, `config_field_value`)

**Audit findings to address:**

- `config_field_value` currently calls `.to_string()` on array/object JSON values, which produces compact single-line JSON that can be extremely wide.
- Long values can overflow table cells because the current renderer does not wrap or scroll within a cell.

**Changes:**

- In `collect_config_rows`, flatten `DynamicWorkflowsConfig` sub-fields as dot-keyed rows (e.g., `dynamicWorkflows.defaultLeader`, `dynamicWorkflows.maxConcurrentSteps`, `dynamicWorkflows.agentsToModels`) so they appear as individual navigable rows rather than one unreadable blob.
- For `agentsToModels`, render each entry as a separate display row with key `dynamicWorkflows.agentsToModels.<agentName>` and value as the comma-separated model list.
- In `render_config_show`, truncate cell values that exceed the available column width with a `…` suffix, and show the full value in a detail line at the bottom of the dialog when that row is selected (similar to how a status bar shows details).
- Mark `agentsToModels` sub-rows as read-only in the TUI edit path (pressing `e` shows a message: "Edit this value directly in .awman/config.json"). The other scalar fields (`defaultLeader`, `maxConcurrentSteps`) remain inline-editable.

## Edge Case Considerations

- **Empty `agentsToModels` map (`{}`)**: treat the same as `None` — fall back to Dockerfile discovery. Log a debug-level warning.
- **`agentsToModels` agent exists in Dockerfiles but has an empty model list (`[]`)**: treat as a configuration error. Fail with: `"dynamicWorkflows.agentsToModels.claude has an empty model list. Provide at least one model name."`.
- **`maxConcurrentSteps: 0`**: reject as invalid during config load — zero concurrent steps would deadlock any workflow. Error: `"dynamicWorkflows.maxConcurrentSteps must be >= 1"`.
- **`defaultLeader` format invalid**: validate with `LeaderSpec::parse` during `RepoConfig::load`. Surface as a config load error so it fails before any UI or workflow starts.
- **`--leader` flag + `defaultLeader` both set**: `--leader` wins silently (no warning needed — precedence is documented).
- **`dynamicWorkflows` key present but all sub-fields absent**: deserializes to `DynamicWorkflowsConfig::default()` with all `None`; treat as if section is absent.
- **Leader agent referenced in `--leader` or `defaultLeader` is not in `agentsToModels`**: proceed without error — the flag takes precedence over the configured subset, and the leader is not a step agent.
- **`agents_to_models` keys use inconsistent casing** (e.g., `"Claude"` vs `"claude"`): compare against discovered agent names case-insensitively, but always store and emit lowercase. Emit a warning if a case-folded match is the only match.
- **Repo has no agent Dockerfiles at all**: `dynamicWorkflows.agentsToModels` being set is still validated, but an empty discovery result with `None` agents_to_models is not an error (existing behavior).

## Test Considerations

- **Unit — `DynamicWorkflowsConfig` serde**: deserialize with all fields, missing fields, extra unknown fields (verify `deny_unknown_fields` is NOT set), and the empty-object case.
- **Unit — `maxConcurrentSteps: 0` rejection**: `RepoConfig::load` returns an error for `"maxConcurrentSteps": 0`.
- **Unit — `defaultLeader` format validation**: invalid format (no `::`, empty component) returns a config load error; valid format parses successfully.
- **Unit — agent validation**: test all-match, partial-mismatch (error lists only missing), complete-mismatch, empty-map (no error), empty-model-list-for-one-agent (error).
- **Unit — `format_agents_with_models`**: correct output for a typical map; sorted output (alphabetical by agent name for determinism); handles a single model and multiple models.
- **Unit — `build_leader_prompt` with `max_concurrent_steps`**: advisory note appears when `Some(n)`, absent when `None`.
- **Unit — leader resolution order**: mock all three sources (flag, config, default) and verify priority; verify config `defaultLeader` is used when flag is absent.
- **Unit — `collect_config_rows` for `dynamicWorkflows`**: flattened dot-keyed rows appear; `agentsToModels` entries each get their own row.
- **Integration — workflow start with valid `dynamicWorkflows` config**: leader prompt contains the configured agent/model list and the `maxConcurrentSteps` advisory.
- **Integration — workflow start with mismatched agents**: fails before container spawn with the expected error message.
- **Integration — `config show` CLI output**: `dynamicWorkflows` fields appear, values are not truncated in CLI text output.
- **E2E — TUI config display**: long `agentsToModels` values are truncated in the cell with `…` and the full value appears in the detail line when the row is focused.

## Codebase Integration

- Follow the `WorkItemsConfig` precedent in `repo.rs` for struct definition, `serde` attributes, and `Default` derive.
- Use `#[serde(rename_all = "camelCase")]` on `DynamicWorkflowsConfig` to match the JSON key style used throughout the config file.
- `HashMap` imports: use `std::collections::HashMap`; it is already used in `exec_workflow.rs`.
- Keep `format_agents_with_models` alongside `format_available_agents` in `exec_workflow.rs`; they share the same call site and conceptual purpose.
- The leader prompt template at `src/assets/dynamic/leader-prompt.md` is embedded via `include_str!`; changes to it require no separate asset registration.
- For TUI cell truncation, follow the pattern used in other table renderers in `render.rs` — measure column width using `ratatui::layout` constraints and shorten the string before rendering.
- `collect_config_rows` returns `Vec<ConfigRow>`; add the flattened dynamic workflow rows in the same block as other repo-config rows, guarded by `if let Some(dw) = &repo_config.dynamic_workflows`.
- All new errors should use the existing `anyhow::bail!` / `anyhow::anyhow!` patterns already present in `exec_workflow.rs` and `config/repo.rs`.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** — update `docs/08-headless-mode.md` (or equivalent dynamic workflows doc) to describe the `dynamicWorkflows` config section, its fields, and the agent-validation behavior.
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-workflow-config.md` covering the full `dynamicWorkflows` reference if no existing doc covers it)
- **Never create work-item-specific docs** (e.g., no "WI 0095 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
