# Work Item: Enhancement

Title: Modular Dockerfiles
Issue: issuelink

## Summary

amux currently uses a single `Dockerfile.dev` at the git root that contains both project-specific tooling (build deps, compilers, runtimes) and agent-specific tooling (Claude Code, Codex, etc.) bundled into one image tagged `amux-{projectname}:latest`. This conflates project setup with agent selection and prevents multi-agent usage.

This work item splits the Dockerfile into two layers:

1. **Project base image** (`Dockerfile.dev` at git root) — contains only project build/runtime dependencies. Produces `amux-{projectname}:latest`. Same name as today.
2. **Agent-specific image** (`.amux/Dockerfile.{agent}`) — uses the project base as `FROM`, installs the agent tooling, creates the `amux` non-root user. Produces `amux-{projectname}-{agentname}:latest`.

Agent Dockerfiles are written to `.amux/` on demand from embedded or downloaded templates, with the project base image tag substituted into the `FROM` directive. The resulting agent image is used for all `chat` and `implement` sessions.

A new `--agent <name>` flag on `chat` and `implement` lets users pick a non-default agent at launch time. If the requested agent image does not exist, amux offers to download the template and build it.

For repos that already have a single Dockerfile.dev containing agent tooling, amux detects the legacy layout and offers a guided migration: recreate a minimal project Dockerfile.dev, generate the agent Dockerfile, build both images, then run the audit agent to fill project deps back into Dockerfile.dev.


## User Stories

### User Story 1
As a: user

I want to: switch between coding agents (e.g. Claude, Codex) using `--agent codex` on any `chat` or `implement` invocation

So I can: experiment with different agents on the same project without changing my config or rebuilding the entire project image.

### User Story 2
As a: user

I want to: have my `Dockerfile.dev` contain only project-specific dependencies, with agent tooling isolated in `.amux/Dockerfile.{agent}`

So I can: update the project build environment without touching agent configuration, and update agents independently without rebuilding project deps.

### User Story 3
As a: user with an existing single Dockerfile.dev

I want to: be offered a smooth migration path to the modular layout

So I can: adopt the new system without manually splitting my Dockerfile, with amux handling the migration and using the audit agent to repopulate project dependencies.


## Implementation Details

### Phase 1 — New image naming and template restructuring

**`src/runtime/docker.rs`**

Add a new free function alongside the existing `project_image_tag()`:

```rust
/// Returns the image tag for an agent-specific image layered on top of the project base.
/// Pattern: amux-{projectname}-{agentname}:latest
pub fn agent_image_tag(git_root: &Path, agent: &str) -> String {
    let project_name = git_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    format!("amux-{}-{}:latest", project_name, agent)
}
```

`project_image_tag()` is unchanged — it still names the project base image.

**`templates/Dockerfile.{agent}` — rewrite all agent templates**

Each agent template (`templates/Dockerfile.claude`, `templates/Dockerfile.codex`, etc.) is rewritten to use a placeholder base image instead of a hardcoded Debian base. The placeholder `{{AMUX_BASE_IMAGE}}` is substituted with the actual project base tag when the file is written to `.amux/`:

```dockerfile
FROM {{AMUX_BASE_IMAGE}}

# Install <agent> ...
RUN curl -fsSL https://... | bash \
    && cp /root/.local/bin/<agent> /usr/local/bin/<agent>

# Create non-root user for agent operations
RUN useradd -m -s /bin/bash amux \
    && mkdir -p /workspace \
    && chown amux:amux /workspace

USER amux
WORKDIR /workspace
```

Remove all system package installation (git, curl, ca-certificates, etc.) from agent templates — these are now guaranteed to exist in the project base image, which starts from `debian:bookworm-slim`.

**`templates/Dockerfile.project` — new project base template**

Add a new embedded template used when writing a fresh project `Dockerfile.dev`:

```dockerfile
FROM debian:bookworm-slim

# System packages required for building, testing, and running this project.
# Add language runtimes, compilers, and tool dependencies here.
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    ca-certificates \
    curl \
    make \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
```

Note: no `USER amux` in the project Dockerfile — that is the agent dockerfile's responsibility.

### Phase 2 — Agent dockerfile write and build logic

**`src/commands/init.rs`**

Refactor `write_dockerfile()` into two distinct functions:

