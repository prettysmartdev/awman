# Work Item: Feature

Title: Headless Mode Part 3
Issue: issuelink

## Summary:
- Add `amux remote run <command>` subcommand: connects to a remote headless amux instance and executes a command there, with optional live log streaming via `--follow`/`-f`
- Add live log streaming endpoint `GET /v1/commands/:id/logs/stream` to the headless server using Server-Sent Events (SSE), replaying historical output then tailing the live log file
- In the TUI only, show a session-selection modal when `remote run` is invoked without `--session`/`AMUX_REMOTE_SESSION`; remember the last-used session per tab
- Add `amux remote session start <dir>` and `amux remote session kill [session-id]` subcommands to manage sessions on a remote headless host, with TUI-only interactive saved-dir selection and y/n save prompt
- Add `remote.defaultAddr` and `remote.savedDirs` fields to `GlobalConfig`; wire all new commands fully into CLI, TUI, and remote (headless server) modes with compile-time parity enforcement via `parity.rs`
- Interactive pickers (session selection, saved-dir selection) are TUI-only; CLI and headless modes require explicit arguments and return clear errors when required params are missing

## User Stories

### User Story 1:
As a: developer or CI operator

I want to: run `amux remote run execute prompt "Fix the tests" --yolo --follow --session abc123` against a remote amux headless host

So I can: dispatch an agent task to a remote machine and watch its output stream in real time in my terminal, getting a tidy summary table at the end

### User Story 2:
As a: developer working from the TUI

I want to: type `remote run execute prompt "hello"` without specifying a session and be presented with an arrow-key-selectable list of active sessions on the configured remote host

So I can: pick the right session interactively, and have amux remember my choice so the next time I run a remote command in the same tab I just press Enter

### User Story 3:
As a: developer managing remote workspaces

I want to: run `amux remote session start` in the TUI with no arguments and pick a directory from my pre-saved list, or run it with a new path and be asked whether to save it for next time

So I can: quickly spin up and tear down sessions on a remote host without remembering paths or session IDs


## Implementation Details:

### 1. New CLI surface (`src/cli.rs`)

Add a new top-level `Remote` variant to the `Command` enum:

```rust
/// Connect to a remote headless amux instance and execute commands.
Remote {
    #[command(subcommand)]
    action: RemoteAction,
},
```

Add `RemoteAction` enum:

```rust
#[derive(Subcommand)]
pub enum RemoteAction {
    /// Execute a command on the remote headless amux host.
    Run {
        /// The amux subcommand and arguments to execute on the remote host
        /// (e.g. "execute prompt hello --yolo").
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,

        /// Address of the remote headless amux host (e.g. http://1.2.3.4:9876).
        /// Overrides AMUX_REMOTE_ADDR env var and remote.defaultAddr config.
        #[arg(long)]
        remote_addr: Option<String>,

        /// Session ID to run the command in. Required in CLI/headless modes.
        /// In TUI mode, if omitted, shows an interactive session picker.
        /// Overrides AMUX_REMOTE_SESSION env var.
        #[arg(long)]
        session: Option<String>,

        /// Stream logs from the remote host until the command completes,
        /// then print a summary table.
        #[arg(long, short = 'f')]
        follow: bool,
    },

    /// Manage sessions on the remote headless amux host.
    Session {
        #[command(subcommand)]
        action: RemoteSessionAction,
    },
}

#[derive(Subcommand)]
pub enum RemoteSessionAction {
    /// Start a new session on the remote host for the given directory.
    Start {
        /// Working directory to use for the new session (absolute path on remote host).
        /// Required in CLI/headless modes.
        /// In TUI mode, if omitted, shows an interactive selection from remote.savedDirs.
        dir: Option<String>,

        /// Address of the remote headless amux host.
        /// Overrides AMUX_REMOTE_ADDR env var and remote.defaultAddr config.
        #[arg(long)]
        remote_addr: Option<String>,
    },

    /// Kill a session on the remote host.
    Kill {
        /// Session ID to kill. Required in CLI/headless modes.
        /// In TUI mode, if omitted, shows an interactive session picker.
        session_id: Option<String>,

        /// Address of the remote headless amux host.
        /// Overrides AMUX_REMOTE_ADDR env var and remote.defaultAddr config.
        #[arg(long)]
        remote_addr: Option<String>,
    },
}
```

### 2. Config additions (`src/config/mod.rs`)

Add a `RemoteConfig` struct and wire it into `GlobalConfig`:

```rust
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteConfig {
    /// Default remote headless amux server address (e.g. "http://1.2.3.4:9876").
    #[serde(rename = "defaultAddr", skip_serializing_if = "Option::is_none")]
    pub default_addr: Option<String>,

    /// List of working directory paths pre-saved for `remote session start`.
    #[serde(rename = "savedDirs", skip_serializing_if = "Option::is_none")]
    pub saved_dirs: Option<Vec<String>>,
}
```

Add to `GlobalConfig`:

```rust
/// Remote headless amux connection configuration.
#[serde(skip_serializing_if = "Option::is_none")]
pub remote: Option<RemoteConfig>,
```

Add accessor helpers:

```rust
pub fn effective_remote_default_addr() -> Option<String> {
    load_global_config().ok()?.remote?.default_addr
}

pub fn effective_remote_saved_dirs() -> Vec<String> {
    load_global_config()
        .ok()
        .and_then(|c| c.remote?.saved_dirs)
        .unwrap_or_default()
}
```

Add config field definitions to `ALL_FIELDS` in `src/commands/config.rs`:

```rust
ConfigFieldDef {
    key: "remote.defaultAddr",
    scope: FieldScope::GlobalOnly,
    hint: "URL of the remote headless amux host (e.g. http://1.2.3.4:9876)",
    builtin_default: "(not set)",
    settable: true,
},
ConfigFieldDef {
    key: "remote.savedDirs",
    scope: FieldScope::GlobalOnly,
    hint: "comma-separated absolute paths; empty string clears",
    builtin_default: "(empty)",
    settable: true,
},
```

Add `config get/set` dot-path support for `remote.defaultAddr` and `remote.savedDirs` using explicit match arms in `src/commands/config.rs` alongside the existing `headless.*` arms from WI 0058. Add `global_display`, `config_get`, and `config_set` handling for both fields. This ensures `config show` displays them and the TUI config dialog includes them.

