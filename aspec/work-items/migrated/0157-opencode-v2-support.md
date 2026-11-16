# Work Item: Feature

Title: Support OpenCode 1.x, OpenCode 2.x, and Cursor as built-in agents; retire built-in Gemini
Issue: issuelink

Depends on: 0143 (completed daemon block).

## Summary:

- Provide three explicit built-in agent types: `opencode` for the 1.x line, `opencode-v2` for the 2.x line, and `cursor` for Cursor Agent CLI. Both OpenCode lines must be independently installable, selectable, ready-checkable, and runnable across awman's supported runtimes. Preserve `--agent opencode` for existing 1.x users; adding V2 must not silently change their version or behavior.
- Remove `gemini` from the built-in agent catalogue, generated templates/kits, default-agent choices, and current documentation. After 0139, an `agents.default: "gemini"` selection gets a clear error naming `antigravity` or a user-defined custom Gemini agent; do not silently replace the user's chosen agent. Legacy `agent`/`default_agent` keys retain 0139's moved-key error. Preserve the `.gemini/antigravity-cli` paths used by the separate built-in `antigravity` agent.
- Be precise about the reason: [Google ended Gemini CLI access for consumer Google-login/AI plans on June 18, 2026](https://developers.google.com/gemini-code-assist/docs/deprecations/code-assist-individuals), while enterprise subscriptions and paid API-key access continue. Retiring awman's built-in Gemini integration is the requested product choice, not a claim that Google shut down Gemini CLI for everyone.

## User Stories

### User Story 1:
As a: existing OpenCode user

I want to:
keep using `awman chat --agent opencode` with a pinned OpenCode 1.x image

So I can:
upgrade awman without being moved to a different major version.

### User Story 2:
As a: OpenCode V2 user

I want to:
select `opencode-v2` for interactive and automated runs, with my selected model, prompt, credentials, and skills delivered correctly

So I can:
use V2 without replacing the V1 setup I still need.

### User Story 3:
As a: Cursor user

I want to:
select `cursor` for interactive sessions and non-interactive workflow steps

So I can:
use Cursor Agent CLI with awman's normal image, overlay, authentication, output, and permission controls.

### User Story 4:
As a: user whose config names `gemini`

I want to:
receive an actionable migration message

So I can:
choose Antigravity or intentionally define Gemini as a custom agent without an unexpected agent switch.

## Implementation Details:

### 1. Make built-in identity and version explicit

| awman agent | CLI in its own image | Version policy | User-facing behavior |
|---|---|---|---|
| `opencode` | `opencode` | Pin an explicit 1.x release | Existing choice and V1 behavior remain stable. |
| `opencode-v2` | `opencode` | Pin an explicit 2.x release | New opt-in choice; V2-specific invocation and config behavior. |
| `cursor` | `cursor-agent` | Pin/verify a tested Cursor CLI build | New built-in agent, with version updates reviewed deliberately. |

- Update `FIRST_PARTY_AGENT_NAMES`, `first_party.rs`, template lookup, agent discovery, config validation, default choices, `new`/`ready`/launch flows, and generated Docker/Sandbox assets together. Version-specific adapters must live with agent definitions/templates, not as scattered runtime checks. Use distinct image tags and kit names for both OpenCode majors even though both binaries are named `opencode` inside their containers.
- Replace `templates/Dockerfile.opencode`'s `releases/latest` download with an official, explicitly pinned 1.x artifact and verify `opencode --version` reports 1.x. Add a separate V2 template using the [official V2 installation channel](https://opencode.ai/v2/docs/) and verify 2.x. OpenCode's [V1/V2 migration guide](https://opencode.ai/v2/docs/migrate-v1/) says both majors share the command and config locations and do not install side by side by default; awman's separate images provide the isolation.
- Add a Cursor image from the [official Cursor CLI installation channel](https://docs.cursor.com/en/cli/installation). The installer attempts auto-update by default, so determine a supported version-pinning/verification strategy before accepting the image; a run must not silently change its tested CLI version. Verify Linux architecture support for every runtime awman advertises. A missing supported package/architecture must fail at build or ready with a precise reason.
- For Docker Sandboxes, verify which executable the base kit actually invokes. Use an awman-owned `kind: agent` kit or other controlled path where a mixin's baked-in OpenCode/Cursor binary cannot guarantee the selected version and entrypoint. A ready check and an actual launch must report/use the same major and executable.

### 2. Integrate OpenCode V2 without changing V1

- Keep V1's current interactive `--prompt` and non-interactive `opencode run` behavior, then test it against a pinned 1.x binary. Put V2's private-server `--standalone` behavior in the `opencode-v2` adapter: the [V2 CLI](https://opencode.ai/v2/docs/cli/) otherwise discovers a shared background service whose credentials and sessions may belong to another launch. Confirm prompt delivery, argument order, model syntax, exit status, and interruption with the actual V2 binary.
- V2 `run` supports automation and non-TTY stdin; the full-screen root command supports `--prompt` but interactive model selection may require `opencode mini` ([V2 commands](https://opencode.ai/v2/docs/cli/commands/)). If awman's interactive `--model` selects `mini`, tell the user that the interface changed. Reject unsupported combined options visibly. Keep ordinary interactive launches on the full-screen interface.
- V2 keeps supported V1 config in place but changes plugins, server APIs, and some native config fields. Do not migrate or rewrite a user's OpenCode files. Both majors may read `~/.config/opencode`, but warn if V2-only edits/plugins would break a V1 session. V2 saved credentials are in its SQLite data store; do not mount a live host database into a container. Deliver provider keys through awman's existing secret/env mechanism to the isolated V2 process and document any per-session login persistence.
- Mount V2 skills at `.config/opencode/skills` and retain `AGENTS.md` instruction delivery, per the [V2 skills](https://opencode.ai/v2/docs/skills/) and [instructions](https://opencode.ai/v2/docs/instructions/) guides. Keep V1's existing command/skill behavior separate. Map V2 `--auto` to awman's permission modes only after verifying explicit deny and prompt behavior; do not equate awman's `--auto`, `--yolo`, and `--plan` just because flags sound similar. Refuse a mode or tool restriction that awman cannot enforce.

### 3. Integrate Cursor Agent CLI

- Use `cursor-agent` as the executable. Verify an interactive PTY launch, positional seeded prompt, `--model`, and a non-interactive `--print` launch. Cursor's [output-format contract](https://docs.cursor.com/en/cli/reference/output-format) defaults to stream JSON and may omit a terminal result on failure; select and parse a format deliberately so awman emits readable live output, captures exit status, and never exposes raw protocol JSON in a normal terminal. Handle cancellation and process cleanup.
- Support `CURSOR_API_KEY` through awman's `env()`/secret delivery, without placing it on a command line or in logs. Cursor also offers browser login ([authentication guide](https://docs.cursor.com/en/cli/reference/authentication)); any mounted login state must be copied or scoped to an awman-owned session and tested for safe concurrent use. Determine the actual Cursor config, rules, and skill locations before defining mounts. Preserve project `.cursor/rules` and `AGENTS.md` behavior ([Cursor rules](https://docs.cursor.com/context/rules-for-ai)); do not assume an unverified skills directory.
- Cursor documents interactive plan/ask modes, `--print`, and `--force` ([CLI overview](https://docs.cursor.com/en/cli/overview), [parameters](https://docs.cursor.com/en/cli/reference/parameters)). Verify the exact installed CLI's effect in PTY and print modes before mapping awman's `--plan`, `--auto`, or `--yolo`. The Cursor documentation differs on whether unforced print mode can write, so test actual file and shell permissions in an isolated workspace. Reject a requested mode or allow/deny list when its promised restriction cannot be guaranteed. Do not treat `--force` as an acceptable default for automation.
- Add generated Dockerfile and Sandbox kit/config application support with the same credential, network, overlay, and reviewed-template boundaries as other first-party agents. Confirm builtin/Apple image import and launch use the same tested binary. A host-installed Cursor CLI must not accidentally satisfy an in-container readiness check.

### 4. Retire the built-in Gemini path cleanly

- Remove `gemini` from first-party registration and built-in image/kit generation. Remove or archive the old Gemini Dockerfile and Sandbox template so no generated configuration points at them. Add `gemini` to former-built-in migration handling with a specific error for stale repo/global defaults, per-command selection, workflows, and existing agent images. A custom agent named `gemini` should be allowed through the normal trust/review path if a user deliberately defines one; do not silently ship one.
- Update agent listings, examples, model and permission tables, authentication guidance, default-agent docs, and template tests. Keep Antigravity's independent `.gemini/antigravity-cli` credential/skill paths and tests intact; removing the Gemini built-in must not erase or remap Antigravity state.

## Edge Case Considerations:

- Host V1 and V2 installations can share `opencode` and config paths; never infer a container image's major version from a host binary, never mutate shared host config to make one major work, and never let a V1 binary satisfy `opencode-v2` readiness (or vice versa).
- Provider login/database state, Cursor browser credentials, and global rules must not leak between sessions or into build logs. Multiple sessions may run concurrently with distinct model and secret overlays.
- For Docker Sandbox mixins, the kit's baked-in entrypoint may bypass a binary placed earlier on `PATH`; test the process actually launched. For any unsupported platform/runtime/flag combination, report the limitation rather than producing a misleading successful setup.
- An existing `gemini` default must produce a clear error before image build or agent launch. Preserve user files and custom-agent options; do not auto-change the default to `antigravity`.

## Test Considerations:

- Catalogue/validation tests assert exactly the intended first-party choices, distinguish the two OpenCode majors, allow deliberate custom Gemini, and show migration guidance for stale built-in Gemini references. Cover CLI, TUI, API/workflow, `new`, `ready`, and config defaults.
- Build and inspect pinned amd64/arm64 images and Sandbox kits where supported. Assert the invoked OpenCode major and Cursor version from inside the launched runtime, not just Dockerfile text or a host binary. Test version drift and missing-architecture errors.
- Exercise interactive seed, model selection, non-interactive prompt/output/exit, interruption, skills/instructions, credentials, concurrent launches, and permission/tool restrictions for V1, V2, and Cursor. Use a stub provider or disposable test account where possible. Include a real CLI smoke for each supported major/runtime before declaring it ready.
- Prove that V2's standalone process uses its own credentials and that Cursor's print mode cannot bypass awman's requested plan/deny boundaries. Verify Gemini removal leaves Antigravity auth and skills functional.

## Codebase Integration:

- Follow `aspec/architecture/2026-grand-architecture.md`: Layer 0 owns first-party names, versioned definitions, config and templates; Layer 1 `src/engine/agent/first_party.rs` owns launch adapters and output translation; Layer 2 owns default selection, capability/permission policy and ready/build orchestration through the catalogue; Layer 3 only presents choices and results. Update `src/data/templates/mod.rs`, `templates/`, `src/data/fs/auth_paths.rs`, overlays, ready/build paths, Docker Sandbox adapters, and associated tests together. Distinct agent identity must select one version-specific definition consistently through image naming, build, ready and launch, with the same behavior through builtin, daemon, CLI, TUI and API paths.
- Follow `aspec/architecture/security.md` for mounts, secret delivery, and first-party versus reviewed custom-agent trust. Treat OpenCode's shared config and Cursor login state as untrusted mutable host data; keep auth out of images and logs.

## Documentation

After implementation, update `docs/03-agent-sessions.md`, `docs/07-configuration.md`, `docs/08-overlays.md`, `docs/11-runtimes.md`, and `docs/15-custom-agents.md` where they list agents or describe setup. Show the `opencode` 1.x and `opencode-v2` 2.x distinction, Cursor setup and verified mode capabilities, Gemini migration options, and the correct scope of Google's consumer deprecation. Remove obsolete first-party Gemini examples without deleting Antigravity guidance.