```rust
/// Write the project Dockerfile.dev (base image) to the git root.
/// Uses embedded project template; does not overwrite an existing file.
pub async fn write_project_dockerfile(git_root: &Path, out: &OutputSink) -> Result<bool>

/// Write the agent-specific Dockerfile to .amux/Dockerfile.{agent}.
/// Downloads template from GitHub; falls back to embedded template.
/// Substitutes the project base image tag into the FROM directive.
pub async fn write_agent_dockerfile(
    git_root: &Path,
    agent: &Agent,
    out: &OutputSink,
) -> Result<bool>
```

`write_agent_dockerfile()` computes the base tag via `project_image_tag(git_root)`, fetches the agent template, replaces `{{AMUX_BASE_IMAGE}}` with the computed tag, and writes to `git_root/.amux/Dockerfile.{agent}`.

Update `dockerfile_for_agent_embedded()` to source agent templates (which now use the placeholder). Add `project_dockerfile_embedded()` that returns the project template content.

The `.amux/` directory is created if it does not exist (using `std::fs::create_dir_all`).

**`src/commands/ready.rs`**

Update the build sequence to:
1. Ensure `Dockerfile.dev` exists; write project template if missing.
2. Ensure `.amux/Dockerfile.{agent}` exists for the configured agent; write agent template if missing.
3. Build the project base image: `amux-{project}:latest` from `Dockerfile.dev`.
4. Build the agent image: `amux-{project}-{agent}:latest` from `.amux/Dockerfile.{agent}`.

When `--build` is passed, rebuild both images. When `--no-cache` is passed, pass it to both builds.

The `--refresh` flag (audit agent) continues to operate on `Dockerfile.dev` (the project base), not the agent dockerfile.

### Phase 3 — Agent image selection at launch

**`src/commands/agent.rs` — `run_agent_with_sink()`**

Change the image used for container launch from `project_image_tag()` to `agent_image_tag()`:

```rust
// Before:
let image_tag = docker::project_image_tag(&git_root);

// After:
let agent_name = agent_override.as_deref().unwrap_or(
    config.agent.as_deref().unwrap_or("claude")
);
let image_tag = docker::agent_image_tag(&git_root, agent_name);
```

Add `agent_override: Option<String>` parameter to `run_agent_with_sink()`.

**`apply_dockerfile_user` path**: change the dockerfile path from `git_root.join("Dockerfile.dev")` to `git_root.join(".amux").join(format!("Dockerfile.{}", agent_name))` — the `USER amux` directive now lives in the agent dockerfile, not the project one.

Before launching, verify the agent image exists. If it does not:
1. Check whether `.amux/Dockerfile.{agent}` exists.
2. If yes, offer to build the agent image now (requires the project base image to exist first).
3. If no, offer to download the template and build both (project base if missing, then agent image).
4. If the user declines, exit with a clear error.

### Phase 4 — `--agent` flag on `chat` and `implement`

**`src/cli.rs`**

Add `--agent <name>` to both `chat` and `implement` subcommands:

```rust
/// Agent to use (overrides .amux/config.json). If the agent image does not exist,
/// amux will offer to download and build it.
#[arg(long, value_name = "NAME")]
pub agent: Option<String>,
```

Validate the provided agent name against the known set (`claude`, `codex`, `opencode`, `maki`, `gemini`). Unknown names produce a helpful error listing available agents.

**`src/commands/chat.rs` and `src/commands/implement.rs`**

Pass `args.agent` through to `run_agent_with_sink()` as the new `agent_override` parameter.

### Phase 5 — Migration for existing repos

**Detection**: at the start of `ready`, `chat`, and `implement`, detect the legacy layout:
- `Dockerfile.dev` exists at git root, AND
- `.amux/Dockerfile.{agent}` does NOT exist for the configured agent.

When detected in `ready`, prompt the user:

```
Detected legacy single-file Dockerfile.dev layout.
Would you like to migrate to the modular layout? (agent tools move to .amux/Dockerfile.{agent})

Migrating will:
  1. Recreate Dockerfile.dev with a minimal debian:bookworm-slim base
  2. Write .amux/Dockerfile.{agent} using the agent template
  3. Build both images
  4. Run the audit agent to restore project dependencies in Dockerfile.dev

[y/N]:
```

If the user accepts:
1. Overwrite `Dockerfile.dev` with the project base template (debian-slim + git/curl/make/ca-certs).
2. Write `.amux/Dockerfile.{agent}` from the agent template (substituting the base tag).
3. Build the project base image with streaming output.
4. Build the agent image with streaming output.
5. Run the existing audit/refresh agent (`--refresh` flow) to detect and add project dependencies back into `Dockerfile.dev`.

