# Work Item: Bug + Architecture

Title: CLI/TUI flag unification — single source of truth for all command flags
Issue: issuelink

## Summary:
- The `--agent` flag for `chat` and `implement` is wired in the CLI but silently ignored in the TUI: `parse_chat_flags()` and `parse_implement_flags()` do not extract it, the `PendingCommand` enum has no `agent` field, and the launch functions always fall back to the config value regardless of what the user typed.
- TUI autocomplete hints for `chat` and `implement` are also missing `--agent`.
- Root cause: every command's flags are defined in **three separate places** — the `clap` struct in `src/cli.rs`, the manual parse functions in `src/tui/mod.rs`, and the hint list in `src/tui/input.rs` — with no structural guarantee that they stay in sync.
- This work item fixes the immediate `--agent` bug and replaces the three-headed flag system with a single `CommandSpec` table that both the CLI and TUI are compiled against. It must be structurally impossible for a new flag added to `cli.rs` to be absent from TUI parsing or autocomplete.
- Both `--flag value` and `--flag=value` forms must be correctly parsed in both the CLI and TUI.

## User Stories

### User Story 1:
As a: user

I want to: type `chat --agent codex` or `implement 0042 --agent=opencode` in the TUI command bar and have the session launch with the correct agent

So I can: override the configured agent per-session from either interface without remembering which surface supports which flags

### User Story 2:
As a: contributor

I want to: add a new flag to a command in exactly one place and have that flag automatically appear in TUI parsing, TUI autocomplete, and the CLI — with a test failure if I forget any of the three

So I can: evolve the CLI surface without risking silent TUI regressions

### User Story 3:
As a: user

I want to: see accurate autocomplete hints when I type a partial command in the TUI (e.g. `chat --` shows `--agent`, `--non-interactive`, etc.)

So I can: discover available flags without consulting external documentation


## Implementation Details:

### 1. `CommandSpec` — canonical flag registry (`src/commands/spec.rs`)

Introduce a new file `src/commands/spec.rs` that is the **sole** definition of flags for every amux subcommand. It must not import from `cli.rs`, `tui/mod.rs`, or `tui/input.rs` — it is a leaf that all three import from.

```rust
pub struct FlagSpec {
    /// Long flag name without leading `--` (e.g. `"agent"`)
    pub name: &'static str,
    /// Whether the flag takes a value argument (e.g. `--agent NAME` vs `--non-interactive`)
    pub takes_value: bool,
    /// Metavar shown in autocomplete hints (e.g. `"NAME"`, `"FILE"`). Empty for boolean flags.
    pub value_name: &'static str,
    /// Short description for autocomplete display.
    pub hint: &'static str,
}

pub struct CommandSpec {
    pub name: &'static str,
    pub flags: &'static [FlagSpec],
}

pub static CHAT_FLAGS: &[FlagSpec] = &[
    FlagSpec { name: "agent",           takes_value: true,  value_name: "NAME", hint: "override configured agent" },
    FlagSpec { name: "non-interactive", takes_value: false, value_name: "",     hint: "run without interactive prompt" },
    FlagSpec { name: "plan",            takes_value: false, value_name: "",     hint: "plan mode" },
    FlagSpec { name: "allow-docker",    takes_value: false, value_name: "",     hint: "allow Docker access" },
    FlagSpec { name: "mount-ssh",       takes_value: false, value_name: "",     hint: "mount SSH agent" },
    FlagSpec { name: "yolo",            takes_value: false, value_name: "",     hint: "skip confirmation prompts" },
    FlagSpec { name: "auto",            takes_value: false, value_name: "",     hint: "auto mode" },
];

pub static IMPLEMENT_FLAGS: &[FlagSpec] = &[
    FlagSpec { name: "agent",           takes_value: true,  value_name: "NAME", hint: "override configured agent" },
    FlagSpec { name: "non-interactive", takes_value: false, value_name: "",     hint: "run without interactive prompt" },
    FlagSpec { name: "plan",            takes_value: false, value_name: "",     hint: "plan mode" },
    FlagSpec { name: "allow-docker",    takes_value: false, value_name: "",     hint: "allow Docker access" },
    FlagSpec { name: "workflow",        takes_value: true,  value_name: "FILE", hint: "workflow file path" },
    FlagSpec { name: "worktree",        takes_value: false, value_name: "",     hint: "use git worktree" },
    FlagSpec { name: "mount-ssh",       takes_value: false, value_name: "",     hint: "mount SSH agent" },
    FlagSpec { name: "yolo",            takes_value: false, value_name: "",     hint: "skip confirmation prompts" },
    FlagSpec { name: "auto",            takes_value: false, value_name: "",     hint: "auto mode" },
];

// Add similar statics for INIT_FLAGS, READY_FLAGS, CONFIG_FLAGS, etc. covering every subcommand.

pub static ALL_COMMANDS: &[CommandSpec] = &[
    CommandSpec { name: "chat",      flags: CHAT_FLAGS      },
    CommandSpec { name: "implement", flags: IMPLEMENT_FLAGS },
    // ... all other subcommands
];
```