**Address resolution priority for all `remote` subcommands:**
1. `--remote-addr` CLI flag
2. `AMUX_REMOTE_ADDR` environment variable
3. `remote.defaultAddr` in global config

**Session resolution priority for `remote run`:**
1. `--session` CLI flag
2. `AMUX_REMOTE_SESSION` environment variable
3. (TUI only) `TabState.last_remote_session_id` — a convenience fallback, not available in CLI/headless

### 3. New command module (`src/commands/remote.rs`) — with user input trait

New file. The core design principle: **all interactive pickers live exclusively in the TUI**. The `remote.rs` module uses a `RemoteUserInput` trait to abstract the boundary between "I need a value from the user" and "how to get it." CLI and headless modes provide a `NonInteractiveRemoteInput` impl that returns errors for missing required values. The TUI never calls these functions directly — it resolves the needed values via its own dialog system before calling the non-interactive execution functions.

```rust
/// Trait abstracting user interaction needed by remote commands.
/// CLI/headless modes use `NonInteractiveRemoteInput` which always
/// returns errors for missing required params. TUI mode never calls
/// these — it gathers values via modal dialogs before invoking the
/// underlying execution functions directly.
pub trait RemoteUserInput {
    /// Called when `remote run` has no session.
    fn resolve_missing_session(&self) -> anyhow::Result<String>;

    /// Called when `remote session start` has no directory.
    fn resolve_missing_dir(&self) -> anyhow::Result<String>;

    /// Called when `remote session kill` has no session ID.
    fn resolve_missing_kill_target(&self) -> anyhow::Result<String>;

    /// Called when `remote session start` uses a dir not in savedDirs.
    /// Returns true if the user wants to save it.
    fn offer_save_dir(&self, dir: &str) -> anyhow::Result<bool>;
}

/// Non-interactive implementation: returns descriptive errors for any
/// missing required parameter. Used by CLI dispatch and headless server.
pub struct NonInteractiveRemoteInput;

impl RemoteUserInput for NonInteractiveRemoteInput {
    fn resolve_missing_session(&self) -> anyhow::Result<String> {
        anyhow::bail!(
            "No session specified. Pass --session <ID> or set AMUX_REMOTE_SESSION.\n\
             Use `amux remote session start` to create a session, or list sessions \
             with `curl <remote-addr>/v1/sessions`."
        )
    }

    fn resolve_missing_dir(&self) -> anyhow::Result<String> {
        anyhow::bail!(
            "No directory specified. Pass a directory argument.\n\
             To use saved directories interactively, run this command from the TUI."
        )
    }

    fn resolve_missing_kill_target(&self) -> anyhow::Result<String> {
        anyhow::bail!(
            "No session ID specified. Pass a session ID argument.\n\
             To select a session interactively, run this command from the TUI."
        )
    }

    fn offer_save_dir(&self, _dir: &str) -> anyhow::Result<bool> {
        // Non-interactive: never save. The user can add dirs via
        // `amux config set remote.savedDirs ...` manually.
        Ok(false)
    }
}
```

Functions in the module:

- **`pub async fn run(action: RemoteAction) -> Result<()>`** — top-level dispatch. Creates `NonInteractiveRemoteInput` and delegates to the typed functions below.
- **`pub async fn run_remote_run(remote_addr: &str, session_id: &str, command: &[String], follow: bool, output: &mut dyn Write) -> Result<()>`** — the core execution function that all three modes call once they have resolved addr and session. Submits command to `POST /v1/commands`, optionally follows with SSE streaming, writes output (including summary table) to the `Write` sink.
- **`pub async fn run_remote_session_start(remote_addr: &str, dir: &str) -> Result<String>`** — creates a session via `POST /v1/sessions`, returns the session ID. No user interaction.
- **`pub async fn run_remote_session_kill(remote_addr: &str, session_id: &str) -> Result<()>`** — closes a session via `DELETE /v1/sessions/:id`. No user interaction.
- **`pub fn resolve_remote_addr(flag: Option<&str>) -> Result<String>`** — flag → `AMUX_REMOTE_ADDR` env → `remote.defaultAddr` config. Returns descriptive error if none found.
- **`pub fn resolve_remote_session(flag: Option<&str>) -> Option<String>`** — flag → `AMUX_REMOTE_SESSION` env. Returns `None` if neither set (caller decides whether to error or show picker).
- **`pub async fn fetch_sessions(remote_addr: &str) -> Result<Vec<RemoteSessionEntry>>`** — calls `GET /v1/sessions`, returns parsed list. Used by both the TUI picker and the core functions.
- **`pub async fn stream_command_logs(remote_addr: &str, command_id: &str, output: &mut dyn Write) -> Result<()>`** — connects to the SSE endpoint, writes each line to the output sink, returns when `[amux:done]` sentinel is received.
- **`pub fn save_dir_to_config(dir: &str) -> Result<()>`** — adds `dir` to `remote.savedDirs` in global config if not already present.

`RemoteSessionEntry` is a public struct: `pub struct RemoteSessionEntry { pub id: String, pub workdir: String }`.

**Summary table:** Use plain Unicode box-drawing characters (no new dependency). The table is written to the output sink, so it works identically in CLI (stdout), TUI (execution window output channel), and headless (log file).

```
┌──────────────┬────────────────────────────────────────┐
│ Field        │ Value                                  │
├──────────────┼────────────────────────────────────────┤
│ Command ID   │ 3f2a1b…                                │
│ Session ID   │ c9d4e…                                 │
│ Subcommand   │ execute prompt hello --yolo            │
│ Status       │ done                                   │
│ Exit Code    │ 0                                      │
│ Started      │ 2026-04-22T10:00:00Z                   │
│ Finished     │ 2026-04-22T10:02:31Z                   │
└──────────────┴────────────────────────────────────────┘
```

### 4. SSE log-streaming endpoint (`src/commands/headless/server.rs`)

Add a new route:

```
GET /v1/commands/:id/logs/stream
```