If the user declines, use the existing image unchanged (existing `amux-{project}:latest` still works for the session but cannot be used as a base for new agent images).

When detection occurs during `chat` or `implement` (not `ready`), offer a shorter prompt: "Run `amux ready` to migrate to the modular Dockerfile layout, or pass `--no-migrate` to use the existing image." Exit without launching.

### Summary of changed files

| File | Change |
|---|---|
| `src/runtime/docker.rs` | Add `agent_image_tag()` |
| `src/commands/agent.rs` | Use `agent_image_tag()`, add `agent_override` param, on-demand agent image build |
| `src/commands/chat.rs` | Thread `--agent` flag to `run_agent_with_sink()` |
| `src/commands/implement.rs` | Thread `--agent` flag to `run_agent_with_sink()` |
| `src/commands/ready.rs` | Two-stage build (base + agent), migration prompt |
| `src/commands/init.rs` | Split `write_dockerfile()` into project + agent variants, add project template |
| `src/cli.rs` | Add `--agent` to `chat` and `implement` subcommands |
| `templates/Dockerfile.{agent}` (all) | Rewrite to use `{{AMUX_BASE_IMAGE}}` placeholder, remove system packages |
| `templates/Dockerfile.project` | New project base template |


## Edge Case Considerations

- **Legacy single-image repos that decline migration**: The existing `amux-{project}:latest` image continues to work for `chat` and `implement` as long as the agent image exists. amux detects that `.amux/Dockerfile.{agent}` is absent and falls back to using `project_image_tag()` as the launch image, with a deprecation warning printed each time. This fallback should be explicitly documented as temporary.

- **Base image not yet built when agent image is needed**: Before building `.amux/Dockerfile.{agent}`, verify the project base image (`amux-{project}:latest`) exists. If it does not, build the base first, then build the agent image. Emit clear status for each build step.

- **`--agent` flag with unknown name**: Validate against the canonical agent list at CLI parse time. Return a clear error: `unknown agent "foo"; available agents: claude, codex, opencode, maki, gemini`. Do not attempt to download a template for an unknown agent name.

- **`.amux/Dockerfile.{agent}` exists but agent image does not**: Treat as a normal first-run case — build the agent image from the existing dockerfile without prompting (same behavior as `amux ready` on a fresh clone).

- **Multiple agents active simultaneously**: Each agent has its own image (`amux-{project}-claude:latest`, `amux-{project}-codex:latest`). They share the project base image as a layer, so they do not conflict. Building one agent image does not invalidate another.

- **Project base image changes (e.g., after `amux ready --build`)**: Rebuilding the project base (`amux-{project}:latest`) invalidates the agent image layers. After a base rebuild, agent images must also be rebuilt. `amux ready --build` should rebuild all agent images whose `.amux/Dockerfile.{agent}` files exist, not just the configured default.

- **Concurrent `amux ready` invocations**: Docker build is idempotent; if two builds race, the last one wins and both produce a valid image. No additional locking is needed.

- **The `.amux/` directory and git**: `.amux/Dockerfile.{agent}` files should be committed to version control so teammates share the same agent setup. `amux ready` on a fresh clone finds the dockerfile and builds the image. The `.amux/config.json` file already follows this pattern.

- **Renaming or removing an agent**: If an agent is removed from the config or the `.amux/Dockerfile.{agent}` file is deleted, the built image persists on the host but amux no longer references it. This is acceptable — stale images can be cleaned up by the user via `docker image prune`.

