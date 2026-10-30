# Work Item: Feature

Title: Work item sources: `workItems.source`, a unified `--work-item`, HTTP provider, inline bodies
Issue: issuelink

## Summary:
- Today the only way to give dynamic mode its input is a file named `NNNN-*` in the repository (`dynamic.rs:279-299`). `--issue` exists but dynamic mode rejects it, and GitHub is the only `IssueSource` (`src/engine/issue/router.rs`). API clients that keep specs elsewhere must commit a file onto a branch just so awman can read it, and their numbers can collide with work items already in the repo. Teams that track work in GitHub issues have to keep passing `--issue`, and `new spec` can only ever write a file.
- Add a repo-level work item source, `workItems.source` in `.awman/config.json`. When it is set, **all** work items are created in and read from that location: with `"source": "github"`, `new spec` opens a new GitHub issue instead of writing a file, and `--work-item 42` reads issue #42 on every command.
- Remove `--issue` everywhere and rework `--work-item` into the one flag for every kind of work item: local files, GitHub issue numbers and links, and HTTP work items. A bare number follows `workItems.source`, which defaults to local files. URLs are auto-detected and routed to the right provider.
- Add a generic HTTP work item provider, accept `--work-item` from any source in dynamic mode, and accept an inline work item on `POST /v1/commands`.
- Make `NNNN` resolution deterministic: today `read_dir` order decides which of two `0001-*` files wins (`issue.rs:26-49`). Error when more than one file matches.

## User Stories

### User Story 1:
As a: team that tracks all work in GitHub issues

I want to:
set `"workItems": { "source": "github" }` once, and then have `awman new spec` open an issue and `awman exec workflow implement.toml --work-item 42` read issue #42

So I can:
keep every work item in GitHub without any files in the repo, and without having to remember a different flag or reference format on each command

### User Story 2:
As a: user

I want to:
pass whatever reference I have to one `--work-item` flag, whether that is `0042`, an issue link, or an HTTP work item URL

So I can:
stop deciding between `--issue` and `--work-item`, and have awman work out what I meant

### User Story 3:
As a: API client that stores specs in its own database (e.g. infact)

I want to:
run `exec workflow --dynamic --work-item https://infact.example/api/projects/P/nodes/N/work-item`

So I can:
stop committing a work item file onto a special branch for every run

### User Story 4:
As a: user with work items in the repo

I want to:
get a clear error when two files match `--work-item 0007`

So I can:
avoid running the wrong spec silently

## Implementation Details:

### Configuration
- Put the setting in the existing repo-only `workItems` block, as `workItems.source`, next to `dir` and `template`. All work item settings then live together, and `work_items.source` follows the existing CLI key naming. Add `source: Option<WorkItemSource>` to `WorkItemsConfig` in `data/config/repo.rs`:
  ```json
  { "workItems": { "source": "github" } }
  ```

  | Value | Meaning |
  |---|---|
  | unset or `"local"` | Files in `workItems.dir`: today's behavior |
  | `"github"` | Issues in the repository of the `origin` remote |
  | `"http:<name>"` | The entry with that `name` in global `issueSources.http` |

- Add `issueSources.http: [{ name, baseUrl, tokenEnv }]` to **global** config only. Repo config ships with the checkout and may be untrusted, so it can only select a provider by name. It can never supply a URL or token variable.
- An unrecognized value, or an `http:<name>` with no matching global entry, is an error on any command that needs a work item. It must not fall back to local: that would write specs to a location the team is not using. Warn at load through `load_warnings`, too.
- `workItems.dir` still locates local files when a source is set, including for explicit `local:` references. `workItems.template` still shapes new work items for every source.

### Reference grammar
Add `WorkItemRef::parse(input)` in `data/work_item_ref.rs` (Layer 0, pure, no I/O). The first matching rule wins:

| Input | Resolves to | Follows `source`? |
|---|---|---|
| `https://github.com/<o>/<r>/issues/<N>` | That GitHub issue | No |
| Any other `http(s)://` URL | The `issueSources.http` entry whose `baseUrl` contains it; otherwise an error | No |
| `owner/repo#N` | That GitHub issue | No |
| `github:<N>` or `#<N>` | Issue `N` on `origin` | No |
| `local:<N>` | Local file number `N` | No |
| `http:<name>:<id>` | `<id>` on that provider | No |
| A value with a path separator, or ending in `.md` | Local file | No |
| Anything else, such as `42`, `0042`, `WI-42`, `PROJ-123` | The configured source (below) | **Yes** |

Bare values under each source:
- **Local:** today's number parsing and `NNNN-*` lookup, so `WI-42` and `0042` keep working.
- **GitHub:** must be all digits, with leading zeros ignored (`0042` is issue 42).
- **HTTP:** `<baseUrl>/<percent-encoded id>`, so non-numeric ids work.

The explicit forms always win over `workItems.source`, so a repo set to GitHub can still run one local file, and a repo with no source can still read a single issue.

### Resolver
- Add `src/command/commands/work_item_source.rs` with one entry point that every command uses:
  ```rust
  pub struct ResolvedWorkItem {
      pub origin: WorkItemOrigin,      // Local { path } | Remote { source_id, provider }
      pub number: Option<u32>,
      pub key: String,                 // stable id for state and resume
      pub title: String,
      pub slug: Option<String>,
      pub content: String,             // rendered markdown
  }
  ```
- `key` is `NNNN` for local files, and for remote work items a short hash of the canonical `source_id`, so GitHub #7 and file `0007` never share state. Stop using `numeric_id().unwrap_or(0)`, which makes every non-numeric id `0000`.
- Rewrite `IssueSourceRouter` so routing comes from the parsed `WorkItemRef`, not from `can_handle` guessing. Remove the rule that gives bare integers to GitHub. Keep `fetch_issue_with_progress` for user feedback.
- Make `find_work_item_file` collect every match and return `AmbiguousWorkItem { number, paths }` when there is more than one. Replace the separate lookup in `specs amend`, which ignores `workItems` rules and maps an unparseable value to 0.

### Flag changes
- Remove `--issue` from `exec prompt`, `exec workflow`, `new spec` and, through flag derivation, `remote exec prompt` and `remote exec workflow`. Remove `IssueSourceFlags` and the `issue` and `work-item` `conflicts_with` entries.
- Add `--work-item` to `exec prompt` and `new spec`, and keep it on `exec workflow`. Update the help text to describe all the accepted forms. `exec prompt` then requires a prompt or `--work-item`, as it does for `--issue` now.
- `specs amend <work_item>` keeps its positional argument, and it takes the same grammar.
- Do not keep `--issue` as an alias, since a bare value would have a different meaning under each flag. Parsing `--issue` returns an error that names the replacement, for example: `--issue was replaced by --work-item; use --work-item github:42`. Do this for the CLI, the TUI and API arguments.
- `remote` commands forward the value as written, so the server resolves it against its own repo config. Do not parse it on the client.

### Reading on each command
- **`exec workflow` (`execute.rs`):** replace the `--issue` and `--work-item` branches with one resolver call. Remote work items keep the temp-file overlay and slug-based worktree naming. Local files keep using the checkout's file.
- **`exec workflow --dynamic` (`dynamic.rs`):** build `WorkItemContext` from `ResolvedWorkItem`. Key the dynamic-workflow and resume state by `key`. `--dynamic` still requires `--work-item`.
- **`exec prompt`:** use the resolver, so a local file can now also be the prompt.
- **`api_server/push_back.rs`:** this runs `parse_work_item_number` on the raw flag value, which would read digits out of any URL. Treat the value as a number only if it is a local `WorkItemRef`; otherwise use the default commit message or the resolved title.
- A work item that cannot be found is now always a hard error before any container starts. `execute.rs` currently warns and runs with empty `{{work_item_*}}` placeholders, and this removes that behavior. Call it out in the release notes.

