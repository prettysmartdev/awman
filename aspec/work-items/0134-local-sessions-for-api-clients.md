# Work Item: Feature

Title: Local sessions for API clients: allowlist roots and collision-free worktrees
Issue: issuelink

## Summary:
Two details of local sessions make them hard for an API client that manages many checkouts:

- **The `--workdirs` allowlist matches exact paths.** `src/command/session_create.rs` checks `policy.workdirs.contains(&requested)`, and the list is fixed at startup. A client cannot create a checkout per job or per project without restarting awman, so infact keeps a fixed pool of directories and recycles them.
- **Worktrees are keyed by the checkout's directory name, and the API silently resumes an existing one.**
  - Paths are keyed by the checkout's directory name plus the work item number or workflow name (`src/data/worktree_paths.rs`: `~/.awman/worktrees/<dirname>/<NNNN>` and `.../<dirname>/wf-<name>`).
  - Resuming only checks that the path exists (`WorktreeLifecycle::prepare`, `src/command/commands/worktree_lifecycle.rs`), and every headless profile answers `ExistingWorktreeDecision::Resume` (`src/command/headless.rs`).
  - So two different repositories whose checkouts share a directory name share worktree slots, and a leftover worktree from one job is silently resumed by an unrelated job with the same number, even one in a different repository.
  - infact has to remove every linked worktree before reusing a directory.
- Separately, **"recreate" is not fresh**: `Recreate` removes the worktree directory, but `create_worktree` reuses the existing `awman/work-item-NNNN` branch when it exists (`GitEngine::create_worktree_logged`). The "new" worktree therefore starts from the old run's commits.

Add allowlist *roots*, key worktrees by a stable id of the repository instead of its directory name, make `recreate` really start fresh, and make resume explicit for API callers.

## User Stories

### User Story 1:
As a: API client

I want to:
start awman with `--workdir-root /srv/infact/checkouts` and open local sessions on any repository cloned directly beneath it

So I can:
create one checkout per job without restarting awman

### User Story 2:
As a: API client running unrelated jobs over time

I want to:
be sure a new job never resumes another repository's leftover worktree, and choose explicitly when I do want to resume

So I can:
reuse directories safely and retry a failed run on purpose

### User Story 3:
As a: developer who chooses "recreate" for an existing worktree

I want to:
get a worktree that starts from my current HEAD, not from the previous run's branch

So I can:
actually start over

## Implementation Details:

### Allowlist roots
- New option `--workdir-root DIR` (repeatable), also settable as `api.workDirRoots` in global config. Precedence and merging are the same as for `--workdirs` / `api.workDirs`.
- Roots are canonicalized **at request time**, not only at startup. A root that does not exist at startup logs a warning and works once it exists, with no watcher. A root that is itself inside a git repository, or is a git repository, is refused at startup with a clear error. It may still be listed as an exact `--workdirs` entry.
- A requested workdir is allowed when its canonical path (symlinks resolved first, as today) is either:
  1. an allowlisted workdir (unchanged), or
  2. a direct child of a canonical root **whose git root is the child itself**: `git rev-parse --show-toplevel` in the child equals the child, and `<child>/.git` is a directory, not a linked-worktree pointer file.
- Rule 2's git-root condition is required for security. Without it, a non-repository child would resolve to an enclosing git root, and the API's `mount_scope` answer (`MountGitRoot`) would mount that larger tree, escaping the allowlist (`aspec/architecture/security.md`: only the current directory or Git root may be mounted).
- A grandchild of a root is refused, as is a direct child that is not its own git root. The 403 body says which rule failed.
- `GET /v1/workdirs` returns `{workdirs, roots}`. This is additive; existing clients reading `workdirs` keep working.

### Worktree key
- Key worktrees as `~/.awman/worktrees/<dirname>-<hash8>/...`, where `hash8` is the first 8 hex digits of the SHA-256 of the canonical git root path. Apply it in both `WorktreePaths::for_work_item` (`<NNNN>`) and `WorktreePaths::for_workflow` (`wf-<name>`, used by squad). This stays a pure function in Layer 0; canonicalization happens in the caller (`GitEngine`), which already holds the resolved root.
- Legacy worktrees:
  - When the hashed path does not exist but the legacy unhashed path does, use the legacy path **only if** `git worktree list --porcelain` in this git root lists it. Otherwise ignore it, because it belongs to another repository; that is exactly the collision being fixed.
  - New worktrees always use the hashed form.

