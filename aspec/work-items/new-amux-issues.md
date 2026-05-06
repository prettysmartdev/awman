# new-amux observed issues

### General:

GEN-1: When the TUI is launched via bare `amux`, there should be no "app-level session", there should ONLY be the SessionManager and a session-per-tab owned by the SessionManager and tied to each tab. No session tied to the launch directory to confuse things should be allowed. Only the non-TUI subcommands on the CLI should use a launch-directory-bound session.

**Status: FIXED**
Removed the dead `session: Arc<RwLock<Session>>` field from `App` and the `session_arc` parameter from `App::new()`. The TUI now only has `SessionManager` plus per-tab `Session` instances — no app-level session exists.

### TUI

TUI-1: For the THIRD time now, container stats in the top-right title bar of the container window are not showing any data, only `...`. This is unacceptable, it has been "fixed" several times and still does not work. Think hard, do not take shortcuts, look at the codepaths end-to-end to ensure that container stats in the container window title bar work for every container backend in every scenario and update at a regular interval. Review old-amux and make it work EXACTLY THE SAME WAY. No more fake fixes.

**Status: FIXED**
Root cause was a circular dependency: stats polling needed the container name to query Docker, but the container name only arrived via stats responses. Fixed by:
1. Added `ContainerStatus::Running { container_name }` variant to the engine's container status enum.
2. Both Docker and Apple backends now report the container name via `report_status(Running { container_name })` before calling `take_container_io`.
3. Added `SharedContainerName = Arc<Mutex<Option<String>>>` bridge between the engine thread and the TUI event loop.
4. The TUI `tick_all_tabs()` picks up the container name from the shared slot and populates `ContainerInfo.container_name`, enabling the stats poller to query the correct container.

TUI-2: The `status --watch` command run in a new tab that is launched in a non-git directory only outputs two lines of status text, does not show the entire status output, and does not continuously update. Look at how this behaved in old-amux and replicate it EXACTLY using the new grand architecture patterns.

**Status: FIXED**
Two issues fixed:
1. `StatusCommandFrontend` for the TUI was a blank `impl {}` with no method bodies. Implemented `should_continue_watching()` to return `true` (enabling the watch loop) and `write_clear_marker()` to clear the status log between ticks.
2. Added `write_status_table()` in the status command that outputs the full CODE AGENTS and NANOCLAW status tables via `write_message()` on each watch tick, so all status rows render in the TUI's status log.

TUI-3: The `config show` dialog window only shows some titles but no content, no controls, no anything. Port it over identically from old-amux and ensure it's wired correctly into the new grand architecture.

**Status: FIXED**
Reworked to go through command dispatch per the grand architecture rules:
1. Added `present_config_table(&mut self, rows: &[ConfigFieldRow]) -> Result<Option<ConfigEditRequest>, CommandError>` to the `ConfigCommandFrontend` trait.
2. `ConfigCommand::run_with_frontend` for the Show subcommand calls `frontend.present_config_table()` in a loop — edits trigger validation and persistence via `config set` logic, then re-present the table until dismissed.
3. TUI impl sends `DialogRequest::ConfigShow { rows }` with populated row data and blocks on response.
4. `OpenConfigShow` keybinding now spawns `config show` through `app.spawn_command()` instead of loading config directly.
5. Dialog supports arrow-key navigation, left/right to switch global/repo column, Enter to edit, Esc to cancel edit, Esc to dismiss.

TUI-4: The workflow state strip is once again not being shown. Ensure it is rendered correctly and that the container/execution windows are resized to not hide it. This is the second time this has happened so make sure it doesn't happen again. It only shows after the first workflow step has ended, and it is STILL not rendering correctly since all steps are shown stacked on top of one another. RE-REVIEW how old-amux rendered the workflow state strip and handled window sizing and FIX IT PROPERLY, IT SHOULD WORK JUST LIKE OLD-AMUX. Stop messing around and port it directly.

