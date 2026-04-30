# Work Item: Feature

Title: config subcommand
Issue: issuelink

## Summary:
- Add an `amux config` subcommand with `show`, `get`, and `set` actions so users can view and edit global and repo-level configuration without manually editing JSON files.
- `amux config show` renders a table of all config fields across global and repo scopes, including built-in defaults for unset fields and an override indicator when a repo value shadows a global value.
- `amux config get <field>` displays a single field's global value, repo value, and effective (applied) value with a clear indication of which scope wins.
- `amux config set [--global] <field> <value>` writes a config value at the repo level (default) or global level (`--global`), and warns when a global value is already overridden by a repo config or vice versa.
- The TUI interactive mode gains an on-demand config dialog (triggered by `config show` in the command input) that enumerates all available fields with accepted-value hints and allows inline editing.


## User Stories

### User Story 1:
As a: user

I want to:
run `amux config show` and see every configuration field — even unset ones — displayed in a table with their global value, repo value, effective value, and whether the repo is overriding the global

So I can:
understand the full configuration state at a glance without opening or parsing any JSON files.

### User Story 2:
As a: user

I want to:
run `amux config set agent codex` or `amux config set --global default_agent gemini` to change config values from the terminal

So I can:
switch agents or adjust settings quickly, and receive clear warnings if the value I'm setting will be shadowed by an existing override at the other scope.

### User Story 3:
As a: user

I want to:
run `amux config get terminal_scrollback_lines` and see the global value, repo value, and which one is actually in effect

So I can:
debug configuration precedence without manually cross-referencing two JSON files.


## Implementation Details:

### 1. CLI Parser Changes (`src/cli.rs`)

Add a `ConfigAction` enum and a `Config` variant to the `Command` enum:

```rust
#[derive(Subcommand)]
pub enum ConfigAction {
    /// Display all config fields at both global and repo level
    Show,
    /// Show a single field's global value, repo value, and effective value
    Get {
        field: String,
    },
    /// Set a config field value (repo scope by default)
    Set {
        field: String,
        value: String,
        /// Write to global config instead of repo config
        #[arg(long)]
        global: bool,
    },
}

// In Command enum:
/// View and edit global and repo configuration
Config {
    #[command(subcommand)]
    action: ConfigAction,
},
```

### 2. Field Metadata

Define a `ConfigFieldDef` struct and a static `ALL_FIELDS` slice in `src/commands/config.rs` to enumerate every user-facing config field with metadata. This table drives all display, validation, and help text:

| Field key                  | Scope         | Accepted values                                  | Built-in default      | User-settable |
|----------------------------|---------------|--------------------------------------------------|-----------------------|---------------|
| `default_agent`            | Global only   | `claude \| codex \| opencode \| maki \| gemini` | `claude`              | yes           |
| `runtime`                  | Global only   | `docker \| apple-containers`                     | `docker`              | yes           |
| `terminal_scrollback_lines`| Both          | positive integer                                 | `10000`               | yes           |
| `yolo_disallowed_tools`    | Both          | comma-separated tool names                       | `(empty)`             | yes           |
| `env_passthrough`          | Both          | comma-separated env var names                    | `(empty)`             | yes           |
| `agent`                    | Repo only     | `claude \| codex \| opencode \| maki \| gemini` | inherits `default_agent` | yes        |
| `auto_agent_auth_accepted` | Repo only     | `true \| false`                                  | `(not set)`           | no — managed by auth flow |

Note: `yolo_disallowed_tools` and `env_passthrough` use the JSON names `yoloDisallowedTools` and `envPassthrough` on disk but accept snake_case keys on the CLI for consistency.

### 3. `amux config show` output

Resolve git root via `find_git_root()` from `src/commands/init.rs`. If not inside a git repo, render global-only fields and print a note that repo config is unavailable. Produce a plain-text table to stdout:

```
Field                       Global              Repo              Effective          Override
──────────────────────────  ──────────────────  ────────────────  ─────────────────  ────────
default_agent               claude (built-in)   N/A               claude             —
runtime                     docker (built-in)   N/A               docker             —
terminal_scrollback_lines   10000 (built-in)    5000              5000               yes
yolo_disallowed_tools       (empty)             (not set)         (empty)            —
env_passthrough             HOME, PATH          (not set)         HOME, PATH         —
agent                       N/A                 codex             codex              yes
auto_agent_auth_accepted    N/A                 true (read-only)  true               —
```

