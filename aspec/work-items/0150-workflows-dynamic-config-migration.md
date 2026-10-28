# Work Item: Task

Title: Move `dynamicWorkflows` config to `workflows.dynamic`, allow it in global config, and migrate existing files
Issue: issuelink

Related: 0149 (introduces the top-level `workflows` block and reserves `workflows.dynamic`).

> **To the leader implementing this work item:** read `aspec/architecture/2026-grand-architecture.md` and `aspec/architecture/design.md` before starting. The layer placement below follows them. Where something here seems to conflict with those documents, ask the developer rather than choosing yourself.

## Summary:
- WI 0149 adds a top-level `workflows` object for workflow-wide settings, starting with `workflows.prerequisites`. The dynamic-workflow settings are the only other workflow-wide block, and they still live at the top-level `dynamicWorkflows` key. Users would have to look in two places.
- Move them to `workflows.dynamic`, with the same four fields: `agentsToModels`, `maxConcurrentSteps`, `defaultLeader` and `guidance`. Keep reading the old key as a deprecated alias, and migrate files to the new location automatically, with safeguards for a file that is checked into git.
- **Allow `workflows.dynamic` in the global config too.** Users can then set a personal default leader, an agent/model allowlist, a step cap and standing guidance once, for every repo. A repo's own block layers on top, following the rules in §2.
- **What the code does today:**
  - `DynamicWorkflowsConfig` (`src/data/config/repo.rs:220`) is a field on `RepoConfig` only. Every field is `FieldScope::RepoOnly` (`src/data/config/fields.rs:274`).
  - `RepoConfig::save` serialises the typed struct (`to_string_pretty`), and `awman config set` and the TUI config dialog both save through it (`command/commands/config.rs::write_config_field`).
  - The legacy `envPassthrough` key already shows the alias pattern this item reuses: `#[serde(rename = "envPassthrough", default, skip_serializing)]` plus a warning.
- **Why not rewrite every repo config silently on load:** `.awman/config.json` is version-controlled and shared. A silent rewrite leaves an unexpected uncommitted diff. Worse, once that diff is committed, teammates on an awman version older than this item would silently lose their dynamic-workflow settings. The migration is therefore automatic only where awman is already writing the file, or when the user explicitly asks.

## User Stories

### User Story 1:
As a: developer with an existing `.awman/config.json` that has a `dynamicWorkflows` block

I want to:
keep my settings working unchanged after upgrading, see a clear one-line notice that the key moved, and move it with one command (`awman config migrate`) when my team is ready

So I can:
adopt the new layout without breaking teammates who haven't upgraded yet

### User Story 2:
As a: developer who works across many repositories

I want to:
set `workflows.dynamic.defaultLeader` and some standing `guidance` once in `~/.awman/config.json`

So I can:
get my preferred leader and rules in every repo's dynamic workflows, while each repo can still pin its own leader and agent allowlist and add its own guidance

### User Story 3:
As a: developer editing config through `awman config set [--global]` or the TUI config dialog

I want to:
have awman write `workflows.dynamic.*` and move any old `dynamicWorkflows` values into it as part of that same save

So I can:
end up with one consistent layout without a separate migration step

## Implementation Details:

### Layer placement
| Layer | Items |
|---|---|
| 0 data | `WorkflowsConfig.dynamic: Option<DynamicWorkflowsConfig>` on both `RepoConfig` and `GlobalConfig`. The deprecated alias field on `RepoConfig`. **`EffectiveConfig::dynamic_workflows() -> Result<ResolvedDynamicWorkflows, DataError>`**, which applies all the layering in §2, as every other config merge does. The `ConfigMigration` steps and the `RepoConfigMigration` value. The field-registry changes (scopes, aliases, `ConfigRowShape`) |
| 2 command | Dockerfile-availability filtering of the resolved agent map (it needs filesystem discovery, as today, so it stays in `workflow_preflight.rs`). The `config migrate` subcommand and its catalogue entry. The deprecation `UserMessage`. The `config set` alias mapping. The `MigrateConfigFrontend` confirmation prompt (`Prompt<D>` in `src/command/prompts.rs`) |
| 3 frontend | rendering only. The TUI and CLI config views stop matching on `dynamicWorkflows.*` field-name strings (§5) |

