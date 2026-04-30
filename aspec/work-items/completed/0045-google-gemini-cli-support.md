# Work Item: Feature

Title: Google Gemini CLI Support
Issue: issuelink

## Summary:
- Add `gemini` (https://github.com/google-gemini/gemini-cli) as a first-class agent alongside `claude`, `codex`, `opencode`, and `maki`.
- Create `templates/Dockerfile.gemini` installing `@google/gemini-cli` via npm (requires Node.js 20+).
- Wire gemini into all agent dispatch paths: `chat_entrypoint`, `chat_entrypoint_non_interactive`, `append_plan_flags`, `append_autonomous_flags`, `agent_name`, `dockerfile_for_agent_embedded`, `download_dockerfile_template`, and the `Agent` enum.
- Auth passthrough via two paths: (1) `envPassthrough` for API-key-based auth (`GEMINI_API_KEY`, Vertex AI vars), already available from WI-44; (2) a new optional `~/.gemini/` directory mount in `HostSettings` to forward OAuth tokens from the host into the container.
- The `--yolo` flag maps directly to gemini's native `--yolo` flag. The `--auto` flag maps to `--approval-mode=auto_edit`. The `--plan` flag maps to `--approval-mode=plan`.


## User Stories

### User Story 1:
As a: user

I want to: run `amux init --agent gemini` and `amux chat` to start a gemini session inside a container

So I can: use Google Gemini as my coding agent with the same amux workflow I use for claude and codex, without manual Dockerfile or auth setup.

### User Story 2:
As a: user

I want to: set `"envPassthrough": ["GEMINI_API_KEY"]` in my global config

So I can: have my Gemini API key automatically injected into the agent container at launch time, authenticating gemini without per-session manual steps.

### User Story 3:
As a: user

I want to: run `amux chat --yolo` or `amux chat --auto` with gemini active

So I can: use gemini's autonomous operation modes (`--yolo` and `--approval-mode=auto_edit`) with the same UX as other agents.


## Implementation Details:

### 1. `Dockerfile.gemini` template (`templates/Dockerfile.gemini`)

Install Node.js 20 via apt (NodeSource) and then install `@google/gemini-cli` globally. Gemini requires Node.js 20.0.0+.

```dockerfile
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    ca-certificates \
    curl \
    gnupg \
    && curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*

# Install Gemini CLI via npm
RUN npm install -g @google/gemini-cli

WORKDIR /workspace
```

> **Note:** The existing unit test `dockerfile_for_agent_embedded_does_not_use_npm_install` asserts that no template uses `npm install`. This test must be updated to exempt `Agent::Gemini` (or be changed to check for `npm install` without `-g`). The no-`npm install` guard was intended to prevent accidental local package installs, not to prohibit global CLI installations which are the standard gemini deployment method.

### 2. `Agent` enum (`src/cli.rs`)

Add `Gemini` variant:

```rust
pub enum Agent {
    Claude,
    Codex,
    Opencode,
    Maki,
    Gemini,
}

impl Agent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Opencode => "opencode",
            Agent::Maki => "maki",
            Agent::Gemini => "gemini",
        }
    }
}
```

### 3. Embedded Dockerfile (`src/commands/init.rs`)

Add `Agent::Gemini` arm to `dockerfile_for_agent_embedded`:

```rust
Agent::Gemini => include_str!("../../templates/Dockerfile.gemini").to_string(),
```

Update the loop in all tests that iterate over all agents (e.g., `dockerfile_for_agent_embedded_uses_debian_slim_base`) to include `Agent::Gemini`.

### 4. Download (`src/commands/download.rs`)

Add `Agent::Gemini` to `download_dockerfile_template`:

```rust
Agent::Gemini => "Dockerfile.gemini",
```

### 5. Agent name helper (`src/commands/chat.rs` — `agent_name`)

Add `"gemini"` arm:

```rust
"gemini" => "gemini",
```

### 6. Chat entrypoints (`src/commands/chat.rs`)

`chat_entrypoint`:
```rust
"gemini" => vec!["gemini".to_string()],
```

`chat_entrypoint_non_interactive`:
Gemini supports `-p` / `--prompt` for headless/non-interactive output:
```rust
"gemini" => vec!["gemini".to_string(), "-p".to_string()],
```

### 7. Plan flags (`src/commands/chat.rs` — `append_plan_flags`)

Gemini supports `--approval-mode=plan` for read-only/plan mode (restricts tools to file reading, search, and web fetching — no writes):

```rust
"gemini" => {
    args.push("--approval-mode=plan".to_string());
}
```

### 8. Autonomous flags (`src/commands/agent.rs` — `append_autonomous_flags`)

Gemini flag mappings:
- `--yolo` → `--yolo` (gemini's native flag; identical name to maki's, coincidentally same as amux's `--yolo`)
- `--auto` → `--approval-mode=auto_edit` (auto-approves file edits and writes, prompts for shell/other tools)
- Gemini has no `--disallowedTools` equivalent.

```rust
"gemini" => {
    if yolo {
        // gemini's --yolo skips all tool-call confirmations.
        // Note: this is gemini's own flag, not amux's --yolo flag.
        args.push("--yolo".to_string());
    } else {
        // --auto maps to gemini's auto_edit approval mode.
        args.push("--approval-mode=auto_edit".to_string());
    }
    if !disallowed_tools.is_empty() {
        eprintln!(
            "WARNING: {}: gemini does not support --disallowedTools; yoloDisallowedTools config will be ignored.",
            flag_name
        );
    }
}
```

Update the doc comment on `append_autonomous_flags` to document gemini's mappings alongside claude, codex, opencode, and maki.

### 9. Auth passthrough — `envPassthrough` (primary path)

No new code required. The `envPassthrough` mechanism from WI-44 already handles API-key-based auth. Users add the relevant var names to their global or repo config:

```json
{ "envPassthrough": ["GEMINI_API_KEY"] }
```

Supported auth env vars:
- `GEMINI_API_KEY` — API key from https://aistudio.google.com/apikey (free tier: 1,000 req/day)
- `GOOGLE_API_KEY` — Vertex AI API key (takes precedence over `GEMINI_API_KEY` when both are set)
- `GOOGLE_CLOUD_PROJECT` — Vertex AI project ID
- `GOOGLE_CLOUD_LOCATION` — Vertex AI region
- `GOOGLE_APPLICATION_CREDENTIALS` — path to a service account JSON file (mount the file via `--mount-ssh` or a future secrets volume if needed)
- `GOOGLE_GENAI_USE_VERTEXAI=true` — enables Vertex AI auth path

### 10. Auth passthrough — `~/.gemini/` mount (OAuth path)

Gemini's primary auth method for individual users is browser-based OAuth; tokens are persisted to `~/.gemini/settings.json` on the host. To forward OAuth credentials into the container we need to mount `~/.gemini/` — the same pattern already used for other agents via `HostSettings`.

**Add `GeminiPassthrough`** in `src/passthrough.rs`, following the same `AgentPassthrough` trait pattern as `ClaudePassthrough`, `OpencodePassthrough`, and `CodexPassthrough`:

```rust
/// Top-level entries in `~/.gemini/` to exclude from the container copy.
const GEMINI_DIR_DENYLIST: &[&str] = &["logs"];

/// Passthrough for the Google Gemini CLI agent.
///
/// - **Keychain**: none (gemini does not use the system keychain).
/// - **Env vars**: none (API keys passed via the `envPassthrough` config key).
/// - **Settings**: copies `~/.gemini/` into a temp dir and mounts it (read-write) at
///   `/root/.gemini` inside the container. The mount is read-write because the source is
///   a temp copy, not the live host directory.
///   If `~/.gemini/` does not exist on the host, creates an empty temp dir and mounts
///   that instead, so the container starts with a clean gemini state (gemini will prompt
///   for auth on first use).
pub struct GeminiPassthrough;

impl AgentPassthrough for GeminiPassthrough {
    fn prepare_host_settings(&self) -> Option<HostSettings> {
        let home = dirs::home_dir()?;
        let src = home.join(".gemini");
        let temp_dir = tempfile::TempDir::new().ok()?;
        let dst = temp_dir.path().join("gemini-data");
        if src.exists() {
            crate::runtime::copy_dir_filtered(&src, &dst, GEMINI_DIR_DENYLIST).ok()?;
        } else {
            std::fs::create_dir_all(&dst).ok()?;
        }
        Some(HostSettings::new_agent_dir(
            Some(temp_dir),
            "/root".to_string(),
            Some((dst, "/root/.gemini".to_string())),
        ))
    }

    fn prepare_host_settings_to_dir(&self, dir: &Path) -> Option<HostSettings> {
        let home = dirs::home_dir()?;
        let src = home.join(".gemini");
        std::fs::create_dir_all(dir).ok()?;
        let dst = dir.join("gemini-data");
        if src.exists() {
            crate::runtime::copy_dir_filtered(&src, &dst, GEMINI_DIR_DENYLIST).ok()?;
        } else {
            std::fs::create_dir_all(&dst).ok()?;
        }
        Some(HostSettings::new_agent_dir(
            None,
            "/root".to_string(),
            Some((dst, "/root/.gemini".to_string())),
        ))
    }
}
```

**Update `passthrough_for_agent`** to add the gemini arm:

```rust
pub fn passthrough_for_agent(agent: &str) -> Box<dyn AgentPassthrough> {
    match agent {
        "claude" => Box::new(ClaudePassthrough),
        "opencode" => Box::new(OpencodePassthrough),
        "codex" => Box::new(CodexPassthrough),
        "gemini" => Box::new(GeminiPassthrough),
        _ => Box::new(NoopPassthrough),
    }
}
```

No changes to `HostSettings` struct are required — `agent_config_dir: Option<(PathBuf, String)>` and `HostSettings::new_agent_dir` already exist. No changes to `chat.rs` or `implement.rs` dispatch are required — they already call `passthrough_for_agent(agent)` to resolve auth.

Confirm the Docker/Podman run-args builders in `src/runtime/docker.rs` emit `-v host_path:container_path` (read-write) for `agent_config_dir` when set — same as codex/opencode. Gemini inherits this automatically since `agent_config_dir` is the shared mechanism.

### 11. `amux ready` (`src/commands/ready.rs`)

Add `"gemini"` to `dockerfile_matches_template` and any agent-string match arms so the ready check recognises the gemini Dockerfile template. The gemini template is identified by the presence of `google/gemini-cli` in the Dockerfile content.

### 12. `docs/` updates

Update the agent configuration section to document:
- `gemini` as a supported agent value.
- Auth options: `envPassthrough` with `GEMINI_API_KEY` (API key), Vertex AI env vars, and the `~/.gemini/` OAuth mount.
- Flag mappings: `--yolo`, `--auto`, `--plan`.


## Edge Case Considerations:

- **`--yolo` flag name overlap**: gemini uses `--yolo` as its own autonomous flag, just like maki and amux. When amux passes `--yolo` to gemini, it is gemini's flag — no conflict. Add a comment in the `"gemini"` arm of `append_autonomous_flags` identical in style to the maki comment.
- **`--approval-mode=auto_edit` vs. `--auto`**: `auto_edit` is gemini's closest equivalent to Claude's `--permission-mode auto` — it auto-approves file writes but prompts before shell tool calls. This is intentionally more conservative than `--yolo`. Document this behaviour difference clearly.
- **`-p` flag shared with claude**: gemini uses `-p` (short for `--prompt`) for non-interactive/headless mode, same short flag as claude. The flag is not passed between agents — each agent receives only its own flags — so there is no conflict.
- **`~/.gemini/` absent on host**: if `~/.gemini/` does not exist (user has never run `gemini` on the host), `prepare_gemini` must not error. Create an empty temp dir and mount that instead, so the container starts with a clean gemini state. Gemini will prompt for auth on first use inside the container.
- **`~/.gemini/settings.json` contains sensitive tokens**: `GeminiPassthrough` copies `~/.gemini/` into a temp dir (same pattern as codex/opencode) and mounts it read-write. The live host directory is never directly mounted. No `:ro` enforcement needed since the container writes to a temp copy, not the host.
- **`envPassthrough` and OAuth tokens together**: if a user sets both `GEMINI_API_KEY` in `envPassthrough` and has `~/.gemini/` mounted, gemini will use whichever auth method it encounters first. This is gemini's resolution logic — amux does not need to arbitrate. Document that API key env vars take precedence over stored OAuth tokens in gemini's auth order.
- **Node.js version in Dockerfile.gemini**: gemini requires Node.js ≥ 20. Use the NodeSource 20.x setup script in the Dockerfile. If NodeSource changes its install URL, the template must be updated. Pin the major version (20.x) rather than `current` to avoid surprises.
- **npm global install in container**: `npm install -g @google/gemini-cli` places the binary at `/usr/local/bin/gemini` (standard npm global prefix). Verify with `which gemini` in a test build. If the path is different (e.g. under `/usr/lib/node_modules/.bin/`), add an explicit `ENV PATH` line.
- **Dockerfile test update**: the test `dockerfile_for_agent_embedded_does_not_use_npm_install` must be updated. Exempt `Agent::Gemini` from the `npm install` assertion, or change the assertion to only flag bare `npm install` (without `-g`) which would indicate a local project install by mistake.
- **`GOOGLE_APPLICATION_CREDENTIALS` path**: this env var points to a file path on the host. If a user passes it via `envPassthrough`, the path inside the container will differ from the host path. Document that service account JSON auth requires manual volume mounting or embedding the key in the image — it cannot be handled by `envPassthrough` alone.
- **Gemini sandbox mode**: gemini has its own `--sandbox` flag that nests Docker-within-Docker. Do not pass `--sandbox` automatically — it conflicts with amux's container model. If the user wants gemini's sandbox, they can add it via a future `extraAgentArgs` config field.


## Test Considerations:

- **`chat_entrypoint("gemini", false)`**: returns `["gemini"]`.
- **`chat_entrypoint_non_interactive("gemini", false)`**: returns `["gemini", "-p"]`.
- **`chat_entrypoint("gemini", true)`** (plan mode): returns `["gemini", "--approval-mode=plan"]`.
- **`chat_entrypoint_non_interactive("gemini", true)`** (plan + non-interactive): returns `["gemini", "-p", "--approval-mode=plan"]`.
- **`append_autonomous_flags` for gemini, yolo=true**: `--yolo` appended; `--disallowedTools` never appended.
- **`append_autonomous_flags` for gemini, auto=true**: `--approval-mode=auto_edit` appended; `--dangerously-skip-permissions` and `--yolo` never appended.
- **`append_autonomous_flags` for gemini, yolo=true, disallowed_tools non-empty**: `--yolo` appended; warning printed to stderr; `--disallowedTools` never appended.
- **`append_autonomous_flags` for gemini, yolo=true AND auto=true**: yolo wins — `--yolo` appended, `--approval-mode=auto_edit` not appended.
- **`dockerfile_for_agent_embedded(Agent::Gemini)`**: content contains `debian:bookworm-slim`, `nodesource`, `google/gemini-cli`.
- **Updated `dockerfile_for_agent_embedded_does_not_use_npm_install` test**: the gemini template is explicitly excluded, and a comment explains why.
- **Updated `dockerfile_for_agent_embedded_uses_debian_slim_base`**: loop now includes `Agent::Gemini` and the assertion passes.
- **`GeminiPassthrough.keychain_credentials()`**: returns empty `AgentCredentials`.
- **`GeminiPassthrough.extra_env_vars()`**: returns empty `Vec`.
- **`GeminiPassthrough.prepare_host_settings()`** when `~/.gemini/` exists: returns `Some` with `agent_config_dir = Some((temp_copy_path, "/root/.gemini"))` and `mount_claude_files = false`.
- **`GeminiPassthrough.prepare_host_settings()`** when `~/.gemini/` does not exist: returns `Some` (empty temp dir fallback) with `agent_config_dir` pointing to the empty temp dir and `mount_claude_files = false`. Must not panic or return `None`.
- **`GeminiPassthrough.prepare_host_settings_to_dir(dir)`**: returns `Some` with the same contract as `prepare_host_settings`; falls back to the supplied dir when `~/.gemini/` is absent.
- **`passthrough_for_agent("gemini")`** returns a `GeminiPassthrough`-backed impl: `keychain_credentials` empty, `extra_env_vars` empty, `prepare_host_settings` returns `Some`.
- **`passthrough_for_agent("maki")`** continues to return `NoopPassthrough` (unchanged).
- **Docker run-args include `-v .../gemini:ro` mount** when `agent_config_dir` is set.
- **`dockerfile_matches_template`** with gemini content: returns true for `"gemini"`, false for `"claude"`.
- **`envPassthrough` with `GEMINI_API_KEY`**: existing passthrough tests from WI-44 cover the injection path; add a gemini-specific integration note confirming the var name reaches the container.


## Codebase Integration:

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- The `Agent` enum in `src/cli.rs` and the string-matching in `src/commands/chat.rs` are the two canonical agent identity registration points. All downstream arms (`download.rs`, `init.rs`, `ready.rs`, `agent.rs`) must be updated in the same commit.
- Mirror the `append_plan_flags` claude/codex pattern for gemini's plan arm — gemini does have a plan mode (`--approval-mode=plan`), unlike maki and opencode.
- Mirror the `append_autonomous_flags` warning pattern (used for codex/maki disallowed tools) for gemini's disallowed-tools arm.
- `GeminiPassthrough` lives in `src/passthrough.rs` alongside `ClaudePassthrough`, `OpencodePassthrough`, `CodexPassthrough`, and `NoopPassthrough`. Register it in `passthrough_for_agent` with `"gemini" => Box::new(GeminiPassthrough)`.
- `agent_config_dir: Option<(PathBuf, String)>` and `HostSettings::new_agent_dir` already exist in `src/runtime/mod.rs` — no struct changes needed.
- `src/commands/auth.rs` keychain logic is intentionally claude-only. Do not add gemini keychain logic there. Gemini authentication is handled entirely via `envPassthrough` and the `~/.gemini/` directory mount via `GeminiPassthrough`.
- The `Dockerfile.gemini` template must pass the following automated tests (after updating the npm exclusion guard): uses `debian:bookworm-slim`, installs via `apt-get` and `curl`, installs gemini via `npm install -g`.
- The existing `HostSettings::prepare("gemini").is_none()` assertion in the test suite (`host_settings_prepare_returns_none_for_non_claude`) must remain true — `prepare_gemini` is a separate function, not a branch inside `prepare`.
