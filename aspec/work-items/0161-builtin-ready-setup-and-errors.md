# Work Item: Task

Title: Improve builtin runtime image-source setup and `ready` errors
Issue: issuelink

## Summary:

- Under `builtin-experimental`, a new user can reach `awman ready` without `builtin.imageSource` or a matching `builtin.images` entry. Today `ready` fails with configuration and build instructions. In an interactive CLI or TUI session, offer to configure an explicit source and continue the same `ready` run after the choice is saved. Keep non-interactive and API runs deterministic and free of configuration writes.
- Make `ready` failures readable. The shared `render_summary_box` in `src/frontend/render_helpers.rs` measures the entire status text and writes it into one table cell. Long errors make an enormous box; embedded newlines break its borders. Format concise, width-bounded rows with wrapped, indented error details and one clear recovery instruction. Keep `ready --json` structured and machine-readable.
- For a `docker-local` source with an unavailable local Docker socket, report that Docker is not running and how to retry. Preserve distinct diagnostics for permission errors, malformed endpoints, TLS failures, remote Docker hosts, and missing images. Do not infer daemon state from arbitrary text in an HTTP error.

## User Stories

### User Story 1:
As a: user setting up the builtin runtime

I want to:
choose and save an image source when `awman ready` finds none for the required agent image

So I can:
complete setup without manually locating and editing the `builtin` JSON block

### User Story 2:
As a: user troubleshooting `awman ready`

I want to:
see the failed step, a readable cause, and a concrete next action in the CLI or TUI

So I can:
fix the problem without parsing a broken table or repeated raw socket errors

### User Story 3:
As a: user whose source is `docker-local`

I want to:
see “Docker is not running” when awman cannot connect to the selected local Docker socket

So I can:
start Docker and rerun `awman ready` instead of investigating an unexplained socket-unavailable message

## Implementation Details:

### 1. Offer explicit image-source configuration

- In the builtin `ready` path, distinguish an unconfigured source for an image being prepared from an invalid configured source. Cover the default required image and non-default agent images included by `ready --build`. Use the existing effective-config resolution order: a matching `builtin.images` entry wins over `builtin.imageSource`, and repo values win over global values per field. Do not offer to replace a configured but failing source as if it were missing.
- Add one frontend request, backed by a typed source choice, to the `ReadyFrontend` contract. The interactive CLI and TUI offer `docker-local`, `apple-local` where supported, `archive`, `registry`, and “leave unconfigured.” Ask for the fields required by the selected type, show the destination config file and exact JSON change, and let the user choose repo or global scope. Validate the complete choice with the existing `ImageSourceSpec` and config validators before writing. Write atomically through the existing config-edit path, preserving unrelated keys and file permissions. Re-resolve builtin settings and continue this `ready` invocation after a successful write; do not require a second invocation.
- A local source choice does not silently enable `builtin.builder.enabled` or start Docker/Apple Containers. Explain that import requires an existing image, and offer a separate, explicit builder choice when the selected local store can build. If enabled, the next image-preparation step may build using the existing source-matched builder rules. Registry and archive choices remain import-only.
- Prompt at most once per `ready` run. Decline, cancel, EOF, or failed validation leave config unchanged and retain the current actionable missing-source result. A missing source with an already installed usable image may still be offered, but declining it must not make that image unusable. `--non-interactive`, `--json`, API session setup, workflow preflight, and squad never prompt or write config; their errors identify the missing source and the config fields needed to fix it.

### 2. Render failure output cleanly

- Keep `ReadySummary` and `StepStatus` as the source of ordered stages, status, outcome, and hints. Fix the shared human renderer in `src/frontend/render_helpers.rs` rather than building a different diagnosis in each frontend. Cap display width to the available terminal/TUI width with a sensible non-TTY fallback; use terminal display width, not Unicode character count, for padding. Treat embedded newlines as separate detail lines and wrap long words/paths without breaking borders. Bound any single line and indent continuation lines under its step.
- Keep the table's status cell short (`done`, `skipped`, `warn`, `failed`). Show the full warning/failure reason once beneath its row or in a clearly labeled details section, followed by one actionable `Tip`. Prevent raw ANSI/control characters in diagnostics from corrupting the display. Avoid a second, unformatted copy of the same error in progress or completion messages, including the `ready: complete — ... Debug` line in `src/command/commands/ready.rs`.
- CLI stderr and TUI status log should convey the same stage, cause, and recovery action. API events should retain ordered, readable messages without terminal box characters. `ready --json` keeps its current field names, full diagnostic strings, and clean stdout; human formatting does not alter the JSON contract or readiness exit status.