**Why SSE over WebSocket:** SSE is a plain HTTP long-lived response, requires no protocol upgrade, is trivially consumed by `reqwest` with streaming, is naturally unidirectional (server → client), and integrates cleanly with axum's `Response` type. WebSockets are bi-directional and would require an extra dependency (`axum-ws` / `tokio-tungstenite`). SSE is the correct tool for this use case.

**Implementation in `server.rs`:**

```rust
.route("/v1/commands/:id/logs/stream", get(handle_stream_command_logs))
```

Handler logic:
1. Look up the command in the DB; return 404 if not found.
2. Open the command's `output.log` file (the same file written by `execute_command`).
3. Read all existing content from the file and send it to the client line-by-line as SSE `data:` events.
4. If the command status is already `done` or `error`, send a final `data: [amux:done]\n\n` event and close the response.
5. If the command is still `pending` or `running`, tail the file: poll for new bytes every 250 ms using `tokio::time::sleep`. Each new chunk is split into lines and sent as `data:` events. The poller also re-checks the DB status after each poll; when it transitions to `done` or `error`, flush remaining bytes, send `data: [amux:done]\n\n`, and close.
6. The response `Content-Type` is `text/event-stream`. Use `axum::response::Response` with a `Body::from_stream(...)` using `tokio_stream`.

**SSE event format** (standard):
```
data: <line of log output>\n\n
```
Sentinel event when done:
```
data: [amux:done]\n\n
```

Add `tokio-stream = "0.1"` to `[dependencies]`.

Both the streaming SSE client and the existing `execute_command` task write independently to the same `output.log` file — the SSE handler reads, the executor writes. No coordination is needed beyond the file system; the poller reads new bytes as they arrive.

### 5. Dispatch (`src/commands/mod.rs`)

Add `pub mod remote;` to `src/commands/mod.rs`.

Add `Command::Remote { action }` match arm in the `run()` function:

```rust
Command::Remote { action } => commands::remote::run(action).await,
```

Note: `remote` commands are intentionally **not** affected by `headless.alwaysNonInteractive`. Remote commands do not run local containers — they forward requests to a remote host. The `alwaysNonInteractive` setting is applied by the remote host's own dispatch layer when it executes the forwarded command.

### 6. Compile-time parity enforcement (`src/commands/parity.rs`)

Add three new variants to `CommandId`:

```rust
pub enum CommandId {
    // ... existing variants ...
    RemoteRun,
    RemoteSessionStart,
    RemoteSessionKill,
}
```

Add them to `CommandId::ALL`:

```rust
pub const ALL: &[CommandId] = &[
    // ... existing entries ...
    CommandId::RemoteRun,
    CommandId::RemoteSessionStart,
    CommandId::RemoteSessionKill,
];
```

Update all three `ModeParity` implementations (exhaustive match, no wildcard):

```rust
// CliMode — all implemented directly
impl ModeParity for CliMode {
    fn command_support(cmd: CommandId) -> ModeSupport {
        match cmd {
            // ... existing arms ...
            CommandId::RemoteRun => ModeSupport::Implemented,
            CommandId::RemoteSessionStart => ModeSupport::Implemented,
            CommandId::RemoteSessionKill => ModeSupport::Implemented,
        }
    }
}

// TuiMode — all implemented directly (with TUI-specific interactive pickers)
impl ModeParity for TuiMode {
    fn command_support(cmd: CommandId) -> ModeSupport {
        match cmd {
            // ... existing arms ...
            CommandId::RemoteRun => ModeSupport::Implemented,
            CommandId::RemoteSessionStart => ModeSupport::Implemented,
            CommandId::RemoteSessionKill => ModeSupport::Implemented,
        }
    }
}

// HeadlessMode — delegated to CLI (subprocess)
impl ModeParity for HeadlessMode {
    fn command_support(cmd: CommandId) -> ModeSupport {
        match cmd {
            // ... existing arms ...
            CommandId::RemoteRun => ModeSupport::DelegatesToCli,
            CommandId::RemoteSessionStart => ModeSupport::DelegatesToCli,
            CommandId::RemoteSessionKill => ModeSupport::DelegatesToCli,
        }
    }
}
```

This ensures that if any future work item adds a new command, all three modes must be updated — the compiler enforces it.

### 7. Spec parity and TUI autocomplete (`src/commands/spec.rs`)

Add flag lists using the existing `FlagSpec` struct:

```rust
pub static REMOTE_RUN_FLAGS: &[FlagSpec] = &[
    FlagSpec { name: "remote-addr", takes_value: true,  value_name: "URL",  hint: "remote headless amux host address" },
    FlagSpec { name: "session",     takes_value: true,  value_name: "ID",   hint: "session ID on the remote host" },
    FlagSpec { name: "follow",      takes_value: false, value_name: "",     hint: "stream logs until command completes" },
];

pub static REMOTE_SESSION_START_FLAGS: &[FlagSpec] = &[
    FlagSpec { name: "remote-addr", takes_value: true, value_name: "URL", hint: "remote headless amux host address" },
];

pub static REMOTE_SESSION_KILL_FLAGS: &[FlagSpec] = &[
    FlagSpec { name: "remote-addr", takes_value: true, value_name: "URL", hint: "remote headless amux host address" },
];
```

Add to `ALL_COMMANDS`:

```rust
pub static ALL_COMMANDS: &[CommandSpec] = &[
    // ... existing entries ...
    CommandSpec { name: "remote run",           flags: REMOTE_RUN_FLAGS },
    CommandSpec { name: "remote session start", flags: REMOTE_SESSION_START_FLAGS },
    CommandSpec { name: "remote session kill",  flags: REMOTE_SESSION_KILL_FLAGS },
];
```

Note: The short flag `-f` (alias for `--follow`) is handled by clap in CLI mode. In TUI mode, the flag parser only handles `--`-prefixed long flags. The TUI `execute_command` match arm for `remote run` must manually check for `-f` in the token list and treat it as equivalent to `--follow` (see section 9 below).

Update the `tui_implemented_commands_have_spec_entries` test in `parity.rs` to include the new commands:

```rust
(CommandId::RemoteRun, &["remote run"]),
(CommandId::RemoteSessionStart, &["remote session start"]),
(CommandId::RemoteSessionKill, &["remote session kill"]),
```

