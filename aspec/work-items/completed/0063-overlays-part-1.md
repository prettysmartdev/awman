# Work Item: Feature

Title: overlays part 1
Issue: issuelink

## Summary:
- Introduce a typed overlay system that allows users to selectively grant additional host resources (starting with directories) to agent containers at runtime.
- Overlays are composed from multiple sources in a defined priority order: global config → project config → `AMUX_OVERLAYS` env var → `--overlay` CLI flags.
- Within the same source, duplicate overlay entries are de-duplicated. Across sources, the highest-priority source wins for the same host path, and permission conflicts default to the lower (more restrictive) permission.
- The design is intentionally extensible: all overlay types share a common trait-based resolution pipeline so future types (secrets, skills, context files, etc.) can be added without touching the core merging logic.


## User Stories

### User Story 1:
As a: user

I want to:
mount a read-only reference directory from my host machine into an agent container using a CLI flag (`--overlay "dir(/data/reference:/mnt/reference:ro)"`)

So I can:
give an agent access to large datasets or shared libraries that live outside the Git repo without permanently modifying any config file.

### User Story 2:
As a: user

I want to:
declare project-level overlay directories in `.amux/config.json` under `"overlays": {"directories": [...]}` so they are applied automatically to every agent launched from that repo

So I can:
standardize the extra mounts needed by the project (e.g. a shared fixtures directory) without having to remember to pass flags every time.

### User Story 3:
As a: user

I want to:
set `AMUX_OVERLAYS` in my shell profile to inject personal overlays (e.g. my personal prompt snippets directory) into every agent session regardless of which repo I am working in

So I can:
keep machine-specific context available to agents without committing it to project config.


## Implementation Details:

### 1. New module: `src/overlays/mod.rs`

Create a new `overlays` module that owns all overlay types, parsing, resolution, and merging logic.

**Core trait:**
```rust
pub trait Overlay: Clone + PartialEq {
    /// A string key that uniquely identifies the "target" of this overlay
    /// (e.g. the host source path for directory overlays).
    /// Used to detect conflicts across sources.
    fn conflict_key(&self) -> String;

    /// Merge two overlays that share the same `conflict_key`.
    /// `self` is higher priority; `other` is lower priority.
    /// Returns the resolved overlay (priority wins on most fields,
    /// lower permission wins on permissions).
    fn merge_with_lower(&self, other: &Self) -> Self;
}
```

**Directory overlay type (`src/overlays/directory.rs`):**
```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirectoryOverlay {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub permission: MountPermission,   // ro (default) | rw
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MountPermission {
    ReadOnly,   // :ro  — default
    ReadWrite,  // :rw
}
```

`DirectoryOverlay::merge_with_lower`:
- `container_path`: self wins (higher priority).
- `permission`: take the MORE restrictive of the two (`ro` beats `rw`). Log a `warn!` if they differ.

`conflict_key` returns the canonical string of `host_path`.

**Resolution order (additive list-merge, not replace):**

Unlike the existing `effective_env_passthrough` pattern (where repo replaces global), overlays are *additive*: all sources contribute entries, then conflicts are resolved. Implement `effective_overlays(git_root: &Path, env_overlays: &[DirectoryOverlay], flag_overlays: &[DirectoryOverlay]) -> Vec<DirectoryOverlay>` in `src/overlays/mod.rs`:

1. Collect `global_config.overlays.directories` → priority 0 (lowest)
2. Append `repo_config.overlays.directories` → priority 1
3. Append `env_overlays` (parsed from `AMUX_OVERLAYS`) → priority 2
4. Append `flag_overlays` (parsed from `--overlay` flags) → priority 3 (highest)

After collecting all entries, deduplicate by `conflict_key`:
- Walk entries in **reverse priority order** (highest first).
- If an entry's `conflict_key` has not been seen yet, keep it as-is.
- If it has been seen, call `high.merge_with_lower(low)` and replace the kept entry.
- Result is the deduplicated, permission-resolved list.

### 2. Config changes: `src/config/mod.rs`

Add `OverlaysConfig` and `DirectoryOverlayConfig` structs:

```rust
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct OverlaysConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directories: Option<Vec<DirectoryOverlayConfig>>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct DirectoryOverlayConfig {
    pub host: String,       // host path (absolute or ~ expanded)
    pub container: String,  // container path (absolute)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,  // "ro" | "rw", defaults to "ro"
}
```

Add `pub overlays: Option<OverlaysConfig>` to both `RepoConfig` and `GlobalConfig` with `#[serde(skip_serializing_if = "Option::is_none")]`.