`spec.rs` is checked into the crate root (`src/commands/spec.rs`) and re-exported from `src/commands/mod.rs`.

---

### 2. Generic TUI flag parser (`src/tui/flag_parser.rs`)

Replace the ad-hoc `parse_chat_flags()` and `parse_implement_flags()` functions with a single generic parser driven by `CommandSpec`:

```rust
use crate::commands::spec::{CommandSpec, FlagSpec};
use std::collections::HashMap;

/// Parse `parts` (the tokenized TUI command line) against `spec`.
/// Returns a map of flag name → value (empty string for boolean flags).
/// Supports both `--flag value` and `--flag=value` forms.
pub fn parse_flags<'a>(parts: &[&'a str], spec: &CommandSpec) -> HashMap<&'static str, String> {
    let mut result = HashMap::new();
    let mut i = 0;
    while i < parts.len() {
        let token = parts[i];
        if let Some(rest) = token.strip_prefix("--") {
            // Handle --flag=value form
            if let Some((key, val)) = rest.split_once('=') {
                if let Some(fs) = spec.flags.iter().find(|f| f.name == key) {
                    result.insert(fs.name, val.to_string());
                }
            } else {
                // Handle --flag or --flag value form
                if let Some(fs) = spec.flags.iter().find(|f| f.name == rest) {
                    if fs.takes_value {
                        if let Some(val) = parts.get(i + 1) {
                            if !val.starts_with("--") {
                                result.insert(fs.name, val.to_string());
                                i += 1;
                            }
                        }
                    } else {
                        result.insert(fs.name, String::new());
                    }
                }
            }
        }
        i += 1;
    }
    result
}

/// Convenience helpers for extracting typed values from the parse result.
pub fn flag_bool(flags: &HashMap<&str, String>, name: &str) -> bool {
    flags.contains_key(name)
}

pub fn flag_string<'a>(flags: &'a HashMap<&str, String>, name: &str) -> Option<&'a str> {
    flags.get(name).map(|s| s.as_str())
}
```

All existing `parse_*_flags()` functions in `src/tui/mod.rs` are **deleted** and replaced by calls to `parse_flags()` with the corresponding `CommandSpec`.

---

### 3. TUI autocomplete driven by `CommandSpec` (`src/tui/input.rs`)

Replace the manual hint lists in `flag_suggestions_for()` with a function that generates them from `ALL_COMMANDS`:

```rust
pub fn flag_suggestions_for(cmd: &str) -> Vec<String> {
    use crate::commands::spec::ALL_COMMANDS;
    let Some(spec) = ALL_COMMANDS.iter().find(|c| c.name == cmd) else {
        return vec![];
    };
    spec.flags.iter().map(|f| {
        if f.takes_value {
            format!("--{} <{}>  — {}", f.name, f.value_name, f.hint)
        } else {
            format!("--{}  — {}", f.name, f.hint)
        }
    }).collect()
}
```

The full command-level suggestion list (e.g. `"implement <NNNN>  e.g. implement 0001"`) remains handwritten where it describes positional arguments; only the flag portion is generated from `CommandSpec`.

---

