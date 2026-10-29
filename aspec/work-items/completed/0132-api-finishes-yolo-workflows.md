# Work Item: Bug

Title: Workflows finish cleanly after their last step, worktree merge-back handles a dirty target and a failed merge, and the API serves live workflow state
Issue: issuelink

## Summary:
This item has four parts. Part A is the bug that unblocks every API-mode dynamic run. Parts B and C make merging a worktree back safer for every host. Part D makes `GET /v1/workflows/{command_id}` work at all, so API clients and `awman remote exec workflow` can watch a run, including A–C's outcomes.

### A. API-mode yolo workflows abort after their last step
- Every yolo workflow run through the API server, including every `exec workflow --dynamic`, ends as `aborted` (exit 1) **after all of its steps succeed**, and its teardown never runs: no `run_shell` checks, no `commit_changes`, no `push_branch`. In API mode the worktree is then kept rather than merged, with the work uncommitted.
- Observed with infact on awman 0.12: a four-step dynamic workflow (`implement`, then `tests` ∥ `docs`, then `review`) reported every step `succeeded`, then emitted `Workflow aborted`, `CommandStatus{status: aborted, exit_code: 1}` and `Workflow summary: 4 completed, 1 failed`, kept the worktree, and never started the generated `[[teardown]]` steps.
- Cause:
  - In yolo mode the engine always opens the Workflow Control Board after the last step, "so the user can review the final step's output" (`src/engine/workflow/single_step.rs`, the `else if self.yolo` branch after a successful step).
  - The API frontend answers boards with `HeadlessDefaults::api().workflow_next_action`, which is `can_launch_next ? LaunchNext : Abort` (`src/command/headless.rs`).
  - After the last step there is nothing to launch, so the API chooses **Abort**.
  - The squad and CLI profiles answer `LaunchNext`, which returns `IterationOutcome::Continue`; the run loop then sees `state.is_complete()` and finishes. The engine tests' fake frontend also defaults to `LaunchNext`, which is why no test caught it.
- A second, engine-side defect means fixing the API answer alone is not enough: `can_finish_workflow` comes from `is_last_step()` (`src/engine/workflow/queries.rs`), which compares the current step with the **last entry of the topological order**, not with "everything else is done". In a DAG with more than one sink the step that finishes last is often not topologically last. Verified: steps `a`, `b` (depends on `a`) and `c` (no dependencies) in yolo mode open one final board with `can_launch_next = false` **and** `can_finish_workflow = false`; answering `FinishWorkflow` returns `InvalidAdvanceAction("FinishWorkflow only valid on the last step")`. A dynamic leader can easily generate such a DAG.