JSON config example:
```json
{
  "overlays": {
    "directories": [
      { "host": "/data/reference", "container": "/mnt/reference", "permission": "ro" },
      { "host": "~/shared-prompts", "container": "/mnt/prompts" }
    ]
  }
}
```

### 3. Overlay string parser: `src/overlays/parser.rs`

Parse the `--overlay` flag value and `AMUX_OVERLAYS` env var. Both use the same format: a comma-separated list of typed overlay expressions.

**Grammar:**
```
overlay-list   := overlay-expr ("," overlay-expr)*
overlay-expr   := type-tag "(" overlay-args ")"
type-tag       := "dir"   (additional types reserved for future work items)
overlay-args   := host-path ":" container-path [ ":" permission ]
permission     := "ro" | "rw"
```

Examples:
- `dir(/data/ref:/mnt/ref:ro)`
- `dir(/data/ref:/mnt/ref), dir(~/prompts:/mnt/prompts:rw)`

**`AMUX_OVERLAYS` env var** uses the identical format. Parse it at the same callsite as flag parsing (before merging).

The parser should return `anyhow::Result<Vec<TypedOverlay>>` where:
```rust
pub enum TypedOverlay {
    Directory(DirectoryOverlay),
    // Future: Secret(SecretOverlay), Skill(SkillOverlay), …
}
```

Return a descriptive error (including the offending token) for any malformed input.

### 4. CLI changes: `src/cli.rs`

Add `--overlay` to **all four** agent-launching subcommand structs: `Implement`, `Chat`, `ExecAction::Prompt`, and `ExecAction::Workflow`:

```rust
/// Overlay one or more host resources into the agent container.
/// Format: "dir(/host/path:/container/path[:ro|rw])"
/// Accepts a comma-separated list. May be repeated.
/// Example: --overlay "dir(/data/ref:/mnt/ref:ro)"
#[arg(long = "overlay", value_name = "OVERLAY")]
pub overlays: Vec<String>,
```

Using `Vec<String>` with `long = "overlay"` makes clap accept both `--overlay a,b` and repeated `--overlay a --overlay b`. Concatenate and re-parse as a single comma-joined string before passing to the overlay resolution logic.

**Headless mode** (`amux headless start`) re-dispatches commands by spawning a child `amux` process with the original CLI args (see `src/commands/headless/server.rs`). Because `implement`, `chat`, `exec prompt`, and `exec workflow` are all dispatched this way, the `--overlay` flag flows through to headless sessions automatically — no additional headless-specific code is required. The `AMUX_OVERLAYS` env var is similarly inherited by the child process via `std::process::Command`'s default env inheritance.

### 5. Runtime integration: `src/runtime/docker.rs`

Add a new helper:
```rust
fn append_overlay_mounts(args: &mut Vec<String>, overlays: &[DirectoryOverlay]) {
    for overlay in overlays {
        args.push("-v".to_string());
        let perm = match overlay.permission {
            MountPermission::ReadOnly  => "ro",
            MountPermission::ReadWrite => "rw",
        };
        args.push(format!(
            "{}:{}:{}",
            overlay.host_path.display(),
            overlay.container_path.display(),
            perm,
        ));
    }
}
```

Call `append_overlay_mounts` in every `run_container*` variant after the existing `append_settings_mounts` call, before the Docker socket and SSH helpers. The resolved `Vec<DirectoryOverlay>` must be plumbed from the callsite (TUI / headless dispatch) down into `HostSettings` or passed directly to the run functions.

**Option A (preferred):** Add `pub overlays: Vec<DirectoryOverlay>` to `HostSettings` in `src/runtime/mod.rs`. Zero-cost for callers that don't use overlays (default empty vec).

### 6. Callsite wiring

Overlays must be resolved at every container-launch callsite across all three execution modes. The four user-facing commands that launch agent containers are: **`implement`**, **`chat`**, **`exec prompt`**, and **`exec workflow`**. Each runs in one of three modes:

| Mode | Entry path |
|---|---|
| CLI / TUI interactive | `src/tui/mod.rs` event loop |
| CLI non-interactive (`-n`) | `src/commands/implement.rs`, `src/commands/chat.rs`, `src/commands/exec.rs` |
| Headless | `src/commands/headless/server.rs` → child `amux` process (inherits flags and env) |