Column rules:
- **Global**: value from `load_global_config()`, or `"(built-in)"` suffix when the field is not set in the file. For repo-only fields, show `N/A`.
- **Repo**: value from `load_repo_config()`, or `"(not set)"` when absent. For global-only fields, show `N/A`.
- **Effective**: result of the applicable `effective_*` function from `src/config/mod.rs`. For fields without an `effective_*` helper, resolve manually following the same precedence (repo → global → built-in).
- **Override**: `yes` when the repo value is set and differs from the global value; `—` otherwise.

### 4. `amux config get <field>` output

Validate the field key against `ALL_FIELDS`; print a helpful error listing all valid names on unknown input. For valid fields:

```
Field: terminal_scrollback_lines
  Global:     10000 (built-in default)
  Repo:       5000
  Effective:  5000  ← repo overrides global
```

When neither scope has the field set, show the built-in default for both Global and Effective, and mark Repo as `(not set)`.

### 5. `amux config set [--global] <field> <value>` behavior

- Validate the field key against `ALL_FIELDS`; reject unknown fields with a helpful error.
- Reject writes to `auto_agent_auth_accepted` with an error explaining it is managed by the auth flow.
- Reject writing a global-only field (e.g. `runtime`) without `--global`, or a repo-only field (e.g. `agent`) with `--global`.
- Parse `value` according to field type before writing:
  - `String` enum fields (`agent`, `default_agent`, `runtime`): validate against the known set of accepted values.
  - `usize` fields (`terminal_scrollback_lines`): parse as a positive integer; reject zero or non-numeric input.
  - `Vec<String>` fields (`yolo_disallowed_tools`, `env_passthrough`): split on commas, trim whitespace from each element. An empty string input sets the field to an empty `Vec` (not `None`), effectively clearing any override.
- Load the existing config, update the relevant field, and save via `save_repo_config` or `save_global_config`.
- After writing, print a confirmation showing the new effective value.
- Emit a warning when a scope mismatch creates a silent override:
  - Setting `--global` for a field that the repo config already overrides: `Warning: repo config overrides this field; the new global value will not take effect in this repo.`
  - Setting repo for a field where the global value is already the same: `Note: repo value matches global; no override is active.`

### 6. Command dispatch (`src/commands/mod.rs`)

Add a `Command::Config { action }` arm that dispatches to `config::run(action, runtime)`.

### 7. New module `src/commands/config.rs`

Implement:
- `pub async fn run(action: ConfigAction, runtime: Arc<dyn AgentRuntime>) -> Result<()>`
- `fn show(git_root: Option<&Path>) -> Result<()>`
- `fn get(field: &str, git_root: Option<&Path>) -> Result<()>`
- `fn set(field: &str, value: &str, global: bool, git_root: Option<&Path>) -> Result<()>`
- `struct ConfigFieldDef` with fields for key, scope, hint string, built-in default string, and settability flag.
- `static ALL_FIELDS: &[ConfigFieldDef]` covering all rows in the metadata table above.

### 8. TUI config dialog

A new config show/edit dialog will be added. The config modal dialog is triggered when the user runs `config show` from within the TUI command input.

- When `config show` is entered in the TUI command input, open a large centered modal dialog overlaid on the current view.
- Render the same fields as `amux config show` using a Ratatui `Table` widget inside the dialog.
- Allow column/row navigation with arrow keys.
- When a cell is selected, display a hint line below the table showing the accepted values for that field.
- Press `e` to enter edit mode for the selected field (inline text input); `Esc` to cancel edit without saving; `Enter` to confirm and save.
- Render `[read-only]` in the value cell for `auto_agent_auth_accepted` and skip it in navigation for edit purposes.
- Load both config files when the dialog opens to reflect the current state.
- Press `Esc` (or `Ctrl-C`) to close the dialog and return to the previous view. Press `Ctrl+Enter` to save values to repo and global config.
- Ensure the handling of logic between CLI and TUI for config fetching/editing/etc is common and modular so that all underlying logic is identical, with two different presentation layers. Make it impossible for the field list, possible values, conflict logic, value parsing, etc. to get out of sync between cli and tui.


## Edge Case Considerations:

- **Not in a git repo**: `show` and `get` must succeed — display global fields and note that repo config is unavailable. `set` without `--global` must fail with a clear error directing the user to run inside a git repo or use `--global`.
- **Config files missing**: Treat absent files as all-`None`; show built-in defaults for every field. Never error on missing files during `show` or `get`. On `set`, create the file and its parent directory (`.amux/` or `$HOME/.amux/`) as needed.
- **Invalid field name**: Print a helpful error: `Unknown config field '<name>'. Valid fields: default_agent, runtime, terminal_scrollback_lines, ...`
- **Invalid value for typed fields**: Reject before writing and leave config files unchanged. For enum-typed fields print the list of valid values; for integer fields indicate the expected type.
- **Vec fields — clearing**: An input of `""` (empty string) for `yolo_disallowed_tools` or `env_passthrough` sets the field to an empty `Vec<String>` (not `None`). This matters because an empty repo list actively overrides a non-empty global list. Document this distinction clearly in help text.
- **runtime on non-macOS**: If a user sets `runtime = apple-containers` on Linux or Windows, emit a warning that this value is unsupported on the current platform and will fall back to `docker` at runtime.
- **auto_agent_auth_accepted in set**: Always reject with: `'auto_agent_auth_accepted' is managed by the agent auth flow and cannot be set via 'amux config set'.`
- **Scope mismatch for field write**: Setting `agent` with `--global` or `default_agent` without `--global` must produce a clear error identifying the correct scope.
- **Legacy config migration**: Call `migrate_legacy_repo_config` (from `src/config/mod.rs`) before reading repo config, consistent with every other command that loads repo config.
- **Concurrent writes**: No file locking is present in the current codebase; document this as a known limitation and do not add locking in this work item.
- **JSON field name aliasing**: `yolo_disallowed_tools` and `env_passthrough` use camelCase JSON keys (`yoloDisallowedTools`, `envPassthrough`). The CLI field keys must use snake_case for consistency with other fields. Map between them in `ConfigFieldDef`.


## Test Considerations:

### Unit tests (`src/commands/config.rs`):
- Field lookup by key: known field returns `Some(&ConfigFieldDef)`; unknown field returns `None`.
- Value parsing: valid and invalid inputs for each field type (string enum, usize, bool, comma-separated Vec).
- Scope enforcement: global-only field rejected without `--global`; repo-only field rejected with `--global`.
- Read-only rejection: `auto_agent_auth_accepted` returns an error from `set` regardless of scope flag.
- Override detection: `(global=Some("claude"), repo=None)` → no override; `(global=Some("claude"), repo=Some("codex"))` → override detected; `(global=None, repo=Some("codex"))` → no override (global not set).

### Integration tests:
- `config show` with only a global config file: global fields display set values, repo column shows `(not set)` for all shared fields.
- `config show` with only a repo config file: repo fields display set values, global column shows built-in defaults.
- `config show` with both files set for `terminal_scrollback_lines`: Override column shows `yes` for that field.
- `config show` outside a git repo: exits successfully, prints a note about unavailable repo config, and shows global fields only.
- `config get terminal_scrollback_lines` with global=`10000` (built-in) and repo=`5000`: output shows `Effective: 5000 ← repo overrides global`.
- `config get terminal_scrollback_lines` with neither set: output shows built-in default `10000` for all three lines.
- `config set agent codex` writes `"agent": "codex"` to `.amux/config.json` and subsequent `config get agent` returns `codex`.
- `config set --global default_agent gemini` writes `"default_agent": "gemini"` to `$HOME/.amux/config.json`.
- `config set agent unknown_agent` exits non-zero and does not modify any file.
- `config set auto_agent_auth_accepted true` exits non-zero and does not modify any file.
- `config set --global runtime apple-containers` on Linux emits a platform warning to stderr but still writes the value.
- Warning is printed to stderr when `config set --global default_agent gemini` is run and the repo already sets `agent`.
- `config set env_passthrough ""` sets `envPassthrough` to `[]` (empty array) in JSON, not omitted.

### End-to-end tests:
- Full round-trip: `config set` a field, then `config get` returns the new value, then `config show` reflects it.
- TUI config dialog opens without crashing when both config files are present, when only one is present, and when neither is present.


## Codebase Integration:
- Follow the existing command module pattern: add `ConfigAction` to `src/cli.rs`, add `Command::Config` dispatch in `src/commands/mod.rs`, implement in new `src/commands/config.rs`.
- Reuse `load_repo_config`, `save_repo_config`, `load_global_config`, `save_global_config`, and `migrate_legacy_repo_config` from `src/config/mod.rs` — do not duplicate file I/O.
- Reuse the `effective_*` functions from `src/config/mod.rs` for computing effective values in `show` and `get` rather than reimplementing resolution logic.
- Use `find_git_root()` from `src/commands/init.rs` to locate the git root; treat `None` as the "not in a git repo" case.
- Use `Agent::all()` and `Agent::as_str()` from `src/cli.rs` as the canonical source of valid agent string values for `set` validation.
- For tabular stdout output, use manual column-width formatting (no new crate dependency) consistent with the project's simplicity principle.
- The TUI config dialog should be implemented as a modal overlay (not a tab); reuse the Ratatui `Table`, `Row`, and `Clear` widgets already in the dependency tree.
- All new public functions must have unit tests in `#[cfg(test)]` blocks at the bottom of the file.
