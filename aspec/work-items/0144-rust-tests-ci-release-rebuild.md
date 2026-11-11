# Work Item 0144: Rust tests, CI and release rebuild

Status: implementation and certification in progress; native acceptance deliberately deferred.

## Objective

Replace the pre-WI144 test system with holistic coverage of important real-world
paths. Improve performance, flakiness and developer productivity by removing
duplicate builds and harnesses, isolating resources, making capability ownership
explicit and binding results to the code and artifacts actually tested.

Acceptance is not a one-to-one mapping of old tests. Review important user
journeys, state transitions, security boundaries and failure recovery. Do not
block retirement for inconsequential edge cases or use historical test counts as
a coverage target. Deleted tests remain available only in Git history.

## System contract

`ci/gates.toml` is the single suite/build/platform/gate contract. Generated
workflows and nextest configuration must match it. The supported developer entry
points are `make check`, `make test`, `make test-boundary`, `make test-guest`,
`make test-artifact`, `make pre-push`, and `make validate-native`.

- Pure contracts run in the unit tier. Component tests exercise production
  interfaces with private state, controlled clocks and owned fake boundaries.
- Boundary tests check those assumptions against real named services. Guest
  tests require actual virtualization. Artifact tests exercise the exact signed
  candidate bytes, including offline behavior and previous-state compatibility.
- TestSandbox owns processes, ports and private configuration. No test may
  invoke an agent directly on the host or depend on the developer's real HOME.
- Required tests cannot silently skip. Expected compiled inventories are
  reconciled with actual passing results. The Actions controller independently
  authenticates runs, attempts, jobs, artifacts and source inputs.
- `make release VERSION=vX.Y.Z` uses the build/validate/promote pipeline. No
  fallback tag publisher exists. Candidate identity, evidence, byte digests,
  protected policy and promotion authority remain mandatory.

Coverage decisions by slice live in
[the coverage ledger](../devops/replacement-ledger.md). Authoring and operation
instructions live in [testing](../devops/testing.md), [local development](../devops/localdev.md),
[CI](../devops/cicd.md), and [release](../devops/release.md).

## Independent certification evidence

The versioned [validation report](../review-notes/0144-independent-validation.json) includes selected compiled test identities and SHA-256-linked JUnit files. The independent local comparison found exact agreement: **606 tests in 26 suite invocations passed, with no failures, errors or skipped test results** (shuffle seed 145). Observed wall time was approximately 102 seconds, with 63.404 seconds in test execution, on Linux aarch64. The report explicitly records the uncommitted-source limitation; hosted CI certifies the committed revision separately.

Workspace/all-targets compilation, Clippy with warnings denied, manifest discovery, generated-file validation, the important-path matrix, architecture lint and shell syntax checks passed. The host lacks shellcheck; CI must perform that check. The rebuilt release contract has 36 passing tests. The native packet verifier has two passing local contracts; neither is native runtime proof.

Hosted CI evidence is recorded in [PR40 checks](https://github.com/prettysmartdev/awman/pull/40/checks). It is pending the final push at this revision of the work-item record. Do not mark the repository portion certified until the committed gate passes.

No test-count equivalence claim is made. The earlier 5,903-test mixed-layout run is historical and does not count as post-retirement evidence. The recorded local measurement does not establish p95 or a long-term flake rate.

## Final native validation handoff

The maintainer owns an Apple Silicon Mac that the agent cannot access. Leave
exactly this native acceptance item unverified after repository certification:

- [ ] **Native Apple Silicon acceptance:** on the reviewed clean revision, run
  `make validate-native` and return the printed `target/native-validation/*.tar.gz`
  evidence packet, its SHA-256, and confirmation that the run used the maintainer's
  native Mac. A future agent independently reviews that packet before closing
  this item and marking WI144 complete.

Prerequisites and the full receiving-agent procedure are in
[testing.md](../devops/testing.md#apple-silicon-acceptance-packet). The packet
contains source/input hashes, fixture provenance and bytes, expected compiled
inventories, actual JUnit and process exit codes, command logs/timings, signature
verification and the tested candidate. A failed run still emits diagnostic
evidence when the target can start.

The receiving agent must verify the archive digest and safe extraction, inspect
the exact committed implementation and important assertions, run
`cargo xtask native-validation verify <extracted-evidence-directory>` from that
clean commit, and review actual native provenance and coverage. Do not execute
received binaries. Missing/failed/skipped tests, changed candidate bytes or
mismatched source inputs leave the item open. Record the packet hash, commit,
candidate/fixture digests, test results and observed timing before checking it.
One run does not establish p95 or a long-term flake rate.

The fixture archive is deliberately locally pinned for acceptance. Publishing it
for CI requires the reviewed Fixtures flow and a committed global digest; the
local packet does not alter that policy or bypass release admission. Production
publication requires its own authenticated gate and approved environment.
