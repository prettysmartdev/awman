# Work Item: Refactor

Title: Unify `ready` subcommand logic into a shared engine

## Summary

The `ready` subcommand is implemented twice: once in `src/commands/ready.rs` (CLI) and once
spread across `src/tui/mod.rs` (TUI). The two implementations share `run_pre_audit()` and
`run_post_audit()`, but diverge in every surrounding step — credential gathering, legacy
migration detection, flag computation, allow-docker socket checking, audit image/entrypoint
selection, host-settings timing, interactive notice timing, and summary accumulation. Each
divergence is a future bug vector.

This work item extracts all business logic into shared functions in `src/commands/ready.rs`
so that CLI and TUI call identical code paths. The only remaining differences between CLI
and TUI will be:

- **User Q&A mechanism** — stdin prompts (CLI) vs dialogs/actions (TUI).
- **Audit container execution** — inherited stdio (CLI) vs PTY session (TUI).

All other logic — detection, migration, flag computation, build sequencing, socket checks,
entrypoint selection, image selection, host-settings application, summary accumulation —
must live in one place and be called identically from both paths.

This work item also fixes the migration bug introduced in work item 0049: after migration,
the project image is not rebuilt from the new minimal `Dockerfile.dev` before the agent image
is built on top of it, so the agent container used for the audit runs inside the old legacy
image.


## User Stories

### User Story 1
As a: maintainer

I want to: make a change to any step of the `ready` flow and be certain it applies equally
to both CLI and TUI

So I can: fix bugs and add features in one place without auditing both code paths.

### User Story 2
As a: user

I want to: the `ready` command to behave identically whether I invoke it from the CLI or TUI

So I can: trust that switching between modes does not produce different outcomes.

### User Story 3
As a: user running `amux ready` after migrating from legacy layout

I want to: the audit agent to run in the new minimal project image (not the old legacy one)

So I can: get a correct `Dockerfile.dev` that reflects only the project's actual dependencies,
not the pre-migration cruft.


## Implementation Details

### Inventory of all current divergences

The following steps in the `ready` flow are currently duplicated or differ between CLI and TUI.
Each item below corresponds to a concrete code change.

---

#### DIV-1 — `effective_build` flag computation (duplicated)

- **CLI** `ready.rs:280`: `let effective_build = if refresh { false } else { build };`
- **TUI** `mod.rs:920`: `let effective_build = if refresh { false } else { build };`

**Fix**: Extract to a `pub fn compute_ready_build_flag(refresh: bool, build: bool) -> bool`
in `ready.rs`. Both CLI and TUI call this function.

---

#### DIV-2 — Legacy layout detection (duplicated inline)

- **CLI** `ready.rs:360–363`: `dockerfile_path.exists() && is_known_agent && !agent_dockerfile_path.exists()`
- **TUI** `mod.rs:930–933`: same condition

**Fix**: Extract to `pub fn is_legacy_layout(git_root: &Path, agent_name: &str) -> bool`
in `ready.rs`. Both use this function.

---

#### DIV-3 — Legacy migration file operations (duplicated inline)

- **CLI** `ready.rs:375–383`: `fs::copy` Dockerfile.dev to .bak, then `fs::write` minimal template
- **TUI** `mod.rs:539–548`: same operations

**Fix**: Extract to `pub fn perform_legacy_migration(git_root: &Path) -> Result<Vec<String>>`
in `ready.rs`. Returns display messages. Both call this function and print the messages in
their respective output mechanism.

---

#### DIV-4 — Migration does not set `build = true` (bug — wrong final state)

After migration, neither CLI nor TUI sets `build = true`, so `run_pre_audit()` finds the old
project image already present and skips rebuilding it. The agent image is then built `FROM`
the old legacy image. The audit runs inside the old environment, defeating the purpose of
migration.

**Fix (two parts)**:

Part A — Fix `run_pre_audit()` `ready.rs:641`:
```rust
// Before:
let needs_build = dockerfile_was_missing || !runtime.image_exists(&image_tag);

// After:
let needs_build = dockerfile_was_missing || opts.build || !runtime.image_exists(&image_tag);
```
This makes `opts.build = true` force a project base image rebuild (currently it only forces
the agent image rebuild at line 699).

Part B — Set `build = true` after migration:
- **CLI** `ready.rs`: make `effective_build` mutable; after `perform_legacy_migration()` succeeds,
  set `effective_build = true`.
- **TUI** `mod.rs:ReadyLegacyMigrate`: after `perform_legacy_migration()` succeeds, set
  `app.active_tab_mut().ready_opts.build = true`.

---

