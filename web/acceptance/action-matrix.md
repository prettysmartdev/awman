# webapp v0 — TUI screen/action acceptance matrix

Rendered from `action-matrix.json` (schema version 1); do not edit by hand. One row per TUI screen/action. `daemon` names the engine operation (awman.frontend.v1 frame, awman.terminal.v1 frame, HTTP route, or local view state); `browser` names the control and shortcut (or why it is unavailable).

Legend: parity `engine_trace` = identical engine-side outcome for the scripted action on TUI and web; `wire_frame` = the exact frame is asserted; `view_only` = rendered from the same snapshot; `web_only` = no TUI counterpart.


## startup

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `startup.probe` | Probe the same-machine daemon | n/a | route: GET http://127.0.0.1:9877/v1/health with Origin (browser extension of the health body, design §2.6); re-probe every 3 s on the connect screen | Connect screen: automatic, plus 'Recheck' button [web only] | `health` | engine | required | web_only |
| `startup.loopback_permission` | Browser loopback permission prompt | n/a | none: none (browser permission; fetch marked targetAddressSpace=loopback where supported) | 'Allow local access' button; denied state explained [web only] | `health` | engine | required | web_only |
| `startup.recipe` | Unavailable-engine recipe | n/a | none: two-line awman config set recipe; `awman daemon start --background` only when the probe is inconclusive; `awman daemon status` for diagnostics | copyable recipe block [web only] | `health` | engine | required | web_only |
| `startup.connect_browser` | Connect browser (browser link) | n/a | route: POST /v1/browser-links → poll POST /v1/browser-links/{id}/poll with polling_secret → grant (design §2.4) | 'Connect browser' button → shows web-XXXX-XXXX and `awman link web-…` [web only] | `browser_links` | engine | required | web_only |
| `startup.pick_launch_context` | Choose the workspace (launch context) | n/a | route: GET /v1/engine/launch_contexts; default preselected (design §3.4) | Workspace list with the default preselected [web only] | `launch_contexts` | engine | required | web_only |
| `startup.create_instance` | Start an instance | `awman` | frontend_frame: POST /v1/engine/instances {} then Hello{kind: web, role: primary, launch_context} over the ticketed WebSocket | 'Open workspace' button | `web_frontend` | engine | required | wire_frame |
| `startup.version_handshake` | Version / protocol handshake | n/a | route: GET /v1/health protocols + GET /v1/status capabilities; incompatible ⇒ blocked with update message (no restart from the browser) | unavailable: restarting the daemon is owner-side; the webapp only reports — 'Update required' panel | `frontend_protocol` | engine | required | view_only |
| `startup.initial_tab_ready` | Initial tab runs `ready` in a Git repo | automatic | frontend_frame: engine opens the initial tab (Snapshot tab with execution_phase running: ready; CommandOutput/CommandBytes stream) | execution area shows the run | `frontend_protocol` | engine | required | engine_trace |
| `startup.initial_tab_agents_watch` | Initial tab runs `agents status --watch` outside a repo | automatic | frontend_frame: engine starts the watch; AgentsDashboard deltas; Esc/Ctrl+C ⇒ Cancel{tab_id, repeat:false} while agents_dashboard.watching | dashboard panel; 'Stop watching' button `Esc` | `frontend_protocol` | engine | required | engine_trace |
| `startup.welcome` | Welcome message after ready | automatic | frontend_frame: CommandOutput frames | execution area | `frontend_protocol` | engine | required | view_only |
| `startup.resume_after_reload` | Resume the same instance after a reload | n/a | frontend_frame: Hello{resume: frontend_id} within WEB_PRIMARY_RECONNECT_GRACE (60 s) after re-linking; Snapshot resync; no replay of unacknowledged frames | automatic when sessionStorage holds resume ids [web only] | `web_frontend` | engine | required | web_only |
| `startup.fatal_engine_lost` | Engine lost / detached | Enter/Esc/Ctrl+C | frontend_frame: Detach{reason} or socket loss beyond grace ⇒ fatal panel with the reason; 'Reconnect' re-probes | fatal panel: reason + Reconnect button | `frontend_protocol` | engine | required | view_only |
| `startup.attach_secondary` | Attach a second browser tab as a secondary view | n/a | frontend_frame: Hello{kind: web, role: secondary, view} on the ticketed socket; Welcome.role = secondary; the secondary inherits roots, cannot close tabs or quit and gets no primary-exit prompts | 'Attach read only' button on an existing browser instance in the workspace picker; 'Open workspace' creates a new instance instead | `web_frontend` | engine | required | wire_frame |
| `startup.config_error_fatal` | Startup refused (configuration or runtime error) | Enter/Esc/Ctrl+C | frontend_frame: attach refused: {code, error} text frame then close (AttachRefusal::StartFailed); no Welcome/Snapshot | 'Engine disconnected' panel showing the engine's code and error; 'Reconnect' clears the resume ids and returns to the workspace picker | `frontend_protocol` | engine | required | view_only |

