# Client gaps in awman's daemon plan (November 2026 draft)

**For:** an awman agent amending awman's active work items and drafting new ones.
**Checked against:** awman `main` at `a12db80a` ("re-work WI 139 onwards", 2026-11-05).

Active work-item references below use the current numbering; the code observations remain dated to the checked commit. B1, B2 and per-session secrets are now in 0145; B3 is in 0146.
**From:** infact. infact is an API and work-server client of awman, and it tracks `main` with no fallback for older builds.

## How to use this document

- **Part A** asks for amendments to items that are already active. Each one is a gap that any push client or work server would hit, not only infact.
- **Part B** records three gaps originally proposed as separate work items. Their current homes are noted above; the summaries preserve the original client evidence and requested outcomes.
- **Part C** lists smaller corrections.

Ground rules:

- **Keep awman generic.** infact is named only as the client that found each problem. Don't add fields, routes or behaviour that make sense only for infact. Every addition should be useful to any API client, work server or the GitHub reference server (0153).
- **Follow 0139.** Use its naming rules, route ownership, `{items, next_cursor}` lists, `{error, code}` errors and `_ms`/`_at` fields.
- **No compatibility layers.** Clients like infact will move to the new surface, so don't add aliases for their sake.
- **Prefer extending existing items** when a gap fits inside one (most of Part A).
- **Respect the gate.** Nothing here should reopen the 0139–0143 gate's scope beyond what it needs to be internally consistent.

---

## Part A: amendments to active items

### A1. API capability flags no longer have a home (0139, 0140)

**Problem.** Today `GET /v1/status` returns `features: [string]`, a list of API capability flags from `API_FEATURES` in `src/frontend/api/routes.rs:248`, such as `workflow_state`, `runs`, `structured_results` and `outputs`. Clients check this list to refuse builds that are too old. awman's own rule is that every new route, field or event adds an entry.

0139 §5 and 0140 reuse `features` in `/v1/status` for daemon features (`squad`, `worker`, `engine`) and their state. No item says where the capability flags go. As a result:

- every client that checks capabilities breaks;
- the "advertise each addition" rule has nowhere to live;
- none of the later items (0146 context, 0145 credentials, 0147 inline work items, 0152 read-only, 0148 init) names the flag a client should check.

**Ask.**

- Keep capability flags as their own field in `/v1/status`, separate from daemon-feature state (for example `capabilities: [string]`), and keep the "every addition adds a flag" rule.
- Say whether the flags shipped so far stay as they are or are folded into a baseline protocol version. Either is fine, as long as it is stated.
- Have each later item name its flag in its Implementation Details.
- Use the same vocabulary for the WSP `register` `features` list (0143), so a work server and a push client test the same names.
- The earlier request to advertise `remote_dynamic`, `workdir_roots` and `event_ids` is covered by whatever baseline you choose. Those three are still missing from `API_FEATURES`.

### A2. Pull leases depend on items scheduled after the gate (0143 vs 0145, 0147, 0152)

**Problem.** 0143 closes the daemon gate, but its lease runner cites features that are specified in later items:

- **Credentials:** the lease clones "through a one-shot credential helper, never in the URL or argv (0145)".
- **Work item body:** `body` is passed "as the 0147 inline source, so no file is committed".
- **Read-only:** `exec.read_only` refers to 0152.

All three extensions follow 0143. As written, either the gate closes with WSP fields that do nothing, or 0143 quietly implements parts of three later items.

Every work server needs these:

- expiring lease credentials, kept out of argv and `.git/config`;
- work items that are not files in the repository;
- review or planning jobs that must not modify the workspace.

**Ask.** Choose one of these and write it into the items:

- **(a)** 0143 implements the lease-side minimum of each: the credential helper for lease clones and pushes, an inline work item for leases, and a read-only workspace for leases. 0145, 0147 and 0152 then extend each one to push sessions and the CLI.
- **(b)** Move those parts of 0145, 0147 and 0152 inside the gate.

With either choice, a lease field that a worker cannot honour must be refused, not ignored. Use `release` with reason `ineligible`, or leave the matching capability out of `register` `features` so the server never offers the item.

### A3. Workflow structure is not in the WSP event stream (0143)

**Problem.**

- Push clients get workflow structure from `GET /v1/worker/commands/{id}/workflow`, which is backed by `workflow.state.json`: phases, planned steps, step status and `invocation_id`.
- A work server cannot call the worker, and WSP only carries `ExecutionEvent`s.
- So a work server cannot show a dynamic workflow's plan. It can only show steps once they start, and it cannot tell planned steps from finished ones.
- The GitHub reference server (0153) would hit the same limit if it showed progress.

