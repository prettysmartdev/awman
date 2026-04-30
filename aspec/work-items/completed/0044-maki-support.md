# Work Item: Feature

Title: Maki Support
Issue: issuelink

## Summary:
- Add `maki` (https://maki.sh) as a first-class agent alongside `claude`, `codex`, and `opencode`.
- Create `templates/Dockerfile.maki` with the official maki install script.
- Wire maki into all agent dispatch paths: `chat_entrypoint`, `chat_entrypoint_non_interactive`, `append_plan_flags`, `append_yolo_flags`, `agent_name`, `dockerfile_for_agent_embedded`, `download_dockerfile`, and the `Agent` enum.
- Add an `envPassthrough` config field (global and repo) that allows users to specify an allowlist of host environment variable names that get injected into any agent container at launch time. This mechanism is how maki auth env vars (e.g., `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`) and other per-user secrets reach the container without hardcoding agent-specific logic.

## User Stories

### User Story 1:
As a: user

I want to: run `amux init --agent maki` and `amux chat` to start a maki session inside a container

So I can: use maki as my coding agent with the same amux workflow I use for claude and codex, without manual Dockerfile setup.

### User Story 2:
As a: user

I want to: set `"envPassthrough": ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]` in my global or repo config

So I can: have those env vars automatically read from my host shell and passed into any agent container at launch time, enabling maki (and other API-key-based agents) to authenticate without per-agent keychain logic.

### User Story 3:
As a: user

I want to: run `amux chat --yolo` with maki active

So I can: use maki's `--yolo` flag for fully autonomous operation, with the same UX as other agents that support autonomous mode.


## Implementation Details:

### 1. `Dockerfile.maki` template (`templates/Dockerfile.maki`)

Install maki via its official install script. Maki is a Rust binary distributed via `https://maki.sh/install.sh`. It is a single statically-linked binary, so no Node/npm/npm-global dependencies are needed.

```dockerfile
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Install maki via official installer
RUN curl -fsSL https://maki.sh/install.sh | sh \
    && cp /root/.local/bin/maki /usr/local/bin/maki

WORKDIR /workspace
```

### 2. `Agent` enum (`src/cli.rs`)

Add `Maki` variant:

```rust
pub enum Agent {
    Claude,
    Codex,
    Opencode,
    Maki,
}

impl Agent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Opencode => "opencode",
            Agent::Maki => "maki",
        }
    }
}
```

### 3. Embedded Dockerfile (`src/commands/init.rs`)

Add `Agent::Maki` arm to `dockerfile_for_agent_embedded`:

```rust
Agent::Maki => include_str!("../../templates/Dockerfile.maki").to_string(),
```

### 4. Download (`src/commands/download.rs`)

Add `Agent::Maki` to the `download_dockerfile` match:

```rust
Agent::Maki => "Dockerfile.maki",
```

### 5. Agent name helper (`src/commands/chat.rs` — `agent_name`)

Add `"maki"` arm:

```rust
"maki" => "maki",
```

### 6. Chat entrypoints (`src/commands/chat.rs`)

`chat_entrypoint`:
```rust
"maki" => vec!["maki".to_string()],
```

`chat_entrypoint_non_interactive`:
Maki supports `--print` / `-p` for non-interactive mode:
```rust
"maki" => vec!["maki".to_string(), "--print".to_string()],
```

### 7. Plan flags (`src/commands/chat.rs` — `append_plan_flags`)

Maki has no plan/read-only mode equivalent. Silently skip (same pattern as opencode):
```rust
// Maki has no plan mode.
"maki" => {}
```

### 8. Yolo flags (`src/commands/chat.rs` — `append_yolo_flags`)

Maki supports `--yolo` natively (skips all permission prompts). It has no `--disallowedTools` equivalent.

```rust
"maki" => {
    args.push("--yolo".to_string());
    if !disallowed_tools.is_empty() {
        eprintln!(
            "WARNING: {}: maki does not support --disallowedTools; yoloDisallowedTools config will be ignored.",
            flag_name
        );
    }
}
```

### 9. `envPassthrough` config field

Add to both `RepoConfig` and `GlobalConfig` in `src/config/mod.rs`:

```rust
/// Host environment variable names to pass through into agent containers.
/// Values are read from the host process environment at launch time.
/// Repo config overrides global config when both are set.
#[serde(rename = "envPassthrough", skip_serializing_if = "Option::is_none")]
pub env_passthrough: Option<Vec<String>>,
```

Add a resolver function in `src/config/mod.rs`:

```rust
/// Returns the effective env passthrough list for a given git root.
/// Resolution priority: repo config → global config → empty list.
pub fn effective_env_passthrough(git_root: &Path) -> Vec<String> {
    let repo = load_repo_config(git_root).unwrap_or_default();
    if let Some(names) = repo.env_passthrough {
        return names;
    }
    let global = load_global_config().unwrap_or_default();
    global.env_passthrough.unwrap_or_default()
}
```

### 10. Inject passthrough vars at launch

In `src/commands/chat.rs` and `src/commands/implement.rs`, after resolving `credentials.env_vars`, call `effective_env_passthrough` and for each name, read the current process env (`std::env::var`) and append `(name, value)` pairs to the env_vars vector if the var is present on the host.

This is intentionally generic — it applies to all agents, not just maki. It is the user's responsibility to populate the allowlist with the names they want forwarded (e.g., `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `SYNTHETIC_API_KEY`, `ZHIPU_API_KEY`).

```rust
// In run() and run_with_sink(), after credentials.env_vars:
let mut env_vars = credentials.env_vars.clone();
let passthrough_names = effective_env_passthrough(&git_root);
for name in &passthrough_names {
    if let Ok(val) = std::env::var(name) {
        env_vars.push((name.clone(), val));
    }
}
```

The resolved `env_vars` (with passthrough appended) is then passed to `run_agent_with_sink` as before.

### 11. `amux ready` (`src/commands/ready.rs`)

Add `"maki"` to `dockerfile_matches_template` and any agent-string match arms so the ready check recognises the maki Dockerfile template.

### 12. `docs/` updates

Update the agent configuration section to document:
- `maki` as a supported agent value.
- `envPassthrough` config key (global and repo), its resolution priority, and a worked maki authentication example.


## Edge Case Considerations:

- **`envPassthrough` var not set on host**: if a listed var is absent from the host environment (`std::env::var` returns `Err`), silently skip it — do not error or warn. Users may list vars they set only in some contexts.
- **`envPassthrough` in both configs**: repo config wins entirely (not merged), consistent with the `yoloDisallowedTools` precedence model. Document this clearly.
- **Sensitive values in logs**: `run_args_display` and any display-only arg builders must mask values of passthrough vars the same way other env vars are masked. Confirm the existing `HostSettings` masking path covers runtime-injected `-e KEY=VALUE` args; if not, extend it.
- **maki `--yolo` flag name collision**: maki uses `--yolo` as its own autonomous flag. When amux passes `--yolo` to maki, it is the maki flag, not the amux flag. No conflict — but verify the flag name does not change across maki versions and add a comment in `append_yolo_flags`.
- **maki `--print` non-interactive mode**: maki's `--print` flag is equivalent to Claude's `-p`. Verify it exits after completing the prompt (no interactive TTY required) so `run_container_captured` works as expected for non-interactive flows.
- **Dockerfile.maki install path**: the maki installer places the binary in `$MAKI_INSTALL_DIR` (defaults to `~/.local/bin` on Linux). The `cp` step in the template ensures the binary lands in `/usr/local/bin/maki` for container use. If the installer changes its default path, the template must be updated.
- **Architecture support**: maki is a Rust binary. Verify the install script supports both `amd64` and `arm64` Linux. If it does not, use the same `dpkg --print-architecture` pattern as `Dockerfile.codex` to select the correct target triple from GitHub releases.
- **`envPassthrough` and security**: the mechanism explicitly requires the user to opt-in each variable by name — it cannot forward the entire host environment. This preserves the security constraint that containers receive only the minimum necessary secrets. Document this clearly.
- **Duplicate env vars**: if a passthrough var name is also returned by `keychain_credentials` (e.g., a user who sets `CLAUDE_CODE_OAUTH_TOKEN` in both keychain and `envPassthrough`), the passthrough append may create a duplicate `-e` flag. Either deduplicate by name (last-wins) or document that passthrough vars take lower precedence and skip any name already present in `credentials.env_vars`.


## Test Considerations:

- **`chat_entrypoint("maki", false)`**: returns `["maki"]`.
- **`chat_entrypoint_non_interactive("maki", false)`**: returns `["maki", "--print"]`.
- **`chat_entrypoint("maki", true)`** (plan mode): returns `["maki"]` (no plan flag, silently skipped).
- **`append_yolo_flags` for maki**: `--yolo` appended; `--disallowedTools` never appended; warning printed to stderr when `disallowed_tools` is non-empty.
- **`append_yolo_flags` for maki, no disallowed tools**: no warning printed.
- **`dockerfile_for_agent_embedded(Agent::Maki)`**: content contains `debian:bookworm-slim` and `maki.sh/install.sh`.
- **`dockerfile_matches_template`** with maki content: returns true for `"maki"`, false for `"claude"`.
- **`effective_env_passthrough` unit tests** (`src/config/mod.rs`):
  - Repo config wins over global.
  - Returns empty list when neither config sets the field.
  - Returns global list when repo config is absent.
- **Passthrough injection integration test**: set a test env var in the process, configure `envPassthrough` with its name, confirm the var appears in the constructed Docker run args.
- **Passthrough absent var test**: list a var in `envPassthrough` that is not set in the process env; confirm no entry is added to `env_vars` and no error is returned.
- **No duplicate passthrough test**: if the same var name appears in both `credentials.env_vars` and the passthrough list, verify the deduplication strategy is applied consistently.


## Codebase Integration:

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- The `Agent` enum in `src/cli.rs` and the string-matching in `src/commands/chat.rs` are the two canonical places that define agent identity. All other arms (`download.rs`, `init.rs`, `ready.rs`) are downstream and should be updated in the same commit.
- Mirror the `append_plan_flags` silent-skip pattern (used for opencode) for maki's plan-mode arm — no warning, no error.
- Mirror the `append_yolo_flags` warning pattern (used for codex/opencode disallowed tools) for maki's disallowed-tools arm.
- `effective_env_passthrough` should follow the exact same resolution pattern as `effective_yolo_disallowed_tools` in `src/config/mod.rs` — repo config → global config → empty default.
- The passthrough injection logic in `run()` belongs at the same level as the `resolve_auth` call, not buried inside `run_agent_with_sink` — keep credential resolution and passthrough injection co-located and easy to read.
- `src/commands/auth.rs` keychain logic is intentionally claude-only. Do not add maki keychain logic there. Maki authentication is entirely handled via `envPassthrough`.
- The `Dockerfile.maki` template must pass the same automated tests as other templates: uses `debian:bookworm-slim`, does not use `npm install`, installs via `apt-get` or direct download.