#### DIV-5 — Credential / env-var gathering (different functions, different call sites)

- **CLI** `ready.rs:288–298`: `resolve_auth()` + `effective_env_passthrough()` loop
- **TUI** `mod.rs:1181–1190`: `agent_keychain_credentials()` + `effective_env_passthrough()` loop

These should produce identical results. `resolve_auth()` in CLI mode falls through to
`agent_keychain_credentials()` for keychain-backed agents; the CLI path also handles API-key
auth. The TUI skips the `resolve_auth()` dispatch.

**Fix**: Extract to `pub fn gather_ready_env_vars(git_root: &Path, agent_name: &str) -> Result<Vec<(String, String)>>`
in `ready.rs`. This function calls `resolve_auth()` (which already handles both keychain and
API-key paths) and then appends any additional `effective_env_passthrough()` vars not already
present. Both CLI and TUI call this function.

---

#### DIV-6 — Host settings creation (different call sites)

- **CLI** `ready.rs:300`: created before pre-audit, outside any phase function
- **TUI** `mod.rs:1193`: created inside `launch_ready()` just before spawning pre-audit task

**Fix**: Extract to `pub fn create_ready_host_settings(agent_name: &str) -> Option<crate::runtime::HostSettings>`
in `ready.rs` (a thin wrapper around `passthrough_for_agent(agent_name).prepare_host_settings()`).
Both CLI and TUI call this at the same logical point: before `run_pre_audit()`.

---

#### DIV-7 — `apply_dockerfile_user` timing (different phase)

- **CLI** `ready.rs:414–423`: applied after `run_pre_audit()` returns, before launching audit
- **TUI** `mod.rs:1197–1203`: applied inside `launch_ready()` before spawning pre-audit task

Both paths apply it at the same *logical* point (before the audit runs) but in different
*physical* locations, making the code inconsistent and fragile.

**Fix**: Move the `apply_dockerfile_user` call to a single location: inside `run_pre_audit()`
at the end of the function, after the `ReadyContext` is built. `run_pre_audit()` takes a
mutable reference to `Option<HostSettings>` and mutates it in place. Returns the applied
message (if any) as part of `ReadyContext` so the caller can print it.

Alternatively (simpler): extract
`pub fn apply_ready_user_directive(host_settings: Option<&mut HostSettings>, ctx: &ReadyContext) -> Option<String>`
from `ready.rs` and have both CLI and TUI call it at the same phase boundary (after
`run_pre_audit()` returns, before audit is launched).

Use the second approach (simpler, less intrusive to `run_pre_audit()` signature).
Both CLI and TUI call this in the gap between pre-audit and audit setup.

---

#### DIV-8 — Interactive notice timing (slightly different, logically same)

- **CLI** `ready.rs:427`: in `run()` after `apply_dockerfile_user`, before allow-docker check
- **TUI** `mod.rs:1257–1263`: in `check_ready_continuation(PreAudit)` after pre-audit completes

Logically equivalent (both are before the audit container launches). No behavioral change
needed, but for structural clarity: both should call `print_interactive_notice()` in the same
relative position — after `apply_ready_user_directive()`, before `audit_setup()`.

Both already call the same `print_interactive_notice()` function. No change needed here
beyond ensuring timing consistency with DIV-7.

---

#### DIV-9 — Allow-docker socket check (triplicated)

- **CLI `run()`** `ready.rs:431–442`: checked once in `run()`
- **CLI `run_with_sink()`** `ready.rs:874–886`: checked again in the non-PTY path
- **TUI `launch_ready_audit()`** `mod.rs:1314–1334`: checked for PTY path
- **TUI `launch_ready_audit_captured()`** `mod.rs:1403–1423`: checked again for captured path

**Fix**: Extract to `pub fn check_allow_docker(out: &OutputSink, allow_docker: bool, runtime: &dyn AgentRuntime) -> Result<()>`
in `ready.rs`. Returns `Ok(())` if not allow_docker or socket is found (printing the warning);
returns `Err` if allow_docker and socket not found. All four call sites call this function.

---

#### DIV-10 — Audit image and entrypoint selection (triplicated)

The selection of `audit_image_tag` and `entrypoint` appears in three places:
- **CLI `run()`** `ready.rs:445–453`
- **TUI `launch_ready_audit()`** `mod.rs:1337–1342`
- **TUI `launch_ready_audit_captured()`** `mod.rs:1437–1441`

**Fix**: Extract to `pub struct AuditSetup` and `pub fn build_audit_setup(ctx: &ReadyContext, non_interactive: bool) -> AuditSetup`:

```rust
pub struct AuditSetup {
    pub image_tag: String,
    pub entrypoint: Vec<String>,
}

pub fn build_audit_setup(ctx: &ReadyContext, non_interactive: bool) -> AuditSetup {
    let image_tag = ctx.agent_image_tag.as_deref().unwrap_or(&ctx.image_tag).to_string();
    let entrypoint = if non_interactive {
        audit_entrypoint_non_interactive(&ctx.agent_name)
    } else {
        audit_entrypoint(&ctx.agent_name)
    };
    AuditSetup { image_tag, entrypoint }
}
```

All three call sites call `build_audit_setup()`.

---

#### DIV-11 — Summary accumulation across TUI phases (pre-population hack)

`launch_ready_post_audit()` in TUI (`mod.rs:1479–1484`) creates a fresh `ReadySummary::default()`
and manually pre-populates fields that were already set during `run_pre_audit()`. This is a
workaround for the summary not being passed between phases.

The pre-audit task already sends `(ctx, summary)` via a oneshot channel (`mod.rs:1229`). The
receiver stores only `ctx` in `app.active_tab_mut().ready_ctx` and discards the summary.

**Fix**:

1. Add `ready_summary: Option<ReadySummary>` field to `Tab` in `src/tui/state.rs` (alongside `ready_ctx`).
2. In `check_ready_continuation()`, when receiving `(ctx, summary)` from the channel, store
   both: `app.active_tab_mut().ready_ctx = Some(ctx)` and `app.active_tab_mut().ready_summary = Some(summary)`.
3. In `launch_ready_post_audit()`, retrieve the stored summary and pass it directly to
   `run_post_audit()` instead of constructing a fresh pre-populated one.
4. Clear `ready_summary` when ready phase returns to Inactive.

---

#### DIV-12 — `run_with_sink()` allow-docker check and audit logic (semi-duplication with `run()`)

`run_with_sink()` in `ready.rs` is used by the TUI for the non-refresh path and also contains
allow-docker check + audit logic that partially mirrors `run()`. After DIV-9 and DIV-10 are
fixed (shared `check_allow_docker` and `build_audit_setup`), this duplication is reduced
to an acceptable level. No additional change needed here beyond using the extracted helpers.

---

### Summary of new / changed functions

| Function | Location | Type | Description |
|---|---|---|---|
| `compute_ready_build_flag(refresh, build)` | `ready.rs` | new | Extracted from CLI+TUI flag computation |
| `is_legacy_layout(git_root, agent_name)` | `ready.rs` | new | Extracted from CLI+TUI detection |
| `perform_legacy_migration(git_root)` | `ready.rs` | new | Extracted from CLI+TUI file ops |
| `gather_ready_env_vars(git_root, agent_name)` | `ready.rs` | new | Unified credential + passthrough gathering |
| `create_ready_host_settings(agent_name)` | `ready.rs` | new | Thin wrapper, single call site |
| `apply_ready_user_directive(host_settings, ctx)` | `ready.rs` | new | Extracted apply_dockerfile_user call |
| `check_allow_docker(out, allow_docker, runtime)` | `ready.rs` | new | Extracted from 3 duplicated check blocks |
| `AuditSetup` struct | `ready.rs` | new | Carries image_tag + entrypoint |
| `build_audit_setup(ctx, non_interactive)` | `ready.rs` | new | Extracted image + entrypoint selection |
| `run_pre_audit()` | `ready.rs` | modified | Add `opts.build` to `needs_build` (DIV-4 Part A) |
| `run()` | `ready.rs` | modified | Use all extracted functions; `effective_build` mut |
| `run_with_sink()` | `ready.rs` | modified | Use `check_allow_docker`, `build_audit_setup` |
| `execute_command("ready")` | `mod.rs` | modified | Use `compute_ready_build_flag`, `is_legacy_layout` |
| `Action::ReadyLegacyMigrate` | `mod.rs` | modified | Use `perform_legacy_migration`; set build=true |
| `launch_ready()` | `mod.rs` | modified | Use `gather_ready_env_vars`, `create_ready_host_settings`, `apply_ready_user_directive` |
| `launch_ready_audit()` | `mod.rs` | modified | Use `check_allow_docker`, `build_audit_setup` |
| `launch_ready_audit_captured()` | `mod.rs` | modified | Use `check_allow_docker`, `build_audit_setup` |
| `launch_ready_post_audit()` | `mod.rs` | modified | Use stored `ready_summary` instead of pre-populating |
| `check_ready_continuation()` | `mod.rs` | modified | Store `ready_summary` from channel |
| `Tab` | `state.rs` | modified | Add `ready_summary: Option<ReadySummary>` field |


