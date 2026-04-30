# Work Item: Feature

Title: GitHub Copilot CLI, Crush, and Cline agents
Issue: issuelink

## Summary:
- Add `copilot` (GitHub Copilot CLI), `crush` (Charmbracelet Crush), and `cline` (Cline CLI) as first-class agents alongside the existing agents.
- Create `templates/Dockerfile.copilot`, `templates/Dockerfile.crush`, and `templates/Dockerfile.cline`.
- Wire all three into every agent dispatch path: `Agent` enum, `KNOWN_AGENT_NAMES`, `AGENT_DOCKERFILE_URLS`, `chat_entrypoint`, `chat_entrypoint_non_interactive`, `chat_entrypoint_with_prompt`, `append_plan_flags`, `append_autonomous_flags`, `append_model_flag`, `dockerfile_for_agent_embedded`, `download_dockerfile_template`, and `passthrough_for_agent`.
- Auth passthrough: copilot and crush use env vars only (`NoopPassthrough` + `envPassthrough`); cline uses a config dir mount (`ClinePassthrough` copies `~/.cline/data/` into a temp dir, same pattern as `GeminiPassthrough`).

---

## Agent Research Reference

This section documents the upstream facts gathered during research. All implementation decisions below derive from this data.

### 1. GitHub Copilot CLI (`copilot`)

**Install methods:**
- Bash script: `curl -fsSL https://gh.io/copilot-install | bash`
  (redirects to `https://raw.githubusercontent.com/github/copilot-cli/refs/heads/main/install.sh`)
- Homebrew: `brew install copilot-cli`
- npm: `npm install -g @github/copilot`
- WinGet (Windows): `winget install GitHub.Copilot`
- GitHub releases: `https://github.com/github/copilot-cli/releases/latest/download/copilot-{PLATFORM}-{ARCH}.tar.gz`
  - Platforms: `darwin`, `linux`; architectures: `x64`, `arm64`
  - Default install dir: `/usr/local/bin` (root) or `$HOME/.local/bin` (non-root)

**Binary name:** `copilot`

**Interactive launch:** `copilot` (no flags; drops into a TUI chat session)

**Non-interactive / headless mode:**
- `-p` flag puts copilot into **prompt mode** — reads from stdin, suppresses interactive permission prompts, exits when done
- Usage: `copilot -p` (reads stdin) or `echo "prompt" | copilot -p`
- `--output-format json` emits JSONL in prompt mode for scripting
- `--silent` suppresses stats output for scripting

**Prompt as CLI argument:**
- `-i` / `--interactive` flag accepts an **initial prompt string** that starts the session: `copilot -i "fix the bug in foo.rs"`
- `-i` with `-p` together: `-i "prompt"` starts a non-interactive session with that prompt; this is the pattern for `chat_entrypoint_with_prompt`

**Mode flags:**
- `--mode interactive` — default interactive mode
- `--mode autopilot` — autonomous task completion (experimental; equivalent to `--autopilot`)
- `--mode plan` — implementation planning (equivalent to `--plan`)
- `--autopilot` — direct shortcut for autopilot mode
- `--plan` — direct shortcut for plan mode

**Yolo / auto / allow-all flags:**
- `/allow-all` and `/yolo` are **slash commands** used inside an interactive session (not CLI flags)
- `--available-tools <tools>` — filter which tools the model can use
- `--excluded-tools <tools>` — exclude specific tools
- `--enable-all-github-mcp-tools` — enables all read-write GitHub MCP tools
- No standalone `--yolo` CLI flag (unlike claude/gemini/maki); autopilot mode is the closest equivalent

**Authentication:**
Environment variables (in precedence order):
1. `COPILOT_GITHUB_TOKEN` — highest precedence, dedicated copilot var
2. `GH_TOKEN` — standard GitHub CLI token
3. `GITHUB_TOKEN` — fallback
- `COPILOT_GH_HOST` — GitHub Enterprise hostname override (takes precedence over `GH_HOST`)
- `GITHUB_ASKPASS` — ASKPASS helper for authentication
- Fine-grained PAT with "Copilot Requests" permission also accepted

Auth method for containers: inject `COPILOT_GITHUB_TOKEN` or `GH_TOKEN` via `envPassthrough`. No OAuth config directory equivalent to `~/.gemini/`.

**Config file locations:**
- `~/.copilot/settings.json` — user-level settings (camelCase keys: `includeCoAuthoredBy`, `effortLevel`, `autoUpdatesChannel`, `statusLine`)
- `~/.copilot/settings.local.json` — local user settings overlay
- `~/.copilot/lsp-config.json` — LSP server configuration
- `.github/lsp.json` — repository-level LSP config
- Legacy (deprecated): `.github/copilot/config.json`

**Environment variables (non-auth):**
- `COPILOT_OFFLINE=true` — disables telemetry and restricts network to configured model providers
- `COPILOT_DISABLE_TERMINAL_TITLE=1` — opt out of terminal title updates
- `COPILOT_CLI=1` — set by copilot in subprocesses (git hooks etc.) for subprocess identification
- `COPILOT_GH_HOST` — GitHub hostname override

**Plan/read-only mode:** Yes — `--mode plan` or `--plan` flag.

**No plan mode flag:** N/A — plan mode is supported.

**Model selection:** Via `/model` interactive slash command (not a CLI flag). Default model: Claude Sonnet 4.5. Alternatives include Claude Sonnet 4 and GPT-5.

**BYOK:** Supports custom model providers. Azure OpenAI BYOK defaults to GA versionless v1 route.

---

### 2. Crush by Charmbracelet (`crush`)

**Install methods:**
- Homebrew: `brew install charmbracelet/tap/crush`
- npm: `npm install -g @charmland/crush`
- Go: `go install github.com/charmbracelet/crush@latest`
- Arch Linux: `yay -S crush-bin`
- Nix: `nix run github:numtide/nix-ai-tools#crush`
- Winget: `winget install charmbracelet.crush`
- Scoop: `scoop install crush`
- Debian/Ubuntu via APT, Fedora/RHEL via YUM repositories
- Pre-built binaries via GitHub releases

