# Work Item: Feature

Title: branch, pr, worktree for chat
Issue: issuelink

## Summary:
- This item adds `--branch`, `--pr` and `--worktree` flags to `awman chat` and `awman exec prompt`. They automate common git/GitHub workflows by launching the agent container in a linked worktree instead of the main checkout.
- `--branch <name>` finds (fetches) or creates the branch, creates a worktree for it, and mounts that worktree in the agent container.
- `--pr <number|url>` does the same for a GitHub pull request: `awman chat --pr 29` finds PR #29, fetches its branch, creates a worktree for it, and launches the agent container there.
- `--worktree <name|path>` only binds the session to a worktree that already exists on the machine, given as a full path or as its name in the default awman worktrees directory for the repo. For example, `--worktree 0123` uses the `0123` worktree created for work item 0123. It never fetches, creates or switches anything.
- A new branch is created if, and only if, a branch with the given name does not already exist on the git remote.

## User Stories

### User Story 1:
As a: user

I want to:
run `awman chat --branch fix/login` or `awman exec prompt --branch fix/login "..."` without first working out whether the branch exists locally, on the remote, or not at all

So I can:
start agent work on the right branch in an isolated worktree, without disturbing my main checkout or accidentally starting a second copy of a branch that already exists on the remote

### User Story 2:
As a: user reviewing or fixing a pull request

I want to:
run `awman chat --pr 29` or pass the PR's GitHub URL

So I can:
have an agent work on that PR's code, including a PR from a fork, without fetching it and setting up a worktree by hand

### User Story 3:
As a: user with worktrees left by earlier workflows

I want to:
run `awman chat --worktree 0123` or `awman exec prompt --worktree /path/to/worktree "..."`

So I can:
return to existing work with a new agent session, without changing anything in that worktree before the agent starts


## Implementation Details:

### Flags
- Add the three flags to `chat` and `exec prompt` only, all as `FlagKind::OptionalString`, each conflicting with the other two (`conflicts_with`). A bare `--worktree` with no value is an error on these commands.
- Declare them in a new `SESSION_WORKTREE_FLAGS` array in `dispatch/catalogue/shared_flags.rs`. Do **not** add them to `AGENT_RUN_FLAGS_NO_WORKTREE`: `exec workflow` derives its flags from that set and already has a boolean `--worktree`, whose meaning stays unchanged. Add a `CHAT_FLAGS` array built with `concat_flags`, and append the new flags to `EXEC_PROMPT_FLAGS`.
- Set `frontends: FrontendVisibility::CliAndTui` on all three (see Security below).
- Add `branch` and `pr` to `REMOTE_EXEC_EXCLUDED_FLAG_NAMES`, so `remote exec prompt` does not inherit them. `worktree` is already excluded.
- Add `session_worktree: Option<SessionWorktreeTarget>` to `ChatCommandFlags` and the `exec prompt` flags, parsed in `from_input`:
  ```rust
  pub enum SessionWorktreeTarget {
      Branch(String),
      PullRequest(String),
      Existing(String),
  }
  ```

### Shared resolver
- Add `src/command/commands/session_worktree.rs` with one entry point used by both commands:
  ```rust
  pub struct SessionWorktree {
      pub path: PathBuf,
      pub branch: Option<String>, // None for a detached worktree
      pub invoker_root: PathBuf,  // main checkout; owns repo_key and the image tag
  }
  ```
  It takes the target, the invoking session, the git engine and a `UserMessageSink`, and returns a `SessionWorktree`. Reused and created worktrees take the same path from there on.
- Run it right after agent resolution and trust checks, and before overlay parsing, image setup and credential resolution. A bad branch or PR then fails before anything is built.
- Git and `gh` network calls run through `spawn_blocking` so they do not stall the TUI's runtime. Set `GIT_TERMINAL_PROMPT=0` on them, because a credential prompt cannot be answered from a TUI tab; the failure is reported as an error instead.

### Finding the main checkout
- Use the first entry of `GitEngine::list_worktrees` as `invoker_root`, not `session.git_root()`. When awman is invoked from inside a linked worktree, `session.git_root()` is that worktree, and `repo_key` would hash the wrong path and look in the wrong directory.

### Paths (`data/worktree_paths.rs`)
- Add `WorktreePaths::named(git_root, name)` and `legacy_named`, which join a verbatim directory name under the repo directory. Add a matching `GitEngine::worktree_path_by_name` that goes through `pick_worktree_path`.
- Add directory names for created worktrees, so that each can later be reopened with `--worktree`:
  - `--branch`: `br-<sanitized>`, with `/` and other unsafe characters replaced by `-`. If sanitizing changed the name, append the first 8 hex digits of the branch name's SHA-256, so `a/b` and `a-b` do not collide.
  - `--pr`: `pr-<N>`.

