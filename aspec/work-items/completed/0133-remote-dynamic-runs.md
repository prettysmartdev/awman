# Work Item: Feature

Title: Dynamic workflows in remote sessions, with a base ref and push-back
Issue: issuelink

Depends on: 0132 (without it, API-mode dynamic runs never exit 0, so nothing is ever pushed).

## Summary:
- `exec workflow --dynamic` in a remote session fails with "dynamic workflows are not supported for remote sessions" (`src/command/commands/exec_workflow/dynamic.rs`, the `is_remote()` check before the worktree is prepared). Remote sessions are the natural unit for an API client that wants "clean clone, one job, delete": the clone is already isolated, so it can stand in for the worktree. Allow dynamic mode there.
- Let the caller choose what the branch is based on: `base_ref` (any branch, tag or sha) on `POST /v1/sessions`, plus optional `merge_refs` (branch names) that awman merges in before the session is ready. API clients that chain work (a downstream job builds on two upstream branches) then do not need to run git themselves.
- Let the caller ask for the result to be pushed: `push: true` on the session. After a successful command awman commits any leftover changes, pushes the session branch to `origin` with an upstream set, and reports what it pushed (see 0137).
- Fix `push_branch`: it runs a bare `git push` whenever `remote` or `branch` is missing (`src/engine/workflow/step_commands.rs`, `TeardownStep::PushBranch`), which fails on a new branch with no upstream and silently ignores a `remote` given without a `branch`.

The `push_branch` fix (Part 1) is independent of the rest and can ship first.

## User Stories

### User Story 1:
As a: API client running jobs on behalf of a team (e.g. infact)

I want to:
open a remote session on `repo_url` with `branch: "infact/wi-0042"`, `base_ref: "main"`, `merge_refs: ["infact/wi-0040", "infact/wi-0041"]`, `push: true`, and run `exec workflow --dynamic`

So I can:
get the upstream work merged in, the dynamic workflow run in a fresh clone, and the result pushed back, without a local checkout, a worktree or git plumbing on my side

### User Story 2:
As a: user writing a workflow

I want to:
have `push_branch` work on the branch awman just created, and honour the `remote` I named

So I can:
avoid a teardown failure on the very first push

## Implementation Details:

### Part 1: `push_branch`
- Translate `PushBranch { remote, branch }` to `git push -u <remote or origin> <branch or HEAD>` in every combination. Keep the `GIT_SSH_COMMAND` env override.
- Update the `step_commands.rs` unit tests for all four `(remote, branch)` combinations.

### Part 2: dynamic mode on remote sessions
- Skip worktree creation, as `execute.rs` already does for file workflows (`self.flags.worktree && self.session.session_type().is_remote()`).
- Point the leader session and the leader's mutation guard (leader may only write under the context dir) at the session clone instead of a worktree.
- No `worktree_git_overlay` is needed: the clone's `.git` is a real directory.
- Skip `WorktreeLifecycle::finalize` (and therefore the API-mode squash merge). There is no parent checkout to merge into.
- Workflow state and the saved dynamic `workflow.toml` live under the clone (`workflow_state_root_for(<clone>)`), so they are deleted with it when the session closes.

### Part 3: session fields
- `SessionCreateRequest` / `SessionCreatePlan` (`src/command/session_create.rs`): add `base_ref: Option<String>`, `merge_refs: Vec<String>` and `push: bool`. All three are remote-only; giving them on a local session is a 400 (a new `CommandError` variant naming the field), matching how `repo_url` is validated.
- Branch setup (`src/command/session_setup.rs`, the "setting up branch" stage; the clone is a full clone of the default branch, so every remote branch is available as `origin/<name>`):
  - If `branch` does not exist on `origin`: `git checkout -b <branch> <base_ref or origin/HEAD>`. A `base_ref` that does not resolve fails setup at `setting_up_branch`.
  - If `branch` exists on `origin`: check it out. If `base_ref` was also given, fail setup at `setting_up_branch` with "branch already exists on origin; omit base_ref to continue it", rather than silently ignoring `base_ref`. A client retrying a job whose branch was already pushed simply omits `base_ref`.
