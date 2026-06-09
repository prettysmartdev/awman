# Headless Mode

Headless mode is awman's fully non-interactive operation mode for environments without a terminal (TTY). When running in CI/CD pipelines, containers, or other headless contexts, awman automatically detects the absence of a TTY and enforces non-interactive behavior — no manual intervention required.

---

## What is headless mode?

Headless mode means the workflow runs **without expecting user input from a terminal**. No interactive prompts, no dialog boxes, no pauses for confirmation. The agent runs autonomously from start to finish, and you see the results through logs or event streams.

Headless mode is **automatic**: awman detects when there is no controlling terminal on stdin and switches to non-interactive behavior. You don't need to pass any special flags in most cases.

---

## When headless mode activates

awman automatically enters headless (non-interactive) mode when:

- **stdin is not a TTY** — running in a CI/CD pipeline, cron job, Docker container, or any other automated context where there is no interactive terminal
- **Explicit `--non-interactive` flag** — you explicitly requested non-interactive mode with the `--non-interactive` flag (useful for testing or forcing the behavior even if a TTY is available)

When either condition is true, non-interactive mode is enforced. The agent session runs end-to-end without pausing for input.

---

## CLI: automatic TTY detection

In CLI mode, awman checks whether stdin is connected to a terminal:

```sh
# Runs interactively (TTY detected)
awman chat

# Runs non-interactively (no TTY — maybe redirected from a file or pipe)
awman chat < /dev/null

# Runs non-interactively (explicit flag)
awman chat --non-interactive
```

If a TTY is detected, awman allows interactive features (full-screen agent sessions, terminal raw mode when `--non-interactive` is false). If no TTY is detected, awman automatically switches to non-interactive mode — the agent runs in print/batch mode with no attempt to manage the terminal.

**Example: CI/CD pipeline**

```yaml
# GitHub Actions workflow
- name: Run awman workflow
  run: awman exec workflow aspec/workflows/implement.toml --yolo
```

In this context, GitHub Actions provides no controlling terminal. awman detects this, automatically enforces non-interactive mode, and runs the workflow autonomously. The yolo countdown and auto-advance features still work — the workflow does not stall indefinitely if a step goes silent.

---

## API: always non-interactive

The API frontend (`awman api start`) always runs non-interactive, regardless of the server process's stdin state. The API communicates via HTTP/WebSocket — there is no terminal to speak of.

Clients receive status updates through the event stream. The API applies `yolo` auto-advance behavior the same way the CLI does: when a step goes silent for 30 seconds, a 60-second countdown begins, and the step auto-advances when it expires.

---

## Explicit `--non-interactive` flag

You can force non-interactive mode with an explicit flag, even if a TTY is present:

```sh
# Force non-interactive even though stdin is a TTY
awman chat --non-interactive
```

This is useful for:
- Testing how your workflow behaves in a headless context without leaving your terminal
- Scripting awman commands inside a shell script that you're running interactively
- Environments with a quirky terminal setup where TTY detection is unreliable

---

## Yolo mode in headless environments

Yolo mode (`--yolo`) works seamlessly in headless (non-interactive) contexts:

```sh
awman exec workflow aspec/workflows/implement.toml --yolo
```

When the workflow is running non-interactively (either auto-detected or explicit):

1. **Stuck detection** runs continuously — the engine tracks output on stdout and stderr
2. **Auto-advance after 30 seconds of silence** — when no output is seen for 30 seconds, a 60-second countdown begins
3. **Countdown status via stderr/logs** — progress updates are printed to stderr or logged (one message every 10 seconds to avoid noise)
4. **Auto-advance when countdown expires** — the step advances to the next step without any user input

This means you can safely run yolo workflows in CI/CD pipelines without worrying about them stalling indefinitely. If an agent step gets stuck, the countdown automatically advances the workflow.

**Example: fully autonomous CI/CD integration**

```sh
#!/bin/bash
set -e

# No explicit --non-interactive needed; awman detects headless context
awman exec workflow aspec/workflows/feature-impl.toml \
  --work-item 0042 \
  --yolo
```

When this script runs in CI, awman automatically detects there's no TTY and operates in non-interactive mode. If any workflow step stalls for 30 seconds, the countdown runs and auto-advances after 60 seconds.

---

## Terminal mode in CLI (interactive)

When running `awman chat` (or other interactive commands) with a TTY present and `--non-interactive` **not** passed:

1. **Full-screen terminal control** — the agent can take over the terminal (raw mode) for interactive UIs
2. **PTY passthrough** — terminal input and output are passed directly to the agent
3. **SIGWINCH support** — terminal resize events are sent to the agent's PTY
4. **Yolo mode works too** — if `--yolo` is passed, stuck detection and auto-advance work even in interactive mode

Closing this interactive session (Ctrl-D or agent exit) returns the terminal to normal (cooked) mode.

---

## Error handling in headless mode

When an error occurs in headless mode, awman:

1. **Prints error details to stderr** — includes the step name, error code, and any output from the container
2. **Exits with a non-zero code** — CI/CD systems detect the failure and stop the pipeline
3. **Does not attempt any interactive recovery** — no prompts to retry, no manual intervention options

This makes awman workflow results easy to integrate into CI/CD: check the exit code and act accordingly.

---

## Logging in headless environments

In headless mode, all output goes to stdout and stderr:

- **Step output** — the agent's output is printed to stdout
- **Status messages** — workflow progress ("Step plan completed", "Advancing to implement", etc.) is printed to stdout
- **Errors** — any errors are printed to stderr
- **Yolo countdown** — progress updates are printed to stderr (one every 10 seconds)

Capture and redirect as needed:

```sh
awman exec workflow workflow.toml --yolo > workflow.log 2>&1
```

---

## Configuration

No special configuration is needed for headless mode — TTY detection is automatic. However, you can adjust yolo-specific behavior via config:

- `yoloDisallowedTools` — restrict which tools the agent can use even in headless yolo mode
- Workflow definitions can specify per-step agents and models, which all work identically in headless mode

See [Configuration](08-configuration.md) for the full config reference.

---

## Troubleshooting

**"Step got stuck but didn't auto-advance"**

This should not happen in yolo mode. If a step is truly silent for 30+ seconds, the countdown should start. Check:
1. Is `--yolo` passed on the command line?
2. Is the agent actually silent, or is it producing output very slowly (e.g., downloading large files)?
3. Check the logs to see if a countdown message was printed

**"Got interactive prompts in CI/CD even though there's no TTY"**

This is very unlikely — awman detects stdin being a non-TTY and enforces non-interactive behavior. If this happens:
1. Verify that stdin is not connected to a TTY: `[ -t 0 ] && echo "TTY" || echo "no TTY"`
2. Try explicit `--non-interactive` flag to force the behavior
3. Report this as a bug if the workaround doesn't help

**"My workflow needs special terminal handling"**

If you're running an agent that requires full-screen terminal control (like an interactive text editor), you need a TTY. Headless environments don't support this. Options:
1. Refactor the workflow to avoid full-screen features
2. Run the workflow on a machine with a terminal instead of headless
3. Use a terminal multiplexer (tmux, screen) inside the container to provide a virtual TTY — this is advanced and rarely needed

---

[← Yolo Mode](06-yolo-mode.md) · [Next: Configuration →](08-configuration.md)