### `--branch` algorithm
1. Validate the name with `git check-ref-format --branch`, which also rejects a leading `-`.
2. Query the remote with `git ls-remote --exit-code origin refs/heads/<name>`. Always pass the full ref, because a short pattern such as `foo` also matches `refs/heads/x/foo`. Exit 0 means it exists and exit 2 means it does not. **Any other result is a hard error**: an unreachable remote must never be read as "the branch does not exist", since that would create the duplicate branch this feature forbids.
3. If a worktree already has this branch checked out, find it by parsing the `branch` lines from `git worktree list --porcelain`:
   - A linked worktree: reuse it and say so. If the branch is on the remote, fetch it and fast-forward only if the worktree is clean and strictly behind; otherwise warn and leave it alone.
   - The main checkout: stop with an error that names the path, because git cannot check a branch out twice.
4. Otherwise, choose one of these:

   | On remote | Local branch | Action |
   |---|---|---|
   | yes | no | `git fetch origin +refs/heads/<name>:refs/remotes/origin/<name>`, then `git worktree add <path> -b <name> --track origin/<name>` |
   | yes | yes | Fetch with `git fetch origin refs/heads/<name>:refs/heads/<name>`, which only fast-forwards. If it is rejected because the branches diverged, warn and keep the local commits. Then `git worktree add <path> <name>` |
   | no | yes | `git worktree add <path> <name>`: the branch already exists, so nothing is created |
   | no | no | `git worktree add <path> -b <name>` from the invoking checkout's `HEAD`, and report the base commit. This is the only row that creates a branch |

- Do not use `GitEngine::create_worktree` as it stands for the first row. It only checks local refs, so it would create a new branch from `HEAD` even though the remote has one. Add `create_worktree_tracking` and call `create_worktree` only for rows where the existence check has already been done.
- Only `origin` is consulted. A repo with no `origin` remote counts as "not on remote", with a warning.

### `--pr` algorithm
1. Parse the value as a bare number, a `https://github.com/<owner>/<repo>/pull/<N>` URL, or `owner/repo#N`. Bare numbers resolve against `origin`. Reuse `RemoteSlug` and the `parse_input` shape from `engine/issue/github.rs`, but note that `parse_github_url` is written for `/issues/` URLs, so check it and generalize it or add a PR-specific parser. Only github.com is supported, matching issues.
2. Find the local remote whose `RemoteSlug` matches the PR's repository. If none matches, stop with an error naming the repository: a PR from an unrelated repo cannot share this object store.
3. Look up the PR's `headRefName`, `isCrossRepository` and `state`, trying `gh pr view --json` first and then `GET /repos/<o>/<r>/pulls/<N>` with a token, the same order `GithubIssueSource` uses. Put this in a new `src/engine/git/pull_request.rs`. A PR that does not exist is an error.
4. Choose the branch:
   - Same-repo PR whose head branch still exists: use the `--branch` flow with `headRefName` on the matched remote. The local branch name is `headRefName`, so pushes from the worktree update the PR.
   - Fork PR, or a head branch that has been deleted: fetch `refs/pull/<N>/head` and use the local branch `pr/<N>`. Apply the same fast-forward-only rule when `pr/<N>` already exists. Warn that pushes will not update a fork PR's branch.
- `--pr` never creates a branch from `HEAD`.
- Warn when the PR is closed or merged, but continue.
- `headRefName` is attacker-controlled on any PR. Validate it with `check-ref-format` and never use it unsanitized in a path or a command position.

### `--worktree` resolution
- The value is a **path** if it is absolute or contains a path separator, or starts with `.`; otherwise it is a **name**, resolved with `worktree_path_by_name` under the repo's awman directory. All-digit names also try the 4-digit zero-padded form, so `123` finds `0123`.
- Because a bare value is always a name, use `./foo` for a relative directory. The name lookup rejects `..` and separators.
- Canonicalize the path, then call `validate_worktree_owner(invoker_root, path)`. Do not touch the branch, fetch or modify anything.