### Existing-worktree policy
- `ExistingWorktreeDecision` becomes `Resume | Recreate | Fail`.
  - `Recreate` now removes the worktree **and** force-deletes its branch (`git branch -D`), so the new worktree is created from the current HEAD. This applies everywhere (TUI, CLI and API). Update the TUI and CLI dialog copy to say the branch and its commits are discarded.
  - `Fail` returns a new `CommandError::WorktreeExists { path, branch }`, whose message names the path and how to proceed (`--on-existing-worktree resume|recreate`).
- `exec workflow` gains `--on-existing-worktree ask|resume|recreate|fail`. It is API-allowed, as `exec workflow` is.
  - A value other than `ask` is passed as `preselected` to `WorktreeLifecycle::prepare` and wins on every host.
  - `ask` (the interactive default) shows the existing dialog.
- Headless defaults when the flag is absent:
  - API: `Fail`. This changes today's silent resume, so call it out in release notes.
  - Squad and piped CLI: `Resume`, unchanged.
  - Update the headless answer table and its test.
- Dynamic resume precedence: `dynamic.rs` preselects `Resume` when saved dynamic state exists (`resume_plan.is_some()`). An explicit `--on-existing-worktree` value overrides that preselection.
  - Under the API with no flag, the default `Fail` also applies when saved state exists. The saved state lives inside the leftover worktree, and resuming it is the client's explicit choice (`resume`).
  - Interactive hosts keep today's behaviour: a confirmed resume is not asked twice.
- After a successful API-mode squash merge the worktree is removed (unchanged). After a failure it is kept (unchanged), and the kept path is reported in the command `result` (see 0137) so clients can clean up or resume deliberately.

## Edge Case Considerations:
- A root that does not exist at startup: warn, then evaluate it again on each request; do not drop it silently.
- A root that is itself a git repository or inside one: refused as a root (exact workdir entries are still allowed), to keep the git-root rule simple and safe.
- A child of a root that is a symlink to elsewhere: canonicalization resolves it first, so it is allowed only if its target is itself a direct child of a root.
- A child of a root whose `.git` is a file (a linked worktree of another repository): refused. Its containers would get the main repository's `.git` mounted (`worktree_git_overlay`), which lies outside the root. Only children with a real `.git` directory are allowed.
- Two checkouts of the same repository in different directories: different hashes, so separate worktrees and no collision. Branches still live per checkout.
- `Recreate` when the branch is checked out somewhere else: `git branch -D` fails; surface git's error and stop rather than reusing the branch.
- Two awman users on one machine: worktree keys are per home directory anyway; no change.

## Test Considerations:
- Allowlist:
  - A direct child that is a git root is allowed.
  - A direct child that is not a git root (only an enclosing repository) is refused.
  - A grandchild is refused.
  - A symlink into a root resolves first.
  - A root missing at startup works once it is created.
  - A root inside a git repository is refused at startup.
  - Exact entries are unchanged.
  - `GET /v1/workdirs` includes `roots`.
- Worktree key:
  - Two checkouts with the same directory name get different keys, for both `for_work_item` and `for_workflow`.
  - A legacy unhashed worktree owned by this repository is found.
  - A legacy unhashed worktree owned by **another** repository is ignored, and a new hashed worktree is created.
- Existing-worktree policy:
  - `fail` errors with the path.
  - `recreate` removes the worktree and branch, and the new worktree has none of the old commits.
  - `resume` resumes.
  - The API default is `fail`.
  - Squad and CLI headless keep resuming.
  - An explicit flag overrides the dynamic-resume preselection.
- Headless answer-table row updated for `existing_worktree`.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec, especially `aspec/architecture/security.md` for the allowlist change. Add the root rule (direct child, own git root) to that spec.

## Documentation
- `docs/09-api-and-remote-mode.md`: working directory allowlist and roots, `GET /v1/workdirs`, and the API's `fail` default for existing worktrees.
- `docs/05-workflows.md`: worktrees and resume, `--on-existing-worktree`, and the new meaning of recreate.
- `docs/07-configuration.md`: `api.workDirRoots`.
- `docs/14-command-reference.md`: the new flags.
