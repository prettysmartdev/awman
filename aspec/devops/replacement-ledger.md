# Test system coverage and certification

WI144 replaces the pre-WI144 tests and harnesses with a holistic contract system.
The unit of review is a user journey or failure boundary, not an old test name.
Tests are retained only when independently derived on the new foundations.
Historical per-test migration spreadsheets and shadow-run instructions have been
removed; Git history is the historical record.

## Important paths

| Slice | Contracts and failure boundaries | Executable location |
|---|---|---|
| Foundation | Private state, scoped capabilities, deterministic clocks, owned children/ports, panic/deadline cleanup, no host agents | `crates/awman-test-support`, component security, runner isolation |
| Data | Configuration precedence and invalid writes, SQLite migration/persistence, session/workflow state and event ordering | `tests/component/data`, command state models |
| Engine | Real private Git histories and worktrees, cancellation and push fencing, OCI integrity/whiteouts, HTTP error mapping, workflow dependency scheduling/resume/retry/stop | `tests/component/engine`, `src/engine/workflow/contracts.rs` |
| Commands | Catalogue authorization, durable config edits, lifecycle admission/idempotency, cancellation, workflow preflight and typed failures, scaffold output | `tests/component/command` |
| Daemon and parity | Actual listeners and request routing, local/remote authentication, lifecycle teardown, shared behavior through builtin and daemon backends | `tests/component/daemon`, `tests/component/parity` |
| Frontends | CLI parsing/output, real API requests and event delivery, TUI key handling, rendering and session switching | `tests/component/frontend`, `src/frontend/tui/contracts.rs` |
| Squad and worker | Refusal before resource allocation, task leases and epochs, duplicate events, queue ownership, authenticated transport, interrupted/idempotent Git publishing | `tests/component/squad`, `tests/component/worker`, WSP state model |
| External boundaries | Docker, registry, Git transports, template builds, Apple image-store export/cache and sbx | `tests/boundary` |
| Native guest and artifact | Boot/exec/import/network, persistent upgrade, builders/strategy, signed offline candidate, dependency and byte-integrity checks | `tests/guest`, `tests/artifact`; final native packet required |
| CI and release | Conservative selection, exact compiled inventory/results, authenticated Actions artifacts/jobs, immutable candidate evidence, promotion refusal, operator interaction and runner repair | `xtask` unit, CI, pipeline and release contracts |

The `nextgen` reconciliation also covers remote reconnects and parallel-slot
transitions, stale dialog responses, sequential workflow request handover, and
final terminal output before exit in `src/command/instance/remote_contracts.rs`,
`tests/component/command/relay.rs`, and `tests/component/engine/terminal.rs`.
Workflow report-policy contracts preserve the distinction between a reported
error awaiting repair and a failed process; TUI contracts cover closed dialogs
and status visibility in narrow windows. These use the current private-state
contracts rather than restoring the retired harnesses.

The workflow scheduler tests prove actual state transitions through the production
engine. They do not claim that a fake executed a real agent. Real runtime behavior
belongs to the boundary/guest/artifact tiers. Catalogue help or enumeration alone
is not substantive execution coverage. Routine option combinations do not need
one test per permutation when shared behavior and important failure paths are
already exercised.

## Certification record

The authoritative current results are recorded in
[WI144](../work-items/0144-rust-tests-ci-release-rebuild.md) and its linked evidence.
During implementation, uncommitted source and local passing tests are not a
completed CI certification. The independent validation report records which
checks actually ran, their revision, results and limitations.

The maintainer's inaccessible Apple Silicon Mac is the deliberately deferred
native item. `make validate-native` produces one reviewable packet, including
failed-run evidence. Follow the receiving-agent procedure in
[testing.md](testing.md#apple-silicon-acceptance-packet). Do not mark it verified
from hosted macOS compilation, a Linux run, an earlier agent's report, or the
packet generator's unit tests.

## Continuing policy

- Add tests for a concrete risk and choose the lowest tier that proves it.
- Use TestSandbox and production interfaces; do not restore the removed
  `test-support` feature, old helper trees, raw subprocess harnesses or fallback workflows.
- Maintain every platform/build/suite declaration in the manifest. Required
  missing infrastructure fails closed; it is never a passing skip.
- Keep automated hygiene claims limited to the checks implemented. Resource
  ownership is not a security sandbox for hostile host code.
- Report actual cold/warm timing and repeat counts. p95 and flake-rate claims
  require a series of comparable measurements, not extrapolation from one run.
- Production release admission, environment approval and fixture publication
  remain enforced independently of local acceptance. A maintainer packet is
  evidence for review, not an attestation or authorization to publish.
