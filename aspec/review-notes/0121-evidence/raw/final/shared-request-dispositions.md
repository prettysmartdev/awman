
## Final-verification dispositions — 2026-09-28

This append supersedes OPEN labels only to the extent stated below. A FAIL or
BLOCKED disposition is an explicitly unresolved mandatory requirement, not a
closure or a rejected finding. Details and executed evidence are in
`aspec/review-notes/0121-evidence-register.md` and its `raw/final/` bundle.

| Requests | Disposition |
|---|---|
| REQ-plan-001 | PARTIAL: registered every supplied acquisition/lifecycle/network module. sdk_worker/distribution coverage resides in binary.rs. guest_compat and broader cache test scenarios were not supplied: FAIL D-04/D-09. |
| REQ-plan-002, REQ-impl-network-resources-002/-003 | Wiring APPLIED in both backend branches and SDK adapter; SDK serialization test executes. Enforcement is FAIL D-05 (HTTPS/UDP defects), not completed by wiring. |
| REQ-plan-003, REQ-impl-sdk-worker-001 | APPLIED: pinned microsandbox vendor, one-method isolated builder patch, Cargo path/lock, license/PATCH and WI 0120 inventory. Hostile-config feature test executes. Native coexistence BLOCKED. |
| REQ-plan-004 | FAIL: backend late lease does not cover acquisition return; installed-image use lease/stress missing. |
| REQ-plan-005 | PARTIAL: removed broad Docker skip; ordinary proxy/CODEX_HOME/Docker config/endpoint scrub regression executes. Snapshot fallback and complete disposable external matrices remain FAIL; native/services BLOCKED. |
| REQ-plan-006 | FAIL D-04/D-15: no expanded guest strategy implementation or concrete fixture handoff; cannot resolve as unnecessary. Original fixture identities retained. |
| REQ-impl-sdk-worker-002 | BLOCKED: final-artifact test exists; no optimized/stripped/signed artifact or post-sign hashes. |
| REQ-acquisition-001 | FAIL: requested tls_docker.rs and required-client-certificate scenarios are absent; direct Rustls dependency subsequently added for the feature TLS fixture failure; required scenarios still missing. |
| REQ-impl-network-resources-001 | APPLIED: pure compiler and literals already present; registered network_resources. Default and feature tests compile. |
| REQ-impl-network-resources-004 | APPLIED by final verification: network default added to builtin_hw_driver. Feature Clippy exposed this missing field and passes after the fix. |
| REQ-impl-network-resources-005 | APPLIED: Cargo example, --examples build and native CI gates/signing present. Native execution BLOCKED. |
| REQ-impl-network-resources-006 (including amendment) | APPLIED ordinary scrub; network/pressure/allowed/denied/report variables remain passed through. Real registry proxy fixture must be explicitly injected; missing service enforcement remains FAIL. |
| REQ-impl-build-tooling-001 | PARTIAL: actual-awman smoke registered and stale native selector corrected. Native boot/writable x86 RAM/final-artifact traces BLOCKED. |
| REQ-impl-network-resources-007, REQ-impl-lifecycle-001, REQ-fix-codex-area-001 | APPLIED: multi-module naming/gate check; unused accessor removed. Default tests and Clippy run without warning suppression. |
| REQ-acquisition-002, REQ-impl-lifecycle-002 | APPLIED: modules registered and discovered; native execution BLOCKED. |
| REQ-impl-lifecycle-003 | FAIL: actual TUI/API/workflow/squad/crash/upgrade scenarios not implemented. Old/new artifacts and native runs also BLOCKED. |
| REQ-fix-codex-area-002 | APPLIED: Layer 0 host_var_os, non-UTF-8/overlay regression, embedded caller; architecture lint passes. |

The missing `review-codex-work.md` and `fixes-claude-area.md` prevent a complete
paired review/fix audit. Neither is invented or marked resolved.

### Final feature execution addendum

REQ-acquisition-001: the direct Rustls dev dependency is now APPLIED for a
concrete feature-tier failure: TLS fixtures must select a provider explicitly
when the SDK enables a second provider. The lock diff adds only that existing
crate edge, no version bump. The full builtin test command passes after this
fix. Required mTLS scenarios/tls_docker.rs remain absent (FAIL); adding the
dependency does not close that portion of the request.

### Final registry opt-in preservation

Both `AWMAN_TEST_IMAGE_STORES` and `AWMAN_TEST_REGISTRY` must be true to
preserve caller-supplied disposable registry proxy transport. Ordinary runs
and either gate alone scrub it. The new regression verifies all four cases
without network traffic. `CODEX_HOME` and ordinary Docker credentials remain
scrubbed. The final pre-push was rerun and exited 0 after this adjustment.