**Binary name:** `crush`

**Interactive launch:** `crush` (no args; drops into a TUI session)

**Non-interactive / headless mode:**
- `crush run "<prompt>"` — runs non-interactively with prompt as argument, streams output to stdout, exits when done
- Alias: `crush r "<prompt>"`
- Piped stdin: `cat README.md | crush run "make this more glamorous"`
- File redirection: `crush run "analyze this" <<< file.go`
- When no TTY is present, spinner/progress is suppressed automatically
- `CRUSH_CLIENT_SERVER=1` enables a client-server architecture (not needed for container use)

**Prompt as CLI argument:** `crush run "<prompt text>"` — prompt is all positional args joined with spaces.

**Non-interactive flags on `run` subcommand:**
- `--quiet` / `-q` — hides spinner animation
- `--verbose` / `-v` — logs to stderr
- `--model` / `-m` — selects the large model; accepts `model-name` or `provider/model` format
- `--small-model` — selects the small model
- `--session` / `-s` — continue a previous session by ID
- `--continue` / `-C` — resume most recent session (mutually exclusive with `--session`)

**Persistent / root flags:**
- `--yolo` / `-y` — "Automatically accept all permissions (dangerous mode)"
- `--cwd` / `-c` — working directory
- `--data-dir` / `-D` — custom crush data directory
- `--debug` / `-d` — debug mode
- `--host` / `-H` — connect to a specific crush server host

**Authentication:**
API keys via environment variables (no system keychain):
- `ANTHROPIC_API_KEY`
- `OPENAI_API_KEY`
- `GEMINI_API_KEY`, `GOOGLE_API_KEY`
- `GROQ_API_KEY`
- `OPENROUTER_API_KEY`
- `VERCEL_AI_API_KEY`
- `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, `AWS_PROFILE`, `AWS_BEARER_TOKEN_BEDROCK`
- `AZURE_OPENAI_API_ENDPOINT`, `AZURE_OPENAI_API_KEY`, `AZURE_OPENAI_API_VERSION`
- `VERTEXAI_PROJECT`, `VERTEXAI_LOCATION`
- And others (Groq, Hugging Face, Cerebras, etc.)

**Config file locations (priority order):**
1. `.crush.json` — project-local
2. `crush.json` — project-local (alternate filename)
3. `$HOME/.config/crush/crush.json` — user global

**Data/state storage:**
- Unix: `$HOME/.local/share/crush/crush.json`
- Windows: `%LOCALAPPDATA%\crush\crush.json`

**Config override env vars:**
- `CRUSH_GLOBAL_CONFIG` — override user global config path
- `CRUSH_GLOBAL_DATA` — override data/state directory

**Other env vars:**
- `CRUSH_DISABLE_METRICS=1` — opt out of usage metrics
- `DO_NOT_TRACK=1` — standard opt-out
- `CRUSH_DISABLE_PROVIDER_AUTO_UPDATE=1` — disable automatic provider updates
- `CRUSH_SKILLS_DIR` — custom skills directory

**Notable config options:**
- `allowed_tools` — pre-approve tools without prompting (equivalent to yolo per-tool)
- `disabled_tools` — hide tools from agent
- `disabled_skills` — prevent skills from loading
- `data_directory` — override data dir in config

**Plan/read-only mode:** No dedicated plan/read-only mode flag documented. The `task` agent type in config uses read-only tools only, but there is no `--plan` CLI flag equivalent.

**Model selection:** `--model` / `-m` flag on `crush run`, accepts `model-name` or `provider/model`.

**Context files crush reads automatically (in working dir):**
`.github/copilot-instructions.md`, `.cursorrules`, `.cursor/rules/`, `CLAUDE.md`, `CLAUDE.local.md`, `GEMINI.md`, `CRUSH.md`, `crush.md`, `AGENTS.md`

---

### 3. Cline CLI (`cline`)

**Install method:** `npm install -g cline`

**Binary name:** `cline`

**npm package name:** `cline` (version 2.17.0+; requires Node.js ≥ 20)

**Interactive launch:** `cline` (no args; starts interactive session using TUI or default mode)

**Non-interactive / headless mode:**
Non-interactive mode is triggered automatically when **any** of the following conditions are met:
- Output is redirected (stdout is not a TTY)
- Stdin is piped (`stdinWasPiped` detected)
- `--json` flag is used
- `--yolo` flag is used

**Prompt as CLI argument:** `cline task "<prompt>"` or the shorthand `cline t "<prompt>"`. Also: `cline "<prompt>"` (bare positional arg at root level triggers task).

**Task subcommand flags:**
- `-a, --act` — run in act mode
- `-p, --plan` — run in plan mode (read-only planning, no file modifications)
- `-y, --yolo` — auto-approve all actions (also triggers non-interactive mode)
- `--auto-approve-all` — auto-approve while keeping interactive mode
- `-m, --model <model>` — specify model ID (e.g. `claude-sonnet-4-6`, `gpt-4o`)
- `-t, --timeout <seconds>` — task timeout
- `-T, --taskId <id>` — resume existing task by ID
- `--continue` — resume most recent task in current directory
- `--json` — output as JSON instead of styled text (also triggers non-interactive)
- `-v, --verbose` — verbose output
- `--thinking [tokens]` — enable extended thinking (default: 1024 tokens)
- `--reasoning-effort <effort>` — reasoning level: `none|low|medium|high|xhigh`
- `--max-consecutive-mistakes <count>` — halt threshold in yolo mode
- `--double-check-completion` — force re-verification after completion
- `--auto-condense` — AI-powered context compaction
- `-c, --cwd <path>` — working directory
- `--config <path>` — custom cline configuration directory
- `--hooks-dir <path>` — additional hooks directory

**Root-level flags (without subcommand):**
- `--acp` — run in Agent Client Protocol mode
- `--tui` — use legacy terminal UI
- `--kanban` — launch kanban experience
- `--update` — check for updates

**Auth command:**
- `cline auth -p <provider> -k <apikey> -m <modelid>`
- Provider IDs: `anthropic`, `openai-native`, `openai-compatible`, `moonshot`, etc.
- `--baseurl <url>` — for OpenAI-compatible providers

**Authentication:**
No dedicated auth env vars — API keys are stored in config by the `cline auth` command.
- `CLINE_DIR` — override default config directory (default: `~/.cline/data/`)

**Config file locations:**
- `~/.cline/data/` — default config directory
  - `globalState.json` — global settings and state
  - `secrets.json` — API keys and secrets (written by `cline auth`)
  - `workspace/` — workspace-specific state
  - `tasks/` — task history and conversation data
- `--config <path>` — override config directory per-invocation
- `CLINE_DIR` env var — override config directory globally

**Config directory mount strategy:**
For auth passthrough into containers, mount `~/.cline/data/` (or a filtered copy) at `/home/amux/.cline/data/` inside the container. The `secrets.json` file contains the actual API keys. Unlike claude/gemini, cline has no keychain integration — secrets live only in `secrets.json`.

**Plan/read-only mode:** Yes — `-p` / `--plan` flag on the `task` subcommand.

**Model selection:** `-m, --model <model>` flag on the `task` subcommand.

**Supported providers (via `cline auth`):** Anthropic, OpenAI (native and compatible), Google Gemini, AWS Bedrock, Azure OpenAI, GCP Vertex, Cerebras, Groq, Moonshot, OpenAI-compatible APIs, local models via LM Studio/Ollama.

---

## User Stories

### User Story 1:
As a: user

I want to: run `amux init --agent copilot` and `amux chat` to start a GitHub Copilot session inside a container

So I can: use GitHub Copilot as my coding agent with the same amux workflow I use for claude and gemini, authenticating via `COPILOT_GITHUB_TOKEN` or `GH_TOKEN` in `envPassthrough`, without any manual Dockerfile or auth setup.

### User Story 2:
As a: user

I want to: run `amux init --agent cline` and `amux chat` to start a Cline session inside a container

So I can: use Cline as my coding agent with my `~/.cline/data/` config directory (including `secrets.json`) mounted automatically, without re-running `cline auth` inside the container every session.

### User Story 3:
As a: user

I want to: run `amux chat --yolo` or `amux implement <wi> --plan` with copilot, crush, or cline active

So I can: use their autonomous and planning modes with the same amux UX flags I use for all other agents, with correct flag translation per agent (e.g. crush's `--yolo` inserted before its `run` subcommand, cline's `--plan` appended to its `task` subcommand).

---

## Implementation Details

### 1. `Agent` enum (`src/cli.rs`)

Add three new variants:

```rust
#[derive(Clone, Debug, PartialEq, ValueEnum)]
pub enum Agent {
    Claude,
    Codex,
    Opencode,
    Maki,
    Gemini,
    Copilot,
    Crush,
    Cline,
}