### Launching in the worktree
- Re-root the session with `Session::open_worktree_for(path, path, &invoker)`, the same call `exec_workflow/prepare.rs` makes. This makes `resolve_agent_options` mount the worktree, because the mount comes from `session.git_root()`.
- Keep the invoker's whole `repo_config` (see Security). Add a variant or parameter for this rather than changing what `open_worktree_for` does for workflows.
- Add the main `.git` mount with `worktree_git_overlay`, or git commands inside the container fail on the dangling `.git` pointer. Append it to `run_opts.directory_overlays`, converting from `OverlaySpec` to `DirectorySpec` as needed.
- Keep image setup and trust checks on the invoking session, and set `image_tag_override` to the tag derived from `invoker_root`. Otherwise `agent_image_tag(session.git_root(), …)` looks for an image that was never built.
- Skip `MountScope::resolve` when a target is set: the mount is the worktree root, and the cwd vs repo root question does not apply.
- Print the resolved path and branch before launch, and again when the session exits, since the worktree is kept.
- Add a default no-op `report_session_worktree(&mut self, path, branch)` to the `chat` and `exec prompt` frontend traits. Override it in the TUI to set `active_worktree_path`, so the command box and `tabs/git_poll.rs` follow the worktree.

### Lifecycle
- These flags do not use `WorktreeLifecycle`, and there is no merge or discard prompt when the session ends. The worktree is the user's to keep working in, and removing it stays manual. Do not auto-delete a worktree or branch, including on abort or agent failure.

### Security
- `--worktree <path>` lets the caller choose a host mount source, and the API's working-directory allowlist forbids linked worktrees (`aspec/architecture/security.md`). The flags are therefore CLI and TUI only. Verify that an API request for `exec prompt` containing them is refused.
- `open_worktree_for` normally takes most repo config from the worktree checkout. With `--pr` and fetched branches, that checkout can be someone else's code, and its `.awman/config.json` could add overlays or env passthrough. For these flags, the invoker's repo config applies in full.
- Host git goes through `host_command`, which already disables hooks, fsmonitor and content drivers. Add new git operations there, and do not spawn bare `git`.
- Update `aspec/architecture/security.md`: the "only mount the current directory or repo root" rule needs to record these sessions, which mount a linked worktree of the same repo plus its main `.git`. Update `aspec/uxui/cli.md` too.


## Edge Case Considerations:
- **Remote unreachable or auth failure:** a hard error with git's message, for `--branch` and `--pr`. Never fall through to creating a branch.
- **Branch on remote, no local copy:** the worktree tracks the remote branch and no new line of history starts.
- **Branch name matches a tag:** always use full `refs/heads/...` refs so the tag is not picked up.
- **Local branch ahead of or diverged from the remote:** keep the local commits, warn, and never force-update or reset.
- **Reused worktree with uncommitted changes:** no fetch-driven change is applied to it; warn and launch as is.
- **Default path exists but is not a valid worktree, or holds a different branch:** error naming the path. Never overwrite or delete it.
- **Stale registration:** the directory was deleted but git still lists it, so `worktree add` fails with "already checked out". Report this and suggest `git worktree prune` rather than pruning automatically.
- **Branch checked out in the main checkout:** error telling the user to switch branches or run without `--branch`.
- **Invoked from inside a linked worktree or a subdirectory:** resolve from the main checkout, so names and new paths are the same as from the repo root.
- **`--worktree` given the main checkout, another repo's worktree, or a tampered `.git` pointer:** rejected by `validate_worktree_owner`, with an error stating that it is not a linked worktree of this repo.
- **`--worktree` name with no match:** error naming the directory searched, and list the worktrees that exist there.
- **Detached worktree with `--worktree`:** allowed, with a warning.
- **Not in a git repo, a bare repo, or no commits yet:** clear error, with nothing created.
- **Fork PRs and maintainer edits:** `pr/<N>` has no upstream, and the warning says that pushing needs the fork's remote.
- **Closed or merged PR with a deleted branch:** `refs/pull/<N>/head` still works. A merged PR gets a warning.
- **PR number that is actually an issue, or a PR URL for another repo:** a clear error.
- **`gh` missing and no token:** public repos work over REST without a token. Otherwise, point to `gh auth login`, as issue fetching does.
- **Uncommitted changes in the invoking checkout:** they are not carried into the worktree. Mention this when a new branch is created.
- **LFS or filter-based repos:** `host_command` disables content drivers, so a checkout on the host can contain LFS pointers rather than file contents. Note this in the docs.
- **Two sessions on one worktree, or two simultaneous `--branch` launches:** the first is allowed, as with two sessions in one checkout. For the second, a failed `worktree add` re-runs the lookup once and reuses the worktree it finds.
- **Combining flags:** `--work-item` (see 0139) works with all three on `exec prompt`, and is resolved from the invoking repo, including its `workItems.source`. `--plan`, `--yolo` and `--auto` work unchanged and do not imply any of these flags.


