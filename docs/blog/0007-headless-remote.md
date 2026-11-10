# Manage your fleet of agent machines with amux v0.7 

> Migration reference: commands and routes below describe this historical release. Removed server commands now produce migration errors; they are retained here to identify upgrade changes. Use [the unified daemon](../16-daemon.md) and [current worker routes](../09-api-and-remote-mode.md) for current instructions.

Hello and welcome to my first paternity leave blog post! I'm off work hanging with the family for 2 months, but I still plan to continue moving amux forwards while I'm off. With that, one of the reasons I can make so much progress each week is due to multi-stage agent workflows. They are really powerful because it allows you to create well-defined but signifcant work that an agent can chew on independently and arrive at a good result most of the time. There are two issues that constrain this method though; resource constraints on a single machine, and the fact that you, y'know, close your laptop somtimes.

One of the goals I had for amux when I set out (still sitting in my notes app) is to be able to manage agent workflows that run on a cluster of worker machines. With v0.7, we have the first preview of this capability. It's still fairly basic, but I think it shows what my aspirations for the project are. It won't be perfect off the bat, but I hope you'll give it a try in your homelab or trial it for something you're building and see how it can accelerate your work. Being able to launch a workflow on a remote machine (and then two and then three) is the next step up needed for code agents to be as productive as possible for us while still maintaining the security principles of amux and keeping you, the human, in control.

---

```sh
# install or upgrade
curl -s https://prettysmart.dev/install/amux.sh | sh
```

---

## Headless mode

`amux headless start` runs an HTTP server that manages agent sessions and dispatches commands. It's the same machinery that powers the TUI — same container isolation, same multi-step workflow capabilities, same security model — but driven by an API instead of keystrokes.

```sh
# On the server machine
amux headless start --port 9090
# prints an API key on first run; make sure you store it somewhere safe
```

The server can run in the foreground or daemonize into the OS process manager:

```sh
amux headless start --background   # systemd on Linux, launchd on macOS
amux headless status               # PID, port, active sessions, uptime
amux headless logs                 # tail -f the server log
amux headless kill                 # stop it
```

Working directories are allowlisted at startup. Any session request for a path not on the list gets a 403 — the server will not run agents in arbitrary directories.

```sh
amux headless start --workdirs /workspace/projectA --workdirs /workspace/projectB
# or set it once in config:
amux config set --global headless.workDirs '["/workspace/projectA"]'
```

# Control a remote amux headless machine from your laptop

The new family of `remote` subcommands lets you control your fleet easily:

```sh
amux config set --global remote.defaultAddr http://server:9090
amux config set --global remote.defaultAPIKey <key>

amux remote session start /workspace/projectA
# → session abc-123 created

amux remote run exec prompt "Add docstrings to all public functions" \
  --session abc-123 --follow
# streams logs until the command finishes, then prints a summary table
```

In the TUI, leaving out `--session` shows a live picker of active sessions on the configured remote host. The last-used session per tab is remembered, so the s
econd invocation is just `remote run <command>`.

If you (or your agent) want to interact with the API directly, the REST API is straightforward: create a session, submit a command, poll for completion, fetch the log.

```sh
# Create a session
curl -s -X POST http://server:9090/v1/sessions \
  -H "Authorization: Bearer <key>" \
  -H "Content-Type: application/json" \
  -d '{"workdir": "/workspace/projectA"}'
# → {"session_id": "abc-123"}

# Dispatch a command
curl -s -X POST http://server:9090/v1/commands \
  -H "Authorization: Bearer <key>" \
  -H "x-amux-session: abc-123" \
  -H "Content-Type: application/json" \
  -d '{"subcommand": "exec", "args": ["prompt", "Fix the failing tests", "--yolo"]}'
# → {"command_id": "def-456"}

# Poll and fetch logs
curl -s http://server:9090/v1/commands/def-456
curl -s http://server:9090/v1/commands/def-456/logs
```

Everything is stored in `~/.amux/headless/` on the server — every request, every session, every command, stdout and stderr, start and end times. This ensures you can debug or audit everything that happened on the server. State is stored in a sqlite db here as well that you can inspect. 

## amux exec

In order to make amux workflows more flexible (for local or remote use), I added `amux exec prompt` and `amux exec workflow` as cleaner one-shot entry points for scripting:

```sh
amux exec prompt "Explain the auth flow" --non-interactive
amux exec workflow ./aspec/workflows/implement-feature.md --work-item 0031 --yolo
```

`exec workflow` doesn't require a work item — useful when you have a generic workflow you want to run against any codebase. `-n` is now also a short alias for `--non-interactive` everywhere.

## A few other things

**TOML and YAML workflows** — workflow files now work in `.toml` and `.yaml` as well as Markdown. Format is detected by file extension. The Markdown parser hasn't changed.

**`--model` flag** — pass `--model claude-opus-4-6` (or any model name) to `chat` or `implement` to override the model for that session. Workflow steps can also specify `Model:` per step.

**TUI shortcut changes** — Ctrl-W now opens the workflow control board even when the container window is maximized, so you no longer have to minimize first. Ctrl-M now toggles the container window instead of Esc/c. Esc is now forwarded to the running agent when the window is maximized, so more agent keyboard shortuts actually work now. Ctrl-, opens the config dialog from anywhere, just like `config show`.

---

Both headless and remote modes are shipping as a **preview** in 0.7. The HTTP API surface will likely change, as will the commands. I do hope you'll try it outand if you hit something odd, open an issue. If you have a vision for how you'd like to use this to enhance your workflow or make better use of your hardware, please tell me about it!

Source and issues at [github.com/prettysmartdev/amux](https://github.com/prettysmartdev/amux). More at [prettysmart.dev](https://prettysmart.dev). Feedback and contributions welcome.