### 8. TUI state additions (`src/tui/state.rs`)

#### New `PendingCommand` variants

```rust
pub enum PendingCommand {
    // ... existing variants ...

    /// remote run: waiting for session picker or ready to dispatch.
    RemoteRun {
        remote_addr: String,
        session: Option<String>,
        command: Vec<String>,
        follow: bool,
    },

    /// remote session start: waiting for saved-dir picker or save-dir confirmation.
    RemoteSessionStart {
        remote_addr: String,
        dir: Option<String>,
    },

    /// remote session kill: waiting for session picker.
    RemoteSessionKill {
        remote_addr: String,
        session_id: Option<String>,
    },
}
```

#### New `Dialog` variants

```rust
pub enum Dialog {
    // ... existing variants ...

    /// Remote run: no session configured — show picker before executing.
    /// TUI-only. CLI/headless modes never reach this; they error instead.
    RemoteSessionPicker {
        /// Sessions fetched from the remote host.
        sessions: Vec<RemoteSessionEntry>,
        /// Index of the currently highlighted row.
        selected_idx: usize,
        /// The pending command args to execute after the user picks a session.
        pending_command: Vec<String>,
        /// Whether --follow was requested.
        follow: bool,
        /// Remote address already resolved.
        remote_addr: String,
    },

    /// remote session start: pick from savedDirs.
    /// TUI-only. CLI/headless modes error if no dir is passed.
    RemoteSavedDirPicker {
        /// Saved dirs from global config.
        dirs: Vec<String>,
        /// Currently highlighted index.
        selected_idx: usize,
        /// Remote address already resolved.
        remote_addr: String,
    },

    /// remote session start: new dir not in savedDirs — offer to save it.
    /// TUI-only. CLI/headless modes silently skip saving.
    RemoteSaveDirConfirm {
        /// The directory path to potentially save.
        dir: String,
        remote_addr: String,
    },

    /// remote session kill: show picker of active sessions to kill.
    /// TUI-only. CLI/headless modes error if no session ID is passed.
    RemoteSessionKillPicker {
        sessions: Vec<RemoteSessionEntry>,
        selected_idx: usize,
        remote_addr: String,
    },
}
```

#### `TabState` addition

```rust
/// The last session ID successfully used with `remote run` in this tab.
/// TUI-only; not persisted to disk.
pub last_remote_session_id: Option<String>,
```

`RemoteSessionEntry` is imported from `crate::commands::remote::RemoteSessionEntry`.

### 9. TUI dispatch (`src/tui/mod.rs`) — `execute_command` match arm

Add `"remote"` to the `SUBCOMMANDS` list in `src/tui/input.rs`:

```rust
const SUBCOMMANDS: &[&str] = &[
    "init", "ready", "implement", "chat", "exec", "specs", "claws", "status", "config", "remote",
];
```

Add a `"remote" =>` match arm in `execute_command()`:

```rust
"remote" => {
    match parts.get(1) {
        Some(&"run") => {
            // --- Parse remote-run-specific flags only ---
            // The TUI flag parser handles --remote-addr, --session, --follow.
            // Everything else is the opaque passthrough command.
            let remote_run_spec = crate::commands::spec::ALL_COMMANDS
                .iter().find(|c| c.name == "remote run").unwrap();
            let flags = flag_parser::parse_flags(&parts[2..], remote_run_spec);
            let remote_addr_flag = flag_parser::flag_string(&flags, "remote-addr")
                .map(str::to_string);
            let session_flag = flag_parser::flag_string(&flags, "session")
                .map(str::to_string);
            let follow = flag_parser::flag_bool(&flags, "follow")
                || parts[2..].contains(&"-f");  // Manual short-flag check

            // Resolve remote addr (flag → env → config).
            let remote_addr = match crate::commands::remote::resolve_remote_addr(
                remote_addr_flag.as_deref(),
            ) {
                Ok(addr) => addr,
                Err(e) => {
                    app.active_tab_mut().input_error = Some(e.to_string());
                    return;
                }
            };

            // Build the opaque command vector: everything after "remote run"
            // that is NOT a remote-run flag (--remote-addr, --session, --follow, -f).
            // This preserves inner command flags like --yolo untouched.
            let command: Vec<String> = extract_passthrough_command(
                &parts[2..],
                &["--remote-addr", "--session", "--follow", "-f"],
            );
            if command.is_empty() {
                app.active_tab_mut().input_error = Some(
                    "Usage: remote run <command> [--session ID] [--follow] [--remote-addr URL]"
                        .into(),
                );
                return;
            }

            // Resolve session: flag → env → last_remote_session_id (TUI-only tier).
            let session = crate::commands::remote::resolve_remote_session(
                session_flag.as_deref(),
            ).or_else(|| app.active_tab().last_remote_session_id.clone());

            if let Some(session_id) = session {
                // Session known — dispatch immediately.
                app.active_tab_mut().pending_command = PendingCommand::RemoteRun {
                    remote_addr, session: Some(session_id), command, follow,
                };
                launch_pending_command(app).await;
            } else {
                // Session unknown — fetch session list, then show picker.
                app.active_tab_mut().pending_command = PendingCommand::RemoteRun {
                    remote_addr: remote_addr.clone(),
                    session: None, command, follow,
                };
                // Async fetch sessions, then open RemoteSessionPicker dialog.
                // (See "TUI async remote fetch" below.)
                fetch_and_show_session_picker(app, &remote_addr).await;
            }
        }

        Some(&"session") => {
            match parts.get(2) {
                Some(&"start") => {
                    let remote_session_start_spec = crate::commands::spec::ALL_COMMANDS
                        .iter().find(|c| c.name == "remote session start").unwrap();
                    let flags = flag_parser::parse_flags(
                        &parts[3..], remote_session_start_spec,
                    );
                    let remote_addr_flag = flag_parser::flag_string(
                        &flags, "remote-addr",
                    ).map(str::to_string);

                    let remote_addr = match crate::commands::remote::resolve_remote_addr(
                        remote_addr_flag.as_deref(),
                    ) {
                        Ok(addr) => addr,
                        Err(e) => {
                            app.active_tab_mut().input_error = Some(e.to_string());
                            return;
                        }
                    };

                    // Extract positional dir arg (first non-flag token after "start").
                    let dir: Option<String> = parts[3..].iter()
                        .find(|s| !s.starts_with("--") && !s.starts_with('-'))
                        .map(|s| s.to_string());

                    if let Some(d) = dir {
                        // Dir provided — dispatch, then maybe offer to save.
                        app.active_tab_mut().pending_command =
                            PendingCommand::RemoteSessionStart {
                                remote_addr: remote_addr.clone(),
                                dir: Some(d.clone()),
                            };
                        // Check if dir is in savedDirs; if not, show save confirm.
                        let saved = crate::config::effective_remote_saved_dirs();
                        if !saved.contains(&d) {
                            app.active_tab_mut().dialog = Dialog::RemoteSaveDirConfirm {
                                dir: d, remote_addr,
                            };
                        } else {
                            launch_pending_command(app).await;
                        }
                    } else {
                        // No dir — show saved-dir picker (TUI-only interactive flow).
                        let saved = crate::config::effective_remote_saved_dirs();
                        if saved.is_empty() {
                            app.active_tab_mut().input_error = Some(
                                "No directory specified and no savedDirs configured. \
                                 Pass a directory argument or add paths via: \
                                 config set remote.savedDirs --global".into(),
                            );
                            return;
                        }
                        app.active_tab_mut().pending_command =
                            PendingCommand::RemoteSessionStart {
                                remote_addr: remote_addr.clone(), dir: None,
                            };
                        app.active_tab_mut().dialog = Dialog::RemoteSavedDirPicker {
                            dirs: saved, selected_idx: 0, remote_addr,
                        };
                    }
                }

                Some(&"kill") => {
                    let remote_session_kill_spec = crate::commands::spec::ALL_COMMANDS
                        .iter().find(|c| c.name == "remote session kill").unwrap();
                    let flags = flag_parser::parse_flags(
                        &parts[3..], remote_session_kill_spec,
                    );
                    let remote_addr_flag = flag_parser::flag_string(
                        &flags, "remote-addr",
                    ).map(str::to_string);

                    let remote_addr = match crate::commands::remote::resolve_remote_addr(
                        remote_addr_flag.as_deref(),
                    ) {
                        Ok(addr) => addr,
                        Err(e) => {
                            app.active_tab_mut().input_error = Some(e.to_string());
                            return;
                        }
                    };

                    // Extract positional session-id arg.
                    let session_id: Option<String> = parts[3..].iter()
                        .find(|s| !s.starts_with("--") && !s.starts_with('-'))
                        .map(|s| s.to_string());

                    if let Some(sid) = session_id {
                        // Session ID provided — dispatch directly.
                        app.active_tab_mut().pending_command =
                            PendingCommand::RemoteSessionKill {
                                remote_addr, session_id: Some(sid),
                            };
                        launch_pending_command(app).await;
                    } else {
                        // No session ID — fetch sessions, show kill picker.
                        app.active_tab_mut().pending_command =
                            PendingCommand::RemoteSessionKill {
                                remote_addr: remote_addr.clone(), session_id: None,
                            };
                        fetch_and_show_session_kill_picker(app, &remote_addr).await;
                    }
                }

                _ => {
                    app.active_tab_mut().input_error = Some(
                        "Usage: remote session <start|kill>".into(),
                    );
                }
            }
        }

        _ => {
            app.active_tab_mut().input_error = Some(
                "Usage: remote <run|session>  e.g. remote run implement 0042 --follow"
                    .into(),
            );
        }
    }
}
```

**`extract_passthrough_command` helper** (new function in `tui/mod.rs`):

Filters out known `remote run` flags and their values from the token list, returning everything else as the opaque command vector. This ensures inner command flags like `--yolo` are preserved untouched.

```rust
/// Extract the passthrough command tokens from a `remote run` command line.
/// Strips remote-run-specific flags (and their values) so the inner command
/// (e.g. `execute prompt hello --yolo`) is forwarded intact.
fn extract_passthrough_command(tokens: &[&str], strip_flags: &[&str]) -> Vec<String> {
    let mut result = Vec::new();
    let mut i = 0;
    // Flags that take a value argument (must consume the next token too).
    let value_flags = ["--remote-addr", "--session"];
    while i < tokens.len() {
        let t = tokens[i];
        if strip_flags.contains(&t) {
            if value_flags.contains(&t) {
                i += 1; // skip the value too
            }
            i += 1;
            continue;
        }
        // Handle --flag=value form for remote-run flags.
        if let Some((key, _)) = t.split_once('=') {
            if strip_flags.contains(&key) {
                i += 1;
                continue;
            }
        }
        result.push(t.to_string());
        i += 1;
    }
    result
}
```

**`fetch_and_show_session_picker` / `fetch_and_show_session_kill_picker` helpers:**

These async functions call `crate::commands::remote::fetch_sessions()`, then on success open the appropriate `Dialog` variant on the active tab. On error, they set `input_error` on the tab instead. They use the tab's background task mechanism (spawn a tokio task that posts results to the TUI event channel).

**`launch_pending_command` extension in `tui/mod.rs`:**

Add match arms for the three new `PendingCommand` variants:

```rust
PendingCommand::RemoteRun { remote_addr, session, command, follow } => {
    let session_id = session.expect("session must be resolved before launch");
    launch_remote_run(app, &remote_addr, &session_id, &command, follow).await;
}
PendingCommand::RemoteSessionStart { remote_addr, dir } => {
    let d = dir.expect("dir must be resolved before launch");
    launch_remote_session_start(app, &remote_addr, &d).await;
}
PendingCommand::RemoteSessionKill { remote_addr, session_id } => {
    let sid = session_id.expect("session_id must be resolved before launch");
    launch_remote_session_kill(app, &remote_addr, &sid).await;
}
```

**`launch_remote_run` implementation:**

Runs `remote::run_remote_run()` as a text-mode background task using `spawn_text_command` (same pattern as `status` command). The output sink receives SSE log lines and the summary table. The tab transitions through `ExecutionPhase::Running` → `Done`/`Error`. On successful completion, stores the session_id in `TabState.last_remote_session_id`.

**`launch_remote_session_start` / `launch_remote_session_kill` implementations:**

