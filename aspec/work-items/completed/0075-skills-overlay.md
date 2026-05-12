# Work Item: Feature

Title: skills overlay
Issue: issuelink

## Summary:
- The current overlay system supports `dir()` overlays that mount host directories and files into agent containers, configurable via `--overlay` flag, `AMUX_OVERLAYS` env var, or config files. This work item adds a new `skill()` overlay type that mounts the global amux skills directory (`~/.amux/skills/`) into the correct agent-specific location inside the container, determined by which agent is running. The overlay type requires no path arguments — source and destination are always resolved automatically from the session context.

## User Stories

### User Story 1:
As a: user

I want to:
add `--overlay "skill()"` to any agent launch command (or set it in my config once) and have my global amux skills automatically available inside the container as the agent's native slash commands

So I can:
use custom skills I've built with `amux new skill` in every agent session without manually wiring up directory paths or knowing where each agent stores its commands.

### User Story 2:
As a: user

I want to:
declare `"overlays": {"skills": true}` in my `.amux/config.json` so that skills are always injected for every session in a given repo

So I can:
share a consistent set of team skills across all developers working on the same project, without requiring each person to set shell environment variables or remember to pass flags.

### User Story 3:
As a: user

I want to:
enable skills overlay via `AMUX_OVERLAYS="skill()"` in my shell profile so it applies globally regardless of which repo or agent I launch

So I can:
maintain a personal library of skills that are always present in every agent container I run.


## Implementation Details:

### 1. Per-agent container skills paths

When a skill overlay is applied, `~/.amux/skills/` on the host is mounted read-only to the following container path, replacing `{container_home}` with the resolved container home (detected via `detect_container_home`, defaulting to `/root`):

| Agent | Container target path | Notes |
|---|---|---|
| `claude` | `{container_home}/.claude/commands/` | Claude Code traverses subdirectories; each `<skill-name>/SKILL.md` appears as a namespaced slash command |
| `codex` | `{container_home}/.codex/skills/` | Codex recognizes subdirectories containing `SKILL.md` files; matches amux format directly |
| `opencode` | `{container_home}/.config/opencode/commands/` | OpenCode scans its `commands/` directory for `.md` files |
| `gemini` | `{container_home}/.gemini/commands/` | Gemini CLI custom commands directory; files are scanned at launch |
| `copilot` | `{container_home}/.copilot/instructions/` | Copilot reads `.md` instruction files from this directory |
| `crush` | `{container_home}/.config/crush/commands/` | Custom commands directory; feature is actively developed |
| `cline` | `{container_home}/.cline/skills/` | Cline's skills format matches amux format exactly (`<name>/SKILL.md`) |
| `maki` | *(skip — no known skills directory)* | Log `warn!` and produce no mount; do not fail the launch |

All skill overlays are mounted **read-only** (`:ro`). Skills are a host-side resource and must never be modified by the agent.

### 2. New `TypedOverlay` enum in `src/command/commands/mod.rs`

The current `parse_overlay_list` returns `Vec<DirectorySpec>`, which cannot represent a `skill()` entry (which has no paths). Introduce a `TypedOverlay` enum:

```rust
pub enum TypedOverlay {
    Directory(DirectorySpec),
    Skill,
}
```

Update `parse_overlay_list` to return `Vec<TypedOverlay>`. Update `parse_single_typed_overlay` to handle the `"skill"` tag:

```rust
"skill" => {
    if !args.is_empty() {
        return Err(format!(
            "'skill()' takes no arguments, got '{args}' in '{expr}'"
        ));
    }
    Ok(TypedOverlay::Skill)
}
```

Update the error message for unknown type tags to include `"skill"` in the list of supported types.

### 3. Config schema changes

**`src/data/config/repo.rs` / `src/data/config/global.rs`** — extend `OverlaysConfig`:

```rust
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct OverlaysConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directories: Option<Vec<DirectoryOverlayConfig>>,
    /// When true, mount the global amux skills dir into the agent container.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<bool>,
}
```

JSON example:
```json
{
  "overlays": {
    "skills": true,
    "directories": [
      { "host": "/data/reference", "container": "/mnt/reference", "permission": "ro" }
    ]
  }
}
```

### 4. `OverlayRequest` extension in `src/engine/overlay/mod.rs`

Add a field to opt in to the skills mount:

```rust
pub struct OverlayRequest {
    pub directories: Vec<DirectorySpec>,
    pub include_skills: bool,   // NEW
    pub agent: Option<AgentName>,
    pub yolo: bool,
    pub container_home: Option<String>,
}
```

### 5. `OverlayEngine::build_overlays` changes

After step 2 (agent settings overlays), add step 3 for skills:

