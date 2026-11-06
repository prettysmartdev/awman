# Work Item: Feature

Title: Reference work server: GitHub issues as a work queue
Issue: issuelink

Depends on: 0143 (the complete daemon, including the worker pull protocol and 0139 wire conventions).

## Summary:
- 0143 defines how awman workers pull work from a work server. Add a small, working **reference implementation** in `examples/github-issues-work-server/` that turns GitHub issues into work items:
  - an issue with the `awman` label in a configured repository is queued;
  - its labels decide affinity and priority;
  - its assignee pins it to that person's linked worker;
  - progress, the pushed branch and the pull request are reported back on the issue.
- This is source code for people and agents to study, adapt, or run themselves. It is not part of awman proper: the awman binary, daemon, command catalogue, default build, installers and release artifacts do not include the GitHub server. To run it, a user clones this repository and builds the example from source. It must pass the `awman-wsp` conformance suite.

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
- **Shape.** A standalone executable and Cargo package under `examples/github-issues-work-server/`, built explicitly from a clone of this repository (for example, `cargo build --manifest-path examples/github-issues-work-server/Cargo.toml --release`). Give the example manifest its own `[workspace]` so Cargo does not implicitly add it to the root workspace. It depends on the public `awman-wsp` protocol crate, not on the `awman` application crate or its internal modules. Keep it out of awman's default build, installers and release artifacts. It is not an awman command or daemon feature; it runs wherever the maintainer chooses. Its own `--data DIR` holds a small SQLite database for workers, links, leases, the item cache and the event tail.
- **Config** (`work-server.toml`, or flags):
  ```toml
  listen = "0.0.0.0:8787"          # same name as daemon.network.listen (0139); behind TLS termination, or tls_cert/tls_key
  public_url = "https://work.example.com"
  [github]
  app_id = 123
  client_id = "Iv1.example"          # same GitHub App; enable its device flow
  client_secret_env = "GITHUB_APP_CLIENT_SECRET" # viewer web sign-in
  private_key_path = "app.pem"     # installation authentication
  webhook_secret_env = "GITHUB_WEBHOOK_SECRET"
  repos = ["acme/app", "acme/api"]
  queue_label = "awman"
  [workers]
  allow = ["github:alice", "team:acme/bots"]
  [defaults]
  leader = "claude::claude-opus-5-5"
  interaction = "interactive"
  ```
- **One GitHub App, two roles.** The maintainer installs one GitHub App on the configured repositories and enables its device flow. Its `client_id` identifies the app during worker-link sign-in; its `app_id` and private key mint repository-scoped installation tokens for issue/PR operations and lease Git credentials. The GitHub user token from sign-in only proves the approving person's identity and is never given to a worker as a WSP credential. The optional viewer page uses the same app's web sign-in flow with `client_secret_env`; viewer sessions are separate from worker links. A personal access token or a second OAuth App is not part of this reference setup.
- **Worker-link sign-in and authorization.** Implement 0143's WSP endpoints in the example:
  1. On unauthenticated `POST /v1/link`, start GitHub's device flow using `client_id`. Keep GitHub's `device_code` server-side and return its `user_code` and `verification_uri` plus an independent, unpredictable `link_id` and `interval_ms` to awman. The person opens GitHub's verification page, signs in and approves the app; awman only displays the code and polls this server at `POST /v1/link/{link_id}/token`.
  2. The server polls GitHub at its required interval, honors `slow_down`, denial and expiry, then uses the GitHub user token to fetch the authenticated user. Check that user's permission under `[workers].allow` before approving the link. Derive `owner=github:<login>` from GitHub's authenticated response, never from a worker-provided name or label. Discard the GitHub user token after the identity and authorization checks; do not store it in the worker record.
  3. Treat `link_id` as a short-lived secret that may be polled but may redeem a worker credential only once. Do not return a worker credential before GitHub approval and the allowlist check. On success, atomically create the worker record, consume the link and return an opaque, server-issued WSP refresh token through 0143's link-token response. Expired, denied, consumed and unknown links cannot issue credentials; redact link IDs, device codes and tokens from logs.