Similar pattern: spawn text-mode background tasks, write results to the tab's output channel.

### 10. TUI input handling — new `Action` variants (`src/tui/input.rs`)

Add new `Action` variants for dialog confirmations:

```rust
pub enum Action {
    // ... existing variants ...

    /// Remote session picker: user selected a session for `remote run`.
    RemoteSessionChosen {
        session_id: String,
        remote_addr: String,
        command: Vec<String>,
        follow: bool,
    },

    /// Remote saved-dir picker: user selected a directory for `remote session start`.
    RemoteSavedDirChosen {
        dir: String,
        remote_addr: String,
    },

    /// Remote save-dir confirm: user accepted saving the new dir.
    RemoteSaveDirAccepted {
        dir: String,
        remote_addr: String,
    },

    /// Remote save-dir confirm: user declined saving the new dir.
    RemoteSaveDirDeclined {
        dir: String,
        remote_addr: String,
    },

    /// Remote session kill picker: user selected a session to kill.
    RemoteSessionKillChosen {
        session_id: String,
        remote_addr: String,
    },
}
```

Add key handlers in `handle_key` for the new `Dialog` variants:

**`RemoteSessionPicker` / `RemoteSessionKillPicker` / `RemoteSavedDirPicker`:**
- `↑` / `↓`: move `selected_idx` (clamped to 0..len-1).
- `Enter`: close dialog, return the appropriate `Action` variant with the selected item. For `RemoteSessionPicker`, also store `session_id` in `TabState.last_remote_session_id`.
- `Esc`: close dialog, return `Action::None` (cancel).
- Empty list: both `Enter` and `Esc` close the dialog without action.

**`RemoteSaveDirConfirm`:**
- `y`: return `RemoteSaveDirAccepted`.
- `n`: return `RemoteSaveDirDeclined`.
- `Esc`: return `Action::None` (cancel entirely).

In the main event loop (the `match action { ... }` block in `tui/mod.rs`), add handlers for each new Action:

- `RemoteSessionChosen`: update `TabState.last_remote_session_id`, set `PendingCommand::RemoteRun` with the chosen session, call `launch_pending_command`.
- `RemoteSavedDirChosen`: set `PendingCommand::RemoteSessionStart` with the chosen dir, call `launch_pending_command`.
- `RemoteSaveDirAccepted`: call `remote::save_dir_to_config(dir)`, then set `PendingCommand::RemoteSessionStart` with the dir, call `launch_pending_command`.
- `RemoteSaveDirDeclined`: set `PendingCommand::RemoteSessionStart` with the dir (without saving), call `launch_pending_command`.
- `RemoteSessionKillChosen`: set `PendingCommand::RemoteSessionKill` with the chosen session, call `launch_pending_command`.

### 11. TUI rendering (`src/tui/render.rs`)

Add rendering for all four new `Dialog` variants:

**`RemoteSessionPicker` and `RemoteSessionKillPicker`:**
- Bordered modal centered on screen, title: "Select Session" / "Kill Session".
- Scrollable list showing `session_id | workdir` for each entry.
- Highlighted row uses the standard selection style.
- Footer: `↑↓ navigate  Enter confirm  Esc cancel`.
- Empty list: show "No active sessions on <addr>. Run `remote session start` first."

**`RemoteSavedDirPicker`:**
- Same style, title: "Select Directory".
- List shows directory paths.
- Footer: `↑↓ navigate  Enter confirm  Esc cancel`.

**`RemoteSaveDirConfirm`:**
- Small centered modal: "Save '/path/to/dir' to remote.savedDirs? (y/n)".

### 12. Headless server support (`src/commands/headless/server.rs`)

Add `"remote"` to `KNOWN_SUBCOMMANDS`:

```rust
const KNOWN_SUBCOMMANDS: &[&str] = &[
    "implement", "chat", "ready", "init", "status", "specs", "config", "exec", "remote",
];
```

When the headless server dispatches a `remote` subcommand via `execute_command`, it spawns `amux remote ...` as a subprocess (same pattern as all other subcommands). The spawned process uses the `NonInteractiveRemoteInput` impl from section 3, so it will never attempt interactive pickers. If a required param is missing from the args vector, the subprocess exits with a clear error that appears in the command's log file.

The `remote` command is intentionally **not** in the `supports_non_interactive` list in `execute_command` (line ~617), because `remote` commands have no `--non-interactive` flag — they don't run local containers.

### 13. Dependencies (`Cargo.toml`)

**Promote `reqwest` to regular dependency with streaming support:**

Change the existing `[dependencies]` entry from:
```toml
reqwest = { version = "0.12", features = ["rustls-tls"], default-features = false }
```
to:
```toml
reqwest = { version = "0.12", features = ["rustls-tls", "json", "stream"], default-features = false }
```

The `[dev-dependencies]` entry can keep its current features or be removed (since the regular dep is now a superset).

**Add `tokio-stream`:**
```toml
tokio-stream = "0.1"
```

No other new dependencies. The summary table uses manual Unicode box-drawing (no `comfy-table`).


## Edge Case Considerations:

### Address and session resolution
- **No remote address configured:** If all three resolution sources (flag, env, config) are absent, `resolve_remote_addr` returns a descriptive error: `"No remote address configured. Pass --remote-addr, set AMUX_REMOTE_ADDR, or set remote.defaultAddr in ~/.amux/config.json."` This error is identical across CLI, TUI, and headless modes.
- **Empty command vector for `remote run`:** Reject immediately with a usage error before making any network call. Applies uniformly to all modes.
- **Session not found on remote:** When `--session` is given but the server returns 404, print a clear error with the session ID and suggest `amux remote session start`.
- **Remote host unreachable:** All `reqwest` calls must time out if no connection is established after 10 s connect or 60s of connection silence. Present a human-readable error including the target address.

### CLI/headless mode: no interactive pickers
- **`remote run` without `--session` in CLI/headless:** `NonInteractiveRemoteInput.resolve_missing_session()` returns a clear error with guidance to pass `--session` or set `AMUX_REMOTE_SESSION`. No picker is attempted.
- **`remote session start` without dir in CLI/headless:** `NonInteractiveRemoteInput.resolve_missing_dir()` returns a clear error. No saved-dir picker is attempted.
- **`remote session kill` without session-id in CLI/headless:** `NonInteractiveRemoteInput.resolve_missing_kill_target()` returns a clear error. No picker is attempted.
- **`remote session start` with new dir in CLI/headless:** `NonInteractiveRemoteInput.offer_save_dir()` returns `false` silently. The user can save dirs manually via `amux config set remote.savedDirs`.