### 1. Schema
- `WorkflowsConfig` (`src/data/config/workflows.rs`, created by 0149, or by this item if it lands first) gains `dynamic: Option<DynamicWorkflowsConfig>`. Its serde name is `dynamic` and the JSON path is `workflows.dynamic`. It is read from **both** `~/.awman/config.json` and `.awman/config.json`.
- `DynamicWorkflowsConfig` keeps its fields, the per-scope validation run at load (`GUIDANCE_MAX_ENTRIES`, `GUIDANCE_MAX_ENTRY_LEN`, `maxConcurrentSteps >= 1`, leader-spec checks, empty-model and case-fold-collision checks within one map) and its forward-compatible ignore-unknown-keys behaviour. A global block is validated by `GlobalConfig::load` with the same rules. An invalid global block is a load error that names `~/.awman/config.json` and the key.
- `RepoConfig` keeps `#[serde(rename = "dynamicWorkflows", default, skip_serializing)] legacy_dynamic_workflows: Option<DynamicWorkflowsConfig>`, following the `envPassthrough` precedent.
- **Global config never had a `dynamicWorkflows` block.** It was repo-only and never read from global config. A `dynamicWorkflows` key found in global config is **not** migrated or honoured: it was never in effect, so honouring it now would suddenly activate stale settings. Instead it gets a load warning: "`dynamicWorkflows` in ~/.awman/config.json was never read; move what you want to `workflows.dynamic`". `config migrate --global` doesn't touch it.
- Within the **repo** config, the two locations resolve as follows:
  - only `workflows.dynamic` set: use it;
  - only `dynamicWorkflows` set: use it, and mark the config `uses_legacy_dynamic_workflows`;
  - **both set**: a load error naming both keys, with the hint "remove one, or run `awman config migrate` after deleting the copy you don't want". awman never guesses which wins or merges them field by field.
- Squad task `config.json` files read only their `squad` block and are unaffected.

### 2. Global and repo layering (`EffectiveConfig::dynamic_workflows()`)
The order is flags > repo > global > built-in default. A field the repo leaves unset falls through to global. The per-field rules:

| Field | Rule |
|---|---|
| `defaultLeader` | `--leader` > repo > global > built-in leader resolution (docs/06 "Leader resolution order" gains the global step) |
| `maxConcurrentSteps` | repo > global > unset |
| `agentsToModels` | **A non-empty repo map replaces the global map entirely** (developer decision). This keeps a repo's allowlist of vetted agents and models from ever being widened by global entries. Following today's semantics, a repo map that is absent or `{}` counts as unset and falls through to the global map. If neither scope sets one, the leader falls back to Dockerfile discovery, as today |
| `guidance` | **Concatenated** (developer decision): global entries first, then repo entries, in one "Developer Guidance" block. An entry identical (after trimming) to an earlier one is dropped. Each scope is still validated at load (at most 50 entries of at most 1,000 chars each). The **combined** list must also be at most 50. If it is longer, resolution fails before the leader starts, naming both counts: "workflows.dynamic.guidance: 30 global + 25 repo entries exceeds the limit of 50". Never silently truncated. A repo `[]` adds nothing and does not suppress the global entries |

- `ResolvedDynamicWorkflows` records the **source** of each field (`Global`, `Repo`, `LegacyRepo`, `Flag`). It is used for the deprecation notice, for `config show`'s effective column and for error messages, which always name the key and file the user actually wrote, for example `dynamicWorkflows.maxConcurrentSteps in .awman/config.json`.
- **Dockerfile check for a global `agentsToModels`:** global config spans repos, so a global entry naming an agent this repo has no `.awman/Dockerfile.<agent>` for is **skipped with a warning** ("global workflows.dynamic.agentsToModels: skipping 'gemini' — no Dockerfile in this repo"), not treated as a hard error. If every global entry is skipped, the leader falls back to Dockerfile discovery, with a warning. A **repo** map keeps today's hard error for a missing Dockerfile, because the repo author vetted that list for this repo.
- Leader prompt text is unchanged (`src/assets/dynamic/leader-prompt.md` does not name the config source). A legacy-only repo with no global block produces a byte-identical prompt.
- **Applies everywhere a dynamic workflow resolves its config:** CLI, TUI, API sessions (local and remote, with the session's repo config layered over global) and worktree runs (the invoking checkout's repo config over global).
- **Squad is unchanged by this item.** Squad evaluations keep using the `squad` block (`agentsToModels`, `defaultLeader`, `guidance`), exactly as today, and do not read global `workflows.dynamic`. Unifying the two is a possible later work item for the developer to decide on.