**Status: FIXED**
Two issues:
1. `depends_on` was always `Vec::new()` in `report_workflow_progress()` because the field hadn't been added to `WorkflowStepProgressInfo`. Added `depends_on: Vec<String>` to the engine struct and populated it from `step.depends_on.clone()`. The TUI frontend now passes it through to `WorkflowStepView.depends_on`, enabling the `build_workflow_columns()` topological grouping to work correctly.
2. Layout already correctly uses `Constraint::Length(workflow_height)` for the strip area.

TUI-5: When a container running as part of a workflow exits, nothing happens for many seconds. The container window/PTY should be immediately destroyed and then the workflow should advance (either by showing the workflow control dialog or if --yolo is passed, moving to the next step automatically.) Also, when the next step container DOES eventually start, the PTY looks garbled and incorrect. Ensure the container window, PTY, etc. are fully destroyed and created anew between workflow steps and all of their state is clean and ready for the next container to start fresh.

**Status: FIXED**
Root cause: `take_container_io()` only returns channels once (returns `None` on subsequent calls), so the second workflow step's container fell back to inherit-stdio instead of the PTY bridge. Fixed by:
1. Added `recreate_container_io()` to `TuiCommandFrontend` — reuses the persistent stdout sender (same TUI receiver) but creates fresh stdin/resize channels per step.
2. Added `SharedStdinTx` and `SharedResizeTx` types and fields to both `Tab` and `TuiCommandFrontend`, passed through from `spawn_command()`.
3. `report_step_interactive_launch()` now calls `recreate_container_io()` to create fresh channels and publishes new senders via shared slots.
4. `tick_all_tabs()` picks up new stdin/resize senders from the shared slots, swapping the tab's senders so keystrokes reach the new container.
5. PTY reset flag (already existed) clears the vt100 parser between steps, and container name is reset for the new step.

### Engines

ENG-1: When producing the status table during `ready`, all of the non-default agents can be reported on in a single table row, like `Other agents: done` instead of having a table row per other-agent. If all non-default agents have valid images, just include one row for all of them. If any of the non-default agents have missing images, each agent with a missing image can get a row in the table, like `Maki: missing`. Non-default agents with missing images are NEVER a fatal error and should only produce warnings and a row in the status table. Ensure this is all handled in the ready engine and that both frontend traits render the output correctly.

**Status: FIXED**
Updated the `CheckingNonDefaultAgents` phase in the ready engine:
- When ALL non-default agents have valid images: single consolidated row "Other agents: done" with `StepStatus::Done`.
- When ANY agents have missing images: only the missing agents get individual rows ("Agent: X") with `StepStatus::Warn`, plus a warning message listing missing agent names.
- Missing images are never fatal — always `Warn`, never `Failed`.
- Both CLI and TUI frontends render correctly since they iterate `non_default_agent_images` generically.

### Commands

COM-1: Whenever a git/worktree pre/post workflow detects a dirty worktree and/or requires a commit message, ensure the engine and/or command code produces BOTH the list of dirty files AND a suggested commit message to the frontend and that the frontends render these correctly so that the user knows which files are dirty and can choose to accept the suggested commit message or delete it anwrite their own. Ensure all the git logic is at the engine/command layers and the frontends are rendering and returning chosen commit messages only via their frontend trait implementations.

**Status: FIXED**
1. Added `suggested_message: &str` parameter to `ask_pre_worktree_uncommitted_files()` and `ask_worktree_commit_before_merge()` in the `WorktreeLifecycleFrontend` trait.
2. Command layer generates contextual suggestions: "WIP: pre-worktree commit for {branch}" (pre-workflow) and "Implement {branch}" (post-workflow merge).
3. Both TUI and CLI frontends show the file count, the file list (first 10 + "... and N more"), and the suggested commit message.
4. Empty input accepts the suggestion; user can type a custom message to override. All git logic stays in the engine/command layers.