- **`apply_dockerfile_user` with agent dockerfile**: The `USER amux` directive is now always present in the agent dockerfile (it's part of every agent template). `apply_dockerfile_user()` parses the last `USER` directive; it should now point to `.amux/Dockerfile.{agent}` rather than `Dockerfile.dev`. When the legacy fallback path is used (single-image layout), continue pointing at `Dockerfile.dev`.

- **Audit agent (`--refresh`) scope**: The audit agent runs against `Dockerfile.dev` (project base) only. It should not modify `.amux/Dockerfile.{agent}` since agent installation steps are managed by templates, not by the audit.

- **GitHub template download for agent dockerfiles**: The download URL pattern currently targets `templates/Dockerfile.{agent}` in the amux repo. Update the download logic to fetch the new agent templates (which use the `{{AMUX_BASE_IMAGE}}` placeholder) from the same or a versioned path. The fallback embedded template is always available.


## Test Considerations

**Unit tests (`src/runtime/docker.rs`)**:
- `agent_image_tag(path, "claude")` returns `amux-{project}-claude:latest` for a path whose `file_name` is `{project}`.
- `agent_image_tag(path, "codex")` returns `amux-{project}-codex:latest`.
- `project_image_tag()` is unchanged (regression).

**Unit tests (`src/commands/init.rs`)**:
- `write_agent_dockerfile()` creates `.amux/Dockerfile.claude` with `FROM amux-testproject:latest` when the git root folder name is `testproject`.
- `write_agent_dockerfile()` does not overwrite an existing `.amux/Dockerfile.{agent}`.
- `write_project_dockerfile()` does not overwrite an existing `Dockerfile.dev`.
- `write_project_dockerfile()` creates `Dockerfile.dev` with `FROM debian:bookworm-slim` when the file is absent.
- `{{AMUX_BASE_IMAGE}}` placeholder is substituted correctly in agent templates.
- `write_agent_dockerfile()` creates `.amux/` if it does not exist.

**Unit tests (`src/cli.rs`)**:
- `--agent claude` is accepted and produces `Some("claude")`.
- `--agent unknown` is rejected with an appropriate error at validation time.
- `chat` without `--agent` produces `None` (falls back to config).

**Integration tests (`tests/`)**:
- `ready` with a clean repo writes both `Dockerfile.dev` and `.amux/Dockerfile.claude`, then builds both images in order (base first, agent second).
- `ready --build` rebuilds both images; verifies both image tags exist after completion.
- `ready --build` rebuilds all existing agent dockerfiles when multiple `.amux/Dockerfile.*` files are present.
- `chat --agent codex` correctly uses `amux-{project}-codex:latest` as the launch image.
- `chat --agent codex` when codex image is absent triggers the on-demand build prompt.
- Legacy layout detection: repo with `Dockerfile.dev` and no `.amux/Dockerfile.*` triggers migration prompt in `ready`.
- Migration: after user accepts, `Dockerfile.dev` is overwritten with project template, `.amux/Dockerfile.{agent}` is written, both images are built.
- Migration decline: existing `amux-{project}:latest` is used as the launch image with a deprecation warning.

**End-to-end tests**:
- `amux ready` on a freshly cloned repo (no existing images) produces two Docker images: `amux-{project}:latest` and `amux-{project}-claude:latest`.
- `amux chat --agent codex --non-interactive` launches a container using the codex agent image.
- `amux ready --build` when both images exist triggers both rebuilds and the images are updated.


## Codebase Integration

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- `project_image_tag()` and the new `agent_image_tag()` are pure free functions in `src/runtime/docker.rs` (alongside `generate_container_name`, `format_build_cmd`, etc.) — keep them co-located.
- The `write_dockerfile()` function in `src/commands/init.rs` currently handles both existence check and template fetch. Split it cleanly into `write_project_dockerfile()` and `write_agent_dockerfile()` rather than adding boolean parameters.
- The `download_or_fallback_dockerfile()` and `dockerfile_for_agent_embedded()` functions in `init.rs` are reused by `write_agent_dockerfile()` with minimal changes — agent templates are still fetched and embedded the same way, only the template content changes.
- Add a new `project_dockerfile_embedded()` function in `init.rs` (analogous to `dockerfile_for_agent_embedded()`) to return the new project base template.
- `run_agent_with_sink()` in `src/commands/agent.rs` is the single launch site — add the `agent_override` parameter there rather than duplicating agent resolution logic in `chat.rs` and `implement.rs`.
- The on-demand agent image build path in `run_agent_with_sink()` should reuse the same `build_image_streaming()` runtime call that `ready.rs` uses, streaming output to the same `OutputSink`.
- The migration prompt in `ready.rs` should use the same interactive confirmation pattern already in use in `implement::confirm_mount_scope_stdin()`.
- All new `.amux/Dockerfile.*` write operations must use `std::fs::create_dir_all` on `.amux/` before writing — consistent with how `.amux/config.json` is written.
- The `AgentRuntime` trait (`src/runtime/mod.rs`) requires no changes for this work item; both image build operations go through the existing `build_image_streaming()` method.
- Security constraint: the agent dockerfile writes only to `.amux/` inside the git root — no parent directory writes, consistent with `aspec/architecture/security.md`.
