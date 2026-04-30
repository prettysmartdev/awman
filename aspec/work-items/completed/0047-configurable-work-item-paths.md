# Work Item: Enhancement

Title: Configurable Work Item Paths
Issue: issuelink

## Summary:
- Work items and their template are currently hardcoded to `aspec/work-items/` and `aspec/work-items/0000-template.md`. Users whose repos don't follow the `aspec/` convention have no way to use `specs new` or `implement`. This work item makes those paths configurable via repo-level config (`work_items.dir` / `work_items.template`), adds graceful degradation when neither config nor `aspec/` exists, and wires up discovery, prompting, `init`, `ready`, `config show/get/set`, and the TUI settings table.

## User Stories

### User Story 1:
As a: user in a repo that doesn't use the `aspec/` directory structure

I want to: configure where amux looks for work items and their template via `amux config set work_items.dir ./docs/work-items`

So I can: use `specs new` and `implement` without needing to adopt the `aspec/` folder layout

### User Story 2:
As a: user who has set a `work_items.dir` but hasn't set a template

I want to: amux to discover any `*template.md` in my work items directory, ask me if I'd like to use it, and save the choice to config if I confirm

So I can: avoid manually re-specifying a template path that's already obvious from my directory layout

### User Story 3:
As a: user running `amux init` in a repo with no `aspec/` folder

I want to: be offered the option to set a custom work items directory and template path interactively during `init`

So I can: get a fully working amux configuration in a single setup step without post-hoc `config set` commands


## Implementation Details:

### 1. Config Struct Changes (`src/config/mod.rs`)

Add a nested `WorkItemsConfig` struct and an optional field on `RepoConfig`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkItemsConfig {
    pub dir: Option<String>,       // relative or absolute path to work items directory
    pub template: Option<String>,  // relative or absolute path to template file
}