### 3. Deprecation notice
- When the repo config uses `dynamicWorkflows`, the command layer emits **one** `MessageLevel::Warning` per process per repo:

  > `.awman/config.json` uses `dynamicWorkflows`, which has moved to `workflows.dynamic`. It still works. Run `awman config migrate` to move it.

  The CLI prints it to stderr, the TUI shows it as a toast, and the API carries it as a `StatusMessage`. It goes out once, from the command layer, not from each frontend.
- Squad and API daemons warn once at the first command for that repo, not on every scheduler tick.

### 4. Automatic migration (repo config)
The new location is written, and the old key removed, in exactly these cases.
- **Any awman write to the repo config.** That means `awman config set` with any repo field, the TUI config dialog's save, and any other `RepoConfig::save` caller.
  - Because the alias field is `skip_serializing`, the move happens as part of that write.
  - Before `save`, the command layer copies the legacy value into `workflows.dynamic` if that field is empty.
  - The success message adds "also moved `dynamicWorkflows` → `workflows.dynamic`" so the extra diff is never a surprise.
  - Writing the global config never touches the repo file, and the reverse.
- **`awman config migrate`**, a new subcommand in the catalogue, with the same visibility as `config set`.
  - It loads the repo config, builds a `RepoConfigMigration`, shows the before/after JSON of the affected keys and the file path, and asks a `Prompt<MigrateChoice>`: `Move it` / `Cancel` (default on dismiss).
  - The body notes: "teammates on awman versions before <this release> will stop seeing these settings once this change is committed."
  - `--yes` skips the prompt. With `--non-interactive` and no `--yes`, it refuses with exit 2, per aspec/uxui/cli.md.
  - `--dry-run` prints the change and writes nothing.
  - With nothing to migrate, it prints "nothing to migrate" and exits 0.
  - `--global` runs the global-scope steps. This item adds none, so it reports "nothing to migrate" and points at the never-read `dynamicWorkflows` warning if that key is present.
- **Both locations set** in the repo config: `config migrate` and every repo write refuse, with the same error as at load (§1).
- **Never:** on plain load or on read-only commands. The file on disk is untouched until one of the writes above happens.
- **Writes are atomic** (temp file + rename in the same directory) and keep the file mode. Existing `save` behaviour for top-level keys awman doesn't model is unchanged by this item. The migration must not drop anything that `save` keeps today; there is a test for that.
- `config migrate` is designed to hold future config moves. It runs a list of typed `ConfigMigration` steps owned by Layer 0, each tagged with the scope it applies to. This item adds one repo-scope step, so the next rename needs no new command.

### 5. Field names and scopes (`config set` / `config get` / `config show`)
- `fields.rs`: rename the four entries to `workflows.dynamic.defaultLeader`, `workflows.dynamic.maxConcurrentSteps`, `workflows.dynamic.agentsToModels` and `workflows.dynamic.guidance`, plus the per-agent and per-index rows under them. Change their scope from `RepoOnly` to **`Both`**, so `awman config set --global workflows.dynamic.defaultLeader claude::claude-opus-4-8` works. Per-agent and per-index rows work in either scope.
- **Old names stay valid inputs**, through an alias table in the field registry (`ConfigFieldSpec.aliases`). They are valid in the repo scope only, matching where `dynamicWorkflows` ever existed. For example, `awman config set dynamicWorkflows.guidance.0 "…"` writes `workflows.dynamic.guidance.0`, which migrates the file via §4, and prints "`dynamicWorkflows.*` is now `workflows.dynamic.*`". `--global` with an old name is a usage error that suggests the new name. `config get` accepts either name. `config show` lists only the new names.
- `config show` shows global, repo and effective values for each row, as it does for other `Both` fields. For `agentsToModels`, the effective column states "repo map (replaces global)" or "global map". For `guidance`, the effective summary row shows "N global + M repo entries".
- **Frontend clean-up, required by the grand architecture:** the TUI and CLI currently match on literal `dynamicWorkflows.*` strings to decide row behaviour. Examples:
  - `frontend/tui/app.rs:1189`;
  - `dialog_router.rs:281`;
  - `key_handler/dialog.rs:110`;
  - `render/dialog/input.rs:389`, `:626` and `:632`;
  - `frontend/cli/per_command/render.rs`.

  Those decisions are map rows vs list rows, Ctrl+N to append, and an empty value meaning remove. Move them into the field registry as data: a `ConfigRowShape { Scalar, MapHeader { child_prefix }, ListHeader { child_prefix }, MapEntry, ListEntry }` on each config row the command layer hands to frontends. Frontends then branch on the shape, never on a field name, so this rename, the new global rows and any future rename touch no frontend code. `config.rs`'s `AGENTS_TO_MODELS_PREFIX` / `GUIDANCE_PREFIX` constants become registry data too. The TUI config dialog's global view gets the same map and list editors as the repo view.