For CLI and TUI paths, at the point where `HostSettings` is constructed before calling `run_container*`:
1. Parse the raw `--overlay` strings (join all repeated flag values with `,`, then call `parse_overlay_list`) into `flag_overlays: Vec<DirectoryOverlay>`. Log and skip any malformed entries.
2. Parse `std::env::var("AMUX_OVERLAYS").unwrap_or_default()` using the same parser into `env_overlays`.
3. Call `effective_overlays(git_root, &env_overlays, &flag_overlays)` to get the fully merged list.
4. Validate that each resolved `host_path` exists on the host filesystem; log a `warn!` and drop entries that do not exist.
5. Assign the result to `host_settings.overlays`.

Headless mode inherits both flags and env vars automatically through the child process spawn; no additional wiring is needed there.

### 7. `~` expansion

Any `host` path beginning with `~` must be expanded to the user's home directory (`dirs::home_dir()`) before use. Do this in a single utility function in `src/overlays/mod.rs` called from both the config loader and the parser.

Ensure that env var expansion is handled properly (usually by the shell before the values reach amux itself) so that passing `--overlay dir($HOME/something:/mnt/something)` actually resolves to the home dir.

## Edge Case Considerations:

- **Missing host path**: If a configured overlay's host path does not exist at launch time, log a `warn!` and skip the entry rather than failing the launch. This matches the philosophy of optional mounts (SSH, Docker socket).
- **Same host path, different container paths**: Two overlays that map the same host directory to different container paths come from different purposes and should be treated as separate mounts. The `conflict_key` uses host path only; if two sources specify the same host path to different container paths, the higher-priority source's `container_path` wins and a warning is logged.
- **Permission escalation prevention**: When two sources disagree on permissions for the same host path, `:ro` always wins regardless of which source is higher priority. This is intentional: a lower-priority config (e.g. global config) saying `:ro` should prevent a higher-priority flag from silently upgrading to `:rw`. Log a `warn!` whenever permissions are downgraded.
- **Container path conflicts**: Two overlays mapping different host dirs to the same container path would cause Docker to silently shadow one. Detect this and emit a `warn!` (but still proceed; Docker behavior is well-defined for this case).
- **Relative host paths**: Resolve relative paths that do not start with `~` with a standard rust library filepath parser. Relative paths should be resolved relative to the current working directory (i.e. where the CLI was launched, where the TUI tab is bound to, or the remote session workdir, respectively)
- **Empty `AMUX_OVERLAYS`**: Treat as no overlays (do not attempt to parse an empty string).
- **Symlinks**: Resolve symlinks in host paths before using as conflict keys, so that `/foo/bar` and `/foo/baz/../bar` are not treated as different sources.
- **Windows path separators**: The path parser must handle backslashes in host paths on Windows. Use `Path::new()` rather than raw string splitting for path segments.
- **Malformed `--overlay` value**: Return a descriptive error from the parser including the unparseable token; malformed values are fatal errors and should result in a cancelled command rather than a silent failure or warning. 
- **Apple Containers runtime**: `src/runtime/apple.rs` uses a parallel `run_container*` implementation. Overlay mounts must be applied there as well by reading the same `HostSettings.overlays` field. Docker and Apple Container implementations should support overlays equally via their implementation of the container runtime trait (which should be updated to support overlays).


## Test Considerations:

- **`src/overlays/parser.rs` unit tests:**
  - Parse a single `dir(...)` expression with all three fields.
  - Parse a single `dir(...)` expression with default permission (no third field).
  - Parse a comma-separated list of multiple `dir(...)` expressions.
  - Reject missing `:` separator between host and container paths.
  - Reject unknown type tags (e.g. `secret(...)`).
  - Reject malformed permission strings (e.g. `rw2`).
  - Parse paths containing spaces (quoted or percent-encoded — define and document the chosen convention).
  - Empty input returns an empty vec without error.

- **`src/overlays/mod.rs` unit tests (resolution and merging):**
  - Global + project + env + flag sources, no conflicts → all entries present.
  - Same host path in global and flag → flag entry wins on container path; permission merges to lower.
  - Same host path in project and env, both `:rw` → single `:rw` entry (no warning).
  - Same host path in global (`:rw`) and flag (`:ro`) → `:ro` wins; warning logged.
  - Same host path in global (`:ro`) and flag (`:rw`) → `:ro` wins (lower permission); warning logged.
  - Two entries with the same host path but same container path → de-duplicated to one entry.
  - Two entries with the same host path but different container paths → higher-priority container path wins; warning logged.
  - Two entries with the same container path but different host paths → both kept; warning logged about container path collision.