### 4. Fix `PendingCommand` enum (`src/tui/state.rs`)

Add `agent: Option<String>` to the `Chat` and `Implement` variants:

```rust
Chat {
    agent: Option<String>,   // ← new
    non_interactive: bool,
    plan: bool,
    allow_docker: bool,
    mount_ssh: bool,
    yolo: bool,
    auto: bool,
},
Implement {
    agent: Option<String>,   // ← new
    work_item: Option<String>,
    non_interactive: bool,
    plan: bool,
    allow_docker: bool,
    workflow: Option<String>,
    worktree: bool,
    mount_ssh: bool,
    yolo: bool,
    auto: bool,
},
```

Update all construction sites (the `"chat"` and `"implement"` match arms in `src/tui/mod.rs`) to populate these fields from the parsed flag map.

---

### 5. Wire agent override through TUI launch functions (`src/tui/mod.rs`)

- `launch_chat()`: accept `agent_override: Option<String>` parameter (extracted from `PendingCommand::Chat`). Pass it to `run_agent_with_sink()` exactly as the CLI does.
- `launch_implement()`: same pattern, passed through to `run_workflow()` / `run_agent_with_sink()`.
- Remove the `config.agent` fallback that currently ignores user-supplied flags. The agent resolution order must match the CLI: CLI flag → config → hardcoded default.

---

### 6. CLI: verify `--flag=value` form works

Clap handles `--flag=value` natively. Add a focused unit test in `src/cli.rs` (or an integration test) that parses both `--agent codex` and `--agent=codex` via `Cli::try_parse_from()` and asserts they produce identical `agent` values. This test acts as a regression guard.

---

### 7. Enforcement tests (`src/commands/spec.rs` and `src/tui/`)

#### Test A — CLI/spec parity (compile-time)

Add a `#[test]` in `src/cli.rs` that enumerates all `clap` `Arg` long names for each subcommand and asserts that `spec::CHAT_FLAGS`, `spec::IMPLEMENT_FLAGS`, etc. contain every name listed. This test fails immediately when a flag is added to `cli.rs` but not to `spec.rs`.

Implementation sketch:
```rust
#[test]
fn chat_cli_flags_match_spec() {
    use clap::CommandFactory;
    let cli_flags: Vec<String> = Cli::command()
        .find_subcommand("chat").unwrap()
        .get_arguments()
        .filter_map(|a| a.get_long())
        .map(str::to_string)
        .collect();
    let spec_flags: Vec<&str> = crate::commands::spec::CHAT_FLAGS.iter()
        .map(|f| f.name)
        .collect();
    for flag in &cli_flags {
        assert!(spec_flags.contains(&flag.as_str()),
            "CLI flag --{flag} not found in CHAT_FLAGS spec — add it to src/commands/spec.rs");
    }
    for flag in &spec_flags {
        assert!(cli_flags.contains(&flag.to_string()),
            "Spec flag --{flag} not found in CLI chat subcommand — add it to src/cli.rs");
    }
}
```

Repeat for every subcommand. Alternatively, use a macro to generate these tests for all entries in `ALL_COMMANDS`.

#### Test B — TUI autocomplete driven by spec (structural guarantee)

Because `flag_suggestions_for()` now reads directly from `ALL_COMMANDS`, there is no separate hint list to drift. No additional test is needed; the derivation IS the guarantee.

#### Test C — TUI parser coverage

For each `CommandSpec` in `ALL_COMMANDS`, assert that `parse_flags()` correctly extracts every flag in both `--flag value` and `--flag=value` forms. This is a pure unit test over the generic parser with no TUI dependency.

---

### 8. `spec.rs` completeness: all existing subcommands

Populate `ALL_COMMANDS` with specs for every amux subcommand that has flags: `init`, `ready`, `config`, `worktree`, `run`, and any others. For subcommands whose TUI parsing already works correctly (e.g. `init` with `parse_agent_flag()`), migrate that parsing to `parse_flags()` as well so the mechanism is uniform across all commands.

---

## Edge Case Considerations:

- **`--flag=value` in TUI**: The current manual parsers only check `parts[i] == "--flag"` and never handle `"--flag=value"` as a single token. The new `parse_flags()` function must handle both forms. Add a specific test case with `"--agent=codex"` as a single token in the parts slice.
- **Unknown flags typed by user**: `parse_flags()` silently ignores unknown flags (matching current behavior). Do not error — the user may be mid-typing.
- **Positional argument collision**: For `implement`, the work item number is a positional arg before the flags. `parse_flags()` ignores tokens that do not start with `--`, so positional args are unaffected. Extract positional args separately, before or after calling `parse_flags()`.
- **Flag value looks like a flag**: `"--workflow --non-interactive"` — the parser must not consume the second token as the value for `--workflow`. The `!val.starts_with("--")` guard in step 2 handles this.
- **`--flag=` (empty value)**: Treat as `Some("")` — valid parse, semantics left to the caller.
- **spec.rs import cycle**: `spec.rs` must not import from `cli.rs` or any TUI module. The CLI test in step 7 imports both `cli.rs` and `spec.rs` — that is fine for a test module, which is not subject to the same cycle rules.
- **Subcommand alias drift**: If clap aliases are used for any flag, the enforcement test must check aliases too.


## Test Considerations:

- **Unit: `parse_flags()` with `CHAT_FLAGS`**: test every flag, both `--flag value` and `--flag=value` forms, unknown flags (ignored), empty parts slice.
- **Unit: `parse_flags()` with value-taking flags**: test that `--workflow myfile.md` captures the value, `--workflow=myfile.md` captures the value, and `--workflow --plan` does NOT capture `--plan` as the workflow value.
- **Unit: `flag_suggestions_for("chat")`**: assert result contains an entry for `--agent`.
- **Unit: `flag_suggestions_for("implement")`**: assert result contains entries for `--agent` and `--workflow`.
- **Unit: CLI/spec parity** (test A above) for every subcommand.
- **Integration: TUI chat with `--agent codex`**: construct a TUI app state, submit `"chat --agent codex"`, assert `PendingCommand::Chat { agent: Some("codex"), .. }` is set.
- **Integration: TUI implement with `--agent=opencode`**: submit `"implement 0042 --agent=opencode"`, assert `PendingCommand::Implement { agent: Some("opencode"), .. }`.
- **Integration: TUI implement with `--workflow` flag**: assert the workflow path is correctly extracted alongside other flags.
- **End-to-end regression**: `amux chat --agent codex` and `amux chat --agent=codex` both launch the codex container (existing CLI tests from work item 0052 cover this).
- **Regression: init `parse_agent_flag()` replacement**: after migrating `init` to `parse_flags()`, the existing `parse_agent_flag` sync test (`src/tui/mod.rs` lines ~4427-4454) must be rewritten against `spec::INIT_FLAGS` and must still pass.


## Codebase Integration:

- `src/commands/spec.rs` is new; add `pub mod spec;` to `src/commands/mod.rs`.
- `src/tui/flag_parser.rs` is new; add `mod flag_parser;` to `src/tui/mod.rs`.
- Delete `parse_chat_flags()`, `parse_implement_flags()`, and `parse_agent_flag()` from `src/tui/mod.rs` once all callers are migrated.
- The `flag_suggestions_for()` function in `src/tui/input.rs` shrinks to a thin wrapper over `spec::ALL_COMMANDS`; the handwritten hint strings for positional argument examples (`"implement <NNNN>  e.g. implement 0001"`) are kept separately and prepended to the generated flag hints.
- All construction sites for `PendingCommand::Chat` and `PendingCommand::Implement` in `src/tui/mod.rs` must be updated; the compiler will enforce this once the enum variants gain new fields.
- The `launch_chat()` and `launch_implement()` function signatures change to accept `agent_override: Option<String>`; update all call sites.
- Follow the `OutputSink` / `AgentRuntime` conventions established in prior work items for any new output or runtime interactions.
- Work item 0052 added `--agent` to the CLI and partial TUI plumbing. This work item completes that plumbing and makes future drift structurally impossible. The two can be implemented sequentially; 0053 depends on 0052 being merged first.