## Test Considerations:
- **Unit, `worktree_paths.rs`:** `named` and legacy paths; `br-` sanitizing and hash suffix, including that `a/b` and `a-b` do not collide; `pr-<N>`.
- **Unit, `--pr` and `--worktree` parsing:** every accepted form; wrong host, `/issues/` URL and malformed values; path vs name classification; `..` rejected; zero-padding.
- **Unit, catalogue (`catalogue/tests.rs`):**
  - The three flags are on `chat` and `exec prompt`, and are mutually exclusive.
  - `exec workflow`'s `--worktree` is still boolean and it does not gain `--branch` or `--pr`.
  - `remote exec prompt` has none of them.
  - All are hidden from the API.
  - Update `exec_prompt_flags_are_the_base_set_plus_issue` for the new set.
- **Integration, `GitEngine`, using a real local bare repo as `origin`:**
  - One test per row of the `--branch` table, asserting whether a branch was created and where it points.
  - **Key regression test:** a branch that exists only on the remote is tracked, not created from `HEAD`.
  - `ls-remote` failure (invalid remote URL) creates nothing.
  - Remote `foo/bar` does not match `--branch bar`.
  - A tag with the same name is not used.
  - A diverged local branch keeps its commits.
- **Integration, `--pr`:** a fake `gh` executable, using the existing `try_gh_cli_with_cmd` approach, and a bare remote with `refs/pull/N/head`. Cover a same-repo PR, a fork PR, a deleted head branch, a malicious `headRefName`, and a repository that matches no remote.
- **Integration, `--worktree`:** resolves by name and by path; rejects the main checkout, another repo's worktree, and a replaced `.git` pointer; leaves HEAD and files unchanged.
- **Integration, launch:** using the test runtime in `engine/container/test_support.rs`, assert for all three flags on both commands that:
  - the workspace mount is the worktree;
  - the main `.git` overlay is present;
  - the image tag comes from the invoking root;
  - a `.awman/config.json` in the worktree that adds an overlay or env passthrough is ignored.
- **Inside a linked worktree:** invoking awman from one resolves to the same paths as from the main checkout.
- **End-to-end (`tests/binary_smoke`, `tests/cli_parity`):** `--help` output; conflict errors; catalogue completeness; regenerated command reference passes its check.
- **TUI:** the command box shows the worktree path after `report_session_worktree`.
- **Security:** `git` hooks in the repo do not run on the host during fetch and `worktree add`.


## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Layering: paths in `data/worktree_paths.rs` (Layer 0, no git calls), git and GitHub calls in `engine/git/`, the decision flow in `command/commands/session_worktree.rs`. Frontends only render messages.
- Declare flags in the catalogue, not per frontend. The CLI, TUI and generated reference pick them up from there.
- `src/command/commands/exec_workflow/prepare.rs` (re-rooting and image root) and `exec_workflow/issue.rs` (`worktree_git_overlay`) are the patterns to follow. Move `worktree_git_overlay` somewhere both can use rather than importing from `exec_workflow`.
- `chat.rs` and `exec_prompt.rs` read `session` from `self.session`. Introduce the re-rooted session there, early on, so all later steps use it; only image setup and trust keep the invoker.
- `chat.rs` discards the `MountScope` result (`_mount_path`), and the real mount comes from `session.git_root()`. Re-rooting is therefore what changes the mount; passing a path to `MountScope` would not.
- Use `CommandError::Other` or a new typed variant for refusals. If adding a variant, fill in the exit-code, HTTP-status and error-code matches in `command/error.rs`.
- Update comments that this change makes stale: the `AGENT_RUN_FLAGS_NO_WORKTREE` and `EXEC_PROMPT_FLAGS` doc comments, and the note in `resolved.rs` that `chat` and `exec prompt` have no `--worktree`. The `implies` closure only sets boolean targets, and only `exec workflow` declares those edges, so `--yolo` and `--auto` stay inert here; keep a test for that.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.

For this item, update:
- `docs/03-agent-sessions.md`: describe the three flags. Correct the `--worktree` section and flag table, which call it `exec workflow` only, and the line under `--yolo` that says to pass `--worktree` explicitly on `chat` and `exec prompt`.
- `docs/04-security-and-isolation.md`: worktree isolation for these sessions, and that the invoking checkout's repo config applies.
- `docs/10-github-integration.md`: `--pr` input formats, authentication, and fork behavior.
- `docs/14-command-reference.md`: generated from the catalogue, so regenerate it rather than hand-editing.