pub struct RepoConfig {
    // ... existing fields ...
    pub work_items: Option<WorkItemsConfig>,
}
```

Add helper methods on `RepoConfig` (or a free function):
- `work_items_dir(git_root) -> Option<PathBuf>`: resolves `work_items.dir` relative to `git_root`
- `work_items_template(git_root) -> Option<PathBuf>`: resolves `work_items.template` relative to `git_root`

### 2. Config Field Registry (`src/commands/config.rs`)

Add two new entries to `ALL_FIELDS`:

```rust
ConfigField {
    key: "work_items.dir",
    scope: FieldScope::RepoOnly,
    hint: "Path to the work items directory (relative to repo root)",
    builtin_default: None,
    settable: true,
},
ConfigField {
    key: "work_items.template",
    scope: FieldScope::RepoOnly,
    hint: "Path to the work item template file (relative to repo root)",
    builtin_default: None,
    settable: true,
},
```

Extend `validate_value()`, `apply_to_repo()`, and `get_from_repo()` to handle the nested `work_items.*` keys — extract the sub-key and read/write the appropriate field on `WorkItemsConfig`.

### 3. Work Items Path Resolution Helper (`src/commands/new.rs` or shared utility)

Replace the hardcoded `aspec/work-items/` path with a resolution function used by both `new.rs` and `implement.rs`:

```rust
/// Returns (work_items_dir, template_path_or_none)
pub fn resolve_work_item_paths(
    git_root: &Path,
    repo_config: &RepoConfig,
) -> (PathBuf, Option<PathBuf>)
```

Resolution order for directory:
1. `repo_config.work_items.dir` (resolved relative to `git_root`)
2. `git_root/aspec/work-items/` (fallback if it exists)
3. Return `None`/warn if neither exists

Resolution order for template:
1. `repo_config.work_items.template`
2. `git_root/aspec/work-items/0000-template.md` (legacy path, if it exists)
3. `None` (triggers auto-discovery logic below)

### 4. Template Auto-Discovery (`src/commands/new.rs`)

When `resolve_work_item_paths` returns no template path, scan `work_items_dir` for any file matching `*template.md`:

```rust
fn discover_template(work_items_dir: &Path) -> Option<PathBuf>
```

Returns the first match, or `None` if none found. If `discover_template` finds a candidate, prompt the user: "Found potential template: {path}. Use it? [Y/n]". If confirmed, write `work_items.template = <relative path>` to repo config via `save_repo_config()`.

If the user declines or no template is found, create the new work item with a minimal stub:

```markdown
# {kind}: {title}
```

### 5. `ready` Command Warning (`src/commands/ready.rs`)

In `run_pre_audit()`, after the existing aspec-folder check, add a new check:

```
Check: work_items_config
- Condition: aspec folder is absent AND work_items.dir is not set
- Status: Warn
- Message: "`specs new` and `implement` will not work. Run `amux config set work_items.dir <path>` to configure a work items directory."
```

This check does NOT fail `ready` — it is advisory only (status `Warn`, not `Failed`).

Update `ReadySummary` to include a `work_items_config: CheckStatus` field.

### 6. `init` Command (`src/commands/init.rs`)

At the end of `init` (after existing setup steps), if:
- `--aspec` was NOT passed, AND
- no `aspec/` directory exists, AND
- `work_items.dir` is not already set in repo config

...then offer: "Would you like to configure a work items directory? [y/N]"

If accepted:
- **CLI**: read directory path from stdin, then template path from stdin (allow blank to skip template)
- **TUI**: show two sequential text-input dialogs
- Validate that the entered directory path is not an absolute path outside the repo (security constraint)
- Write `work_items.dir` (and optionally `work_items.template`) to repo config via `save_repo_config()`

Update `InitSummary` to include a `work_items_setup: StepStatus` field.

### 7. `implement` Command (`src/commands/implement.rs`)

Replace the hardcoded `aspec/work-items/` path in `find_work_item()` with a call to `resolve_work_item_paths()`. No other logic changes needed.

### 8. TUI Settings Table

Because the TUI settings table renders dynamically from `ALL_FIELDS` (see `src/tui/render.rs`), adding the two new fields to `ALL_FIELDS` in step 2 is sufficient to include them in the interactive settings table. Verify that the `apply_to_repo()` / `get_from_repo()` paths handle the nested struct correctly so in-TUI editing works end-to-end.


## Edge Case Considerations:

- **Relative vs. absolute paths**: `work_items.dir` and `work_items.template` may be either relative to git root or absolute. Resolve them with `if path.is_absolute() { path } else { git_root.join(path) }`. Reject paths that escape the git root (security constraint per `aspec/architecture/security.md`).
- **`work_items.dir` exists but points to a non-directory**: treat as missing; emit a clear error: "Configured work_items.dir '{path}' is not a directory."
- **`work_items.template` set but file missing**: emit a clear warning at `specs new` time: "Configured template '{path}' not found, falling back to auto-discovery."
- **Multiple `*template.md` files in work_items.dir**: auto-discovery returns the lexicographically first match and notes how many were found so the user can pick manually if needed.
- **`work_items.dir` not set and `aspec/work-items/` doesn't exist**: `specs new` and `implement` should fail with a helpful error message (matching the `ready` warning text), not a panic or cryptic I/O error.
- **Legacy migration**: repos already using `aspec/work-items/` continue working without any config change — the resolution logic falls back to the legacy path transparently.
- **Global config**: `work_items.dir` and `work_items.template` are `RepoOnly` — do not allow `amux config set --global work_items.dir`; emit "work_items.dir is repo-scoped and cannot be set globally."
- **`init` re-run**: if `work_items.dir` is already configured, skip the offer silently during `init`.
- **Empty string value in config**: treat an empty string as "not set" when resolving paths.


## Test Considerations:

- **Unit — `resolve_work_item_paths()`**: test with (a) only config set, (b) only legacy `aspec/work-items/` present, (c) both present (config wins), (d) neither present.
- **Unit — `discover_template()`**: test directory with no `*template.md`, one match, and multiple matches.
- **Unit — config field serialization**: round-trip `RepoConfig` with `work_items: Some(WorkItemsConfig { dir: Some("./items"), template: None })` through JSON.
- **Unit — `validate_value()` / `apply_to_repo()` / `get_from_repo()`**: for `work_items.dir` and `work_items.template` keys, verify correct read/write of the nested struct.
- **Integration — `specs new` with configured dir**: create a temp git repo, set `work_items.dir` in config, run `specs new`, assert file created in correct directory.
- **Integration — `specs new` template auto-discovery**: place a `my-template.md` file in `work_items.dir`, run `specs new`, simulate user confirming prompt, assert template content used and config updated.
- **Integration — `specs new` no template no aspec**: run without aspec and without template config, simulate declining discovery prompt, assert minimal stub created.
- **Integration — `ready` warning**: run `ready` check in a repo without `aspec/` and without `work_items.dir`; assert warning appears in output.
- **Integration — `init` work items offer**: run `init` without `--aspec` in a repo without `aspec/`; simulate user entering a directory path; assert config written correctly.
- **Integration — `config set/get work_items.dir`**: set and retrieve the value; assert round-trip fidelity.
- **Integration — `config set --global work_items.dir`**: assert error emitted.
- **E2E — path escape rejection**: attempt `amux config set work_items.dir ../../outside`; assert rejection.


## Codebase Integration:

- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- The nested `WorkItemsConfig` serialization pattern should mirror any existing nested config structs in `src/config/mod.rs`; if none exist yet, use `#[serde(default)]` and `Option<WorkItemsConfig>` to preserve backwards-compatibility with existing config files.
- `resolve_work_item_paths()` should live close to `find_template()` and `next_work_item_number()` in `src/commands/new.rs`, or in a shared `src/commands/work_items.rs` module if the function is also imported by `implement.rs`.
- The `ready` warning for missing work items config should be styled consistently with the existing aspec-folder warning in `src/commands/ready.rs` — same status enum variant and output formatting.
- The `init` interactive prompts (CLI stdin path) should reuse whatever `prompt_*` helpers already exist in `src/commands/new.rs` or `src/tui/` rather than introducing new I/O patterns.
- Security: path validation (no escaping git root) must be applied whenever a user-supplied path is resolved — do not skip this for the `init` flow even though it is interactive setup.