## Edge Case Considerations

- **`gather_ready_env_vars` on API-key agents**: `resolve_auth()` handles keychain, env-var,
  and file-based auth. The unified function must preserve all branches, not just the keychain
  path currently used by TUI.

- **Migration + `--refresh` together**: After migration, both `refresh` and `build` are true.
  The comment at CLI `ready.rs:279` says "ignore --build when --refresh is set." That comment
  applies to the *user-supplied* `--build` flag, not to the migration-triggered `build = true`.
  Since migration only sets `build = true` programmatically, and the post-audit `rebuild_images()`
  will rebuild everything anyway, the `needs_build` check in `run_pre_audit()` is the only
  place where `build = true` matters for migration correctness (so the pre-audit builds the
  project image from the new minimal Dockerfile.dev before building the agent image on top).
  Document this in the function comment.

- **`apply_ready_user_directive` when agent dockerfile doesn't exist yet**: The first call to
  `run_pre_audit()` may write the agent dockerfile. `apply_ready_user_directive()` is called
  after `run_pre_audit()` returns, so the agent dockerfile will exist by then. No issue.

- **`ready_summary` not set when pre-audit fails**: If `run_pre_audit()` returns an error,
  the channel send never happens and `ready_summary` is never set. `launch_ready_post_audit()`
  must handle `ready_summary = None` gracefully (use `ReadySummary::default()` as fallback).

- **Clearing `ready_summary` on abort**: Add cleanup of `ready_summary` wherever `ready_ctx`
  is cleared in `check_ready_continuation()` and error paths.

- **`compute_ready_build_flag` during migration**: The migration sets `build = true`
  *after* the initial `compute_ready_build_flag()` call. This is intentional — the migration
  overrides the computed value. Keep them separate; do not fold migration into the flag
  computation function.

- **`run_with_sink()` used by integration tests**: This function is also called directly
  in integration tests. Changes to its signature (e.g., using `check_allow_docker`) must
  not break existing test call sites.


## Test Considerations

- **Unit test `compute_ready_build_flag`**:
  - `(refresh=false, build=true)` → `true`
  - `(refresh=true, build=true)` → `false`
  - `(refresh=false, build=false)` → `false`

- **Unit test `is_legacy_layout`**: temp dir with/without `Dockerfile.dev` and
  `.amux/Dockerfile.claude`; known and unknown agent names.

- **Unit test `perform_legacy_migration`**: creates backup file, writes minimal template,
  returns expected messages; errors when source file missing.

- **Unit test `build_audit_setup`**:
  - `non_interactive=false` → `entrypoint` matches `audit_entrypoint()`
  - `non_interactive=true` → `entrypoint` matches `audit_entrypoint_non_interactive()`
  - `agent_image_tag = Some(...)` → `image_tag` uses agent tag
  - `agent_image_tag = None` → `image_tag` falls back to project tag

- **Integration test — migration rebuild**: simulate a repo with legacy Dockerfile.dev and a
  pre-existing project image; confirm that after migration + `run_pre_audit()` with `build=true`,
  the project image is rebuilt (not the cached one).

- **Regression tests**: all existing `ready` tests pass unchanged; no behavioral change for
  users who are not on the migration path.

- **TUI summary continuity test**: confirm that after the full refresh flow, the summary printed
  in post-audit contains correct `docker_daemon`, `dockerfile`, and `dev_image` statuses derived
  from the pre-audit run, not from pre-populated defaults.


## Codebase Integration

- All new functions go in `src/commands/ready.rs` (the engine). `src/tui/mod.rs` is the
  orchestrator and must not contain business logic — only sequencing, I/O routing, and
  state management.
- New functions are `pub` so they are callable from `mod.rs` without reaching into private
  implementation details.
- `AuditSetup` is a plain `pub struct` with public fields — no need for constructors or
  methods; `build_audit_setup()` is the factory.
- The `Tab` struct in `state.rs` gets a new `ready_summary` field initialised to `None` in
  `Tab::default()` (or wherever tabs are constructed).
- `gather_ready_env_vars()` calls `resolve_auth()` which is already imported in `ready.rs`.
  The TUI import of `agent_keychain_credentials` in `mod.rs` becomes unused and should be
  removed.
- Follow all conventions in `aspec/` — idiomatic Rust, no `unwrap()` outside tests,
  `anyhow::Context` for error annotation, streaming output via `OutputSink`.
- After this work item, `src/tui/mod.rs` should contain zero inline Docker/filesystem
  operations related to `ready` — every such operation goes through a function in `ready.rs`.