### Creating with `new spec`
- Extend `IssueSource` with `create_issue(title, body, git_root) -> Issue` and `update_issue(issue, body)`, with default implementations that return a "not supported" `ProviderError`.
- **GitHub:** `gh issue create --repo <o>/<r> --title=<title> --body-file -`, then read the URL from stdout. Fall back to `POST /repos/<o>/<r>/issues` with a token, following the same order as fetching. Pass the body on stdin because of its size, and use the `--title=` form so a title starting with `-` is not read as a flag.
- **HTTP:** `POST <baseUrl>` with `{ "title", "body" }`, returning the same JSON shape as `GET`.
- Split `create_new_spec` at the point where it writes the file. Everything up to the rendered body stays shared. With a remote source:
  - Skip `next_work_item_number` and the work items directory.
  - `{{number}}` renders empty, since the provider assigns the number at creation.
  - With an interview, live sync creates the issue the first time the agent changes the draft (see below), so an interview abandoned before any edit leaves nothing on the tracker. Without an interview, create it right away.
  - `NewSpecOutcome` gains the created URL next to `created_path`. Print the URL and number.
- `new spec --work-item <ref>` replaces `new spec --issue <ref>`: it seeds a new work item from the reference, and implies an interview as now. If the reference is already in the configured source, stop with an error that says the work item already exists and suggests `specs amend`, because creating it would make a duplicate. A reference from another location is allowed, so a local file or foreign issue can be brought into the configured source.

### Editing: interview and `specs amend`
- The agent cannot write to a tracker directly, so it edits a markdown draft in a scratch file in the `context(repo)` overlay, and awman uploads the result. This reuses an existing mount instead of adding a new one:
  1. Add a `context(repo)` spec with `rw` permission to the run's `collected.context_overlays`. Overlays have union semantics, so this is safe if the user already has one. If config narrows it to `context(repo:ro)`, the most restrictive permission wins and the agent cannot save, so stop with an error before launching.
  2. Write the draft on the host to `<repo context dir>/scratch/work-items/<uuid>.md`, resolving the directory with `ContextDirResolver::repo_dir`. The agent sees it at `/awman/context/repo/scratch/work-items/<uuid>.md`. Because the whole directory is mounted, agents that save by rename work normally. Add a helper in `data/fs/context_dirs.rs` for this path.
  3. Pass that container path to the prompt. `render_interview_prompt` and `render_amend_prompt` currently take a number and assume a file in the repo, so they need a path parameter.
  4. Print the draft's full host path through `write_message` before the agent container launches, while the terminal still belongs to awman, so the user can open the file directly. Edits the user saves there are treated like the agent's and are picked up by live sync.
  5. Run live sync while the agent runs, and a final upload after it exits (below).
  6. Delete the draft after the final upload succeeds, because `context(repo)` contents persist and are visible to later sessions.
- Seed the draft from the raw `issue.body`, not `format_as_markdown`. That function prepends `# <title>`, so each amend would otherwise add another heading to the issue. Give the agent the title in the prompt.
- **Every read of the draft is untrusted.** The agent controls this directory, so open the file without following symlinks, require a regular file, and enforce a size limit. Otherwise, an agent could replace the draft with a symlink and have awman upload a host file to the tracker. A read that fails these checks or is not valid UTF-8 is skipped, with a warning, and does not stop the agent.