- **Server-to-worker authentication.** Store only hashes of high-entropy refresh and access tokens in the example's SQLite database, bound to a worker record and the GitHub user ID; support server-side revocation. `POST /v1/token {refresh_token}` validates an active worker link and issues a short-lived, opaque WSP access token. Every authenticated WSP route validates `Authorization: Bearer <access token>` and resolves the worker identity and server-assigned labels from server state, not request fields. Revocation blocks refresh immediately and invalidates outstanding access tokens; expired access tokens require refresh. Use HTTPS for the public URL. The worker stores its refresh token in 0143's private token file, never in `workServers` config. GitHub user, GitHub installation and WSP tokens have distinct purposes and are never interchangeable. If an operator supplies 0143's `--token` option, accept only a separately provisioned WSP refresh token, never a GitHub token.
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
  - `awman:needs/<key>` becomes required `<key> Exists`. Keys are WSP label keys, so awman's reported labels are written with their `awman/` prefix (0139), for example `awman:needs/awman/agent.codex=true`.
  - `awman:needs/<key>=<value>` becomes required `<key> In [value]`.
  - `awman:prefer/<key>=<value>` becomes preferred with weight 50.
  - `priority:high`, `priority:low` and the `P0`–`P3` labels set priority. Ties are ordered by issue creation time.
  - `awman:interactive` and `awman:headless` set `exec.interaction` (0139's values).
  - `awman:leader/<agent>::<model>` overrides the leader.
- **Assignees as affinity.** The server-assigned `owner=github:<login>` label comes from the authenticated GitHub account used during linking. An issue with assignees becomes required `owner In [github:<login>…]`, so it is offered only to its assignees' linked machines. Unassigned issues are open to any worker the maintainer has allowlisted under `[workers].allow`.
- **Reporting back.**
  - On claim: add `awman:running`, and post one status comment, edited in place afterwards, that says which worker took the item.
  - On heartbeat: update the comment with the current step.
  - On a successful result: open a PR from `awman/issue-<n>` with "Closes #n", using the app's credentials; mark the issue `awman:done` and link the PR in the comment.
  - On failure: mark it `awman:failed`, with the error stage and message and the failed steps. Removing `awman:failed` re-queues it.
  - On release: remove `awman:running`.
- **Cancel.** Removing `queue_label` or closing the issue while a lease is running returns `cancel: true` on the next heartbeat.
- **Viewers.** It accepts the lease terminal WebSocket (0143, `awman.terminal.v1`) and serves a minimal read-only page per running item (`/items/<id>`) with xterm.js, for maintainers who sign in through the GitHub App's separate web flow. Viewer sessions are not worker credentials. It never grants remote input.
- **Workflow progress.** Persist 0143's `workflow_state` snapshots from the WSP event stream with their `(attempt, sequence)` ordering. The item page can show planned phases and steps, including pending ones, from the latest snapshot. Match requested work to `register.features`; do not offer a worker a capability it did not advertise. WI0165 adds optional workflow context attachments to this server later.

## Edge Case Considerations:
- Fork PRs and untrusted issue authors: only issues opened or labelled by people with write access are queued. The `queue_label` is applied by a maintainer, and the event's actor permission is checked, so an outsider cannot put work into the queue.
- An issue edited while running: the lease keeps the body it was claimed with, and the status comment notes that the issue changed.
- The branch already exists, from a previous failed attempt: use it as the branch and `base_ref` as the base, with no force-push. Overwriting it needs a maintainer's `awman:retry-fresh` label.
- Rate limits: cache issue reads, use conditional requests, and back off on secondary limits.

## Test Considerations:
- Pass the `awman-wsp` conformance suite.
- Build and test the example through its own manifest. Check that the normal awman build, CLI help, install packages and release artifacts contain no GitHub work server executable or command.
- Label-to-affinity mapping table tests. Assignee affinity: an assigned issue is not listed for another owner's worker.
- Webhook signature verification, the missed-webhook reconcile, and permission checks for who may queue.
- Mock the GitHub device endpoints and user lookup: approved allowlisted user, denied user, expired code, `slow_down`, and a second poll of a consumed link. Verify that an unapproved link, guessed or replayed link ID, revoked refresh token, expired access token, worker-supplied owner label, and GitHub token used as a WSP bearer token cannot authenticate. Check token values never enter config or logs.
- End to end with a mocked GitHub API, a local bare repo standing in for the clone URL, and a real worker: queued, claimed, pushed, PR opened, labels updated.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Keep all GitHub-specific code, configuration and persistence in `examples/github-issues-work-server/`. Organize its modules so wire data and SQLite persistence, GitHub HTTP/WSP transport, queue and lease policy, and HTTP routes/viewer rendering have clear boundaries consistent with `aspec/architecture/2026-grand-architecture.md`. The executable entry point only wires these parts together. Do not add `work-server github` to awman's command catalogue, import awman's internal `GithubIssueSource`, or add a GitHub-server module under `src/`. The only shared code with awman is the generic public `awman-wsp` protocol and conformance suite; put any missing generic protocol behavior there rather than in awman's application code.

## Documentation
- New `docs/18-github-work-server.md`: where to find the reference source, how to clone and build its separate executable, and how to set up one GitHub App with device flow, repository permissions, installation credentials, webhook secret and viewer web sign-in; explain WSP worker tokens, allowlisted links, labels, assignees and TLS deployment. Link it from `docs/17-workers-and-work-servers.md` as reference code, without implying it ships with awman.