impl Agent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Opencode => "opencode",
            Agent::Maki => "maki",
            Agent::Gemini => "gemini",
            Agent::Copilot => "copilot",
            Agent::Crush => "crush",
            Agent::Cline => "cline",
        }
    }
    // all_agents() must also include the three new variants
}
```

### 2. Dockerfile templates

#### `templates/Dockerfile.copilot`

GitHub Copilot CLI is a native binary distributed via GitHub releases (linux/darwin, x64/arm64). Install via the official install script which selects the correct platform binary.

```dockerfile
FROM {{AMUX_BASE_IMAGE}}

# Install GitHub Copilot CLI via official install script.
# The script detects platform (linux) and arch (x64/arm64) automatically.
# Installs to /usr/local/bin/copilot for root users.
RUN curl -fsSL https://gh.io/copilot-install | bash

# Create non-root user for agent operations
RUN useradd -m -s /bin/bash amux \
    && mkdir -p /workspace \
    && chown amux:amux /workspace

USER amux
WORKDIR /workspace
```

**Note on the npm test exemption:** The existing test `dockerfile_for_agent_embedded_does_not_use_npm_install` checks that templates do not use bare `npm install` (without `-g`). The copilot Dockerfile does not use npm at all, so no exemption is needed.

#### `templates/Dockerfile.crush`

Crush is a Go binary distributed via GitHub releases. Use the npm install path (`npm install -g @charmland/crush`) since it is cross-platform and works well in containers with Node.js 20. Alternatively, install via the Charmbracelet APT/YUM repositories for linux containers. The npm path is simplest:

```dockerfile
FROM {{AMUX_BASE_IMAGE}}

RUN apt-get update && apt-get install -y --no-install-recommends \
    gnupg \
    && curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*

# Install Crush via npm (cross-platform, requires Node.js 20+)
RUN npm install -g @charmland/crush

# Create non-root user for agent operations
RUN useradd -m -s /bin/bash amux \
    && mkdir -p /workspace \
    && chown amux:amux /workspace

USER amux
WORKDIR /workspace
```

**Note on the npm test exemption:** The `dockerfile_for_agent_embedded_does_not_use_npm_install` test must be updated to exempt `Agent::Crush` (and `Agent::Cline` below), the same way `Agent::Gemini` was previously exempted. Add a comment explaining that `npm install -g` is the official distribution method for these agents.

#### `templates/Dockerfile.cline`

Cline CLI is distributed as an npm package (`npm install -g cline`). Requires Node.js ≥ 20.

```dockerfile
FROM {{AMUX_BASE_IMAGE}}

RUN apt-get update && apt-get install -y --no-install-recommends \
    gnupg \
    && curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*

# Install Cline CLI via npm (requires Node.js 20+)
RUN npm install -g cline

# Create non-root user for agent operations
RUN useradd -m -s /bin/bash amux \
    && mkdir -p /workspace \
    && chown amux:amux /workspace