### Live sync for interview and `specs amend`
- While the agent runs, awman watches the draft and uploads changes, so the user can read the work item on the tracker and keep iterating with the agent. Sync is one-way, from the draft to the tracker. It applies only to remote sources.
- Add `src/engine/issue/draft_sync.rs` with a task that runs alongside `execution.wait().await` in `create_new_spec` and `specs amend`, and share that wiring between the two.
- **Detecting changes:** poll the draft once per second, comparing size and modification time, then a hash of the content. Do not use filesystem events: they do not reliably cross the VM-backed mounts used by the Apple and builtin runtimes.
- **Debounce:** upload after the draft has been unchanged for 5 seconds. During continuous edits, upload at most once every 30 seconds, using the latest content. Allow one request at a time, and on `RateLimited`, back off and let later uploads replace the skipped ones. Define these as constants in `draft_sync.rs`; do not add config settings. Put the timing decisions in a small pure state machine so tests do not depend on real time.
- **What is uploaded:** skip any content identical to the last upload. Never upload an empty draft during the run, since this is usually a save in progress.
- **Interview:** the first upload that differs from the seeded draft calls `create_issue`, using the title the user entered, and later uploads call `update_issue`. Write the new issue URL and number to the user message channel as soon as the issue is created. The agent has no part in this, so nothing needs adding to its prompt.
- **Amend:** later uploads call `update_issue`. Print the issue URL before launch. Before each upload, re-fetch the issue and compare it with the last body awman uploaded or fetched, ignoring line-ending differences. If it differs, someone else edited it, so **pause sync for the rest of the run** and do not overwrite their change. Apply this check to interview uploads after the first as well.
- **Messages:** keep the user message channel quiet. Report only the scratch file's host path (before launch), the issue URL when the issue is created, warnings and errors, using `write_message`. **A successful sync prints nothing.** The TUI shows these messages in the execution window, and the CLI already queues them while `pty_active` is set and prints them at `replay_queued` after the agent quits. The polling task does not hold the frontend: it sends events over a channel, and the command forwards them by selecting over that channel and `execution.wait()`.
- **Upload failures during the run:** never interrupt the agent. Retry on the next cycle, and report the failure once, when it first happens, rather than on every retry. An auth or permission failure stops further attempts.
- **Final upload after the agent exits, however it exits**, including a non-zero exit, a crash or Ctrl+C:
  1. Stop the polling task, waiting for any in-flight request to finish, so two uploads cannot race or arrive out of order.
  2. Read the draft once more, with the same checks.
  3. Upload it immediately, ignoring the debounce, if it differs from the last upload. Keep the conflict check.
  4. If nothing needed uploading, make no API call. A successful final upload prints nothing beyond any warnings from the run.
  5. If the final upload fails, or sync is paused by a conflict, keep the draft and print its host path. Exit with an error, because the tracker does not have the latest version. Otherwise, delete the draft.
- If the draft is empty at exit, or unchanged since seeding, make no upload. An interview that already created an issue keeps it, with its last uploaded content. Do not delete issues automatically.

### HTTP provider
- `HttpIssueSource` handles URLs under a configured `baseUrl`. It sends `GET <url>` with `Accept: application/json` and `Authorization: Bearer $tokenEnv`. The response is `{ "id": "...", "title": "...", "body": "markdown", "number": 42? }`, mapped onto `Issue`. A `text/markdown` response is also accepted, with the title taken from the first heading.
- The URL match compares parsed scheme, host, port and path segments. A string prefix check would let `https://a.example.evil.com` match `https://a.example`.
- Require `https`, except for loopback addresses. Do not follow redirects to another origin with the token. Never send a request to a URL that matches no configured entry; this also stops an API client from using the awman host to reach arbitrary addresses.

### Inline work items
- `POST /v1/commands` accepts an optional `work_item: { title, body, id? }`. awman writes it to the same temp file and treats it as a remote `ResolvedWorkItem`, whatever `workItems.source` says. This covers callers that cannot expose an HTTP endpoint the awman host can reach. Passing it along with a `--work-item` argument is a 400.

