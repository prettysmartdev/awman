# Work Item: Feature

Title: Reference work server: GitHub issues as a work queue
Issue: issuelink

Depends on: 0147 (Work Server Protocol, `awman-wsp` crate).

## Summary:
- 0147 defines how awman workers pull work from a work server, but nothing ships that serves one. Add a small, production-usable **reference work server** that turns GitHub issues into work items:
  - an issue with the `awman` label in a configured repository is queued;
  - its labels decide affinity and priority;
  - its assignee pins it to that person's linked worker;
  - progress, the pushed branch and the pull request are reported back on the issue.
- It is the simplest useful work server, and the reference third parties read when writing their own. It must pass the `awman-wsp` conformance suite.

## User Stories

### User Story 1:
As a: maintainer

I want to:
label an issue `awman` and have a team build box pick it up, push `awman/issue-42` and open a PR that says "Closes #42"

So I can:
turn a backlog of small issues into reviewable PRs without anyone starting a run

### User Story 2:
As a: developer

I want to:
assign an `awman`-labelled issue to myself and see it in my TUI's queue panel, and nobody else's

So I can:
run it interactively on my machine, with GitHub as the coordination point

### User Story 3:
As a: maintainer

I want to:
label an issue `awman:needs/gpu`

So I can:
make sure only workers labelled `gpu=true` take it

## Implementation Details:
- **Shape.** A subcommand in the awman binary, `awman work-server github`, built on `awman-wsp`. There is no separate install, and it is not a daemon mode: it serves other machines, so it runs wherever the maintainer likes. State is a small SQLite database (`--data DIR`): workers, links, leases, the item cache and the event tail.
- **Config** (`work-server.toml`, or flags):
  ```toml
  bind = "0.0.0.0:8787"            # behind TLS termination, or tls_cert/tls_key
  public_url = "https://work.example.com"
  [github]
  app_id = 123; private_key_path = "app.pem"   # GitHub App (recommended), or token_env = "GITHUB_TOKEN"
  repos = ["acme/app", "acme/api"]
  queue_label = "awman"
  [defaults]
  leader = "claude::claude-opus-5-5"
  interaction = "interactive"
  ```
- **Intake.** Webhooks (`issues`, `issue_comment`, `label`) at `/github/webhook` with signature verification, plus a reconciling poll every 5 minutes that also covers a missed webhook. An open issue with `queue_label` and without `awman:running` or `awman:done` is a queued item.
- **Item mapping.**
  - `title` is the issue title.
  - `body` is the issue body, rendered into awman's work-item template:
    - the issue becomes the Summary;
    - a "Codebase Integration" section says to open no PR and to push only the branch;
    - comments from maintainers are appended under "Discussion".
  - `source.url` is the issue URL, and `kind` is `workflow` with `dynamic: true`.
  - `repo.url` is the repository's HTTPS clone URL, with `base_ref` set to the default branch and `branch`/`push.branch` set to `awman/issue-<n>`.
  - Credentials are a GitHub App installation token scoped to that repository with `contents: write`, minted per lease (1 h) and re-minted by `/credentials`.
- **Labels to scheduling.**
  - `awman:needs/<key>` becomes required `<key> Exists`.
  - `awman:needs/<key>=<value>` becomes required `<key> In [value]`.
  - `awman:prefer/<key>=<value>` becomes preferred with weight 50.
  - `priority:high`, `priority:low` and the `P0`–`P3` labels set priority. Ties are ordered by issue creation time.
  - `awman:interactive` and `awman:non-interactive` set `exec.interaction`.
  - `awman:leader/<agent>::<model>` overrides the leader.
- **Assignees as affinity.** Linking is a GitHub device flow: the work server is a GitHub OAuth App, and the person who approves the `awman worker link` code signs in with GitHub. The worker then gets the server-assigned label `owner=github:<login>`. An issue with assignees becomes required `owner In [github:<login>…]`, so it is offered only to its assignees' machines. Unassigned issues are open to any worker the maintainer has allowlisted (`[workers] allow = ["github:alice", "team:acme/bots"]`).
- **Reporting back.**
  - On claim: add `awman:running`, and post one status comment, edited in place afterwards, that says which worker took the item.
  - On heartbeat: update the comment with the current step.
  - On a successful result: open a PR from `awman/issue-<n>` with "Closes #n", using the app's credentials; mark the issue `awman:done` and link the PR in the comment.
  - On failure: mark it `awman:failed`, with the error stage and message and the failed steps. Removing `awman:failed` re-queues it.
  - On release: remove `awman:running`.
- **Cancel.** Removing `queue_label` or closing the issue while a lease is running returns `cancel: true` on the next heartbeat.
- **Viewers.** It accepts the lease terminal WebSocket (0147) and serves a minimal read-only page per running item (`/items/<id>`) with xterm.js, for maintainers who are signed in, so the reference also demonstrates terminal relay. It never grants remote input.

## Edge Case Considerations:
- Fork PRs and untrusted issue authors: only issues opened or labelled by people with write access are queued. The `queue_label` is applied by a maintainer, and the event's actor permission is checked, so an outsider cannot put work into the queue.
- An issue edited while running: the lease keeps the body it was claimed with, and the status comment notes that the issue changed.
- The branch already exists, from a previous failed attempt: use it as the branch and `base_ref` as the base, with no force-push. Overwriting it needs a maintainer's `awman:retry-fresh` label.
- Rate limits: cache issue reads, use conditional requests, and back off on secondary limits.

## Test Considerations:
- Pass the `awman-wsp` conformance suite.
- Label-to-affinity mapping table tests. Assignee affinity: an assigned issue is not listed for another owner's worker.
- Webhook signature verification, the missed-webhook reconcile, and permission checks for who may queue.
- End to end with a mocked GitHub API, a local bare repo standing in for the clone URL, and a real worker: queued, claimed, pushed, PR opened, labels updated.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- `src/command/commands/work_server/github/`; reuse `src/engine/issue/` (`GithubIssueSource`) for issue reads and its rendering for the body.

## Documentation
- New `docs/18-github-work-server.md`: setting up the GitHub App, the labels, assignees, linking workers, deployment behind TLS. Link it from `docs/17-workers-and-work-servers.md` as the reference implementation.
