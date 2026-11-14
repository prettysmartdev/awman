# GitHub Integration

awman treats GitHub issues as work items through the shared `--work-item`
option and the `workItems.source` setting. You can use an issue as input for
`exec prompt` or a workflow, create a new issue with `new spec`, and edit an
existing issue with `specs amend`. A separate `--pr` option on `chat` and
`exec prompt` opens a pull request in a worktree; see
[Pull requests for `--pr`](#pull-requests-for---pr).

## Choose GitHub as the default source

Set this in the repository config to make bare numbers refer to issues in the
repository's GitHub `origin`:

```sh
awman config set workItems.source github
```

Or select GitHub explicitly for an individual command, regardless of the
configured default:

| Reference | Example |
|---|---|
| GitHub issue number | `github:84` or `#84` (uses `origin`) |
| Repository shorthand | `prettysmartdev/awman#84` |
| GitHub issue URL | `https://github.com/prettysmartdev/awman/issues/84` |

With `workItems.source = "github"`, a bare number such as `84` also selects
issue 84 on `origin`. A bare number requires `origin` to point to GitHub. For
explicit references, the owner and repository come from the reference.
Supported GitHub remotes include HTTPS, `git@github.com:...`, and
`ssh://git@github.com/...` forms. Issue URLs must be on `github.com`.

Use `github:<N>` when you want a value to clearly select GitHub even if the
repository defaults to local work item files. See
[work item source configuration](07-configuration.md#work-item-sources) for
all source values and trusted HTTP providers.

## Use an issue as input

```sh
# Run a prompt based on an issue
awman exec prompt --work-item github:84

# Add your own framing before the issue content
awman exec prompt "Review this for security issues" --work-item github:84

# Supply the issue to a workflow and its work item template variables
awman exec workflow aspec/workflows/plan.toml --work-item github:84
```

The issue title and body are made available as the work item. Workflows can
use `{{work_item_content}}`, `{{work_item_title}}`, and the other documented
[work item inputs](06-dynamic-workflows.md#work-item-inputs). Dynamic workflows
require a work item; if none is supplied, the command fails before launch.

## Create an issue with `new spec`

With `workItems.source = "github"`, `new spec` creates an issue instead of a
numbered local file. Start a new issue from the configured template:

```sh
awman new spec
```

Without `--interview`, awman creates the issue immediately from the new spec
content. The created issue URL and number are reported. Before anything is
published, awman names the destination (for example
`creating a github work item in github:acme/widgets`). If you dismiss a question
(Ctrl+C or closing the dialog), nothing is created. To review and shape
the content in an interview, use:

```sh
awman new spec --interview
```

During an interview, awman publishes edits to the new issue while the agent is
running. Creation happens on the first meaningful edit. awman says so before the
agent starts (`draft edits are published to <destination> while the agent
runs`). If you abandon the
interview after an issue has been created, that issue may contain only part of
the intended spec. The issue URL is printed: report the partial issue and
close it if you do not intend to finish it.

You can also seed a new spec from an existing GitHub issue by passing its
reference with `--work-item` while the configured creation target is local or
another provider. If the seed is already in the configured target, awman
refuses to duplicate it and suggests `awman specs amend <ref>` instead.

## Amend an existing issue

```sh
awman specs amend github:84
```

The issue's current title and raw body seed an editable draft. awman prints the
issue URL and draft path before the agent starts, then publishes edits while
the agent runs. If you abandon an amend, the issue may contain a partial
update; use the reported URL to review it and close the issue if it should not
remain open. A failed upload keeps the draft path available for recovery. awman retries a
failed upload after one second, then waits twice as long after each further
failure, up to five minutes; a successful upload resets the delay. If the agent
cannot be launched at all, the unedited draft is deleted.

Remote edit drafts are stored under
`.awman/context/scratch/work-items/` in the repository context. You can
delete a draft file there after confirming that you no longer need it. A
successful edit removes its draft automatically.

## Review issue content as untrusted input

Issue titles and bodies are written by their authors. In public repositories,
they can contain prompt injection that asks an agent to ignore your
instructions, reveal secrets, or take unrelated actions. Review the issue and
limit the credentials, overlays, tools, and repository access available to
the run. `--yolo` does not make issue content trustworthy; the same warning
applies when running without approval prompts.

## Authentication

Authenticate the GitHub CLI:

```sh
gh auth login
```

Or set `GITHUB_TOKEN` in the environment. A fine-grained personal access token
needs read access to **Issues** for fetching and write access to create or
update issues. awman never prints the token.

Issue fetches use the GitHub CLI when available and authenticated, with the
GitHub REST API as a fallback. Private repositories require authentication;
public repositories can be fetched without a token subject to GitHub's
unauthenticated rate limits.

### Tokens for workflow pull requests and CI

Fetching issues and authenticating workflow teardown steps are separate. You
do not need `env(GITHUB_TOKEN)` or `ssh()` overlays for `push_branch`,
`create_pull_request` or `poll_ci`: before a workflow starts, awman checks the
remote and offers a credential source when a step needs one — a host variable
(`GH_TOKEN` or `GITHUB_TOKEN` by default), `gh auth token`, a pasted token, or
the configured `workflows.prerequisites.githubToken.acquire.argv` command. If
you decline, the workflow stops by default; `--prerequisite-unresolved warn`
lets it continue with a warning.

A granted token goes only to the step that needs it: an HTTPS `push_branch`
gets Git push authentication and `create_pull_request` gets `GH_TOKEN` for
`gh`. `poll_ci` uses an authenticated host `gh` when available, otherwise the
granted token. Tokens are redacted from workflow output and never written to
the remembered-decision file. The agent image still needs `gh` installed for
`create_pull_request`.

For GitHub Enterprise, a token is consented for the remote's exact host.
Interactive acquisition runs `gh auth token --hostname HOST` and verifies
against `https://HOST/api/v3/user`. Unattended Enterprise runs need an explicit
`--prerequisite-token env:NAME` or a remembered decision for that host; a
github.com token policy never sends a token to another host.

See [Workflow prerequisite checks](05-workflows.md#workflow-prerequisite-checks)
for the full flow.

## Pull requests for `--pr`

`awman chat --pr <ref>` and `awman exec prompt --pr <ref>` run the session in a
worktree for a GitHub pull request. See
[Agent Sessions](03-agent-sessions.md#--branch---pr-and---worktree) for the
worktree behavior.

### Accepted formats

| Form | Example |
|---|---|
| PR number | `--pr 482` |
| github.com URL | `--pr https://github.com/acme/widgets/pull/482` |
| `owner/repo#N` | `--pr acme/widgets#482` |

A bare number uses your `origin` remote, which must be on github.com. For the
other two forms, the pull request's repository must match one of your
repository's configured remotes (`origin` is preferred if several match); a
pull request from an unrelated repository is refused. Only
`https://github.com/` pull request URLs are accepted: issue URLs, `http://`,
other hosts, and URLs with queries or fragments are rejected.

### How the pull request is looked up

Like issues, the lookup runs on the host. awman first runs `gh pr view` against
`github.com` explicitly (a `GH_HOST` pointing at a GitHub Enterprise server is
not used for these lookups) and, if `gh` is missing or fails, falls back to
the GitHub REST API. Authenticate once with `gh auth login`, or export
`GITHUB_TOKEN`. Without a token, public repositories still work (at GitHub's
unauthenticated rate limit); private repositories need authentication. A
missing or unauthorized pull request fails with a hint to run `gh auth login`
or set `GITHUB_TOKEN`.

### Fork and deleted-branch pull requests

- **Same repository.** The worktree uses the PR's head branch, so pushes from
  the session update the PR.
- **From a fork, or head branch deleted.** The worktree uses a local branch
  named `pr/<N>`, created from GitHub's pull ref. It does not track the fork's
  branch, so pushing to `origin` will not update the pull request. awman warns
  about this; to push to the fork, add the fork as a remote and push there
  yourself.
- **Closed or merged.** awman warns and continues with the PR's code.

Worktrees for pull requests are kept in a `pr-<N>` directory; reopen one with
`--worktree pr-<N>`.

---

[← API & Remote Mode](09-api-and-remote-mode.md) · [Next: Runtimes →](11-runtimes.md)