## Edge Case Considerations:
- **Migration from `--issue 42`:** with no source set, `--work-item 42` now means local file 0042. If that file does not exist, the error also suggests `--work-item github:42`. Scripts using `--issue` fail with the replacement message rather than doing something else silently.
- **Same number in both places:** with no source, `--work-item 42` reads the file even if issue 42 exists; with GitHub, it reads the issue even if `0042-*.md` exists. There is no fallback between sources, because a work item must never depend on what happens to exist.
- **Files left in the repo after switching to GitHub:** they are ignored by bare references and still readable with `local:`. Nothing is migrated automatically.
- **Fetch failures (401, 403, 404, rate limit, timeout):** fail at preflight with the provider's error, before any container starts.
- **Secrets in logs:** the token is never logged, and the URL is logged without its query string.
- **Create failure during or after an interview:** the draft stays in the `context(repo)` scratch directory. Print its host path, so the interview's output is not lost.
- **Create request times out:** the issue may have been created. Do not retry automatically, including on later sync cycles, because that can duplicate it. Pause sync, keep the draft, and tell the user to check the tracker.
- **User and agent editing the draft at once:** the file has no locking, so whichever saves last wins, and that version is what gets uploaded. Editors that detect outside changes will warn the user; the agent should re-read the file before changing it, and the prompt should tell it so.
- **Abandoned interview:** because the issue is created during the run, quitting after the agent's first edit leaves a partial issue on the tracker. Report its URL so the user can close it. The docs should say this, too.
- **Partly written drafts:** the debounce makes these unlikely, but an in-progress save can still be uploaded. The next cycle or the final upload corrects it.
- **Upload history:** on GitHub, each upload appears in the issue's edit history, and on HTTP providers each is a request. The debounce limits this, and the docs should mention it.
- **Agent writes unexpected content:** live sync publishes the agent's edits with no review step, on a tracker that may be public, while the agent may have read private repo content. `--plan` and permission modes do not change this, since the upload is done by awman. The docs must say that interview and amend publish while they run.
- **awman is killed mid-run:** the final upload does not happen. The tracker has the last debounced version, and the draft stays in the scratch directory.
- **`gh` missing and no token:** creating needs authentication, even on public repos. Point to `gh auth login`. Lack of write permission is reported as such.
- **No GitHub `origin`:** `NoRemoteDetected`, naming `workItems.source` as the reason GitHub was used. If `origin` is a fork, issues go to the fork; use a full URL or `owner/repo#N` to read upstream. Targeting a different repository for creation is out of scope.
- **Issue number that is really a PR:** GitHub's issue APIs accept PR numbers. Keep today's behavior.
- **Prompt injection:** on public repos, anyone can write an issue, and a bare `--work-item 42` makes it easy to give its text to an agent, including under `--yolo`. Always print the source and title before launch, and warn about this in the docs. Treat the body only as prompt data, never as flags or paths.
- **Malicious titles and ids:** slugs from provider data go through `slugify`, and ids are percent-encoded. Neither reaches a path or argument unsanitized.
- **Private content on disk:** temp files hold private issue text. `issue_source_overlay` currently writes them to the shared temp directory with default permissions and a predictable name; make them owner-only with an unpredictable name.
- **A resumed run whose work item has changed:** resume uses the saved workflow, and logs that the spec changed since the workflow was designed. Changing `workItems.source` between runs changes `key`, so nothing resumes against the wrong item.
- **Worktree sessions:** read `workItems.source` from the invoking session, as for other settings, so a branch's older config cannot redirect work items.
- **Offline use:** a remote source needs the network for every work item. `local:` remains available.
- **File paths outside the repo:** an explicit file value must resolve inside the checkout. A path that escapes it, including by symlink, is refused, so `--work-item` cannot read arbitrary host files into a container.
- **Leftover or concurrent drafts:** each draft has a unique name, so concurrent sessions do not collide, but any session with `context(repo)` can see them. Failed runs leave drafts in `scratch/work-items/` on purpose, so the docs should say they can be deleted.

## Test Considerations:
- **Unit, `WorkItemRef::parse`:** a table of every row in the grammar, leading zeros, `WI-42`, `PROJ-123`, malformed values, and precedence between rules.
- **Unit, config:** `workItems.source` round trip; each accepted value; an unknown value and an unknown `http:<name>` give errors and do not fall back to local; `issueSources` in repo config has no effect; `config set work_items.source` works.
- **Resolver matrix:** every reference form against each of the three source settings, asserting which provider is called. Assert that a bare value never tries a second source.
- **Router and provider tests:** HTTP base URL match including the look-alike host; provider tests against a local test server (JSON, markdown, 401, 404, a cross-origin redirect does not receive the token); non-`https` refused except on loopback.
- **Creating:** fake `gh` executable (the `try_gh_cli_with_cmd` approach) and a test server for REST and HTTP. Cover: no file is written and no number is allocated with a remote source; a title starting with `-`; a create failure keeps the draft; no retry on timeout.
- **Interview and amend:** using the test runtime, the draft is written under the `context(repo)` directory, mounted read-write, and read back; a symlinked or oversized draft is refused; `context(repo:ro)` stops before launch; the draft is removed after the final upload. An unchanged draft makes no call, and a concurrent remote change is not overwritten. A seeded and re-uploaded body does not gain a `# <title>` heading.
- **Draft path message:** the host path is written before the agent launches, for interview and amend.
- **Live sync, state machine (no real time):** waits out the 5-second quiet period; a burst of edits produces one upload; continuous edits upload no more than once per 30 seconds, with the latest content; identical content is skipped; empty content is skipped during the run.
- **Live sync, integration (test server or fake `gh`, with a draft changed while a held test agent runs):**
  - An interview creates exactly one issue, then updates it, and an interview with no edits creates none.
  - The issue URL is written to the user message channel once, when the issue is created, while the agent is still running.
  - Successful syncs write nothing to the user message channel, and failures and warnings do.
  - A draft swapped for a symlink mid-run is never uploaded.
  - A failed upload does not stop the agent and is retried; an auth failure is not retried.
  - A remote edit pauses sync; a create timeout does not create a second issue.