USER amux
WORKDIR /workspace
```

Same npm test exemption note as crush applies.

### 3. Embedded Dockerfile dispatch (`src/commands/init.rs`)

Add arms to `dockerfile_for_agent_embedded`:

```rust
Agent::Copilot => include_str!("../../templates/Dockerfile.copilot").to_string(),
Agent::Crush => include_str!("../../templates/Dockerfile.crush").to_string(),
Agent::Cline => include_str!("../../templates/Dockerfile.cline").to_string(),
```

### 4. Download dispatch (`src/commands/download.rs`)

Add arms to `download_dockerfile_template`:

```rust
Agent::Copilot => "Dockerfile.copilot",
Agent::Crush => "Dockerfile.crush",
Agent::Cline => "Dockerfile.cline",
```

### 5. Chat entrypoints (`src/commands/chat.rs`)

#### `chat_entrypoint` (interactive, no prompt)

```rust
"copilot" => vec!["copilot".to_string()],
"crush"   => vec!["crush".to_string()],
// cline's interactive entry is via the `task` subcommand (bare `cline` may
// enter a different UI mode depending on version; `cline task` is stable).
"cline"   => vec!["cline".to_string(), "task".to_string()],
```

#### `chat_entrypoint_non_interactive` (non-interactive, no prompt)

```rust
// copilot: -p puts copilot into prompt/non-interactive mode (reads from stdin)
"copilot" => vec!["copilot".to_string(), "-p".to_string()],
// crush: `crush run` with no additional args; prompt supplied separately via stdin or args
"crush"   => vec!["crush".to_string(), "run".to_string()],
// cline: `cline task --json` triggers non-interactive (structured) output mode
// without implying autonomous operation. `--yolo` is added separately by
// append_autonomous_flags when the user passes --yolo to amux.
"cline"   => vec!["cline".to_string(), "task".to_string(), "--json".to_string()],
```

**Note on cline non-interactive trigger:** Cline enters non-interactive mode when stdout is redirected, `--json` is used, or `--yolo` is used. Since amux always redirects container stdout, `--json` is a belt-and-suspenders flag to ensure structured output and prevent interactive prompts. `--yolo` is added separately by `append_autonomous_flags` when the user requests it; do not bake it into the non-interactive entrypoint.

#### `chat_entrypoint_with_prompt` (non-interactive, prompt as arg)

```rust
// copilot: -p (prompt mode) + -i <prompt> (initial prompt string)
"copilot" => vec!["copilot".to_string(), "-p".to_string(), "-i".to_string(), prompt.to_string()],
// crush: `crush run "<prompt>"` — prompt is positional argument
"crush"   => vec!["crush".to_string(), "run".to_string(), prompt.to_string()],
// cline: `cline task "<prompt>"` — autonomous flags added separately by append_autonomous_flags
"cline"   => vec!["cline".to_string(), "task".to_string(), prompt.to_string()],
```

### 6. Plan flags (`src/commands/chat.rs` — `append_plan_flags`)

```rust
// copilot: --plan flag starts directly in plan mode
"copilot" => {
    args.push("--plan".to_string());
}
// crush: no dedicated plan/read-only mode; silently skip
"crush" => {}
// cline: -p / --plan flag on the task subcommand
"cline" => {
    args.push("--plan".to_string());
}
```

Update the doc comment on `append_plan_flags` to document copilot (`--plan`), crush (none), and cline (`--plan`).

### 7. Autonomous flags (`src/commands/agent.rs` — `append_autonomous_flags`)

```rust
"copilot" => {
    // copilot's only CLI autonomous mode is --autopilot (equivalent to yolo).
    // There is no CLI-level --yolo flag for copilot; /yolo is an interactive slash command only.
    // Both amux --yolo and --auto map to --autopilot (copilot has no finer-grained auto-edit mode).
    args.push("--autopilot".to_string());
    if !disallowed_tools.is_empty() {
        eprintln!(
            "WARNING: {}: copilot does not support --disallowedTools via CLI flags; \
             yoloDisallowedTools config will be ignored.",
            flag_name
        );
    }
}

"crush" => {
    // crush's --yolo is a persistent root flag that MUST precede the `run`
    // subcommand: `crush --yolo run "prompt"`. Insert at index 1 (after "crush",
    // before "run") rather than pushing to the end.
    // Both --yolo and --auto map here because crush has no intermediate mode.
    args.insert(1, "--yolo".to_string());
    if !yolo {
        // --auto was requested; crush has no intermediate mode, so map to --yolo.
        eprintln!(
            "WARNING: {}: crush has no intermediate permission mode; \
             mapping --auto to --yolo (crush's only autonomous flag).",
            flag_name
        );
    }
    if !disallowed_tools.is_empty() {
        eprintln!(
            "WARNING: {}: crush does not support --disallowedTools; \
             yoloDisallowedTools config will be ignored.",
            flag_name
        );
    }
}