- User-facing error and hint strings that name the key are updated, for example the `agentsToModels references agents that have no Dockerfile` error in `workflow_preflight.rs`, and the `config.rs:249` / `:259` hints.

### 6. Removal of the alias
This item only deprecates. Removing the `dynamicWorkflows` alias, and turning it into a load error with a migration hint, is a separate future work item whose release the developer will choose. Do not add a removal date or version here.

## Edge Case Considerations:
- **Both repo keys present with identical content:** this is still the "both set" error. Equal content today doesn't stop the copies drifting later, and the fix is a one-line delete.
- **Repo `dynamicWorkflows: {}` or `workflows.dynamic: {}` (empty object):** every field is unset, so global values apply. A legacy `{}` migrates to `workflows.dynamic: {}`.
- **`dynamicWorkflows: null`:** treated as absent. Nothing to migrate, and the key is dropped on the next save.
- **Invalid legacy block** (for example `maxConcurrentSteps: 0`): the same validation error as today, naming the key the user actually wrote (`dynamicWorkflows.maxConcurrentSteps`), so the message matches their file.
- **Invalid global block:** a load error for global config. It names `~/.awman/config.json` and fails the same commands an invalid global config fails today. It is reported in `config show` so it can be fixed with `config set --global`.
- **Repo wants Dockerfile discovery even though global sets `agentsToModels`:** not supported in this item, because a repo `{}` falls through to global (developer decision). The repo sets its own explicit map instead. Docs say so.
- **Global `defaultLeader` names an agent this repo has no Dockerfile for:** leader resolution treats it like an unavailable repo leader today, failing with the existing message, which now names the global key and file. `--leader` overrides it.
- **Guidance combined over 50 only after a repo adds entries:** the error names both scopes' counts and suggests trimming either. Deduplicated entries don't count.
- **Worktree runs:** a workflow worktree keeps the invoking checkout's repo config (`Session::open_worktree_for`), layered over global. Migration writes only to the invoking checkout's `.awman/config.json`, never to the worktree's copy.
- **A repo config that is a symlink, or read-only:** `config migrate` refuses with the path and reason. It never follows a symlink out of the repo, consistent with `agent_dirs::publish`'s treatment of repo-controlled symlinks.
- **Older awman reading a migrated file:** `workflows.dynamic` is an unknown key to it and is ignored, so its dynamic-workflow settings silently vanish. This is the reason the migration is never silent, and the reason the prompt body warns about it. A global `workflows.dynamic` is likewise ignored by older versions. Note both in the release notes.
- **API sessions and squad tasks:** these read config but never write it. They get resolution and the one-time warning, never a migration. Squad ignores `workflows.dynamic` (§2).

## Test Considerations:
- **Unit (Layer 0), repo resolution:** new only, legacy only, both (error naming both keys), empty object, null, and an invalid legacy value (error names the legacy key).
- **Unit (Layer 0), layering, one test per row of the §2 table:**
  - `defaultLeader`: flag > repo > global.
  - `maxConcurrentSteps`: fall-through.
  - `agentsToModels`: a non-empty repo map replaces global (a global-only agent is absent from the result); a repo `{}` falls through to global.
  - `guidance`: concatenation order, dedup, and the combined-cap error with counts.
  - Source attribution for every field.
- **Unit (Layer 0), global and save:**
  - A global `dynamicWorkflows` key gives the warning and is not honoured.
  - An invalid global `workflows.dynamic` is a load error naming the file.
  - `save` after loading a legacy-only repo file writes `workflows.dynamic`, has no `dynamicWorkflows`, keeps every other key `save` kept before, and is byte-identical in the parts unrelated to the move.
  - The `ConfigMigration` step is idempotent.
  - Alias table: every `dynamicWorkflows.*` name maps to its `workflows.dynamic.*` name (repo scope only).