**Ask.** Put the workflow state into the event stream:

- Add an execution event carrying a workflow-state snapshot, with the same shape as the `/workflow` route.
- Emit it whenever the state is persisted.
- Give it the same `sequence`/`attempt` envelope as other events, so it arrives in order through both `GET /v1/worker/commands/{id}/events` and `POST /v1/leases/{id}/events`.

Push SSE followers benefit too: they stop polling `/workflow`. Give the event a capability flag (A1).

### A4. Context does not reach pulled jobs (0146, 0143)

**Problem.**

- 0146 makes `context(repo)` addressable by `context_key`, but only through the daemon's own API (`/v1/contexts/{key}`, push sessions).
- A work server never reaches the worker's API. WSP's `WorkItem` has no `context_key`, no way to send context files in, and no way to return context changes.
- And because the store lives on one host, a client spreading jobs over several workers would see a different context on each one.

So shared context (memory carried across a client's runs) only works for push clients on one host.

**Ask.** Add a pulled-job form of 0146, and keep it generic. One shape that would work:

- The `WorkItem` carries an optional `context: {files: [{path, content_base64 | url}]}`. The worker mounts it as the run's `context(repo)`.
- The lease result carries `context_changes`, with the same shape as `GET /v1/worker/commands/{id}/context_changes` and the changed files inlined up to a size cap (like `outputs.max_bytes`).

The server stays the owner of the context, and the worker stays stateless across leases. Give it a capability flag. B3 covers the related problem of workflow context files.

### A5. Label syntax and existing command labels (0139 §1, 0135)

0139 §1 says labels and annotations are `<namespace>/<name>`, lowercase `[a-z0-9.-]`. Command labels from 0135 (`labels` on sessions and commands) already exist, and clients use dotted keys such as `infact.kind`.

**Ask.** State whether the rule applies to command labels, and what happens to existing keys that break it: refused at submit, or only warned. A clean break is fine, but write it down so clients know to move to `infact/kind`-style keys.

### A6. 0145's credential helper and the git transport builder (0145)

Our draft of per-session credentials said the one-shot credential helper belongs inside one shared host-side git transport command builder. awman's 0145 dropped that sentence, because the builder came from a proposal that awman has not filed (B1).

**Ask.** Once B1 has an item, 0145, and 0143's lease clone and push, should add the helper through that one builder. Then a run's clone, setup fetches, `ls-remote` and push always present the same credential.

---

## Part B: gaps originally proposed as new work items

These came from real failures seen by a client. Don't copy our original drafts' wording or their exact choices; they were written from one client's point of view. The summaries below state the problem and what clients need.

### B1. Bug: one remote run uses different git credentials for clone and push-back

**Evidence on `main`.**

- Host clones and setup go through `src/engine/git/host_command.rs`, which sets `GIT_CONFIG_NOSYSTEM=1` (line 21).
- Push-back transport in `src/engine/git/remote_push.rs` sets `GIT_CONFIG_NOSYSTEM` only when `!transport` (around line 282). The `ls-remote` and `push` that publish a result therefore read the system gitconfig, but the clone did not.

**Effect.** Git asks credential helpers in config order. macOS Git (Xcode CLT, Homebrew) sets `credential.helper=osxkeychain` in the system config, so push-back can present a stale keychain token for the same host when the clone used the operator's configured credential.

A run whose every step succeeded then ends in `error` at stage `push` with `Authentication failed`. We reproduced it every time with a stale keychain entry.

**Outcome needed.**

- Every host-side transport command in one session (clone, fetch, merge-ref `ls-remote`, push-back `ls-remote` and `push`) uses the same git configuration and the same credentials. Build them from one place, so a later per-session credential helper (0145) or lease credential (0143) is added once.
- Pick one policy and document it in `security.md` and `docs/09`. "No system gitconfig anywhere", which matches clones today, breaks nothing that works now.
- An authentication failure during push-back has its own `failure.stage` or `code`, distinct from other push failures, and its message names the operation and remote but never a credential. 0145 already plans `git_auth` for expired per-session tokens; use one stage or code for both.

**Scope.** Push-back in push sessions today. The same builder must serve worker lease clones and pushes (0143).

**Tests to consider.** A regression with a system config whose helper returns a stale password and a global config that holds the right one, against a local smart-HTTP server. With that setup, clone, `ls-remote` and push all succeed.

### B2. Bug: push-back gives up on a remote that is briefly unreachable

**Evidence on `main`.**

- `push_after_success` (`src/command/commands/api_server/push_back.rs:73`) makes one transport attempt.
- Any failure becomes `stage: "push"`, `code: "push_failed"` (`PushBackError::code`).
- With `close_on_finish`, the clone holding the result commit is then deleted.

**Effect.** A Git server restart or network blip during the few seconds of push-back throws away a run whose every step succeeded, for example a ten-minute dynamic workflow. The client has to run all the agents again.

The client also cannot tell "could not reach the remote" from "the remote refused the push", so it cannot safely retry one and escalate the other. Clients currently match git's message text.

**Outcome needed.**

- Push-back retries failures that never reached the remote (connection refused, DNS, timeouts, resets) with a short, bounded backoff measured in seconds, before the session closes. It never retries an answer from the remote: authentication, hook rejection or non-fast-forward.
- Each retry is visible in the command's event stream.
- When retries run out, the result uses a distinct code (for example `push_unreachable`), separate from a refused push and from B1's auth failure.
- A retry that finds the branch already at `head` reports it as pushed. Cancelling during backoff reports `cancelled`.
- The same behaviour applies to worker lease pushes (0143). A worker pushing to a work server's Git host meets the same restarts. For leases, the retry window must stay within the lease's heartbeat and expiry.

Give the new code a capability flag (A1).

### B3. Feature: a run's workflow context files can be read after the run

**Evidence on `main`.**

- Each workflow mounts `context(workflow)` at `/awman/context/workflow`, stored at `<home>/context/workflows/<invocation_id>/`.
- Steps are told to leave notes there, and the final summary often cites them by path.
- No route serves the directory. `/workflow` returns the `invocation_id` but not the files. In worker mode, nothing returns them at all.

**Effect.** Anything that reviews or reports on a finished run is handed a summary that points at files it cannot open. That includes a second agent, a person in a web UI, or a work server posting a comment on an issue. The only workaround is to share a filesystem with the awman host, which rules out remote hosts and workers.

**Outcome needed.**

- **Push:** read-only listing and download of a command's workflow context, for as long as the command record exists. For example `GET /v1/worker/commands/{id}/context` and `…/context/files/{path}`, in 0146's list shape with `size_bytes`, `sha256` and `modified_at`.
  - Validate paths and don't follow symlinks.
  - Return an empty list, not 404, for commands without a workflow.
  - Serve the current attempt's invocation.
  - Survive `close_on_finish`.
- **Pull:** the lease result can carry the workflow context, inlined up to a size cap like `outputs`, or alongside A4's `context_changes`.
- **Retention:** document how long the directory is kept.

This is related to, but separate from, 0146. 0146 shares a long-lived `context(repo)` between runs. This item exposes one run's scratch notes after the run. Consider writing them as one item with A4 if that reads better. Give it a capability flag (A1).

---

## Part C: small corrections

- **0143 Summary:** "infact (its work item 0023) is one implementation". infact's work-server item is 0019. Better still, drop the number and say "infact is one implementation".
- **Whether `/awman/output` is mounted.** `outputs` is advertised on every runtime, but nothing is mounted on sandbox runtimes, so an agent's answer silently never arrives. Report per command whether outputs were mounted, for example in `result`, or advertise it per runtime. This matters more once the WSP `register` `features` includes `outputs`.
- **A failure code for `PrerequisiteWideningRefused`.** It shares the class code `PREFLIGHT` with about 25 other errors (`src/command/error.rs:606`), so clients match its message text. Give it its own `code` in 0139's single code table.
- **`docs/09-api-and-remote-mode.md`:**
  - `StepActivity` is shown flat (around line 1259), but the wire form is `{type, data: {kind, summary}}`.
  - A setup failure's `error` is documented as a message, but the code writes the literal `"setup_error"`.
  - Fold both fixes into 0139's docs rewrite.
- **Upgrading in-flight records (0139 edge cases).** IDs gain type prefixes and the store migrates them. Say what a client holding pre-upgrade IDs gets after the upgrade (404, or the old ID still accepted for lookup), so clients can reconcile runs that were in flight across the upgrade.
- **Upgrade table in the docs.** 0139 promises a one-time "upgrading" table for config keys. Add the same table for routes and fields (old path → new path, `/logs` → `/events`, removed `/status` aliases, `non_interactive` → `headless`), so every client migrates from one list.

## Out of scope

infact will absorb these on its side, so please don't change awman for them:

- the route moves under `/v1/worker/*`;
- the TLS-only network listener;
- `AWMAN_HOME`;
- removing `awman api start`;
- the WSP renames to `_ms`, `{error, code}`, `awman/` labels and `headless`.