### B. Dirty merge target
- When a workflow that ran in a worktree completes and the user (or the API policy) chooses to merge the worktree branch back (Merge or Squash), awman runs `git merge` / `git merge --squash` in the parent checkout (`WorktreeLifecycle::finalize`, `src/command/commands/worktree_lifecycle.rs`) without checking that checkout first.
- If the parent checkout has uncommitted changes, git either refuses ("Your local changes would be overwritten by merge") or mixes the user's edits into the squash. `merge_branch_logged` maps every non-zero exit to `EngineError::MergeConflict`, so the user sees a misleading "merge conflict" report.
- awman should check the merge target before merging and, when it is dirty, offer to **commit** the changes or **`git stash`** them (the user's choice), or abort.

### C. Failed merge
- Today a failed merge only prints `report_merge_conflict` and keeps the worktree. The user resolves it by hand.
- awman should offer to launch an agent to fix the merge. The agent's prompt is rendered from a prompt template plus the failing merge command and its error output. After the agent exits, awman checks the result and completes the merge (commit and worktree cleanup) or reports that it is still broken.

### D. `GET /v1/workflows/{command_id}` never returns a workflow
- **Nothing writes the file the route reads.** `handle_get_workflow` (`src/frontend/api/routes.rs`) reads `ApiPaths::command_workflow_state_path`, which is `<api home>/sessions/<session>/commands/<command>/workflow.state.json` (`src/data/fs/api_paths.rs`). That path has no caller besides its reader (`read_command_workflow_state_raw`). Every call answers `404 {"error": "no workflow for this command"}`.
  - API dispatch goes through the generic `Dispatch`, so the engine's `WorkflowStateStore` writes only its own state file under the worktree's git root (`<worktree>/.awman/workflows/...`). Only squad redirects that file (`with_workflow_state_root`).
- **It is documented as working.** `docs/09-api-and-remote-mode.md` says the API server writes `workflow.state.json` "atomically on every step transition". The parity test `tests/api_parity/wi_0079.rs` writes the file by hand before calling the route, so it never noticed that no writer exists.
- **awman's own remote TUI is broken by it.** `RemoteWorkflowPoller` (`src/command/commands/remote_client.rs`) polls this endpoint to draw the Workflow Overview for `remote exec workflow`, and always gets 404.
- **Teardown steps are invisible until teardown starts.** `WorkflowState::new` leaves `setup_step_states` and `teardown_step_states` empty. `run_phase` (`src/engine/workflow/phases.rs`) replaces each list only when its phase begins. So during the main phase no client (local TUI included) sees the teardown steps.
- **Setup and teardown progress is free text.** `ApiDispatchFrontend::on_phase_step_started / completed / failed` (`src/frontend/api/command_frontend.rs`) emit only `StatusMessage { phase, message: "started: <desc>" | "completed: <desc>" | "failed: ..." }`. They emit no `WorkflowPhaseTransition`, unlike agent steps, so clients parse strings.

## User Stories

### User Story 1:
As a: API client running dynamic workflows (e.g. infact)

I want to:
have a workflow whose steps all succeed run its teardown, commit, merge, and end as `done` with exit 0, whatever the shape of its DAG

So I can:
use awman's API to execute work without every run being reported as aborted

### User Story 2:
As a: developer who kept editing their main checkout while a worktree workflow ran

I want to:
be asked to commit or stash my changes before awman merges the worktree branch back

So I can:
merge safely without git refusing, and without my unrelated edits being folded into the squash commit

### User Story 3:
As a: developer (or API client) whose worktree merge-back failed

I want to:
have awman offer to launch an agent that is told exactly which command failed and why, and then finish the merge if the agent fixed it

So I can:
get the work merged without resolving conflicts by hand

### User Story 4:
As a: API client drawing a running workflow (e.g. infact's workflow view)

I want to:
read every setup, agent and teardown step, pending or not, from `GET /v1/workflows/{command_id}` as soon as the workflow starts, and see their statuses change while it runs

So I can:
show the whole workflow at once, the way the TUI's Workflow Overview does

### User Story 5:
As a: user running `awman remote exec workflow`

I want to:
see the Workflow Overview fill in, as it does for a local run

So I can:
follow a remote workflow without attaching to a container

### User Story 6:
As a: API client after a run finishes

I want to:
read the final state, including teardown steps that never ran, until the session is purged

So I can:
show the verdict next to what actually happened

## Implementation Details:

### A. Finishing after the last step
- Engine: offer finish once the workflow is complete. In `compute_available_actions` (`src/engine/workflow/control.rs`) set `can_finish_workflow: self.is_last_step() || self.state.is_complete()`, and accept the same condition in `handle_finish_workflow`'s guard. The parallel-peer override (`can_finish_workflow = false` while peers run) stays.
- Headless: `HeadlessDefaults::workflow_next_action` for `Profile::Api` becomes `can_launch_next ? LaunchNext : can_finish_workflow ? FinishWorkflow : Abort`. Failure boards (`compute_failure_actions`) always set `can_finish_workflow = false`, so a failed step still aborts.
- Update the module's answer table and the recorded-table test with a new row: `workflow_next_action(last step: can_finish_workflow = true)` gives `FinishWorkflow` / `LaunchNext` / `LaunchNext`. Document why the API needs this answer in a doc comment on `workflow_next_action`.
- Make the engine tests' `FakeWorkflowFrontend` default to the API's headless answers, so the profile that runs unattended is the one exercised by default. Fix any tests that relied on the old default explicitly.

### B. Dirty merge target check
- Where: `WorktreeLifecycle::finalize`, in the `Merge` branch, after `ask_merge_mode` returns `Merge` or `Squash` and after the existing commit-before-merge of the **worktree's** own changes, and before `merge_branch_logged`. `LeaveBranch` skips the check.
- Check: `git_engine.uncommitted_files_logged(&self.git_root, ...)`, the same query `prepare` uses before it creates a worktree. A clean target proceeds as today.
- Ask the frontend through a new `WorktreeLifecycleFrontend` method:
  ```rust
  fn ask_dirty_merge_target(
      &mut self,
      target_branch: &str,
      files: &[String],
      suggested_message: &str,
  ) -> Result<DirtyMergeTargetDecision, CommandError>;

  pub enum DirtyMergeTargetDecision {
      Commit { message: String },
      Stash,
      Abort,
  }
  ```
  - The suggested commit message is `WIP: changes on <target> before merging <branch>`. Build it in the command layer, as `PostWorkflowWorktreePrompt` does.
  - `Commit` runs `commit_all_logged(&self.git_root, &message, ...)`.
  - `Stash` runs a new `GitEngine::stash_push_logged(git_root, message)`: `git stash push --include-untracked -m "awman: before merging <branch>"`. awman does **not** pop the stash after the merge, because popping can conflict with the merged changes. It reports the stash (`stash@{0}` plus the message) and tells the user to run `git stash pop` when ready.
  - `Abort` keeps the worktree and branch, reports them with `report_worktree_kept`, and ends the command with a clear "merge target has uncommitted changes" error. Do not report it as a merge conflict.
- Command flag on `exec workflow` (API-allowed, since `exec workflow` is): `--on-dirty-merge-target ask|commit|stash|fail`.
  - `ask` (the default for interactive CLI and TUI) uses the frontend dialog.
  - `commit` uses the suggested message, `stash` stashes, and `fail` behaves like `Abort`.
  - With the flag set to anything but `ask`, the frontend is not consulted, matching how `preselected` works for `ask_existing_worktree`.
- Config default: `worktree.onDirtyMergeTarget` in repo and global config (same values). Precedence: flag, then repo config, then global config, then the host's headless default.
- Headless defaults: add a `dirty_merge_target` row. API: `Fail`. CLI (piped or `--non-interactive`): `Fail`. Squad: *(unused)*, because squad's `merge_mode` is `LeaveBranch`. The API default is `fail` on purpose, so awman never touches a server-side checkout's uncommitted state unless the client asks.
- Frontends, following the existing pre-worktree uncommitted-files dialogs:
  - TUI: a `DialogRequest::Custom` in `src/frontend/tui/per_command/worktree_lifecycle.rs` listing the dirty files with `c` Commit, `s` Stash, `a` Abort. Commit then asks for the message with `DialogRequest::TextInput` pre-filled with the suggestion, exactly as `ask_pre_worktree_uncommitted_files` does.
  - CLI: a prompt in `src/frontend/cli/per_command/worktree_lifecycle_marker.rs` with the same choices and the same file listing.

### C. Agent-assisted merge fix
- Where: the `Err(EngineError::MergeConflict { .. })` arm in `finalize`, and also non-conflict merge errors, so that every failed merge in a worktree finalize gets the offer.
- Error detail: extend `EngineError::MergeConflict` (or add a `MergeFailed` variant) to carry the exact command (`git merge -m ... <branch>` or `git merge --squash <branch>`, then the squash commit), its exit code and its stderr/stdout. Today the variant drops them.
- Ask the frontend:
  ```rust
  fn ask_launch_merge_fix_agent(&mut self, failure: &MergeFailure) -> Result<bool, CommandError>;
  ```
  `MergeFailure` holds the branch, target branch, worktree path, git root, command, exit code, output and unmerged paths (`git diff --name-only --diff-filter=U`).
- Prompt template:
  - The built-in default lives at `src/assets/worktree/merge-fix-prompt.md`, loaded with `include_str!` as the dynamic-workflow assets are (`src/data/dynamic_workflow_assets.rs`).
  - Config key `worktree.mergeFix.promptTemplate` (repo or global) may point at a file relative to the git root that replaces it.
  - It is rendered with the existing `{{...}}` substitution (`substitute_prompt` in `src/data/workflow_prompt_template.rs`).
  - Placeholders: `{{merge_command}}`, `{{merge_exit_code}}`, `{{merge_output}}`, `{{branch}}`, `{{target_branch}}`, `{{unmerged_paths}}`, plus the existing work-item placeholders when the workflow had a work item.
  - The default prompt tells the agent to resolve conflicts in place, to keep the intent of both sides, not to commit, and not to run `git merge --abort`.
- Running the agent:
  - The agent runs in a container like every other agent (never on the host). It mounts the parent checkout's git root, which is where the merge is in progress; that is the normal mount scope.
  - It uses the agent and model the workflow's last step used. `--agent` and `--model`, when given, override them.
  - Reuse the launch path the `on_failure` remediation agent uses for phase steps (`src/engine/workflow/phases.rs`) rather than adding a new one.
  - TUI and CLI run it interactively, as other agent sessions are run on those hosts. The API runs it non-interactively and streams its output as command output events.
- After the agent exits, awman verifies and completes:
  - The merge is resolved when there are no unmerged paths and no conflict markers (`<<<<<<<`, `=======`, `>>>>>>>` at line start) in the files that were unmerged.
  - If it is resolved, awman completes the commit (`git commit --no-edit` for a plain merge; the usual squash commit message for a squash), then runs the normal post-merge cleanup (`confirm_worktree_cleanup`, worktree removal, branch deletion).
  - If the merge never started (the command failed before touching the index), awman retries the merge command once.
  - If it still fails, awman keeps the worktree, leaves the merge state for the user, and reports it with `report_merge_conflict`, so the command ends as an error. There is one agent attempt per merge, with no loop.
- Command flag on `exec workflow` (API-allowed): `--on-merge-failure ask|agent|keep`.
  - `ask` (the interactive default) uses the frontend.
  - `agent` launches the agent without asking.
  - `keep` is today's behaviour.
- Config default: `worktree.mergeFix.onFailure` (same values). Same precedence as above.
- Headless defaults: add a `launch_merge_fix_agent` row. API: `false` (`keep`). CLI: `false`. Squad: *(unused)*. An API client opts in with `--on-merge-failure agent`.
- Frontends:
  - TUI: after the existing merge-conflict report, a dialog `Launch an agent to fix the merge? [y/N]` showing the failed command and the first lines of its output.
  - CLI: the same prompt.
  - Both then report the verification result: "merge completed" or "merge still unresolved; worktree kept at …".

### D. Live workflow state over the API
1. **A Layer 0 writer.**
   - Add `ApiPaths::write_command_workflow_state(session_id, command_id, &WorkflowState)` next to `write_command_metadata` (`src/data/fs/api_paths.rs`). It writes `workflow.state.json.tmp` and renames it over `workflow.state.json`, so a poll never reads a half-written file, as the docs already promise.
   - Wrap it in a small `CommandStateWriter { paths, session_id, command_id }` with `write(&WorkflowState)`, beside `CommandLogWriter` (`src/data/fs/api_command_log.rs`). It owns the filesystem access, so the frontend stays free of filesystem calls.
2. **The engine reports every state it persists.**
   - Add a default no-op `WorkflowFrontend::report_workflow_state(&mut self, _state: &WorkflowState) {}` (`src/engine/workflow/frontend.rs`).
   - **Forward it in the blanket `impl WorkflowFrontend for Arc<Mutex<Box<F>>>`** in the same file, and in any other wrapper that forwards calls. A missed forward silently falls back to the no-op default, and the API never writes.
   - `WorkflowEngine::persist` (`src/engine/workflow/queries.rs`) becomes `&mut self` and calls `self.frontend.report_workflow_state(&self.state)` after `state_store.save` and `mirror_summary`. Verified: this change compiles as-is against all 44 `persist()` callers in `src/engine/workflow/`. One hook covers every transition, phase change, `mark_done` and remediation, because all of them persist.
   - The engine's own state file is unchanged: resume still reads it from the worktree.
3. **The API frontend writes it.**
   - `ApiFrontendFactory::frontend_for` (`src/frontend/api/mod.rs`, implemented in `src/command/commands/api_server/queue_worker.rs`) receives a `CommandStateWriter`, built by the queue worker the same way it builds `CommandLogWriter`. `ApiDispatchFrontend` keeps the writer.
   - `ApiDispatchFrontend::report_workflow_state` calls `writer.write(state)`. A failed write is logged with `tracing::warn!` and never fails the command.
   - With this, the existing route and `RemoteWorkflowPoller` work without changes. The file lives as long as the command directory (until the session purge), including after a successful run, when the engine's own state in the worktree has been deleted along with the worktree.
4. **Setup and teardown are in the state from the start.**
   - Add `WorkflowEngine::declare_phase_steps(&[SetupStep], &[TeardownStep])`. It fills `setup_step_states` and `teardown_step_states` with every step as `Pending`, described by `PhaseStepSpec::description()` after work-item substitution (the strings `run_phase` uses today), and persists once.
   - Call it in `src/command/commands/exec_workflow/prepare.rs` right after `WorkflowEngine::resume` succeeds and before setup runs; `setup_steps` and `teardown_steps` are already in scope there. A dynamic run reaches this through `execute_generated_workflow` after the leader's design validates.
   - `run_phase` no longer rebuilds the list (`*self.state.phase_step_states_mut(kind) = ...`). Instead it **resets that phase's statuses to `Pending`**, keeping the declared list, and then updates statuses by index as today.
     - The reset is needed because on resume `run_phase` re-runs every step of the phase it enters, so statuses carried over from the earlier attempt would be stale.
     - A phase that is skipped on resume (`setup_completed`) keeps its recorded statuses.
     - If the list length no longer matches the steps (the workflow file changed under a confirmed resume), `run_phase` rebuilds the list as today.
   - When teardown is skipped (`abort_on_failure`, or a failed main phase without `teardown_on_failure`), its steps stay `Pending` in the final state, so clients can show "never started".
5. **Structured phase-step progress.**
   - In `ApiDispatchFrontend::on_phase_step_started / completed / failed`, also emit `EventPayload::WorkflowPhaseTransition { phase: kind.label(), step_desc: <desc>, status: Running | Succeeded | Failed }` (`src/data/execution_event.rs`).
   - `step_desc` is exactly the description in `setup_step_states` / `teardown_step_states`, so clients can match the two.
   - Keep the `StatusMessage` lines for one release, following the API's additive-versioning rule.
6. **The route says what it means.**
   - For an `exec workflow` command with no state yet (queued, or still in the dynamic leader phase), answer `404 {"error": "workflow not started"}`.
   - Keep `no workflow for this command` for commands that never run a workflow (`exec prompt`, `chat`), and `command not found` for unknown ids.
   - Add a `features: Vec<String>` field to `StatusResponse` (`GET /v1/status`, which has no such field today), with `workflow_state` as its first entry. Clients then detect a working endpoint from the capability rather than the version. Later items (e.g. 0137's step output) append to the same list.

## Edge Case Considerations:
- A parallel group with peers still running: `compute_available_actions` forces `can_finish_workflow = false` and `can_launch_next` stays true, so the answer is unchanged (`LaunchNext`).
- A resumed workflow whose remaining steps are all complete: the same last-step board, so it finishes.
- A terminal parallel group: the parallel path opens no end-of-workflow board, so it already completes. Cover it with a test so a later change cannot regress it.
- `teardown_on_failure = false` workflows: unaffected, because finishing is not a failure.
- The merge target is dirty only with untracked files: still dirty (they can block the merge). Stash uses `--include-untracked`.
- The merge target is in a detached HEAD: the existing warning stands. The dirty check still applies.
- Stash requested but `git stash` fails (e.g. nothing stashable once ignored files are excluded): treat as `Abort` with the git error.
- The agent edits files outside the conflict: allowed (the merge may need it). The verification only looks at unmerged paths and markers.
- The agent fails to start (image missing, runtime error): report it and fall back to `keep`.
- The API client disconnects while the merge-fix agent runs: same as any running command; cancellation (0135) applies once it exists.
- Squad never merges (`LeaveBranch`), so parts B and C never fire there.
- Live state, leader repair loop: only the validated design reaches the engine, so rejected designs never appear in the state.
- Live state, awman restart mid-run: the command is re-queued and resumes from the engine's state. The first persist after the restart rewrites the command's file, so it never moves back to an earlier state.
- Live state, parallel groups: concurrent transitions all persist through the one engine, so writes are serialised, and the rename keeps every read whole.
- Live state, many fast transitions: the file is small (names, dependencies, statuses), and the engine already writes its own copy on every persist, so one more write per transition is acceptable.
- Live state, wire shape: `StepState` and `PhaseStepStatus` are externally tagged enums (`"Pending"`, `"Succeeded"`, `{"Running": {"container_id": …}}`, `{"Failed": {"exit_code": 1, "error_message": …}}`, `{"Remediating": {"attempt": 1, "of": 2}}`). Keep the shape, because `workflow_state_to_view_state` and squad's attach read it, and document it.
- Live state, session closed and purged: the file goes with the command directory. Clients that need the record keep their own copy.
- Live state and Part A: after a successful last step the API now finishes, so the final file shows `current_phase` reaching teardown and then done, rather than an aborted main phase.

## Test Considerations:
- Unit (headless): table rows for `workflow_next_action` on the last step, `dirty_merge_target` and `launch_merge_fix_agent`, for all three profiles.
- Engine:
  - A yolo workflow `a → b` opens exactly one board, after the last step, with `can_launch_next = false` and `can_finish_workflow = true`; `FinishWorkflow` completes it.
  - A yolo workflow `a`, `b ← a`, `c` (more than one sink): the final board offers finish and `FinishWorkflow` completes it.
  - With `FakeWorkflowFrontend` defaulting to API answers, a failed step still aborts.
- `WorktreeLifecycle::finalize` (with the existing `RecordingWorktreeLifecycleFrontend` and real temp repos):
  - A clean target merges without asking.
  - A dirty target with `Commit` makes a commit, then merges.
  - A dirty target with `Stash` leaves a stash entry and merges.
  - A dirty target with `Abort` keeps the worktree, makes no merge attempt, and returns an error that is not a merge conflict.
  - A conflicting merge with the agent declined calls `report_merge_conflict` as today.
  - A conflicting merge with a fake agent that resolves the files: the merge is committed and the worktree removed.
  - A conflicting merge with a fake agent that leaves markers: the worktree is kept and the conflict reported.
- Prompt rendering: every placeholder substituted; a custom template from config is used.
- Flag and config precedence for `--on-dirty-merge-target` and `--on-merge-failure`.
- Live state, unit:
  - `persist` calls `report_workflow_state` with the state it saved (a spy frontend), including through the `Arc<Mutex<Box<F>>>` forwarding impl.
  - `declare_phase_steps` lists every setup and teardown step as pending, with substituted descriptions.
  - `run_phase` keeps the declared list, resets that phase's statuses on entry, and rebuilds only when the length differs.
  - `write_command_workflow_state` is atomic: no temp file left behind, and a reader sees the old or the new file, never a partial one.
- Live state, API parity: replace the hand-written file in `tests/api_parity/wi_0079.rs` with a real API-mode `exec workflow` run through the engine with a fake agent factory. Assert that:
  - the route returns 200 with every agent, setup and teardown step pending before the first setup step starts;
  - statuses change as steps run;
  - after `done`, the final state is still served, including after the worktree is removed.
- Live state, events: setup and teardown `WorkflowPhaseTransition`s arrive in order, and each `step_desc` matches the state's description.
- Live state, route: `workflow not started` for a queued `exec workflow`; existing 404 cases unchanged; `GET /v1/status` lists `workflow_state` in `features`.
- Live state, remote TUI: `RemoteWorkflowPoller` against the real router draws setup, agent and teardown columns.
- Integration: an API-mode dynamic workflow with a teardown `run_shell` step ends `done`, runs the teardown, and squash-merges, and `GET /v1/workflows/{id}` shows each of those transitions. With a dirty server checkout and no flag it fails with the dirty-target error. With `--on-dirty-merge-target stash` it merges.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Keep all dialog copy in the command layer (as `PostWorkflowWorktreePrompt` does); frontends only render and map keys.
- New git operations (`stash_push_logged`, unmerged paths, merge-in-progress detection) go in `GitEngine` with `_logged` variants, matching the existing ones.

## Documentation
- `docs/09-api-and-remote-mode.md`:
  - API-mode workflows finish automatically after their last step and abort on a failed step.
  - Document `--on-dirty-merge-target` and `--on-merge-failure` and their API defaults (`fail`, `keep`).
  - Correct the `/v1/workflows` example: the real body is a `WorkflowState` with `steps[]` (`name`, `depends_on`, `agent`, `model`), `step_states`, `current_phase`, `setup_step_states` and `teardown_step_states`, with the tagged status encoding. There is no top-level `status` field.
  - Say when the state first exists: before setup; for `--dynamic`, after the leader's design validates.
  - Document the `workflow not started` 404 and the `features` list in `GET /v1/status`.
  - Add setup and teardown `WorkflowPhaseTransition`s to the event reference.
- `docs/05-workflows.md`: the merge-back flow (dirty target check, commit or stash, merge-fix agent, verification).
- `docs/07-configuration.md`: `worktree.onDirtyMergeTarget`, `worktree.mergeFix.onFailure`, `worktree.mergeFix.promptTemplate` and the template placeholders.
- `docs/14-command-reference.md`: the two new `exec workflow` flags.

## Out of scope (separate issue)
- Step (agent) containers in a local worktree cannot run git: the worktree's `.git` file points into the main checkout's `.git/worktrees/<name>`. Only setup/teardown containers get the main `.git` mounted (`worktree_git_overlay`, used in `execute.rs` and `dynamic.rs`). Agents therefore cannot run `git status` or `git diff` on their own changes. Worth a separate item: give step containers a usable, preferably read-only, git view.