### TUI interactive flows
- **TUI session picker with zero active sessions:** Show the modal with an empty list and a message: "No active sessions on `<addr>`. Run `remote session start` first." `Enter` and `Esc` both cancel.
- **TUI session picker fetch failure:** If fetching sessions from the remote fails (network error, non-200), set `input_error` on the tab rather than opening the modal.
- **TUI `remote session kill` with no sessions:** Same empty-list pattern as session picker.
- **TUI `remote session start` with no savedDirs and no dir argument:** Show `input_error`: "No directory specified and no savedDirs configured."
- **`remote run` session env var (`AMUX_REMOTE_SESSION`) takes precedence over `TabState.last_remote_session_id` in TUI:** The env var is treated as an explicit user choice; the tab memory is a convenience fallback only.
- **TUI session picker pre-selection:** When `TabState.last_remote_session_id` is set and the sessions list contains a matching entry, `selected_idx` initializes to that entry's index; otherwise defaults to 0.

### SSE streaming
- **`--follow` with already-completed command:** The SSE endpoint replays historical log output then immediately sends `[amux:done]`. The client handles this as a normal completion.
- **SSE client disconnects mid-stream:** The server drops the stream; the underlying `execute_command` task continues unaffected.
- **Log file not yet created when stream is requested:** The SSE handler waits up to 10s (polling every 1s) for the log file to appear before returning a 404.
- **`--follow` without a terminal (piped output):** Log lines are written to stdout without ANSI decoration, making output script-friendly.

### Config and saving
- **Saving a dir to `remote.savedDirs`:** If the dir is already in the list, silently skip the save rather than duplicating.
- **`remote.savedDirs` config set via CLI:** JSON array values must be accepted (e.g. `amux config set remote.savedDirs '["\/workspace\/a","\/workspace\/b"]' --global`).
- **Concurrent `remote run` calls to the same session:** The remote server enforces the one-command-per-session rule (HTTP 403). The client surfaces this error clearly.

### Passthrough command parsing in TUI
- **Inner command flags like `--yolo` must not be consumed by the TUI flag parser.** The `extract_passthrough_command` helper strips only `remote run`-specific flags (`--remote-addr`, `--session`, `--follow`, `-f`). All other tokens — including flags belonging to the inner command — are forwarded intact.


## Test Considerations:

### Unit tests — `src/commands/remote.rs`
- `resolve_remote_addr`: flag wins over env which wins over config; missing all three returns an error with the expected message.
- `resolve_remote_session`: flag wins over env; missing both returns `None`.
- Empty command vector triggers early error before any HTTP call.
- `NonInteractiveRemoteInput` returns descriptive errors for each `resolve_missing_*` method.
- `save_dir_to_config` adds a new dir; skips duplicate dirs.

### Unit tests — `src/config/mod.rs`
- `RemoteConfig` round-trips through JSON with both fields, only `defaultAddr`, only `savedDirs`, and neither.
- `GlobalConfig` with nested `remote` block serializes with camelCase keys (`defaultAddr`, `savedDirs`) and omits the block when `None`.
- `effective_remote_default_addr()` returns `None` when not configured.
- `effective_remote_saved_dirs()` returns an empty `Vec` when not configured.
- Old flat `remoteDefaultAddr` key (if it ever existed) does not deserialize into `GlobalConfig.remote` (consistency with the `headlessWorkDirs` breaking-change pattern).

### Unit tests — `src/commands/config.rs`
- `config get remote.defaultAddr` and `config get remote.savedDirs` return expected values.
- `config set remote.defaultAddr http://1.2.3.4:9876 --global` persists correctly.
- `config set remote.savedDirs` with comma-separated paths persists as a JSON array.
- `config show` output includes `remote.defaultAddr` and `remote.savedDirs` rows.

### Unit tests — `src/cli.rs`
- `remote run execute prompt hello --follow` parses to `RemoteAction::Run` with `command = ["execute", "prompt", "hello"]` and `follow = true`.
- `remote run --remote-addr http://1.2.3.4:9876 --session abc123 implement 0042` parses correctly.
- `remote session start /workspace/proj` parses to `RemoteSessionAction::Start { dir: Some("/workspace/proj"), .. }`.
- `remote session start` with no args parses to `RemoteSessionAction::Start { dir: None, .. }`.
- `remote session kill` with no session-id parses to `RemoteSessionAction::Kill { session_id: None, .. }`.
- `-f` is accepted as a short form of `--follow`.

### Unit tests — `src/commands/headless/server.rs`
- `is_valid_subcommand("remote")` returns `true`.
- SSE handler returns 404 for an unknown command ID.
- SSE handler returns `text/event-stream` content type.
- SSE handler sends `[amux:done]` sentinel for a command already in `done` status.

### Unit tests — `src/commands/parity.rs`
- `CommandId::ALL` includes `RemoteRun`, `RemoteSessionStart`, `RemoteSessionKill`.
- `CliMode::command_support` returns `Implemented` for all three.
- `TuiMode::command_support` returns `Implemented` for all three.
- `HeadlessMode::command_support` returns `DelegatesToCli` for all three.
- The existing `tui_implemented_commands_have_spec_entries` test passes with the new entries.

### Unit tests — TUI
- `extract_passthrough_command`: `remote run implement 0001 --yolo` → `["implement", "0001", "--yolo"]`
  (inner command flag `--yolo` must be preserved, not stripped).
- `extract_passthrough_command`: `remote run implement 0001 --session abc123 --yolo` →
  `["implement", "0001", "--yolo"]` (both the `--session` flag AND its value `abc123` must be stripped).