"cline" => {
    if yolo {
        // cline's --yolo skips all tool-call confirmations and implies non-interactive mode.
        args.push("--yolo".to_string());
    } else {
        // --auto maps to --auto-approve-all (keeps interactive mode but auto-approves actions).
        args.push("--auto-approve-all".to_string());
    }
    if !disallowed_tools.is_empty() {
        eprintln!(
            "WARNING: {}: cline does not support --disallowedTools via CLI flags; \
             yoloDisallowedTools config will be ignored.",
            flag_name
        );
    }
}
```

**Implementation note on crush `--yolo` placement:** Crush's `--yolo` is a persistent root flag that must appear before the `run` subcommand. The entrypoint vector is `["crush", "run", <prompt>]`. Inserting at index 1 produces `["crush", "--yolo", "run", <prompt>]`. Verify this is correct by checking crush's cobra-based CLI flag parsing (persistent flags are parsed before subcommand dispatch).

Update the doc comment on `append_autonomous_flags` to include copilot, crush, and cline mappings.

### 8. Auth passthrough (`src/passthrough.rs`)

#### `CopilotPassthrough`

GitHub Copilot authenticates via `COPILOT_GITHUB_TOKEN` or `GH_TOKEN`. These are passed via `envPassthrough` — no config directory needs mounting (copilot has no equivalent of `~/.gemini/` with stored OAuth tokens in a file-based format suitable for container reuse; the token is stateless and short-lived).

```rust
/// Passthrough for the GitHub Copilot CLI agent.
///
/// - **Keychain**: none (copilot does not use the system keychain).
/// - **Env vars**: none hardcoded; auth via envPassthrough (COPILOT_GITHUB_TOKEN or GH_TOKEN).
/// - **Settings**: no config directory mounting needed. Copilot config lives in
///   `~/.copilot/settings.json` but contains only UX preferences, not auth tokens.
///   Auth is entirely token-based via env vars.
pub struct CopilotPassthrough;

impl AgentPassthrough for CopilotPassthrough {
    fn prepare_host_settings(&self) -> Option<HostSettings> {
        None
    }
    fn prepare_host_settings_to_dir(&self, _dir: &Path) -> Option<HostSettings> {
        None
    }
}
```

#### `CrushPassthrough`

Crush authenticates entirely via API key env vars. No config directory to mount.

```rust
/// Passthrough for the Crush agent (Charmbracelet).
///
/// - **Keychain**: none.
/// - **Env vars**: none hardcoded; auth via envPassthrough (ANTHROPIC_API_KEY, OPENAI_API_KEY, etc.).
/// - **Settings**: no config directory mounting needed. Crush's global config is at
///   `~/.config/crush/crush.json` but contains provider/model setup, not secrets.
///   Secrets are API keys passed via env vars.
pub struct CrushPassthrough;

impl AgentPassthrough for CrushPassthrough {
    fn prepare_host_settings(&self) -> Option<HostSettings> {
        None
    }
    fn prepare_host_settings_to_dir(&self, _dir: &Path) -> Option<HostSettings> {
        None
    }
}
```

#### `ClinePassthrough`

Cline stores API keys in `~/.cline/data/secrets.json`. This directory must be mounted (read-only from a temp copy) for auth passthrough.

```rust
/// Top-level entries in `~/.cline/data/` to exclude from the container copy.
const CLINE_DATA_DENYLIST: &[&str] = &["tasks", "workspace"];

/// Passthrough for the Cline CLI agent.
///
/// - **Keychain**: none (cline does not use the system keychain).
/// - **Env vars**: none (API keys stored in ~/.cline/data/secrets.json).
/// - **Settings**: copies `~/.cline/data/` (minus task history and workspace state)
///   into a temp dir and mounts it at `/home/amux/.cline/data` inside the container.
///   The mount is read-write (temp copy, not the live host dir).
///   If `~/.cline/data/` does not exist on the host, creates an empty temp dir and
///   mounts that instead, so the container starts with no credentials (cline will
///   prompt for auth on first use).
pub struct ClinePassthrough;

impl AgentPassthrough for ClinePassthrough {
    fn prepare_host_settings(&self) -> Option<HostSettings> {
        let home = dirs::home_dir()?;
        let src = home.join(".cline").join("data");
        let temp_dir = tempfile::TempDir::new().ok()?;
        let dst = temp_dir.path().join("cline-data");
        if src.exists() {
            crate::runtime::copy_dir_filtered(&src, &dst, CLINE_DATA_DENYLIST).ok()?;
        } else {
            std::fs::create_dir_all(&dst).ok()?;
        }
        Some(HostSettings::new_agent_dir(
            Some(temp_dir),
            "/root".to_string(),
            Some((dst, "/home/amux/.cline/data".to_string())),
        ))
    }

