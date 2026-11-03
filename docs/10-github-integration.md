# GitHub Integration

The `--issue` flag runs awman commands directly against a GitHub issue, without creating a local work item file first:

| Command | What the issue becomes |
|---|---|
| `awman new spec --issue 84` | Input for a new, numbered work item spec |
| `awman exec workflow my-workflow --issue 84` | The workflow's work item (`{{work_item_content}}`, `{{work_item_number}}`, …) |
| `awman exec prompt --issue 84` | The agent's prompt |
| `awman exec prompt "extra context" --issue 84` | Appended to your prompt |

---

## Issue references

`--issue` accepts three forms:

| Form | Example | Resolved how |
|---|---|---|
| Bare number | `84` | Owner and repo come from the current repo's `origin` remote |
| Shorthand | `prettysmartdev/awman#84` | Owner and repo taken from the reference; the local remote is not read |
| URL | `https://github.com/prettysmartdev/awman/issues/84` | Owner, repo and number parsed from the path |

A bare number requires `origin` to point at `github.com`. awman recognises `https://github.com/owner/repo(.git)`, `git@github.com:owner/repo.git` and `ssh://git@github.com/owner/repo.git` remotes, with the host in any case. Issue URLs passed to `--issue` must be `https://` (or `http://`) `github.com` URLs; other hosts are rejected.

---

## How issues are fetched

Issues are fetched on the host, never inside a container:

1. **GitHub CLI.** awman first runs `gh issue view <N> --repo <owner>/<repo> --json number,title,body,url`. If `gh` is installed and authenticated, this is all that happens.
2. **GitHub REST API.** If `gh` is missing or fails, awman calls `GET https://api.github.com/repos/<owner>/<repo>/issues/<N>`. When `GITHUB_TOKEN` is set in your environment, the request is authenticated, so private repos work and the rate limit is 5,000 requests/hour. Without it, only public repos work, at 60 requests/hour.

awman prints which of the two it is running. The token is never logged.

### Authentication

Either authenticate the GitHub CLI once:

```sh
gh auth login
```

or export a token:

```sh
export GITHUB_TOKEN="ghp_..."
```

A fine-grained personal access token needs read access to **Issues** on the repositories you fetch from. Create one at [github.com/settings/tokens](https://github.com/settings/tokens).

### Tokens for workflow pull requests and CI

Fetching issues and authenticating workflow teardown steps are separate. You do not need `env(GITHUB_TOKEN)` or `ssh()` overlays for `push_branch`, `create_pull_request` or `poll_ci`: before a workflow starts, awman checks the remote and offers a credential source when a step needs one — a host variable (`GH_TOKEN` or `GITHUB_TOKEN` by default), `gh auth token`, a pasted token, or the configured `workflows.prerequisites.githubToken.acquire.argv` command. If you decline, the workflow stops by default; `--prerequisite-unresolved warn` lets it continue with a warning.

A granted token goes only to the step that needs it: an HTTPS `push_branch` gets Git push authentication and `create_pull_request` gets `GH_TOKEN` for `gh`. `poll_ci` uses an authenticated host `gh` when available, otherwise the granted token. Tokens are redacted from workflow output and never written to the remembered-decision file. The agent image still needs `gh` installed for `create_pull_request`.

For GitHub Enterprise, a token is consented for the remote's exact host. Interactive acquisition runs `gh auth token --hostname HOST` and verifies against `https://HOST/api/v3/user`. Unattended Enterprise runs need an explicit `--prerequisite-token env:NAME` or a remembered decision for that host; a github.com token policy never sends a token to another host.

See [Workflow prerequisite checks](05-workflows.md#workflow-prerequisite-checks) for the full flow.

---

## Command behavior

### `new spec --issue <ref>`

Creates a work item in your configured work items directory, named `<NNNN>-ghb<issue>-<title-slug>.md`. `NNNN` is the next free work item number (not the issue number); `ghb<issue>-<title-slug>` identifies the issue, with the title slug cut to 40 characters.

```
Issue:  #84 "GitHub Integration Part 1"
File:   0023-ghb84-github-integration-part-1.md
```

The issue's title and body are sent to the spec interview agent as if you had run `new spec --interview` and submitted them unchanged. Add `--interview` to review and edit the issue text before the agent runs:

```sh
awman new spec --issue 84 --interview
```

### `exec workflow <workflow> --issue <ref>`

Runs the workflow with the issue as its work item. The issue is written to a temporary file on the host and mounted read-only at `<work-items-dir>/<issue-number>-ghb<issue>-<title-slug>.md` inside the container; the file is deleted when the command exits. All `{{work_item_*}}` template variables are populated from it.

`--issue` and `--work-item` are mutually exclusive.

With `--worktree` (or `--yolo`/`--auto`, which imply it), the worktree's branch is named after the issue:

```sh
awman exec workflow my-workflow --issue 84 --worktree
# branch: awman/workflow-ghb84-github-integration-part-1
```

If that worktree already exists, awman asks whether to resume, recreate or stop; answer up front with `--on-existing-worktree`. See [Worktrees](05-workflows.md#worktrees).

### `exec prompt --issue <ref>`

Sends the issue to the agent as the prompt. The positional prompt is optional when `--issue` is given. When both are given, your text comes first, then the issue:

```sh
awman exec prompt "Review this for security issues" --issue 84
```

The agent receives:

```
Review this for security issues

# GitHub Integration Part 1

<issue body>
```

No files are created.

---

## Errors

Errors are prefixed with the command, for example `exec prompt: failed to fetch issue: …`, followed by one of:

| Message | Cause and fix |
|---|---|
| `GitHub: no GitHub remote detected for this repository` | A bare number was given but `origin` is missing or not on github.com. Use `owner/repo#N` or a URL, or add a GitHub `origin`. |
| `GitHub: invalid issue reference '<input>': <hint>` | The reference is not a number, `owner/repo#N`, or a `github.com/<owner>/<repo>/issues/<N>` URL. |
| `GitHub: issue not found: <url>` | Wrong number or repo, or a private repo without authentication. |
| ``GitHub: unauthorized — ensure gh is authenticated (`gh auth login`) or GITHUB_TOKEN is set`` | The REST API refused the request. Authenticate as above. |
| `GitHub: API rate limit exceeded` | Unauthenticated limit reached. Authenticate, or wait. |
| `GitHub: network error: <detail>` | api.github.com could not be reached. |

---

## Examples

```sh
# Spec from an issue, editing the text first
awman new spec --issue 42 --interview

# Planning workflow driven by an issue
awman exec workflow aspec/workflows/plan.toml --issue 42

# Your framing plus the issue
awman exec prompt "Security review: check for SQL injection, XSS, and auth bypass" --issue 42

# Private repo in another org
export GITHUB_TOKEN="ghp_..."
awman exec prompt --issue owner/private-repo#99
```

---

[← API & Remote Mode](09-api-and-remote-mode.md) · [Next: Runtimes →](11-runtimes.md)
