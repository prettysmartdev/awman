# Work Item: Task

Title: webapp v0 acceptance follow-ups
Issue: issuelink

Depends on: 0159 (webapp v0) and 0160 (webapp UX overhaul) merged, and a real Cloudflare deployment of the web app at https://awman.prettysmart.dev for the deployed-origin gates. This item collects the work that 0159's final acceptance audit deferred honestly: items that need the deployed origin, Docker-backed agents or a new harness, plus small refinements found during the audit. It changes no product behavior unless a deferred gate exposes a defect.

## Summary:
- Prove, rather than infer, the parts of 0159 that were accepted on unit and localhost evidence: run the matrix as a real-daemon TUI-versus-web engine-trace parity suite, verify loopback access from the deployed HTTPS origin on Chromium and one other engine, drive a real agent PTY from a browser, exercise the installed service worker's offline and atomic update lifecycle, and record comparable web performance baselines.
- Close the audit's small refinements: two unverified squad matrix rows, a cancellation-latency measurement for browser sockets during session start and blocked sends, and the owner-side deployment checklist steps that only a deployment can confirm.
- Preserve the browser security boundary established by 0159 and explicitly revised for reload continuity in 0160. Nothing here widens listener binds, adds LAN or relay access, or weakens the CSP. Remote-machine browser access remains a separate, later design and review.

## User Stories

### User Story 1:
As a: maintainer releasing the web app

I want to:
see the acceptance matrix executed against a real daemon with identical scripted actions from the TUI and the browser, with their engine traces compared

So I can:
ship web changes knowing browser behavior still matches the TUI instead of trusting component tests.

### User Story 2:
As a: user opening https://awman.prettysmart.dev

I want to:
know that the loopback permission prompt, browser linking, service-worker install, offline reopen and shell update were verified from the real hosted origin on my browser engine

So I can:
trust the install and offline guidance in the user guide.

### User Story 3:
As a: privacy-conscious user

I want to:
the deployed Worker's counting, logging posture and zone settings confirmed against the live Cloudflare account, not only in tests

So I can:
rely on the documented aggregate-only usage counts.

## Implementation Details:

### 1. Engine-trace parity harness (deferred from 0159 §1 and Test Considerations)
- Add a Rust-side trace capture for instances: a test-only hook or owner-only `awman daemon api` action that records, per instance, the ordered engine-side events already modelled in `src/command/instance/trace.rs` (submitted commands, state transitions, dialog requests and answers with their outcomes, terminal ownership changes, quit/cancel reasons). Expose it only on the owner socket and only in test builds or behind an explicit test-mode setting; never on network listeners.
- Build a parity runner under `web/acceptance/` that reads `web/acceptance/action-matrix.json`, drives each `parity: engine_trace` row once through the daemon-backed TUI (existing PTY harness in `tests/`) and once through the browser (Playwright against the exported app), and diffs the two traces after normalising ids and timestamps. Rows with `parity: wire_frame` assert the exact outbound frame instead. `view_only` and `web_only` rows keep their component tests.
- Docker-backed rows (workflow steps, parallel containers, squad attach, agent terminals) run under the existing `AWMAN_TEST_DOCKER=1` gate used by `make test-full`; they must not run in `make test-fast` or `make test-web`.
- Report per-row PASS/FAIL into a generated artifact next to `action-matrix.md`; fail the run when any `required` row is missing from the report. The 64 rows the 0159 audit listed as "implemented and unit-tested, not trace-proven" are the first targets.

### 2. Deployed-origin browser verification (0159 Edge Cases, Test Considerations)
- From the real `https://awman.prettysmart.dev` origin on the same machine as a daemon, verify on Chromium and Firefox (and Safari on macOS where available): the Local Network Access permission prompt from the `Allow local access` gesture, `fetch` with `targetAddressSpace: "loopback"` and the WebSocket upgrade to `ws://127.0.0.1:9877`, denied and revoked permission states, the `Connect browser` link flow with `awman link web-<code>`, and the unsupported-browser error when a browser blocks the transport. Do not change the bind address or weaken authentication if a browser blocks it; record the exact browser version and failure.
- Verify the installed PWA: install after first load, disconnect Cloudflare (hosts-file block or airplane mode) while the daemon stays reachable, reopen the installed shell and work normally, then restore Cloudflare, deploy a new build and confirm the `Update available` flow replaces the shell atomically in every open page. Confirm that first-time offline installation fails with the documented explanation.
- Inspect browser storage after a full session (Cache Storage, IndexedDB, localStorage, sessionStorage, cookies, service-worker cache contents): no grant, polling secret, ticket, snapshot, terminal bytes or command text may be present.
- Record results per browser in the work item or `web/acceptance/baselines.json`, not in `docs/`.