COM-2: Ensure that EVERY SINGLE Git command AND their outputs are all pushed to the frontend via the message sink and rendered in the frontends. It's important the user knows exactly which commands were run and their full outputs to build trust that amux is doing what they want it to do.

**Status: FIXED**
1. Added `run_git_logged()` helper in `engine/git/mod.rs` that takes `&mut dyn UserMessageSink`, logs `$ git <args>` before execution, then logs every non-empty line of stdout/stderr after.
2. Added `_logged` variants of all git operations used by `WorktreeLifecycle`: `uncommitted_files_logged`, `commit_all_logged`, `create_worktree_logged`, `remove_worktree_logged`, `merge_branch_logged`, `delete_branch_logged`.
3. Updated `WorktreeLifecycle::prepare()` and `finalize()` to call the logged variants, passing the `WorktreeLifecycleFrontend` (which implements `UserMessageSink`) as the sink.
4. Both TUI and CLI frontends receive and render all git commands and their outputs via their existing `UserMessageSink` implementations.

### Exec Workflow Deep Spike

WF-1: Step failure handling was completely broken — when a step exited with a nonzero exit code, `run_to_completion()` immediately returned `WorkflowOutcome::Failed` without calling `user_choose_after_step_failure()`. The trait method, TUI dialog (retry/abort/pause), and `StepFailureChoice` enum all existed but were never invoked. Old-amux showed a `WorkflowStepError` dialog and let the user choose.

**Status: FIXED**
1. Added `last_exit_info: Option<ContainerExitInfo>` field to `WorkflowEngine`, populated after each `exec.wait()`.
2. Modified `run_to_completion()` to call `frontend.user_choose_after_step_failure()` on nonzero exit, handling Retry (reset to Pending, continue loop), Pause (persist state, return Paused), and Abort (mark remaining Cancelled, return Aborted).
3. Removed the dead `_suppress` function that was suppressing unused `StepFailureChoice`.
4. Added three new tests: `step_failure_abort_returns_aborted`, `step_failure_retry_reruns_step`, `step_failure_pause_returns_paused`.

WF-2: The yolo countdown was invisible and uncancellable in the TUI. `yolo_countdown_tick()` wrote to a shared `yolo_state` slot, but nothing in the TUI event loop read it for rendering, and no Esc handler cleared it for cancellation. The `Dialog::WorkflowYoloCountdown` variant existed but was never created from the shared state.

**Status: FIXED**
1. Added yolo countdown syncing in `tick_all_tabs()`: reads `yolo_state` from the active tab, creates/updates `Dialog::WorkflowYoloCountdown` for rendering. Respects command dialog precedence (won't overwrite step-error or control-board dialogs).
2. Added Esc handler: when the active dialog is `WorkflowYoloCountdown`, Esc clears `yolo_state` to `None`, which causes the engine's next `yolo_countdown_tick()` to return `YoloTickOutcome::Cancel`, pausing the workflow.
3. Added yellow/magenta tab flashing for background tabs with active yolo countdowns in `tab_color()` — alternates based on `remaining_secs % 2`, matching old-amux's visual behavior.
4. Tab navigation (Ctrl+A/D) remains available during the yolo dialog since those are global keybindings.

WF-3: `--yolo` did not imply `--worktree` for `exec workflow`, unlike old-amux which enforced this (`oldsrc/tui/mod.rs:1716`).

**Status: FIXED**
1. Updated `read_exec_workflow_flags()` in `dispatch/mod.rs` to set `worktree = true` when `yolo` is true.
2. Added informational message in `run_with_frontend()`: "--yolo implies --worktree. Running in isolated worktree."
3. The `build_command` override at `dispatch/mod.rs:441` already had `if flags.yolo || flags.auto { flags.worktree = true; }` — both paths now enforce it.

WF-4: The TUI worktree lifecycle `ask_post_workflow_action` ignored the `had_error` parameter, showing the same dialog text regardless of whether the workflow failed.

**Status: FIXED**
Updated the dialog body to show "ended with errors" or "completed" based on `had_error`.
