# Work Item: Feature

Title: Support OpenCode V2 as the built-in `opencode` agent
Issue: issuelink

## Summary:

- Move awman's built-in `opencode` agent to OpenCode V2. The current [Dockerfile](../../templates/Dockerfile.opencode) downloads GitHub's `releases/latest` binary, which can remain on the V1 line even when V2 is available. V2 has a separate [installation path](https://opencode.ai/v2/docs/) and deliberately changed its CLI, credential storage, plugins, and server API ([migration guide](https://opencode.ai/v2/docs/migrate-v1/)).
- Keep the existing `--agent opencode` spelling. Install an explicit V2 version in every runtime that awman manages, verify that version, and adapt invocation, skills, authentication, and permission handling. Do not rewrite the user's OpenCode configuration or mount their host V2 credential database into a container.
- Scope this item to awman's current stdio/PTY integration. V2's `opencode acp` is real ([ACP guide](https://opencode.ai/v2/docs/cli/acp/)), but enabling ACP needs a separate protocol-level verification of awman's ACP client and is not a prerequisite for V2 CLI support. awman does not use OpenCode's HTTP server API or plugin API directly, so those V2 breaking changes require migration guidance, not an awman adapter.

## User Stories

### User Story 1:
As a: user

I want to:
launch OpenCode V2 with `awman chat --agent opencode` and `awman exec prompt --agent opencode`

So I can:
use the current OpenCode release in an isolated awman session, with a seeded prompt and an optional model where the selected interface supports it

### User Story 2:
As a: user running automated workflows

I want to:
run a V2-backed OpenCode step with a provider API key and receive its output and exit status

So I can:
use OpenCode in non-interactive workflows without depending on a host background service or a host credential database

## Implementation Details:

### 1. Install and identify V2

- Replace the V1-prone download in `templates/Dockerfile.opencode` with an official V2 distribution, pinned to an explicit 2.x version and appropriate Linux architecture. Verify the installed binary reports the pinned major/version, rather than merely checking that it is executable. Update the version deliberately; image builds must not change agent major versions because an upstream `latest` label moved.
- Ensure the Docker and builtin/Apple image paths use that template. For `docker-sbx-experimental`, inspect the version in Docker's built-in OpenCode kit. If the mixin cannot guarantee the pinned V2 executable and entrypoint, provide an awman-owned `kind: agent` kit or another validated V2 install path. `awman ready` and a launched sandbox must report the same intended major version. Preserve the kit's credential and network rules when changing its kind.
- Document that the built-in `opencode` now targets V2. Do not silently migrate or rewrite V1 user configuration; V2 reads supported V1 config in place. State that V1 and V2 share config locations, and advise users with V1-only plugins or integrations to follow the upstream migration guide.

### 2. Invoke the V2 CLI correctly

- Update `src/engine/agent/first_party.rs` and any entrypoint builders needed to use V2's private-server `--standalone` mode for container launches. V2 otherwise discovers a shared background service that owns sessions, credentials, permissions, and tool execution ([CLI guide](https://opencode.ai/v2/docs/cli/)). A private server makes per-launch environment credentials apply to that execution. Do not attach a containerized agent to a host OpenCode service.
- Preserve `opencode run` for non-interactive launches. Confirm awman's piped prompt reaches V2: the V2 [run implementation](https://raw.githubusercontent.com/anomalyco/opencode/dev/packages/cli/src/run/run.ts) merges non-TTY stdin into its message. Preserve the existing `--model provider/model` delivery for `run`, including `#variant` where accepted.
- Preserve `--prompt <text>` for seeded interactive launches. The V2 full-screen root command accepts `--prompt` but does not accept `--model` or `--agent`; V2's `mini` interface accepts those flags ([CLI command definitions](https://raw.githubusercontent.com/anomalyco/opencode/dev/packages/cli/src/commands/commands.ts)). For an interactive awman `--model` override, launch `opencode mini` and show a visible note that the minimal interface was selected. Ordinary interactive launches continue to use the full-screen interface. Check combined mode flags before selecting `mini`: its CLI option set differs from the root command, so an unsupported combination must fail visibly. Test argument ordering so `--standalone`, subcommand, model and prompt are parsed by the intended command.
- Keep `ping_argv` and ready checks compatible with V2 and the private-server behavior. A successful version check is distinct from a successful provider/model ping.

### 3. Credentials, config, skills and modes

- Continue mounting `~/.config/opencode` for supported user configuration and `AGENTS.md` behavior. V2 stores saved API keys and OAuth tokens in its SQLite database, usually `~/.local/share/opencode/opencode.db`, outside that mount ([provider guide](https://opencode.ai/v2/docs/cli/providers/)). Do not claim host saved-account passthrough. Support API keys through awman's existing `env(VAR)`/secret delivery to the standalone server; verify the injected variable reaches the server process, including in Docker Sandboxes. If persistent V2 login is offered, use an awman-owned per-session or per-sandbox data store and document its lifecycle. Never bind-mount a concurrently used host OpenCode database.
- Change OpenCode's `skills_mount` from `.config/opencode/commands` to `.config/opencode/skills`; those are distinct V2 features. Keep the overlay read-only and verify a mounted `SKILL.md` is discoverable ([skills guide](https://opencode.ai/v2/docs/skills/)). `AGENTS.md` remains the instruction delivery path ([instructions guide](https://opencode.ai/v2/docs/instructions/)).
- Map awman `--yolo` only after checking the V2 permission semantics: V2 `--auto` auto-approves requests but continues to honor explicit denies ([permissions guide](https://opencode.ai/v2/docs/permissions/)). awman's narrower `--auto` must not be mapped to V2 `--auto`. Keep awman `--plan` rejected until workspace read-only enforcement or equivalent verified restrictions make awman's read-only promise true; V2's built-in `plan` agent may still run permission-controlled shell commands and write plan files ([agents guide](https://opencode.ai/v2/docs/agents/)). For unsupported modes under each runtime, show the existing explicit error or a visible warning; do not silently imply that the requested mode took effect.
- Preserve fail-closed handling of tool allow/deny lists. Do not convert them to V2 config unless their semantics, precedence, and all launch paths are tested.

## Edge Case Considerations:

- Host V1 and V2 installations use the same `opencode` command and configuration locations. Version detection must distinguish them; a V1 host binary must not make a V2 image appear ready.
- Global or project config may select a model or include V1 plugins. A per-run model override must apply to the selected V2 interface; plugin failures should surface the upstream migration guidance without deleting user files.
- Multiple awman sessions must not share a mutable SQLite database unless that use is deliberately designed and tested. Short-lived container runtimes must not imply that an in-container `/connect` login will persist.
- A Docker Sandbox mixin may launch its own baked-in binary even when a V2 binary was added to `PATH`; verify the executable actually invoked.

## Test Considerations:

- Unit tests for OpenCode V2 entrypoints and argument ordering: full TUI, seeded full TUI, interactive `--model` via `mini`, combined `--model`/permission-mode requests, `run` with stdin, `run --model`, `--standalone`, and unsupported permission/tool modes.
- Template/kit tests that identify an explicit V2 install and assert `opencode --version` returns 2.x on amd64 and arm64 targets where those runtimes are supported.
- Integration smoke tests with a disposable workspace and a non-production provider key or stub provider: interactive seed, non-interactive stdin prompt, model override, environment credential visibility to the private server, discovered skill, and `AGENTS.md` instruction loading. Exercise Docker and builtin/Apple container paths; exercise Docker Sandboxes on a supported host.
- Permission tests showing `--plan` cannot start with write access, `--auto` is not silently treated as V2 `--auto`, `--yolo` does not wait at an approval request when supported, and explicit deny rules remain effective.

## Codebase Integration:

- Follow the current agent-definition and runtime boundaries in `src/engine/agent/`, `src/engine/container/`, `src/engine/sandbox/dsbx/`, `src/engine/overlay/`, and `src/data/templates/`. Keep version-specific decisions in the OpenCode adapter or templates rather than branching on the agent name across runtimes.
- Preserve the security and mount constraints in `aspec/architecture/security.md`. Do not introduce a host OpenCode service connection or expose the host credential database.

## Documentation

After implementation, update the existing user-facing agent and runtime guides (`docs/03-agent-sessions.md`, `docs/08-overlays.md`, and `docs/11-runtimes.md`) with V2 install/auth behavior, the `mini` model-override behavior, and the exact permission-mode capability table. Update other existing guides only where their current OpenCode claims become inaccurate. Keep implementation details in this work item or code comments.