### 3. Live agent terminal in a browser (0159 §3.4)
- With Docker available, start a real agent session from the browser and verify against a live PTY: colors and cursor, alternate screen, mouse reporting, paste (bracketed, chunked above 64 KiB, refused above 1 MiB), resize only while holding control, read-only and take-control states, parallel terminals, reconnect snapshot after a forced socket close, and the `Lagged` path. Compare the rendered screen with the TUI's `vt100` view of the same PTY at the same size.
- Confirm the WASM core loads from the self-hosted asset under the production CSP and from the offline service-worker cache.

### 4. Offline and update e2e with a real service worker (0159 §4.1)
- Add a Playwright project that allows service workers (currently blocked to keep secrets out of traces) and runs only the offline/update specs against the exported shell: install, serve a second build, approve the update, assert exactly one reload per open page and no stale chunk requests. Keep traces, video and screenshots off and never pair a browser in that project.

### 5. Web performance baselines (0159 Test Considerations)
- Measure in a browser against a real daemon: frame latency (submit to first output), terminal throughput for 4 KiB frames, reconnect-to-snapshot time, and narrow-viewport memory after a long session. Record them in `web/acceptance/baselines.json` under `web` with the same sample sizes as the native baseline, set `release_ready` accordingly, and add a regression check that fails release above `release_regression_limit_percent`.

### 6. Browser-socket cancellation latency (daemon review observation)
- Measure how long a revoked or expired grant keeps a frontend or terminal socket alive while the task is inside session startup or a blocked send, and bound it. Prefer making the `select!` loops in `src/frontend/api/frontend_socket.rs` and `src/frontend/api/terminals.rs` cancellation-safe around the awaited handler and send paths rather than adding timeouts. Add a regression that revokes during a slow start and during a stalled reader and asserts a close within a stated bound.

### 7. Matrix refinements and deployment checklist
- Verify and add the two matrix rows the 0159 audit left out: `squad.attach_interrupt` (Ctrl+C in an attached squad terminal cancels the attach command rather than sending SIGINT to the agent) and `squad.copy_selection` (Ctrl+Y in the squad list). Implement or correct the browser behavior if it differs, then regenerate `action-matrix.md` with `node web/acceptance/render-matrix.mjs`.
- Confirm on the live Cloudflare account: Pseudo IPv4 off, no zone or account logging or Logpush, no `wrangler tail` on production, "Always Use HTTPS" on, secrets supplied as secret bindings, and the owner export reachable only over HTTPS. Record the check date in the work item; keep the user-facing privacy statement in `docs/20-webapp.md` unchanged unless a check fails.

## Edge Case Considerations:
- Trace comparison must tolerate legitimate differences (ids, timestamps, frontend kind in `Hello`, the web 60 s reconnect grace versus the TUI's 2 s) without hiding real divergence; list every normalisation explicitly in the runner.
- A browser that blocks loopback from a public HTTPS origin is a release blocker for that browser only; document it as unsupported rather than changing the daemon.
- Service-worker-enabled runs must never pair a browser or hold a grant; a leaked grant in a cache would be a test defect, not a product one.
- Docker-backed parity rows must skip cleanly, with a visible reason, when Docker is absent; they must not be reported as passed.

## Test Considerations:
- The parity runner is the acceptance suite for this item: every `required` row produces a PASS, FAIL or SKIP(reason) entry; `make test-web` keeps running the non-Docker rows, `make test-full` runs the rest.
- Deployed-origin checks are recorded per browser engine and version with the daemon build hash; they are manual or scheduled against the deployment, not part of `make test-fast`.
- Cancellation-latency regressions run in `make test-fast` with clock injection, not wall-clock sleeps.
- Baseline regression check fails the release script when `web` values regress by more than the configured percentage against the recorded baseline.

## Codebase Integration:
- Follow `aspec/architecture/2026-grand-architecture.md`: trace capture lives with Layer 2 instance policy; the parity runner and browser harness stay under `web/acceptance/` and `web/e2e/`; no web-only business operations are added to the daemon.
- Reuse the existing `web/e2e` daemon helper, fixtures and `web/acceptance/render-matrix.mjs`; extend the `web` CI job in `.github/workflows/test.yml` for new non-Docker gates and `test-full` for Docker-backed rows.
- Keep all dependencies exactly pinned and installs frozen, as in 0159.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- Update `docs/20-webapp.md` only where a deployed-origin check changes supported browsers, the install or offline guidance, or the troubleshooting steps; list the verified browser engines plainly.
- Update `docs/16-daemon.md` and `docs/04-security-and-isolation.md` only if the cancellation-latency work changes observable revocation behavior.
- Keep trace formats, harness design, baseline numbers and deployment checklists in this work item, `web/acceptance/` or code comments, never in published user docs.