- `extract_passthrough_command`: `--remote-addr=http://host:9876` (equals form) is stripped correctly.
- `extract_passthrough_command`: `-f` is stripped from the passthrough (boolean short flag).
- `extract_passthrough_command`: inner command short flags like `-n` are preserved.
- `-f` short form of `--follow`: command `remote run implement 0001 -f` produces `follow = true`.
- `--follow` long form: command `remote run implement 0001 --follow` produces `follow = true`.
- Neither `-f` nor `--follow` present: `follow = false`.
- The `SUBCOMMANDS` list includes `"remote"`.
- `closest_subcommand("remte")` returns `Some("remote")` (typo correction).

### TUI unit tests — Dialog state
- `RemoteSessionPicker` with non-empty sessions: `↓` increments `selected_idx` (capped at `sessions.len() - 1`); `↑` decrements (floored at 0); `Enter` returns `RemoteSessionChosen` with the correct `session_id`.
- Pre-selection: when `last_remote_session_id` is `Some("abc")` and the sessions list contains an entry with `id = "abc"`, `selected_idx` initializes to that entry's index.
- Pre-selection fallback: when `last_remote_session_id` is `Some("unknown-id")` and no session has that ID, `selected_idx` defaults to 0.
- Empty session list: `Enter` and `Esc` both close the modal without action.
- `RemoteSaveDirConfirm`: `y` returns `RemoteSaveDirAccepted`, `n` returns `RemoteSaveDirDeclined`, action proceeds with session start.
- `RemoteSaveDirConfirm`: `Esc` returns `Action::None` AND clears `pending_command` (session start is cancelled entirely, not just the save).
- `RemoteSaveDirConfirm`: `Enter` returns `RemoteSaveDirDeclined` (proceed without saving).

### TUI unit tests — launch guards
- `launch_remote_run` with `session_id = ""` sets `input_error`, clears `pending_command`, does not spawn any task.
- `launch_remote_session_start` with `dir = ""` sets `input_error`, clears `pending_command`, does not spawn any task.

### Integration tests — remote run
- Without `--follow`: spin up the headless server on a random port, create a session, submit a command via the `run_remote_run` code path, poll until done, assert DB state and log file.
- With `--follow`: same setup, use the SSE stream, assert log output matches `output.log` and `[amux:done]` is the final event.

### Integration tests — remote session start/kill
- `run_remote_session_start` against a live test server creates a session.
- `run_remote_session_kill` marks the session as closed.

### Integration tests — SSE endpoint
- Completed command: connect, receive all log lines, then `[amux:done]`, connection closes.
- Running command: partial log content arrives during execution; `[amux:done]` arrives after process completes.

### CLI spec parity tests (`src/commands/spec.rs`)
- Verify `REMOTE_RUN_FLAGS`, `REMOTE_SESSION_START_FLAGS`, `REMOTE_SESSION_KILL_FLAGS` are registered in `ALL_COMMANDS` and that their flag names match the clap struct definitions.

### Test infrastructure
- Use `AMUX_REMOTE_ADDR` env var override in tests that exercise the resolution helper.
- Use `AMUX_CONFIG_HOME` for global config isolation (pattern from WI 0058).


## Codebase Integration:
- Follow the `Command::Variant { action: ActionEnum }` pattern from `Command::Headless` / `Command::Exec` in `src/cli.rs`. Mirror the dispatch in `src/commands/mod.rs` with `Command::Remote { action } => commands::remote::run(action).await`.
- `src/commands/remote.rs` is a new file; register it with `pub mod remote;` in `src/commands/mod.rs`.
- The `RemoteUserInput` trait in `remote.rs` follows the same pattern as other user-input abstractions in the codebase: CLI/headless modes get a non-interactive impl that errors on missing params; TUI mode gathers the needed values via its own dialog system and then calls the parameter-complete execution functions directly.
- The `GlobalConfig` struct gains a `remote: Option<RemoteConfig>` field following the same `#[serde(skip_serializing_if = "Option::is_none")]` pattern as `headless: Option<HeadlessConfig>`.
- Add `remote.defaultAddr` and `remote.savedDirs` entries to `ALL_FIELDS` in `src/commands/config.rs` so they appear in `config show` and the TUI config dialog.
- For `config get/set` dot-path support: add explicit match arms in `src/commands/config.rs` alongside the existing `headless.*` arms from WI 0058.
- The SSE endpoint in `server.rs` uses `axum::response::Response` + `Body::from_stream` with `tokio_stream`. No heavy SSE library needed.
- Promote `reqwest` from dev-dependency to regular dependency with `features = ["rustls-tls", "json", "stream"]`. Align with existing version `0.12`.
- Add `tokio-stream = "0.1"` as a new dependency.
- All new TUI `Dialog` variants must be added to `Dialog` enum in `src/tui/state.rs`, rendered in `src/tui/render.rs`, and handled in `src/tui/input.rs`.
- All new `Action` variants must be handled in the main event loop in `src/tui/mod.rs`.
- All new `PendingCommand` variants must be handled in `launch_pending_command` in `src/tui/mod.rs`.
- Add `"remote"` to the `SUBCOMMANDS` list in `src/tui/input.rs` for tab-completion and typo suggestions.
- Add `CommandId::RemoteRun`, `CommandId::RemoteSessionStart`, `CommandId::RemoteSessionKill` to `src/commands/parity.rs` with exhaustive match arms in all three mode implementations. Update `CommandId::ALL`. Update the `tui_implemented_commands_have_spec_entries` test.
- Add `REMOTE_RUN_FLAGS`, `REMOTE_SESSION_START_FLAGS`, and `REMOTE_SESSION_KILL_FLAGS` to `src/commands/spec.rs` and include them in `ALL_COMMANDS`.
- Add `"remote"` to `KNOWN_SUBCOMMANDS` in `src/commands/headless/server.rs`.
- The `last_remote_session_id` field on `TabState` is per-tab persistent only for the lifetime of the TUI process; not persisted to disk.
- The `extract_passthrough_command` helper in `tui/mod.rs` ensures inner command flags are not consumed by the TUI flag parser.
- Remote commands are not affected by `headless.alwaysNonInteractive` (they don't run local containers). The remote host applies its own `alwaysNonInteractive` setting when executing the forwarded command.
- Documentation regarding remote mode and execution must show examples using `amux remote...` AND cURL when possible so that users/consumers/agents can choose either option when interacting with a remote headless amux server.