    fn prepare_host_settings_to_dir(&self, dir: &Path) -> Option<HostSettings> {
        let home = dirs::home_dir()?;
        let src = home.join(".cline").join("data");
        std::fs::create_dir_all(dir).ok()?;
        let dst = dir.join("cline-data");
        if src.exists() {
            crate::runtime::copy_dir_filtered(&src, &dst, CLINE_DATA_DENYLIST).ok()?;
        } else {
            std::fs::create_dir_all(&dst).ok()?;
        }
        Some(HostSettings::new_agent_dir(
            None,
            "/root".to_string(),
            Some((dst, "/home/amux/.cline/data".to_string())),
        ))
    }
}
```

#### `passthrough_for_agent` dispatch

```rust
pub fn passthrough_for_agent(agent: &str) -> Box<dyn AgentPassthrough> {
    match agent {
        "claude"   => Box::new(ClaudePassthrough),
        "opencode" => Box::new(OpencodePassthrough),
        "codex"    => Box::new(CodexPassthrough),
        "gemini"   => Box::new(GeminiPassthrough),
        "copilot"  => Box::new(CopilotPassthrough),
        "crush"    => Box::new(CrushPassthrough),
        "cline"    => Box::new(ClinePassthrough),
        _          => Box::new(NoopPassthrough),
    }
}
```

### 9. `amux ready` (`src/commands/ready.rs`)

Add `"copilot"`, `"crush"`, and `"cline"` to `dockerfile_matches_template` and any agent-string match arms. Detection strings:
- copilot: presence of `gh.io/copilot-install` or `github/copilot-cli`
- crush: presence of `@charmland/crush`
- cline: presence of `npm install -g cline` (or similar; must be distinct from `@charmland/crush`)

### 10. Auth usage patterns

**Copilot:** User sets `envPassthrough: ["COPILOT_GITHUB_TOKEN"]` (or `"GH_TOKEN"`) in global or repo config. The token must have the "Copilot Requests" fine-grained PAT permission, or be a standard GitHub OAuth token obtained via `gh auth token`.

**Crush:** User sets `envPassthrough: ["ANTHROPIC_API_KEY"]` (or whichever provider they use) in global or repo config. Multiple API keys can be listed; only those present in the host environment are forwarded.

**Cline:** No env var needed. `ClinePassthrough` copies `~/.cline/data/` (excluding task history) into a temp dir and mounts it at `/home/amux/.cline/data`. The `secrets.json` inside carries all provider API keys set up via `cline auth`. If `~/.cline/data/` is absent, an empty dir is mounted and cline will prompt for `cline auth` on first interactive use inside the container.

### 11. `docs/` updates

Update the agent configuration section to document:
- `copilot`, `crush`, `cline` as supported agent values.
- Auth options for each: envPassthrough vars for copilot and crush; automatic `~/.cline/data/` mount for cline.
- Flag mappings: `--yolo`, `--auto`, `--plan` for each agent.
- Note that copilot's `/yolo` slash command is interactive-only; `--autopilot` is the CLI equivalent.

---

## Edge Case Considerations

**Copilot `--autopilot` vs. `--yolo`:** Copilot has no standalone CLI `--yolo` flag. Both amux `--yolo` and amux `--auto` map to copilot's `--autopilot` flag. This is intentional — autopilot is copilot's only autonomous mode at the CLI level. Document this clearly.

**Copilot `-i` flag syntax for `chat_entrypoint_with_prompt`:** The `-i` flag takes the prompt as the next argument (`-i "text"`), not as `--interactive "text"`. Verify against the copilot changelog that `-i` is stable and not renamed.

**Crush `--yolo` insertion position:** Crush's `--yolo` is a persistent root flag. When `append_autonomous_flags` is called, the vector already contains `["crush", "run", ...]` (or `["crush", "run", "<prompt>", ...]`). Insert `--yolo` at index 1 (between `"crush"` and `"run"`). Write a dedicated test verifying the insertion position.

**Crush plan mode absent:** Crush has no `--plan` or read-only mode. When `amux chat --plan` is used with crush, silently skip (same pattern as maki and opencode). Document this behavior.

**Cline `--json` + `--yolo` combination:** `chat_entrypoint_non_interactive` for cline uses `--json`. When `append_autonomous_flags` is additionally called with `yolo=true`, both `--json` and `--yolo` appear in the args. Verify cline handles this combination gracefully (both flags are compatible — `--yolo` implies non-interactive, and `--json` adds structured output).

**Cline `--plan` position:** Cline's `--plan` flag belongs to the `task` subcommand. The entrypoint for plan mode is `["cline", "task", "--plan", ...]` (interactive) and `["cline", "task", "--yolo", "--plan", ...]` (non-interactive). `append_plan_flags` pushes `--plan` after the subcommand is already in the vector, so the order is correct.

**Cline `~/.cline/data/` missing:** If `~/.cline/data/` does not exist (user has never run `cline auth` on the host), `ClinePassthrough` must not error or return `None` — it must create an empty temp dir and mount that. Cline will prompt for auth on first interactive use inside the container.

**Cline `secrets.json` contains API keys:** The secrets file is sensitive. `copy_dir_filtered` already excludes task history (`tasks/`) and workspace state (`workspace/`), but does copy `secrets.json` and `globalState.json`. This is intentional — these are the files cline needs for auth. The copy goes into a `tempfile::TempDir` (cleaned up on drop), never the live host dir.

**Copilot telemetry / offline:** In container environments, set `COPILOT_OFFLINE=true` optionally to suppress telemetry and restrict network to configured model providers. Consider adding this as a default env var in `CopilotPassthrough::extra_env_vars()` or documenting it as an `envPassthrough` candidate.

**Crush npm global binary path:** `npm install -g @charmland/crush` places the binary at `/usr/local/bin/crush` under the standard npm global prefix. Verify with `which crush` in a test container build. Add an `ENV PATH` line if needed.

**Cline npm global binary path:** Same as crush — `npm install -g cline` places `cline` at `/usr/local/bin/cline`. Verify.

**Node.js version for crush and cline Dockerfiles:** Both require Node.js ≥ 20. Use the NodeSource 20.x setup script. Same pattern as `Dockerfile.gemini`.

**Copilot GitHub Enterprise:** `COPILOT_GH_HOST` can override the GitHub hostname for GHE users. Document in `envPassthrough` examples.

**Crush config in container:** Crush reads `.crush.json` from the working directory (project-local config) and `$HOME/.config/crush/crush.json` (global). The working directory is mounted as `/workspace` in amux containers, so project-local `.crush.json` files work automatically. No additional mounts are needed for the global config since auth is via env vars.

**Dockerfile npm test:** The test `dockerfile_for_agent_embedded_does_not_use_npm_install` currently exempts `Agent::Gemini`. It must also exempt `Agent::Crush` and `Agent::Cline`. The exemption rationale (global CLI install via npm is the official distribution method) applies equally.

---

## Test Considerations

### chat_entrypoint tests

- `chat_entrypoint("copilot", false)` → `["copilot"]`
- `chat_entrypoint("copilot", true)` → `["copilot", "--plan"]`
- `chat_entrypoint("crush", false)` → `["crush"]`
- `chat_entrypoint("crush", true)` → `["crush"]` (no plan mode; silently skipped)
- `chat_entrypoint("cline", false)` → `["cline", "task"]`
- `chat_entrypoint("cline", true)` → `["cline", "task", "--plan"]`

- `chat_entrypoint_non_interactive("copilot", false)` → `["copilot", "-p"]`
- `chat_entrypoint_non_interactive("copilot", true)` → `["copilot", "-p", "--plan"]`
- `chat_entrypoint_non_interactive("crush", false)` → `["crush", "run"]`
- `chat_entrypoint_non_interactive("crush", true)` → `["crush", "run"]` (no plan flag)
- `chat_entrypoint_non_interactive("cline", false)` → `["cline", "task", "--json"]`
- `chat_entrypoint_non_interactive("cline", true)` → `["cline", "task", "--json", "--plan"]`

- `chat_entrypoint_with_prompt("copilot", "fix bug", false)` → `["copilot", "-p", "-i", "fix bug"]`
- `chat_entrypoint_with_prompt("copilot", "fix bug", true)` → `["copilot", "-p", "-i", "fix bug", "--plan"]`
- `chat_entrypoint_with_prompt("crush", "fix bug", false)` → `["crush", "run", "fix bug"]`
- `chat_entrypoint_with_prompt("crush", "fix bug", true)` → `["crush", "run", "fix bug"]` (no plan)
- `chat_entrypoint_with_prompt("cline", "fix bug", false)` → `["cline", "task", "fix bug"]` (no `--yolo` for explicit-prompt path; autonomous flags appended separately by `append_autonomous_flags`)
- `chat_entrypoint_with_prompt("cline", "fix bug", true)` → `["cline", "task", "fix bug", "--plan"]`

### append_autonomous_flags tests

**copilot:**
- `yolo=true` → `--autopilot` appended; `--disallowedTools` never appended; warning if `disallowed_tools` non-empty
- `yolo=false, auto=true` → `--autopilot` appended
- `yolo=true, disallowed_tools=["bash"]` → `--autopilot` appended + warning to stderr

**crush:**
- `yolo=true`, base `["crush", "run"]` → `["crush", "--yolo", "run"]` (inserted at index 1)
- `yolo=true`, base `["crush"]` (interactive) → `["crush", "--yolo"]`
- `yolo=true`, base `["crush", "run", "prompt"]` → `["crush", "--yolo", "run", "prompt"]`
- `auto=true` → `--yolo` inserted at index 1 + warning to stderr (no intermediate mode)
- `disallowed_tools` non-empty → warning to stderr; `--yolo` still inserted

**cline:**
- `yolo=true` → `--yolo` appended; `--json` may already be present from `chat_entrypoint_non_interactive` (compatible)
- `auto=true` → `--auto-approve-all` appended; `--yolo` not appended
- `yolo=true AND auto=true` → yolo wins; `--yolo` appended, `--auto-approve-all` not appended
- `disallowed_tools` non-empty → warning to stderr; no disallowed-tools flag forwarded

### Passthrough tests

**CopilotPassthrough:**
- `keychain_credentials()` → empty `AgentCredentials`
- `extra_env_vars()` → empty `Vec`
- `prepare_host_settings()` → `None`
- `prepare_host_settings_to_dir(dir)` → `None`
- `passthrough_for_agent("copilot")` → returns CopilotPassthrough-backed impl

**CrushPassthrough:**
- Same shape as CopilotPassthrough — all `None` / empty

**ClinePassthrough:**
- `prepare_host_settings()` when `~/.cline/data/` exists → `Some` with `agent_config_dir = Some((temp_copy, "/home/amux/.cline/data"))`, `mount_claude_files = false`
- `prepare_host_settings()` when `~/.cline/data/` does not exist → `Some` (empty temp dir fallback), not `None`, not panic
- `prepare_host_settings_to_dir(dir)` → same contract with caller-supplied dir
- Task history excluded: `CLINE_DATA_DENYLIST` contains `"tasks"` and `"workspace"`; verify filtered copy does not include them
- `passthrough_for_agent("cline")` → ClinePassthrough-backed impl

### Dockerfile tests

- `dockerfile_for_agent_embedded(Agent::Copilot)` → contains `debian:bookworm-slim` and `gh.io/copilot-install`
- `dockerfile_for_agent_embedded(Agent::Crush)` → contains `debian:bookworm-slim`, `nodesource`, `@charmland/crush`
- `dockerfile_for_agent_embedded(Agent::Cline)` → contains `debian:bookworm-slim`, `nodesource`, `npm install -g cline`
- Updated `dockerfile_for_agent_embedded_does_not_use_npm_install` → `Agent::Crush` and `Agent::Cline` explicitly exempted with comment
- Updated `dockerfile_for_agent_embedded_uses_debian_slim_base` → loop includes `Agent::Copilot`, `Agent::Crush`, `Agent::Cline`

### Ready tests

- `dockerfile_matches_template` with copilot content → returns `true` for `"copilot"`, `false` for `"claude"`
- `dockerfile_matches_template` with crush content → returns `true` for `"crush"`, `false` for `"claude"`
- `dockerfile_matches_template` with cline content → returns `true` for `"cline"`, `false` for `"crush"`

### implement.rs entrypoint tests (`src/commands/implement.rs`)

These tests cover the `amux implement` dispatch path, which is distinct from the chat path tested
above. `agent_entrypoint` and `agent_entrypoint_non_interactive` are used by both CLI and TUI
modes for the `amux implement` command; `workflow_step_entrypoint` is used for multi-step workflow
execution; `append_plan_flags` in implement.rs is a separate function from the one in chat.rs.

#### `agent_entrypoint` (interactive)

- `agent_entrypoint("copilot", "fix bug in foo.rs", false)` → `["copilot", "-i", "fix bug in foo.rs"]`
- `agent_entrypoint("copilot", "fix bug in foo.rs", true)` → `["copilot", "-i", "fix bug in foo.rs", "--plan"]`
  (plan flag appended after the prompt)
- `agent_entrypoint("crush", "fix bug in foo.rs", false)` → `["crush", "run", "fix bug in foo.rs"]`
- `agent_entrypoint("crush", "fix bug in foo.rs", true)` → `["crush", "run", "fix bug in foo.rs"]`
  (no plan flag for crush; silently skipped)
- `agent_entrypoint("cline", "fix bug in foo.rs", false)` → `["cline", "task", "fix bug in foo.rs"]`
- `agent_entrypoint("cline", "fix bug in foo.rs", true)` → `["cline", "task", "fix bug in foo.rs", "--plan"]`

#### `agent_entrypoint_non_interactive`

- `agent_entrypoint_non_interactive("copilot", "fix bug in foo.rs", false)` → `["copilot", "-p", "-i", "fix bug in foo.rs"]`
- `agent_entrypoint_non_interactive("copilot", "fix bug in foo.rs", true)` → `["copilot", "-p", "-i", "fix bug in foo.rs", "--plan"]`
- `agent_entrypoint_non_interactive("crush", "fix bug in foo.rs", false)` → `["crush", "run", "fix bug in foo.rs"]`
- `agent_entrypoint_non_interactive("crush", "fix bug in foo.rs", true)` → `["crush", "run", "fix bug in foo.rs"]`
  (no plan flag; crush has no plan mode)
- `agent_entrypoint_non_interactive("cline", "fix bug in foo.rs", false)` → `["cline", "task", "--json", "fix bug in foo.rs"]`
- `agent_entrypoint_non_interactive("cline", "fix bug in foo.rs", true)` → `["cline", "task", "--json", "fix bug in foo.rs", "--plan"]`

#### `workflow_step_entrypoint`

- `workflow_step_entrypoint("copilot", "step prompt", true)` (non-interactive) → `["copilot", "-p", "-i", "step prompt"]`
- `workflow_step_entrypoint("copilot", "step prompt", false)` (interactive) → `["copilot", "-i", "step prompt"]`
- `workflow_step_entrypoint("crush", "step prompt", true)` → `["crush", "run", "step prompt"]`
- `workflow_step_entrypoint("crush", "step prompt", false)` → `["crush", "run", "step prompt"]`
  (crush `run` is always non-interactive; both modes produce the same vector)
- `workflow_step_entrypoint("cline", "step prompt", true)` → `["cline", "task", "--json", "step prompt"]`
- `workflow_step_entrypoint("cline", "step prompt", false)` → `["cline", "task", "step prompt"]`

#### `append_plan_flags` in `implement.rs`

Note: `implement.rs` has its own `append_plan_flags` separate from the one in `chat.rs`.
Both must be kept in sync for the new agents.

- `append_plan_flags("copilot", &mut args)` → `args` gains `"--plan"` at end
- `append_plan_flags("crush", &mut args)` → `args` unchanged (crush has no plan mode)
- `append_plan_flags("cline", &mut args)` → `args` gains `"--plan"` at end
- `append_plan_flags("maki", &mut args)` → `args` unchanged (maki has no plan mode; regression guard)

#### ClinePassthrough mount path after `apply_dockerfile_user` remapping

The passthrough spec says the container path uses `/root/.cline/data` as the key, which gets
remapped by `apply_dockerfile_user` to `/home/amux/.cline/data` because `Dockerfile.cline` sets
`USER amux`. Verify this remapping chain:

- `ClinePassthrough::prepare_host_settings()` returns `HostSettings` where `agent_config_dir`
  has destination path `/root/.cline/data`
- After `apply_dockerfile_user(host_settings, "Dockerfile.cline content with USER amux")`,
  the destination path becomes `/home/amux/.cline/data`
- The final Docker `-v` mount maps the temp copy to `/home/amux/.cline/data` inside the container

#### End-to-end entrypoint scenarios

These describe integration-level tests that exercise the full command-building pipeline:

- `amux implement 0001 --agent copilot` → Docker command contains `copilot -i <prompt text>`
  (interactive, no plan, no yolo)
- `amux implement 0001 --agent copilot --non-interactive` → Docker command contains
  `copilot -p -i <prompt text>`
- `amux implement 0001 --agent copilot --plan` → Docker command contains `copilot -i <prompt text> --plan`
- `amux implement 0001 --agent crush` → Docker command contains `crush run <prompt text>`
- `amux implement 0001 --agent crush --plan` → Docker command contains `crush run <prompt text>`
  (no `--plan`; silently skipped for crush; no error or warning to user)
- `amux implement 0001 --agent crush --yolo` → Docker command contains `crush --yolo run <prompt text>`
  (`--yolo` inserted at index 1, before `run`)
- `amux implement 0001 --agent cline` → Docker command contains `cline task <prompt text>`
- `amux implement 0001 --agent cline --non-interactive` → Docker command contains
  `cline task --json <prompt text>`
- `amux implement 0001 --agent cline --plan` → Docker command contains
  `cline task <prompt text> --plan`
- `amux implement 0001 --agent cline --yolo` → Docker command contains
  `cline task <prompt text> --yolo`

---

## Codebase Integration

- Follow established conventions, best practices, testing, and architecture patterns from the project's `aspec/`.
- The `Agent` enum in `src/cli.rs` and the string-matching in `src/commands/chat.rs` are the two canonical agent identity registration points. All downstream arms (`download.rs`, `init.rs`, `ready.rs`, `agent.rs`, `passthrough.rs`) must be updated in the same commit.
- `CopilotPassthrough` and `CrushPassthrough` are minimal structs (both return `None` from `prepare_host_settings`). They exist for consistency with the passthrough dispatch pattern and to provide a hook for future expansion (e.g., if copilot adds file-based OAuth tokens).
- `ClinePassthrough` follows the exact same pattern as `GeminiPassthrough` — copy a config directory into a temp dir, mount it at a known container path. Use `copy_dir_filtered` with the `CLINE_DATA_DENYLIST`.
- The `append_autonomous_flags` for crush requires inserting `--yolo` at index 1 rather than pushing to the end. This is unique to crush (all other agents accept autonomous flags at the end). Add a comment explaining why.
- `src/commands/auth.rs` keychain logic is intentionally claude-only. Do not add copilot/crush/cline keychain logic there.
- Auth for copilot and crush is entirely via `envPassthrough`. Auth for cline is via `ClinePassthrough` directory mount. Both patterns are already established by WI-44 (maki) and WI-45 (gemini) respectively.
- The three new Dockerfiles using Node.js 20 follow the same NodeSource pattern as `Dockerfile.gemini`. Consider extracting a shared base image (`FROM debian-node20-base`) in a future work item if the count of Node.js-based Dockerfiles grows.