- **Command layer:**
  - Global `agentsToModels` entries without a Dockerfile are skipped with a warning; all skipped falls back to discovery; a repo entry without a Dockerfile is still a hard error.
  - The deprecation warning is emitted once per process per repo.
  - `config migrate` covers prompt accepted, cancelled (file unchanged, byte-for-byte), `--yes`, `--dry-run` (no write), `--non-interactive` without `--yes` (exit 2), nothing to migrate (exit 0), both keys set (refused) and `--global` (nothing to migrate).
  - `config set --global workflows.dynamic.guidance.0 …` writes global config only.
  - `config set --global dynamicWorkflows.…` is a usage error.
  - `config set` through an old name writes the new repo location and reports the move.
- **Frontend parity:**
  - The TUI and CLI config views render map, list and scalar rows from `ConfigRowShape` alone, in both the global and repo views.
  - A test asserts no frontend source file contains the literal `dynamicWorkflows` or `workflows.dynamic` (a grep-style test, as other catalogue-parity tests do).
  - Existing TUI render tests for the agent/model and guidance editors pass with the new names, plus new ones for the global view.
- **E2E:**
  - A legacy-only repo with no global block runs `exec workflow --dynamic` with a byte-identical leader prompt (guidance block and concurrency note).
  - A global block with no repo block applies the global leader and guidance.
  - Global plus repo produces the expected merged guidance block and the repo's agent map.
  - `awman config migrate --yes`, then the same run produces the identical prompt.
  - An older-format repo file plus `config set work_items.dir x` ends up migrated.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec, especially `aspec/architecture/2026-grand-architecture.md` and `aspec/uxui/cli.md`.
- **Data:**
  - `src/data/config/repo.rs`: `DynamicWorkflowsConfig`, the alias field.
  - `src/data/config/global.rs`: load and validate `workflows.dynamic`; the never-read `dynamicWorkflows` warning.
  - `src/data/config/workflows.rs`: `WorkflowsConfig.dynamic`.
  - `src/data/config/effective.rs`: `dynamic_workflows()`, `ResolvedDynamicWorkflows`.
  - `src/data/config/fields.rs`: renames, `Both` scope, `aliases`, `ConfigRowShape`.
  - `src/data/config/config_json.rs`: dot-path handling for the new prefix.
- **Readers** switch from `RepoConfig.dynamic_workflows` to `EffectiveConfig::dynamic_workflows()`: `src/command/commands/exec_workflow/dynamic.rs`, `src/command/commands/workflow_preflight.rs` (plus the Dockerfile filtering split per §2), `src/data/config/leader_spec.rs`, `src/data/dynamic_workflow_assets.rs` and `src/command/commands/config.rs`. No code reads either raw field directly.
- **Frontends:** remove the field-name matching listed in §5, from `src/frontend/tui/{app.rs, dialog_router.rs, key_handler/dialog.rs, render/dialog/input.rs, dialogs/mod.rs}` and `src/frontend/cli/per_command/render.rs`.
- Reuse the `envPassthrough` alias pattern (`repo.rs:480`) and its tests as the template. Reuse `HeadlessDefaults` / `Prompt<D>` for the migrate confirmation. Follow the existing `Both`-scope layering in `EffectiveConfig` (for example `maxConcurrentAgents`).
- Add the `config migrate` catalogue entry and its flags (`--yes`, `--dry-run`, `--global`) with visibility data, so CLI, TUI and API behave identically.
- Mention the change in the next release notes, including the older-awman caveat.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.

Specifically:
- `docs/06-dynamic-workflows.md`:
  - every `dynamicWorkflows` example and field name becomes `workflows.dynamic`;
  - a new "Global and repo settings" subsection explaining the §2 rules (repo agent map replaces global, guidance concatenates, the combined limit, and global agents skipped when a repo lacks their Dockerfile);
  - the leader resolution order gains the global step;
  - a short "Upgrading from `dynamicWorkflows`" note on `awman config migrate` and the older-version caveat.
- `docs/07-configuration.md`: the field reference (now global and repo), the `workflows` block, and `awman config migrate`.
- `docs/02-using-the-tui.md`: the config dialog field names and their anchors, and editing these maps and lists in the global view.
- `docs/05-workflows.md`: the one reference.
- `docs/14-command-reference.md`: `config migrate`.