- **Final upload:** edits made inside the debounce window are uploaded after exit, for a normal exit, a non-zero exit and a cancelled run; nothing races an in-flight request; nothing is uploaded when already current; a failed final upload keeps the draft and returns an error.
- **Commands:** `exec workflow`, `--dynamic`, `exec prompt` and `specs amend` each with local, GitHub and HTTP sources; a missing work item stops before any container starts; resume keying by `key`, including GitHub #7 vs file `0007`.
- **Inline:** dynamic preflight with an inline work item; inline plus `--work-item` is a 400.
- **Catalogue and parity:**
  - `--issue` is gone from every command, including derived `remote` flag sets, and its error names the replacement.
  - Update `exec_prompt_flags_are_the_base_set_plus_issue`, `exec_workflow_flags_keep_their_documented_order`, and the `build_*_issue_flag_*` tests in `dispatch/mod.rs`.
  - The conflict test between `--issue` and `--work-item` is deleted.
  - The regenerated command reference passes `markdown_reference_matches_committed_docs`.
- **`push_back.rs`:** a URL value containing digits does not produce an "Implement work item NNNN" message.
- **Ambiguous `NNNN`:** the error lists both paths, for `--work-item` and `specs amend`.
- **End-to-end:** `new spec` in a repo set to GitHub, with a fake `gh`, creates no file and prints the URL.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Layering: config and `WorkItemRef` in `data/` (no I/O), providers in `engine/issue/`, the flow in `command/commands/work_item_source.rs`. Frontends only render.
- Keep the `Issue` and `IssueSource` type names to limit churn. This item changes the user-facing concept, not the provider abstraction.
- `parse_work_item_number` and `find_work_item_file` now serve only the local path. Move them out of `exec_workflow/issue.rs` to the new module. `push_back.rs` and `dynamic.rs` import them from `exec_workflow` today.
- Other code that assumes a work item is a file or number: `worktree_lifecycle.rs` (`name_for`), `build_leader_prompt` placeholders, and `render_interview_prompt` and `render_amend_prompt`. Search for `work_item_number` and `{{work_item_` when implementing.
- Update `aspec/uxui/cli.md` and `aspec/architecture/security.md` for the new flag, the global-only provider rule and the outbound request limits. Add `workItems.source` to the settings table used by `awman config` (`command/commands/config.rs`).
- Related work items that mention `--issue` or this item's old scope: `aspec/work-items/README.md`, 0147 (uses the inline source), and 0151 (`--issue` with its new flags on `exec prompt`).

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.

For this item, update:
- `docs/07-configuration.md`: `workItems.source` and global `issueSources.http`.
- `docs/10-github-integration.md`: rewrite around `--work-item` and `workItems.source`, including creating issues with `new spec`, how interview and amend publish edits while running, and the prompt injection warning.
- `docs/06-dynamic-workflows.md`: work item inputs.
- `docs/09-api-and-remote-mode.md`: inline work items, and how `remote` values are resolved on the server.
- Replace `--issue` in `README.md`, `docs/00-getting-started.md`, `docs/03-agent-sessions.md`, `docs/05-workflows.md`, and `docs/contents.md`.
- `docs/releases/unreleased.md`: `--issue` removal and how to migrate, and that a missing work item is now an error.
- `docs/14-command-reference.md`: generated from the catalogue, so regenerate it rather than hand-editing. Leave past release notes and blog posts as they are.