- Merge stage: a new `SessionSetupStatus::MergingRefs` (`merging_refs`) between `setting_up_branch` and `running_ready`. For each `merge_refs` entry in order, run `git merge --no-ff --no-edit origin/<ref>`.
  - A ref that does not exist fails setup. It is never skipped silently.
  - A conflict runs `git merge --abort` and fails setup at `merging_refs` with the conflicting paths in `error.message`.
- Committer identity: the setup merges create commits on the host, in a clone that may have no `user.name` / `user.email` (a fresh server). Run the merges and the leftover-changes commit (below) with `-c user.name=awman -c user.email=awman@localhost`, unless the clone already resolves an identity (`git config user.email` succeeds). Record the chosen identity in the setup log.
- Persist `repo_url`, `branch`, `base_ref`, `merge_refs` and `push` on the session row, so a restored session keeps them. Today `repo_url` and `branch` are restored as empty strings: `src/command/commands/api_server/runtime.rs` has a "Known gap" comment. Add the columns with `ApiDb::add_column_if_missing` (`src/data/fs/api_db.rs`), as `cloned_path` was added.

### Part 4: push
- When: after a command in the session finishes with exit 0 and the session has `push: true`. This applies to `exec workflow` (file or dynamic) and `exec prompt`.
- Leftover changes first. If the clone has uncommitted changes (the workflow had no `commit_changes` step, or `exec prompt` edited files), commit them with `git add -A && git commit -m "<message>"`. The message is `Implement work item NNNN` when there is a work item, else `awman: <command> in session <id>`. Use the identity rule above. Without this step the agent's work would not be pushed at all: in API mode it is the worktree squash-merge that commits leftovers today, and remote sessions skip that.
- Then `git push -u origin HEAD:<branch>` from the clone. Record the pushed sha and branch for the command result (0137).
- Credentials: the same as the host-side clone uses today (the git credentials of the user running `awman api`). When 0141 lands, per-session credentials replace them for both clone and push. Document this in `aspec/architecture/security.md`: the push is a host-side git operation on awman's own clone, not agent execution. Git hooks do not run on push from a fresh clone, because hooks are not cloned.
- A push failure turns the command into `error` with `stage: "push"` and git's stderr in the message. The local commit stays in the clone.

## Edge Case Considerations:
- The leader's workflow also contains `push_branch`: pushing twice is harmless; `create_pull_request` still works.
- `merge_refs` names a ref that does not exist: fail setup, do not skip silently.
- `merge_refs` given a sha or tag: refused at request validation. Only branch names are allowed, because the merge target is `origin/<ref>`.
- The command fails: do not push, so the remote branch is not overwritten by broken work. Callers who want partial work can add `push_branch` to the workflow.
- The command succeeds with no changes at all: no commit and no push. Report `pushed: null` (0137).
- The remote rejects the push (non-fast-forward because someone else pushed): `error` at `push`. Never force-push.
- A restored session from before this change has no `push` or `base_ref` columns: treat them as `false` / `None`.

## Test Considerations:
- Unit: `push_branch` translation for all four `(remote, branch)` combinations.
- Unit: session request validation (remote-only fields; `merge_refs` must be branch names).
- Integration with a local bare repo served by `git daemon` (a `git://` URL; `file://` is not in `DEFAULT_REPO_URL_SCHEMES`, though a test policy may add it to `allowed_schemes`):
  - A remote dynamic run on a fresh branch with `base_ref` and two `merge_refs`; the push verified with `git ls-remote`.
  - A merge conflict leads to `failed` at stage `merging_refs`, with the conflicting path in the message.
  - An existing branch plus `base_ref` fails at `setting_up_branch`.
  - `exec prompt` with `push: true` and uncommitted agent edits: commit and push happen.
  - A failed command does not push.
  - Merges work in a clone with no git identity configured.
  - `push_branch` on a branch with no upstream succeeds.
  - A restored session keeps `repo_url`, `branch` and `push`.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- New git operations (`merge_ref_logged`, `push_upstream_logged`, `commit_all_with_identity_logged`) go in `GitEngine` alongside `clone_repo_logged` and `checkout_or_create_branch_logged`.

## Documentation
- Update `docs/06-dynamic-workflows.md` (remote sessions), `docs/09-api-and-remote-mode.md` (session fields, the `merging_refs` stage, push and which credentials it uses) and `docs/05-workflows.md` (`push_branch` sets an upstream).