## repo

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `repo.new_tab_dialog` | Open the New Tab dialog | Ctrl+T | local: none until confirmed | '+' button on the tab strip `Ctrl+T` (alt `Alt+T`) | `frontend_protocol` | engine | required | view_only |
| `repo.new_tab_submit` | Open a tab for a directory | Enter | frontend_frame: OpenTab{dir, options{kind: normal, startup: ready\|agents_status_watch}} + UiRequest::OpenTab{dir}; Web frontends may only name a launch-context root or a directory beneath it (dir_outside_context otherwise) | root picker + path field, 'Open' button `Enter` | `launch_contexts` | engine | required | engine_trace |
| `repo.new_tab_cancel` | Cancel the New Tab dialog | Esc | local: none | Cancel button `Esc` | `frontend_protocol` | engine | required | view_only |
| `repo.new_tab_failure` | Failed to open a tab | Enter/Esc | frontend_frame: Notice{level: error, text} (engine) or dir_outside_context (web) | modal with the error; Dismiss `Esc` | `frontend_protocol` | engine | required | view_only |
| `repo.new_tab_non_git` | Open a non-Git directory | Enter | frontend_frame: OpenTab{dir, options{startup: agents_status_watch}} chosen by the engine | same dialog | `frontend_protocol` | engine | required | engine_trace |
| `repo.new_tab_squad` | Open the squad tab from the New Tab dialog | Ctrl+S | frontend_frame: OpenTab{dir, options{kind: squad}} + UiRequest::OpenTab; squad data then comes from /v1/squad/* (design D11) | 'Open squad' button in the dialog `Ctrl+S` | `engine_instances` | squad | required | engine_trace |
| `repo.mount_scope` | Mount scope question for a subdirectory | g / c / Esc | frontend_frame: DialogRequest::MountScope → DialogResponse::Char('g'\|'c') / Dismissed | dialog with two buttons `g / c / Esc` | `frontend_protocol` | engine | required | engine_trace |
| `repo.context_display` | CWD / worktree in the status row | n/a | frontend_frame: TabSnapshot.session.cwd, worktree_path (Workflow delta) | status row | `frontend_protocol` | engine | required | view_only |

## tabs

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `tabs.prev` | Switch to the previous tab | Ctrl+A | frontend_frame: FocusTab{tab_id} (active-tab hint) + UiRequest::PrevTab | tab strip click; swipe left (narrow) `Ctrl+A` (alt `Alt+Left`) | `frontend_protocol` | engine | required | wire_frame |
| `tabs.next` | Switch to the next tab | Ctrl+D | frontend_frame: FocusTab{tab_id} + UiRequest::NextTab | tab strip click; swipe right (narrow) `Ctrl+D` (alt `Alt+Right`) | `frontend_protocol` | engine | required | wire_frame |
| `tabs.close_confirm` | Close-tab confirmation (multiple tabs) | Ctrl+C | local: none until confirmed | tab '×' or Ctrl+C opens the dialog; buttons 'Close tab' / 'Quit awman' / Cancel `Ctrl+C` | `frontend_protocol` | engine | required | view_only |
| `tabs.close` | Close the current tab | Ctrl+T (in CloseTabConfirm) | frontend_frame: CloseTab{tab_id} + UiRequest::CloseTab{tab_id} | 'Close tab' button in the confirmation `Ctrl+T` (alt `Alt+T`) | `frontend_protocol` | engine | required | engine_trace |
| `tabs.close_last_quits` | Closing the last tab quits the instance | Ctrl+C, Ctrl+C | frontend_frame: Quit (after QuitConfirm) | same dialog; 'Quit awman' `Ctrl+C` | `frontend_protocol` | engine | required | engine_trace |
| `tabs.label` | Tab label, colour and command | n/a | frontend_frame: TabLabel{text,color} deltas; Phase deltas | tab strip | `frontend_protocol` | engine | required | view_only |
| `tabs.program_status_glyph` | Program status glyph in the tab | n/a | frontend_frame: ProgramStatus{tab_id, slot_id, status} frames; SlotSnapshot.program_status | tab strip | `agent_status` | engine | required | view_only |
| `tabs.stuck_yellow` | Stuck warning (⚠️, yellow) | any key / switch / scroll clears | frontend_frame: Stuck/SlotStuck deltas; Activity{tab_id} (coalesced ≤ 1/s) when the user types or scrolls | tab strip; any interaction sends Activity | `frontend_protocol` | engine | required | engine_trace |
| `tabs.yolo_countdown_label` | Background yolo countdown in the tab label | n/a | frontend_frame: Yolo/SlotYolo deltas (YoloState) | tab strip | `frontend_protocol` | engine | required | view_only |
| `tabs.width_truncation` | Tab width and truncation | n/a | local: none | responsive tab strip; overflow scrolls horizontally | `frontend_protocol` | engine | required | view_only |
| `tabs.squad_singleton` | Squad tab is a singleton | Ctrl+T, Ctrl+S | frontend_frame: FocusTab{tab_id} when a squad tab exists (TabKindWire::Squad) | 'Open squad' focuses the existing tab | `frontend_protocol` | squad | required | engine_trace |
| `tabs.remote_session_magenta` | Remote (server-cloned) session colour | n/a | frontend_frame: TabLabel color remote | tab strip | `frontend_protocol` | engine | required | view_only |

## command_box

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `cmd.type` | Type into the command box | printable keys | local: none (local editor); Activity{tab_id} coalesced | composer textarea | `frontend_protocol` | engine | required | view_only |
| `cmd.submit` | Execute the typed command | Enter | frontend_frame: Submit{tab_id, input: command_box{raw}, pty_size}; the engine parses and runs it | Enter or 'Run' button `Enter` | `frontend_protocol` | engine | required | engine_trace |
| `cmd.parse_error` | Unknown command / flag feedback | n/a | frontend_frame: engine answers a failed Submit with CommandFinished{result.error{code,message}} / Notice; the webapp shows the same text and keeps the input | inline error under the composer | `frontend_protocol` | engine | required | engine_trace |
| `cmd.newline` | Insert a newline | Ctrl+Enter / Shift+Enter | local: none | Shift+Enter `Shift+Enter` (alt `Ctrl+Enter`) | `frontend_protocol` | engine | required | view_only |
| `cmd.autocomplete_next` | Cycle to the next suggestion | Tab | route: GET /v1/engine/catalogue (cached by ETag); completion computed locally with the TUI algorithm | suggestion chips; Tab `Tab` | `engine_catalogue` | engine | required | view_only |
| `cmd.autocomplete_prev` | Cycle to the previous suggestion | Shift+Tab | route: same catalogue | suggestion chips; Shift+Tab `Shift+Tab` | `engine_catalogue` | engine | required | view_only |
| `cmd.flag_hints` | Flag hints for the typed command | n/a | route: GET /v1/engine/catalogue flags[] | hint row under the composer | `engine_catalogue` | engine | required | view_only |
| `cmd.cursor` | Cursor movement and editing | ← → Ctrl+← Ctrl+→ Home End Backspace Ctrl+Backspace Delete | local: none | native textarea editing `native` | `frontend_protocol` | engine | required | view_only |
| `cmd.focus_execution` | Focus the execution window from the box | ↑ | local: none | click the log; ↑ on an empty single-line composer `↑` | `frontend_protocol` | engine | required | view_only |
| `cmd.locked_while_running` | Command box locked while a command runs | n/a | frontend_frame: Phase delta running ⇒ composer disabled | disabled composer with hint | `frontend_protocol` | engine | required | view_only |
| `cmd.q_quit` | `q` opens the quit confirmation | q | local: none until confirmed (then Quit) | q in the empty composer `q` | `frontend_protocol` | engine | required | view_only |
| `cmd.copy_selection` | Copy the execution-window selection without leaving the box | Ctrl+Y | local: none | 'Copy' button on the selection toolbar; Ctrl+Y (user gesture) `Ctrl+Y` | `frontend_protocol` | engine | required | view_only |
| `cmd.ghost_text` | `q to quit` ghost text | n/a | local: none | placeholder | `frontend_protocol` | engine | required | view_only |

## execution_log

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `exec.output` | Streamed command output | n/a | frontend_frame: CommandOutput{tab_id, stream, level, text} and CommandBytes{tab_id, stream, bytes} frames; virtualized list | execution area | `frontend_protocol` | engine | required | view_only |
| `exec.scroll` | Scroll the output | ↑/↓, PgUp/PgDn, b/e, mouse wheel | local: none | scroll; Home/End buttons; keys when the log has focus `↑/↓ PgUp/PgDn b/e` | `frontend_protocol` | engine | required | view_only |
| `exec.focus_back` | Return focus to the command box | Esc | local: none | Esc or click the composer `Esc` | `frontend_protocol` | engine | required | view_only |
| `exec.status_log_toggle` | Toggle the status log (collapsed/expanded) | l | frontend_frame: StatusLine/StatusLogReset deltas (data); toggle is local | 'Status log' disclosure `l` | `frontend_protocol` | engine | required | view_only |
| `exec.select_copy` | Select and copy text | mouse drag + Ctrl+Y | local: none | native selection + Copy button / Ctrl+Y / Ctrl+C with a selection `Ctrl+Y` | `frontend_protocol` | engine | required | view_only |
| `exec.border_colour` | Execution window border colour | n/a | frontend_frame: Phase deltas | header state badge | `frontend_protocol` | engine | required | view_only |
| `exec.agents_watch_stop` | Stop `agents status --watch` | Esc / Ctrl+C | frontend_frame: Cancel{tab_id, repeat:false} | 'Stop watching' button `Esc` | `frontend_protocol` | engine | required | engine_trace |
| `exec.crash_log_notice` | Crash log pointer | n/a | frontend_frame: StatusLine level error | status log | `frontend_protocol` | engine | required | view_only |
| `exec.last_container_summary` | Container exit summary bar | n/a | frontend_frame: LastContainerSummary delta | summary bar | `frontend_protocol` | engine | required | view_only |

## agent_status

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `status.agents_dashboard` | Agents status dashboard | n/a | frontend_frame: AgentsDashboard{data{overview, watching}} deltas | dashboard table | `frontend_protocol` | engine | required | view_only |
| `status.container_title` | Container title: agent, state, stats | n/a | frontend_frame: SlotLaunched/SlotContainerName/SlotStats/ProgramStatus | terminal header | `agent_status` | engine | required | view_only |
| `status.minimized_bar` | Minimized container status bar | n/a | frontend_frame: SlotSnapshot fields + SlotStats | collapsed terminal bar | `frontend_protocol` | engine | required | view_only |
| `status.squad_indicator` | Squad ● health indicator | n/a | frontend_frame: SquadHealth deltas (InstanceSnapshot.squad_health) | status row dot | `frontend_protocol` | squad | required | view_only |
| `status.working_silent` | `working · silent Nm` note | n/a | frontend_frame: ProgramStatus + local timer since last output | terminal header | `agent_status` | engine | required | view_only |
| `status.stats_poll` | CPU/memory stats refresh (~3 s) | n/a | frontend_frame: SlotStats deltas | terminal header / bars | `frontend_protocol` | engine | required | view_only |

## git_sidebar

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `git.toggle` | Toggle the Git sidebar | Ctrl+G | local: view state; data from GitSummary deltas | 'Git' button; Ctrl+G `Ctrl+G` | `frontend_protocol` | engine | required | view_only |
| `git.summary` | `+X -Y (branch)` summary in the status row | n/a | frontend_frame: GitSummary{summary{branch, files, added, removed}} | status row | `frontend_protocol` | engine | required | view_only |
| `git.file_list` | Changed files with counts and colours | n/a | frontend_frame: GitSummary files[] | sidebar list | `frontend_protocol` | engine | required | view_only |
| `git.title` | Sidebar title (`main: 3 changed`, `main: clean`, `HEAD: …`, `git status`) | n/a | frontend_frame: GitSummary branch/files | sidebar header | `frontend_protocol` | engine | required | view_only |
| `git.narrow` | Narrow terminal treats the sidebar as closed | n/a | local: none | narrow viewport: sidebar becomes a sheet; summary stays in the status row | `frontend_protocol` | engine | required | view_only |
| `git.refresh` | 2-second refresh | n/a | frontend_frame: GitSummary deltas | sidebar | `frontend_protocol` | engine | required | view_only |

## config

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `config.open` | Open the config dialog | Ctrl+, or `config show` | frontend_frame: Submit{tab_id, input: argv{["config","show"]}} ⇒ DialogRequest::ConfigShow{rows, selected, rejected} | 'Config' button; Ctrl+, `Ctrl+,` | `frontend_protocol` | engine | required | engine_trace |
| `config.navigate` | Move between rows and columns | ↑/↓ PgUp/PgDn ←/→ | local: none | table navigation `↑/↓ PgUp/PgDn ←/→` | `frontend_protocol` | engine | required | view_only |
| `config.edit_begin` | Start editing a field | Enter or e | local: none | click a cell / Enter / e `Enter` | `frontend_protocol` | engine | required | view_only |
| `config.edit_save` | Save an edit | Enter (while editing) | frontend_frame: DialogResponse::ConfigEdit{field, value, global}; engine re-raises ConfigShow with `rejected` on failure | Enter / Save `Enter` | `frontend_protocol` | engine | required | engine_trace |
| `config.edit_cancel` | Cancel an edit | Esc (while editing) | local: none | Esc / Cancel `Esc` | `frontend_protocol` | engine | required | view_only |
| `config.close` | Close the dialog | Esc | frontend_frame: DialogResponse::Dismissed | Close / Esc `Esc` | `frontend_protocol` | engine | required | wire_frame |
| `config.new_entry` | Add an agentsToModels mapping or guidance entry | Ctrl+N | frontend_frame: DialogResponse::ConfigEdit for the new member field (ConfigEditRequest::new_member) | 'Add' button on map/list header rows `Ctrl+N` (alt `Alt+N`) | `frontend_protocol` | engine | required | engine_trace |
| `config.remove_entry` | Remove a mapping/entry by saving an empty value | Enter with empty value | frontend_frame: ConfigEdit{value: ""} | Clear + Save | `frontend_protocol` | engine | required | engine_trace |
| `config.value_hint` | Format hint / detail pane | n/a | frontend_frame: ConfigShowRow.value_hint | detail pane | `frontend_protocol` | engine | required | view_only |

## squad

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `squad.unavailable_panel` | Squad not served on this listener | n/a | route: GET /v1/health features/listener.features; squad routes 404/503 | panel with recipe `awman config set daemon.network.listeners.loopback.features engine,squad` [web only] | `named_listeners` | squad | required | web_only |
| `squad.grid` | Task card grid | n/a | route: GET /v1/squad/tasks polled every 2 s while the tab is focused | card grid | `resource_routes` | squad | required | view_only |
| `squad.grid_nav` | Move among cards | ↑ ↓ ← → | local: none | arrow keys / click `↑ ↓ ← →` | `frontend_protocol` | squad | required | view_only |
| `squad.detail` | Task detail modal | Enter | route: GET /v1/squad/tasks/{id} | card click / Enter `Enter` | `resource_routes` | squad | required | view_only |
| `squad.history` | Run history modal; Enter shows one run | h | route: GET /v1/squad/tasks/{id}/runs | 'History' button / h `h` | `resource_routes` | squad | required | view_only |
| `squad.attach` | Attach to the running task | a | frontend_frame: Submit{tab_id, input: argv{["squad","attach",name]}}; terminal via GET /v1/terminals?owner_id=task_… and the ticketed awman.terminal.v1 socket | 'Attach' button / a `a` | `frontend_protocol` | squad | required | engine_trace |
| `squad.detach` | Detach from the attached task | Ctrl+\ | frontend_frame: Cancel{tab_id, repeat:false} for the attach command (the TUI cancels the attach command's token); terminal socket detached{reason:user} | 'Detach' button; Ctrl+\ `Ctrl+\` | `frontend_protocol` | squad | required | engine_trace |
| `squad.new` | Create a task (interview) | n | frontend_frame: Submit{tab_id, input: argv{catalogue NewSquadTask input}} ⇒ relayed DialogRequests | 'New task' button / n `n` | `frontend_protocol` | squad | required | engine_trace |
| `squad.edit` | Edit a task (interview) | e | frontend_frame: Submit argv{["squad","edit",name,"--interview"]} ⇒ relayed DialogRequests | 'Edit' / e `e` | `frontend_protocol` | squad | required | engine_trace |
| `squad.pause` | Pause a task | p | route: GET /v1/engine/prompts?kind=squad_task_confirm&action=pause&name=… for the wording; on yes PATCH /v1/squad/tasks/{id} {paused:true} | 'Pause' / p then confirm `p` | `engine_catalogue` | squad | required | engine_trace |
| `squad.resume` | Resume a task | r | route: PATCH /v1/squad/tasks/{id} {paused:false} | 'Resume' / r `r` | `resource_routes` | squad | required | engine_trace |
| `squad.trigger` | Trigger a task now | t | route: prompt route then POST /v1/squad/tasks/{id}/runs | 'Trigger' / t then confirm `t` | `engine_catalogue` | squad | required | engine_trace |
| `squad.cancel` | Cancel the task's running run | c | route: prompt route then POST /v1/squad/runs/{id}/cancel for the active run | 'Cancel run' / c then confirm `c` | `engine_catalogue` | squad | required | engine_trace |
| `squad.delete` | Remove a task | d | route: DELETE /v1/squad/tasks/{id} after the local confirmation | 'Delete' / d then confirm `d` | `resource_routes` | squad | required | engine_trace |
| `squad.action_error` | Failed squad action | n/a | route: {error, code} body shown in the hint bar | hint bar | `frontend_protocol` | squad | required | view_only |
| `squad.start_confirm` | Squad daemon not running | y / n | route: squad feature off ⇒ 503 feature_disabled ⇒ panel with `awman config set daemon.features.squad.enabled true` | unavailable: starting a daemon feature is owner-side — recipe panel | `frontend_protocol` | squad | required | view_only |
| `squad.command_box_inactive` | Command box inactive on the squad tab | n/a | local: none | composer hidden; reminder text | `frontend_protocol` | squad | required | view_only |

## workflow_overview

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `wf.overview_toggle` | Minimize / maximize the Workflow Overview | Ctrl+O | frontend_frame: UiRequest::OpenOverview{tab_id} on open (presenter claim) ; view data from Workflow deltas | 'Overview' toggle; Ctrl+O `Ctrl+O` | `frontend_protocol` | engine | required | wire_frame |
| `wf.overview_hscroll` | Scroll stages sideways | Shift+← / Shift+→, horizontal wheel, Shift+wheel | local: none | horizontal scroll / ‹ › buttons `Shift+← / Shift+→` | `frontend_protocol` | engine | required | view_only |
| `wf.overview_vscroll` | Scroll an oversized parallel stage | mouse wheel over the overview | local: none | scroll | `frontend_protocol` | engine | required | view_only |
| `wf.step_boxes` | Step boxes: glyph, colour, agent/model title, [setup]/[teardown] | n/a | frontend_frame: Workflow{view{steps[], current_step, max_concurrent, reported}} | overview strip | `frontend_protocol` | engine | required | view_only |
| `wf.context_path_copy` | Copy the workflow context host directory | Ctrl+Shift+C | local: none (TabSnapshot.context_path) | unavailable: Ctrl+Shift+C is browser-reserved (DevTools) in Chromium; button only — 'Copy context path' button in the status row | `frontend_protocol` | engine | required | view_only |
| `wf.focus_window` | Focus the workflow window (presenter request) | n/a | frontend_frame: UiRequest::FocusWorkflowWindow{tab_id} | implicit | `frontend_protocol` | engine | required | wire_frame |

## workflow_control

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `wfc.open` | Open the Workflow Control Board | Ctrl+W | frontend_frame: WorkflowRequest{tab_id, request: open_control_board{step_name}} (focused slot's step); DialogResponse::Char('W') when WorkflowStepConfirm is open | 'Workflow' button; Alt+W `Ctrl+W` (alt `Alt+W`) | `frontend_protocol` | engine | required | wire_frame |
| `wfc.next_new_container` | Next step in a new container | → | frontend_frame: DialogResponse::Char('>') | → button `→` | `frontend_protocol` | engine | required | wire_frame |
| `wfc.next_same_container` | Next step in the same container | ↓ | frontend_frame: DialogResponse::Char('v') | ↓ button `↓` | `frontend_protocol` | engine | required | wire_frame |
| `wfc.restart` | Restart the current step | ↑ | frontend_frame: DialogResponse::Char('^') | ↑ button `↑` | `frontend_protocol` | engine | required | wire_frame |
| `wfc.back` | Cancel to the previous step | ← | frontend_frame: DialogResponse::Char('<') | ← button `←` | `frontend_protocol` | engine | required | wire_frame |
| `wfc.finish` | Finish the workflow | Enter | frontend_frame: DialogResponse::Char('f') | 'Finish' button `Enter` | `frontend_protocol` | engine | required | wire_frame |
| `wfc.retry_failed_peer` | Retry a failed parallel peer | r | frontend_frame: DialogResponse::Char('r') | 'Retry' (red) button `r` | `frontend_protocol` | engine | required | wire_frame |
| `wfc.pause` | Pause the workflow | p | frontend_frame: DialogResponse::Char('p') | 'Pause' button `p` | `frontend_protocol` | engine | required | wire_frame |
| `wfc.abort` | Abort the workflow | Ctrl+C | frontend_frame: DialogResponse::Char('a') | 'Abort' button; Ctrl+C inside the board `Ctrl+C` | `frontend_protocol` | engine | required | wire_frame |
| `wfc.dismiss` | Dismiss (mid-step) / pause (between steps) | Esc | frontend_frame: DialogResponse::Dismissed | Esc / Close `Esc` | `frontend_protocol` | engine | required | wire_frame |
| `wfc.unavailable` | Greyed-out actions with reasons | n/a | frontend_frame: WorkflowControlBoardState can_* and *_unavailable_reason | disabled buttons with reason | `frontend_protocol` | engine | required | view_only |
| `wfc.parallel_labels` | Group-wide labels while a parallel group runs | n/a | frontend_frame: WorkflowControlBoardState fields | board | `frontend_protocol` | engine | required | view_only |

## workflow_approvals

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `appr.step_confirm_advance` | Lightweight step-confirm: advance | Enter | frontend_frame: DialogResponse::Char('>') to WorkflowStepConfirm | 'Next' button `Enter` | `frontend_protocol` | engine | required | wire_frame |
| `appr.step_confirm_pause` | Lightweight step-confirm: pause | Esc | frontend_frame: DialogResponse::Dismissed | 'Pause' / Esc `Esc` | `frontend_protocol` | engine | required | wire_frame |
| `appr.step_confirm_board` | Lightweight step-confirm: open the board | Ctrl+W | frontend_frame: DialogResponse::Char('W') then WorkflowRequest open_control_board | 'Workflow' button `Ctrl+W` (alt `Alt+W`) | `frontend_protocol` | engine | required | wire_frame |
| `appr.yolo_countdown_cancel` | Cancel a yolo countdown | Esc | frontend_frame: CancelCountdown{tab_id, step_name} | 'Cancel countdown' / Esc `Esc` | `frontend_protocol` | engine | required | wire_frame |
| `appr.yolo_countdown_board` | Open the board from the countdown | Ctrl+W | frontend_frame: WorkflowRequest open_control_board | 'Workflow' button `Ctrl+W` (alt `Alt+W`) | `frontend_protocol` | engine | required | wire_frame |
| `appr.yolo_countdown_tab_switch` | Switch tabs while the countdown shows | Ctrl+A / Ctrl+D | frontend_frame: FocusTab + UiRequest::PrevTab/NextTab | tab strip `Ctrl+A / Ctrl+D` (alt `Alt+Left / Alt+Right`) | `frontend_protocol` | engine | required | wire_frame |
| `appr.yolo_countdown_slot_cycle` | Rotate focused container while the countdown shows | Ctrl+S | frontend_frame: FocusedSlot delta (local focus) ; the modal is re-derived | container tabs `Ctrl+S` | `frontend_protocol` | engine | required | view_only |
| `appr.cancel_workflow_confirm` | Ctrl+C while a workflow runs | Ctrl+C then y / n / Esc | frontend_frame: y ⇒ Cancel{tab_id, repeat:false} (workflow abort via the engine); n/Esc local | 'Cancel workflow?' dialog `Ctrl+C` | `frontend_protocol` | engine | required | engine_trace |
| `appr.auto_open_board_on_stuck` | Control board opens automatically on stuck/blocked/error | n/a | frontend_frame: DialogRequest::WorkflowControlBoard | board appears | `frontend_protocol` | engine | required | engine_trace |
| `appr.active_tab_suppression` | Active-tab suppression of idle/done dialogs | typing / scrolling | frontend_frame: Activity{tab_id} frames | any interaction | `frontend_protocol` | engine | required | engine_trace |
| `appr.first_answer_wins` | First answer wins across frontends | n/a | frontend_frame: DialogClosed{dialog_id, answered_by} | dialog closes with 'answered by …' | `frontend_protocol` | engine | required | wire_frame |

## containers

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `ctr.window_cycle` | Cycle the container window (maximized → minimized → hidden) | Ctrl+M | frontend_frame: UiRequest::SetContainerWindow{tab_id, slot_id, state} (presenter claim); PTY resize via terminal Resize{cols,rows} while holding control | window state buttons; Ctrl+M `Ctrl+M` | `frontend_protocol` | engine | required | wire_frame |
| `ctr.detach` | Detach from the container view | Ctrl+\ | frontend_frame: UiRequest::SetContainerWindow minimized (ordinary tab); see squad.detach for the squad tab | 'Detach' button; Ctrl+\ `Ctrl+\` | `frontend_protocol` | engine | required | wire_frame |
| `ctr.focus_next_slot` | Switch the focused parallel container | Ctrl+S | local: FocusedSlot is a view choice; the webapp sends terminal Resize for the newly focused terminal | container tabs; Ctrl+S `Ctrl+S` | `frontend_protocol` | engine | required | view_only |
| `ctr.parallel_bars` | Minimized bars for background containers | n/a | frontend_frame: SlotLaunched/SlotExited/SlotStats/SlotStuck/SlotYolo deltas | bars under the focused terminal | `frontend_protocol` | engine | required | view_only |
| `ctr.exit_close` | Window closes when the container exits | n/a | frontend_frame: SlotExited + LastContainerSummary | terminal unmounts | `frontend_protocol` | engine | required | view_only |
| `ctr.setup_teardown` | Setup/teardown step containers | n/a | frontend_frame: SlotLaunched with phase_step_title | terminal | `frontend_protocol` | engine | required | engine_trace |
| `ctr.acp_window` | ACP agent window (purple) | n/a | frontend_frame: SlotAcpUpdate/SlotAcpPermission deltas; permission answered by DialogResponse paths used by the TUI | ACP panel | `frontend_protocol` | engine | required | view_only |
| `ctr.yolo_modal_focused` | Yolo countdown modal for the focused container | n/a | frontend_frame: SlotYolo delta + DialogRequest::WorkflowYoloCountdown | modal | `frontend_protocol` | engine | required | view_only |

## terminal

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `term.open` | Open the agent terminal | automatic | terminal_frame: POST /v1/browser-tickets{route: terminal, terminal_id} then GET /v1/terminals/{id} upgrade with `awman.terminal.v1, awman.ticket.<t>`; Snapshot then Output frames | terminal view | `browser_tickets` | engine | required | wire_frame |
| `term.typing` | Type into the agent | printable keys / Esc / Tab / arrows / Ctrl+letters | terminal_frame: Input{bytes} encoded exactly as key_handler::key_to_bytes; requires the input grant | focused terminal `keys while the terminal has focus` | `frontend_protocol` | engine | required | wire_frame |
| `term.ctrl_c` | Ctrl+C reaches the agent | Ctrl+C | terminal_frame: Input{bytes: 0x03} | focused terminal; 'Send Ctrl+C' toolbar button `Ctrl+C` | `frontend_protocol` | engine | required | wire_frame |
| `term.take_control` | Take / show input control | n/a | terminal_frame: TakeControl; errors control_held / input_not_allowed; read-only state shown | 'Take control' button; automatic on first keypress when unheld | `terminal` | engine | required | wire_frame |
| `term.resize` | Resize the PTY to the view | automatic on window/sidebar/overview changes | terminal_frame: Resize{cols, rows} debounced 150 ms, only while holding control | automatic | `frontend_protocol` | engine | required | wire_frame |
| `term.scrollback` | Scrollback and live view | mouse wheel, Shift+wheel | local: renderer scrollback bounded to 10 000 lines | scroll; 'Jump to live' button | `frontend_protocol` | engine | required | view_only |
| `term.mouse_forward` | Mouse wheel forwarded to the agent | wheel at live view without Shift | terminal_frame: Input{bytes: SGR mouse report or arrow keys} | wheel over the terminal | `frontend_protocol` | engine | required | wire_frame |
| `term.select_copy` | Select text and copy | mouse drag + Ctrl+Y | local: none | drag + Copy button / Ctrl+Y `Ctrl+Y` | `frontend_protocol` | engine | required | view_only |
| `term.paste` | Paste into the agent | terminal paste | terminal_frame: Input{bytes} (bracketed when mode 2004 is on); >64 KiB chunked; >1 MiB refused with a message | paste / toolbar Paste `Ctrl+V / Shift+Insert` | `frontend_protocol` | engine | required | wire_frame |
| `term.alt_screen` | Alternate screen and redraws | n/a | terminal_frame: Output frames; Snapshot after Lagged | renderer | `frontend_protocol` | engine | required | view_only |
| `term.lagged_snapshot` | Lagged ⇒ fresh snapshot | n/a | terminal_frame: Lagged then Snapshot (renderer reset + write) | automatic | `frontend_protocol` | engine | required | wire_frame |
| `term.exit` | Terminal exit | n/a | terminal_frame: Exit{code}; socket close 1000 | terminal unmounts; summary bar | `frontend_protocol` | engine | required | wire_frame |
| `term.reconnect` | Reconnect a terminal socket | n/a | terminal_frame: new ticket, new upgrade, Snapshot resync | automatic | `browser_tickets` | engine | required | wire_frame |
| `term.touch_toolbar` | Control-key toolbar on touch devices | n/a | terminal_frame: Input{bytes} for Esc/Tab/Ctrl/Alt/arrows | toolbar row above the terminal [web only] | `terminal` | engine | required | web_only |
| `term.untrusted_output` | Untrusted escape sequences are inert | n/a | local: renderer config: OSC 52/7/9/777/1337, DCS, APC, graphics disabled; OSC 8 links listed, opened only on gesture | 'Links detected' list | `frontend_protocol` | engine | required | view_only |
| `term.terminal_release` | Host process terminal (TerminalRelease/Restore) | n/a | frontend_frame: DialogRequest::TerminalRelease{terminal_id} ⇒ inline hub terminal, DialogResponse::Yes; TerminalRestore ⇒ Dismissed | inline release view | `frontend_protocol` | engine | required | wire_frame |
| `term.parallel` | Several terminals at once | Ctrl+S cycles | terminal_frame: one socket per visible terminal; background terminals keep a socket only while shown as bars (snapshot on focus) | container tabs | `frontend_protocol` | engine | required | view_only |

## dialogs

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `dialog.yes_no` | DialogRequest::yes_no | y / n / Esc | frontend_frame: DialogRequest{kind: yes_no} ⇒ DialogResponse::Yes \| No \| Dismissed | Yes / No buttons `y / n / Esc` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.yes_no_cancel` | DialogRequest::yes_no_cancel | y / n / c / Esc | frontend_frame: DialogRequest{kind: yes_no_cancel} ⇒ Yes \| No \| Cancel \| Dismissed | Yes / No / Cancel buttons `y / n / c / Esc` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.text_input` | DialogRequest::text_input | type, Enter / Esc | frontend_frame: DialogRequest{kind: text_input} ⇒ Text(value) \| Dismissed | text field; OK / Cancel `Enter / Esc` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.secret_input` | DialogRequest::secret_input | type (masked), Enter / Esc | frontend_frame: DialogRequest{kind: secret_input} ⇒ Secret(value) \| Dismissed; never logged or persisted | password field; OK / Cancel `Enter / Esc` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.terminal_release` | DialogRequest::terminal_release | automatic | frontend_frame: DialogRequest{kind: terminal_release} ⇒ Yes once the inline hub terminal is mounted (web-native release) | inline terminal | `frontend_protocol` | engine | required | wire_frame |
| `dialog.terminal_restore` | DialogRequest::terminal_restore | automatic | frontend_frame: DialogRequest{kind: terminal_restore} ⇒ Dismissed when the inline terminal is unmounted | automatic | `frontend_protocol` | engine | required | wire_frame |
| `dialog.multiline_input` | DialogRequest::multiline_input | type, Ctrl+Enter/Shift+Enter/Ctrl+S submit, Enter newline, Esc | frontend_frame: DialogRequest{kind: multiline_input} ⇒ Text(value) \| Dismissed | textarea; Submit / Cancel `Ctrl+Enter / Shift+Enter / Esc` (alt `Ctrl+S`) | `frontend_protocol` | engine | required | wire_frame |
| `dialog.list_picker` | DialogRequest::list_picker | ↑/↓, Enter / Esc | frontend_frame: DialogRequest{kind: list_picker} ⇒ Index(i) \| Dismissed | list; Select / Cancel `Enter / Esc` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.kind_select` | DialogRequest::kind_select | ↑/↓ or key, Enter / Esc | frontend_frame: DialogRequest{kind: kind_select} ⇒ Index(i) \| Dismissed | radio list; Select / Cancel `Enter / Esc` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.workflow_control_board` | DialogRequest::workflow_control_board | arrows, Enter, r, p, Ctrl+C, Esc | frontend_frame: DialogRequest{kind: workflow_control_board} ⇒ Char('>' \| 'v' \| '^' \| '<' \| 'f' \| 'r' \| 'p' \| 'a') \| Dismissed (see wfc.*) | board buttons `see wfc.*` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.workflow_yolo_countdown` | DialogRequest::workflow_yolo_countdown | Esc / Ctrl+W / Ctrl+A / Ctrl+D / Ctrl+S | frontend_frame: DialogRequest{kind: workflow_yolo_countdown} ⇒ CancelCountdown \| WorkflowRequest open_control_board \| tab switch | countdown modal with Cancel / Workflow `Esc / Ctrl+W` (alt `Alt+W`) | `frontend_protocol` | engine | required | wire_frame |
| `dialog.workflow_step_confirm` | DialogRequest::workflow_step_confirm | Enter / Esc / Ctrl+W | frontend_frame: DialogRequest{kind: workflow_step_confirm} ⇒ Char('>') \| Dismissed \| Char('W') | Next / Pause / Workflow buttons `Enter / Esc / Ctrl+W` (alt `Alt+W`) | `frontend_protocol` | engine | required | wire_frame |
| `dialog.agent_setup` | DialogRequest::agent_setup | s / f / Esc (keys from the request) | frontend_frame: DialogRequest{kind: agent_setup} ⇒ Char(key) \| Dismissed | buttons per offered key `keys` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.mount_scope` | DialogRequest::mount_scope | g / c / Esc | frontend_frame: DialogRequest{kind: mount_scope} ⇒ Char('g'\|'c') \| Dismissed | Git root / Current directory buttons `g / c / Esc` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.agent_auth` | DialogRequest::agent_auth | key / Esc | frontend_frame: DialogRequest{kind: agent_auth} ⇒ Char(key) \| Dismissed | buttons per offered key `keys` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.quit_confirm` | DialogRequest::quit_confirm | Ctrl+C / Esc | frontend_frame: DialogRequest{kind: quit_confirm} ⇒ Yes (then Quit) \| Dismissed | Quit / Cancel buttons `Ctrl+C / Esc` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.close_tab_confirm` | DialogRequest::close_tab_confirm | Ctrl+C / Ctrl+T / Esc | frontend_frame: DialogRequest{kind: close_tab_confirm} ⇒ local confirmation: clear locally, then FrontendFrame::Quit or FrontendFrame::CloseTab{tab_id}; Esc clears locally with no frame. For a relayed request, Yes \| No \| Cancel \| Dismissed only | Quit awman / Close tab / Cancel buttons `Ctrl+C / Ctrl+T / Esc` (alt `Alt+T`) | `frontend_protocol` | engine | required | wire_frame |
| `dialog.workflow_cancel_confirm` | DialogRequest::workflow_cancel_confirm | y / n / Esc | frontend_frame: DialogRequest{kind: workflow_cancel_confirm} ⇒ Char('a') \| Dismissed | Cancel workflow / Keep running buttons `y / n / Esc` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.config_show` | DialogRequest::config_show | see config.* | frontend_frame: DialogRequest{kind: config_show} ⇒ ConfigEdit{field,value,global} \| Dismissed | config table `see config.*` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.loading` | DialogRequest::loading | none | frontend_frame: DialogRequest{kind: loading} ⇒ no answer (awaits_answer=false) | spinner panel | `frontend_protocol` | engine | required | wire_frame |
| `dialog.custom` | DialogRequest::custom | offered keys / Enter (single key) / Esc | frontend_frame: DialogRequest{kind: custom} ⇒ Char(key) \| Dismissed | one button per key `keys / Esc` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.review` | DialogRequest::review | offered keys (gated keys unlock after reading), ↑/↓ scroll, Enter when accepted, Esc | frontend_frame: DialogRequest{kind: review} ⇒ Char(key) \| Dismissed | scrollable body with footer buttons; gated buttons enable at the end `keys / Esc` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.key_setup_notice` | DialogRequest::key_setup_notice | Enter / Esc, c / z copy | frontend_frame: DialogRequest{kind: key_setup_notice} ⇒ no answer; copy on gesture | notice with Copy key / Copy snippet buttons `Enter / Esc / c / z` | `frontend_protocol` | engine | required | wire_frame |
| `dialog.global_keys_suppressed` | Global shortcuts are inert while a dialog has focus | Ctrl+A / Ctrl+D / Ctrl+M / Ctrl+O while a dialog is open | local: none | while a modal is open, tab/container/overview shortcuts do nothing and the controls behind the modal are disabled; the countdown modal keeps Ctrl+A/Ctrl+D (Alt+Left/Alt+Right) | `frontend_protocol` | engine | required | view_only |

## quit_cancel

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `quit.confirm` | Quit confirmation | Ctrl+C (single tab) or q | local: none until confirmed | Quit dialog `Ctrl+C` | `frontend_protocol` | engine | required | view_only |
| `quit.quit` | Quit awman | Ctrl+C again | frontend_frame: Quit | 'Quit awman' button; Ctrl+C in the dialog `Ctrl+C` | `frontend_protocol` | engine | required | engine_trace |
| `quit.cancel_command` | Cancel the running command (double Ctrl+C semantics) | Ctrl+C | frontend_frame: Cancel{tab_id, repeat:false} then Cancel{tab_id, repeat:true} | 'Cancel' button; Ctrl+C `Ctrl+C` | `frontend_protocol` | engine | required | engine_trace |
| `quit.browser_tab_close` | Closing or reloading the browser tab | killing the TUI | frontend_frame: socket loss ⇒ WEB_PRIMARY_RECONNECT_GRACE 60 s ⇒ primary exit policy; beforeunload warning while work runs | beforeunload prompt shows the effect [web only] | `web_frontend` | engine | required | web_only |
| `quit.engine_disabled` | Engine disabled / daemon shutdown | n/a | frontend_frame: Detach{reason: engine_disabled \| daemon_shutdown} | fatal panel | `frontend_protocol` | engine | required | view_only |
| `quit.grant_revoked` | Browser grant revoked | n/a | frontend_frame: socket close 1008 `grant revoked`; HTTP 401 browser_grant_invalid | connect screen with 'access revoked' notice [web only] | `browser_links` | engine | required | web_only |

## queue

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `queue.panel` | Queue panel of eligible work items | Ctrl+Q | frontend_frame: WorkerQueue{Snapshot,Refresh,Seen} on the instance-owned frontend protocol | Work queue panel `Ctrl+Q` | `worker_queue` | engine | required | engine_trace |
| `queue.pick` | Pick an item | Enter then p | frontend_frame: WorkerQueue{Pick} then OpenTab{lease_id} and Submit{Lease}; engine verifies the prepared workspace | Pick button | `worker_queue` | engine | required | engine_trace |
| `queue.checkout_choice` | Run in checkout worktree or fresh clone | checkout prompt | frontend_frame: WorkerQueue{CheckoutOffer,Checkout} before OpenTab{lease_id} | Worktree and fresh clone buttons | `worker_queue` | engine | required | engine_trace |
| `queue.release_on_close` | Release or cancel when closing a picked tab | Ctrl+C in the tab | frontend_frame: WorkerQueue{Finish{release\|cancel}} before CloseTab; KeepRunning leaves the tab open | Close running work item dialog | `worker_queue` | engine | required | engine_trace |

## keys

| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |
|---|---|---|---|---|---|---|---|---|
| `keys.shortcut_list` | In-app shortcut list | n/a (hint bar) | local: none | `?` with an empty composer or Help button; marks reserved keys and shows alternates [web only] | `frontend_protocol` | engine | required | web_only |
| `keys.reserved` | Browser-reserved combinations | Ctrl+T, Ctrl+W, Ctrl+N, Ctrl+Shift+C | local: none | Alt+T, Alt+W, Alt+N and buttons registered on every platform [web only] | `frontend_protocol` | engine | required | web_only |

## Counts

| status | rows |
|---|---|
| required | 184 |
| total | 184 |