### 3. Classify Docker-local unavailability

- At the `docker-local` source adapter (`src/engine/oci/docker_engine.rs`) classify the underlying local Unix-socket failure by error kind before formatting it. A missing socket or refused local connection becomes a concise “Docker is not running” diagnostic with the selected endpoint and “Start Docker, then run `awman ready` again.” The same diagnosis applies to a source-matched local build when its Docker daemon is stopped.
- Keep permission denied as “Cannot access the Docker socket” with a permissions/connection-context remedy. A configured `tcp://` endpoint, TLS or authentication failure, unsupported host scheme, timeout, and a running daemon that lacks the requested image each keep their own accurate cause. Use typed error information or the error source chain, not a substring match on reqwest's display text.
- Preserve acquisition rules: a stopped Docker daemon never triggers fallback to a registry or another local store. An already installed matching builtin image may be used under the existing warning-and-continue rule; a required build or import fails `ready` and exits nonzero. Classify a clearly stopped local daemon as a final unavailable-source condition, so `ready` does not waste its bounded retry budget on the same absent socket. Preserve retries for genuinely transient remote/transfer failures.

## Edge Case Considerations:

- When only a non-default agent lacks a source, offer a `builtin.images` entry for that image instead of changing a default source that already serves other agents. A default source with a fixed `reference` or archive may name only one image; do not apply it to a different agent tag.
- A repo config may override a global source. The preview must show the effective result and the selected scope, so a global write is not presented as fixing a repo override that still wins.
- `DOCKER_HOST` or an explicit `host` may name a custom Unix socket. Report the actual selected endpoint. Reserve the “Docker is not running” wording for a local socket that is absent or refusing connections; a remote endpoint failure must not claim the user's local Docker installation is stopped.
- Narrow terminals, Unicode text, multiline causes, very long paths, and malicious control characters must leave the summary legible. The original full diagnostic remains available in JSON and logs where appropriate.

## Test Considerations:

- Frontend tests for every source choice, preview, repo/global write, validation failure, decline, cancel, EOF, and single-prompt behavior. Verify immediate continuation uses the newly resolved source. Verify non-interactive, JSON, API, workflow, and squad paths do not prompt or mutate config.
- Pure renderer tests that assert aligned borders and bounded line width for multiline and long failures, Unicode display widths, narrow viewports, and control characters. Assert the failed stage and recovery tip appear once, and that the CLI, TUI, API, and JSON outcomes agree on readiness.
- Hermetic Docker-source tests with a missing Unix socket, a socket that refuses connections, a permission-denied socket, an unavailable TCP endpoint, TLS failure, a reachable daemon with a missing image, and an installed-image reuse case. Assert both diagnosis and retry classification; no real Docker daemon is needed for these tests.

## Codebase Integration:

- Follow the existing `ReadyEngine`/`ReadyFrontend` separation (`src/engine/ready/`, `src/frontend/cli/per_command/ready.rs`, `src/frontend/tui/per_command/ready.rs`, API frontend) and the shared image-source resolver in `src/engine/image_prep/` and `src/engine/oci/`. Keep source selection explicit as required by `aspec/architecture/security.md`; a prompt acceptance is the authorization for the chosen config write.
- Reuse config writing and validation behavior from `src/command/commands/config.rs`. Keep structured error classification below presentation, and keep human layout in Layer 3.

## Documentation

After implementation, update the builtin runtime setup and troubleshooting sections in `docs/11-runtimes.md`, plus the relevant `ready` and `builtin` entries in `docs/03-agent-sessions.md` and `docs/07-configuration.md`. Describe the interactive source choices, non-interactive behavior, builder opt-in, and Docker-not-running message. Keep implementation notes in this work item or code comments.