```rust
// 3. Skills overlay (mount ~/.amux/skills/ read-only into agent's native path).
if request.include_skills {
    if let Some(agent) = &request.agent {
        for spec in self.skill_overlays(agent, &request.container_home)? {
            let key = OverlayPathResolver::conflict_key(&spec.host_path);
            insert_or_merge(&mut by_key, key, spec);
        }
    }
}
```

Add a new `skill_overlays` method:

```rust
pub fn skill_overlays(
    &self,
    agent: &AgentName,
    container_home_override: &Option<String>,
) -> Result<Vec<OverlaySpec>, EngineError> {
    let skill_dirs = SkillDirs::from_process_env(None).map_err(EngineError::Data)?;
    let host_skills_dir = skill_dirs.global_dir();
    if !host_skills_dir.exists() {
        tracing::debug!(
            path = %host_skills_dir.display(),
            "global skills directory does not exist; skipping skills overlay"
        );
        return Ok(vec![]);
    }

    let home = self.auth_resolver.home();
    let container_home = container_home_override
        .clone()
        .unwrap_or_else(|| {
            detect_container_home(home, agent.as_str())
                .unwrap_or_else(|| "/root".to_string())
        });

    let container_path = match agent.as_str() {
        "claude"    => format!("{container_home}/.claude/commands"),
        "codex"     => format!("{container_home}/.codex/skills"),
        "opencode"  => format!("{container_home}/.config/opencode/commands"),
        "gemini"    => format!("{container_home}/.gemini/commands"),
        "copilot"   => format!("{container_home}/.copilot/instructions"),
        "crush"     => format!("{container_home}/.config/crush/commands"),
        "cline"     => format!("{container_home}/.cline/skills"),
        "maki"      => {
            tracing::warn!(
                agent = "maki",
                "skills overlay is not supported for maki; no known skills directory"
            );
            return Ok(vec![]);
        }
        other => {
            tracing::warn!(
                agent = other,
                "skills overlay: unknown agent, skipping"
            );
            return Ok(vec![]);
        }
    };

    Ok(vec![OverlaySpec {
        host_path: OverlayPathResolver::canonicalize_lossy(&host_skills_dir),
        container_path: PathBuf::from(container_path),
        permission: OverlayPermission::ReadOnly,
    }])
}
```

### 6. `collect_all_overlay_specs` / callsite wiring

**`src/command/commands/mod.rs`** — update `collect_all_overlay_specs` to also return whether skills overlay is enabled:

```rust
pub fn collect_all_overlay_specs(
    session: &Session,
    cli_typed_overlays: Vec<TypedOverlay>,
) -> (Vec<DirectorySpec>, bool) {
    // ... existing directory collection logic, adapted for TypedOverlay ...
    // Also check:
    // 1. global config overlays.skills
    // 2. repo config overlays.skills
    // 3. AMUX_OVERLAYS env var contains TypedOverlay::Skill
    // 4. cli_typed_overlays contains TypedOverlay::Skill
    // skills_enabled = any of the above is true
    (directory_specs, skills_enabled)
}
```

**Callsites** (`exec_workflow.rs`, `exec_prompt.rs`, `implement.rs`, `chat.rs`): update to parse `--overlay` values into `Vec<TypedOverlay>`, call `collect_all_overlay_specs`, and set `request.include_skills` from the returned bool.

**Headless mode**: inherits `--overlay` flags and `AMUX_OVERLAYS` env var through child process spawn, same as directory overlays. No additional wiring needed.

### 7. CLI flag parsing

The `--overlay` flag format is extended with the `skill` type tag. No changes to flag names or signatures required. The `parse_overlay_list` update in step 2 handles the new tag. For commands that still pass raw strings, parse each raw string using the updated `parse_overlay_list` (wrapping them in `dir(...)` if no type tag is present) or teach `parse_overlay_spec` to also accept plain `"skill()"`.

The simplest backward-compatible approach: in `parse_single_typed_overlay`, if the input has no `(`, treat it as a legacy bare path spec and forward to `parse_dir_overlay_args`. The `skill()` tag always requires parentheses.


## Edge Case Considerations:

- **Global skills directory does not exist**: If `~/.amux/skills/` does not exist on the host (user has never created any skills), log a `debug!` message and skip the mount — do not emit a warning or fail the launch. Skills are optional.
- **Empty skills directory**: If `~/.amux/skills/` exists but is empty, the mount is still applied. Docker allows mounting empty directories. No warning needed.
- **Container path already mounted by a `dir()` overlay**: If a `dir()` overlay targeting the same container path is also in the request, `insert_or_merge` handles the conflict as with any duplicate. The `dir()` overlay wins on container path if it shares the same host conflict key; log a `warn!` otherwise. Since source paths differ, both mounts would collide in the container — warn on the container-path collision.
- **`maki` and unknown agents**: Always skip with a `warn!` rather than failing; skills are a best-effort enhancement, not a required mount.
- **`skill()` with arguments**: Reject with a descriptive parse error: `"'skill()' takes no arguments"`.
- **Multiple `skill()` entries**: Deduplicate silently — a request with `skill()` twice is equivalent to one `skill()`.
- **`include_skills = true` but no agent in request**: No-op; skills mount requires a resolved agent to determine the container target path.
- **Custom container home**: `container_home_override` in `OverlayRequest` propagates into `skill_overlays` so non-root container users get the right path.
- **Apple Containers runtime**: `src/engine/container/apple.rs` must apply skills overlays (and all overlay specs) via the same `OverlaySpec` list that Docker does; no agent-specific logic needed there since `OverlayEngine` resolves the path before the runtime layer.
- **Skills directory is a symlink**: `canonicalize_lossy` resolves symlinks in the host path, so skills stored in a symlinked directory work correctly.
- **`AMUX_OVERLAYS` contains `skill()` alongside `dir()` entries**: The comma-separated parser handles both types in the same string: `"skill(),dir(/data:/mnt/data:ro)"`.

## Test Considerations:

### Unit tests — parser (`src/command/commands/mod.rs`)
- `skill()` parses to `TypedOverlay::Skill`.
- `skill(anything)` returns a parse error ("takes no arguments").
- `skill()` alongside `dir(...)` in a comma-separated string produces `[Skill, Directory(...)]`.
- Unknown tags still return descriptive errors; error message now lists `dir` and `skill` as valid types.

### Unit tests — `OverlayEngine::skill_overlays`
- Returns a single `:ro` `OverlaySpec` with host path equal to the global skills dir for each supported agent.
- Returns empty vec when the skills dir does not exist; no error raised.
- Returns empty vec for `maki`; logs a warn.
- Container path uses the container home from `OverlayRequest.container_home` when set.
- Container path defaults to `/root` when `detect_container_home` returns `None`.

### Unit tests — `collect_all_overlay_specs`
- Returns `skills_enabled = true` when repo config has `"skills": true`.
- Returns `skills_enabled = true` when global config has `"skills": true`.
- Returns `skills_enabled = true` when `AMUX_OVERLAYS` contains `skill()`.
- Returns `skills_enabled = true` when CLI `--overlay "skill()"` is present.
- Returns `skills_enabled = false` when none of the above sources is set.
- `skills_enabled` is `true` even when only one source sets it (additive OR, not AND).

### Unit tests — config deserialization (`src/data/config/repo.rs`, `global.rs`)
- `"overlays": {"skills": true}` deserializes to `OverlaysConfig { skills: Some(true), directories: None }`.
- `"overlays": {"skills": false}` deserializes to `skills: Some(false)`.
- Missing `skills` key deserializes to `skills: None` (treated as false at callsite).
- Existing config with only `directories` continues to deserialize without error.

### Integration tests
- `amux implement --overlay "skill()"` produces a Docker `-v ~/.amux/skills:{container_home}/.claude/commands:ro` mount for the claude agent.
- `amux exec workflow --overlay "skill()"` produces the correct mount for whichever agent is configured.
- `AMUX_OVERLAYS="skill()"` env var produces the same mount without a CLI flag.
- Repo config `"overlays": {"skills": true}` produces the skills mount automatically for every agent launch.
- Skills overlay combined with a `dir()` overlay: both `-v` entries appear in Docker args.
- Skills overlay for `maki` agent: no `-v` for the skills dir, warn is logged, launch proceeds.
- Global skills dir absent: no skills `-v` entry in Docker args, launch proceeds without error.
- `skill(anything)` as a flag value: command fails with a descriptive error; no container is launched.

### Parity tests (CLI ↔ TUI ↔ Headless)
- `--overlay "skill()"` produces identical `-v` args in CLI mode, TUI command bar, and headless dispatch.
- `AMUX_OVERLAYS="skill()"` is respected in both CLI and TUI modes.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- `SkillDirs` already exists at `src/data/fs/skill_dirs.rs`; import and use it in `OverlayEngine::skill_overlays` rather than reimplementing path resolution.
- All new public types should derive `Debug`, `Clone`, `PartialEq` to match existing conventions.
- Use `tracing::warn!` and `tracing::debug!` (never `eprintln!`) for all runtime diagnostics.
- `TypedOverlay` should live in `src/command/commands/mod.rs` alongside the existing overlay parsing functions, and be `pub` so frontends can use it.
- `OverlayRequest.include_skills: bool` follows the pattern of `OverlayRequest.yolo: bool` — a boolean flag that gates a self-contained block of overlay construction logic.
- The per-agent path table in `skill_overlays` is the single source of truth for container skill paths; keep it in one place (no copies in CLI or TUI layers).
- Apple Containers support (`src/engine/container/apple.rs`) does not require agent-aware changes: skills are resolved by `OverlayEngine` into plain `OverlaySpec` entries before the runtime layer processes them.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