- **`src/config/mod.rs` unit tests:**
  - `RepoConfig` and `GlobalConfig` serialize/deserialize `overlays` field correctly.
  - Missing `overlays` key in JSON deserializes to `None`.
  - `permission` defaults to `"ro"` when not specified.

- **Integration tests (in `tests/`):**
  - All four commands (`implement`, `chat`, `exec prompt`, `exec workflow`) with `--overlay "dir(/tmp/test:/mnt/test:ro)"` produce a `docker run` invocation containing `-v /tmp/test:/mnt/test:ro`.
  - `AMUX_OVERLAYS=dir(/tmp/env:/mnt/env)` env var results in the mount being added for each of the four commands.
  - Flag overlay overrides project config for the same host path.
  - Missing host path in overlay logs a warning and does not appear in the Docker args.
  - Overlay flag is forwarded correctly when commands are dispatched through the headless server (verify the child `amux` process receives the correct `-v` args by inspecting the spawned docker command).

- **Parity tests (CLI ↔ TUI ↔ Headless consistency):**
  - `--overlay "dir(/tmp/ref:/mnt/ref:ro)"` produces identical `-v /tmp/ref:/mnt/ref:ro` Docker args when the command is launched via CLI (`amux implement`), TUI (`implement 42 --overlay "dir(/tmp/ref:/mnt/ref:ro)"`), and headless (via delegated child process). Verify by inspecting the args passed to `DockerRuntime::run_container_pty` / `run_container_text` in each mode.
  - `AMUX_OVERLAYS` env var is respected in both CLI and TUI modes: set it in the test environment and confirm the mount appears in Docker run args for all four agent-launching commands in both modes.
  - A malformed `--overlay` value (`--overlay "notvalid"`) causes a **fatal error** in CLI mode (non-zero exit) and displays an `input_error` in the TUI command bar — the container is never launched in either case.
  - The `overlay` field in `PendingCommand` survives dialog interruptions: when a command is interrupted by a dialog (e.g., `AgentSetupConfirm` because the Dockerfile is missing, or `WorktreePreCommitWarning`), the `overlay` value present when the command was first entered is re-applied when the command resumes after the dialog resolves. Test this for all four commands (`Implement`, `Chat`, `ExecPrompt`, `ExecWorkflow`) and all relevant dialog types.
  - Comma-separated overlays in a single TUI `--overlay` value (`--overlay "dir(/a:/b:ro),dir(/c:/d:rw)"`) produce two separate `-v` mounts — equivalent to passing `--overlay dir(/a:/b:ro) --overlay dir(/c:/d:rw)` on the CLI.

- **End-to-end tests (full container launch simulation):**
  - Run `amux implement 0001 --overlay "dir(/tmp:/mnt/tmp:ro)"` against a test repo and confirm the spawned docker command contains `-v /tmp:/mnt/tmp:ro`.
  - Run `amux chat --overlay "dir(/tmp:/mnt/tmp:rw)"` and confirm `:rw` appears in the docker args.
  - Verify permission downgrade: project config sets `:rw` for `/data`, CLI flag sets `:ro` for `/data` — the resulting Docker mount must be `:ro`.
  - Verify `~` expansion: `--overlay "dir(~/data:/mnt/data:ro)"` expands to the current user's home directory in the `-v` arg.


## Codebase Integration:

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- The new `overlays` module should be declared in `src/lib.rs` (or `src/main.rs`) alongside existing modules.
- Overlay types must derive `Debug`, `Clone`, `PartialEq`, `Serialize`, `Deserialize` to be consistent with existing config types in `src/config/mod.rs`.
- Use `serde(rename = "camelCase")` on public JSON-facing fields to match the existing style (`envPassthrough`, `yoloDisallowedTools`).
- `tracing::warn!` (not `eprintln!`) for all runtime warnings — consistent with the rest of the codebase's logging.
- The `HostSettings` struct lives in `src/runtime/mod.rs:534+`; add `pub overlays: Vec<DirectoryOverlay>` there and update all construction sites to supply an empty vec by default.
- Both `DockerRuntime` and `AppleContainersRuntime` implement `AgentRuntime`; both must call `append_overlay_mounts` so overlays work regardless of the configured runtime.
- The `dirs` crate (for `home_dir()`) is likely already a transitive dependency; confirm via `Cargo.toml` before adding it explicitly.
- Keep the parser in its own file (`src/overlays/parser.rs`) to make it easy to extend with new type tags in future work items without modifying the resolution logic.
