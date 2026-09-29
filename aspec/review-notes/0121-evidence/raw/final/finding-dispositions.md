# Final adversarial finding dispositions — 2026-09-28

Input review: `review-claude-work.md`, findings 1–16. `review-codex-work.md`
and `fixes-claude-area.md` were absent at final verification. The available
`fixes-codex-area.md` explicitly calls itself interim. No finding is rejected
on the strength of a handoff. Complete-diff review remains BLOCKED because
baseline.md records no accessible base/HEAD. Current files and local gates
are the scope of this review.

| Finding | Final disposition | Evidence / remaining work |
|---|---|---|
| 1 allowed HTTPS | FAIL | `network.rs` still sets strict with SDK TLS interception disabled. Positive allowed-HTTPS enforcement is absent. Docs/spec now expose the known defect. |
| 2 UDP/QUIC | FAIL | Domain rules still have unrestricted protocols. No equivalent hostname authority check exists for UDP. No parity exception. |
| 3 acquisition lease gap | FAIL | Acquirer still returns a pathname after unleased lookup/commit; backend acquires the lease later. Atomic handoff and installed-image use stress remain absent. |
| 4 deadline | FAIL | `run_bounded` still returns successful attempts without deadline recheck; validation/publication loops are not bounded by it. |
| 5 cancellation | FAIL | Concrete-only token is not a production caller contract; blocking header waits remain. |
| 6 ambient injected environment | PARTIAL / FAIL | Isolated runner now scrubs upper/lowercase proxies, Docker config/endpoint state and CODEX_HOME, with an executed sentinel regression. `sources.rs` still falls back from an injected snapshot to host variables/home; retry credentials are not frozen. The underlying production defect remains. |
| 7 registration | FIXED locally; native execution BLOCKED | Acquisition, lifecycle and network_resources are registered; driver example already registered. Default and feature compilation and inventories cover discovery. Missing guest_compat scenarios are finding 12, not hidden registrations. |
| 8 fast filter | FIXED | Removed broad `--skip docker`; fast inventory now retains all hermetic Docker-source tests. Service opt-ins remain in tests/runner. |
| 9 naming gate | FIXED | Every discovered top-level guest module must be registered and each builtin_hw body must start with its gate. Existing hardware module/name/count checks preserved. Full gate executes. |
| 10 Clippy | FIXED | Removed unused facts_for. Both all-target Clippy modes execute with `-D warnings`. Also supplied the missing network default in builtin_hw_driver, exposed by feature Clippy. |
| 11 guest probes | PARTIAL / FAIL | `fetch` now dispatches a real bounded wget executable. Setup exit 125/126/127 cannot count as denial. Exact prelude executes in a hermetic regression with a successful wget stand-in and missing command. TLS/setup failures and unreachable endpoints still need controlled positive controls and server observations; native denial remains unproven. |
| 12 strategy matrix | FAIL | Expanded actual-awman agent × settings × prompt matrix/onboarding remains absent. Hardware unavailability does not excuse missing code. |
| 13 Apple adapter | FAIL | All targets still return typed refusal. Manual archive export is only a workaround. |
| 14 real matrices | FAIL, execution also BLOCKED | Required mTLS, real auth/CA/proxy/retry matrices and format × template corpus assertions remain incomplete. `tls_docker.rs` named in the request does not exist. |
| 15 premature PASS report | PARTIAL / BLOCKED verification | Docker and registry PASS writes moved after every scenario assertion. No real disposable-service late-stage failure fixture is available; do not treat the source correction as tested full closure. |
| 16 docs | FIXED | Allowlist JSON parsed and resolved by an executing test. Release configuration distinguishes signed Mac job from unexecuted distribution. SDK isolation and network security prose reconciled with current implementation and failures. |

Additional final findings/fixes:

- `tools/native-builtin-ci.sh` expected a nonexistent `builtin_hw_awman_typed_provider` selector. It now requires the registered actual CLI smoke selector. Inventory verifies its presence; no native execution is inferred.
- Existing Mac hardware CI signed only examples. It now also signs/checks the actual awman entrypoint. Execution remains BLOCKED; this is configuration only.
- Architecture lint rejected the direct non-UTF-8 environment presence check above Layer 0. Added `host_var_os` in data config, preserving overlay precedence/non-UTF-8 values, and used it in embedded resolution. Its regression executes without weakening override refusal.
- WI 0119's last two boxes were still checked despite the register's BLOCKED verdicts. Cleared both. All WI 0119/0121 acceptance boxes remain unchecked.
- SQLite's earlier PASS transcript references an absent `/tmp/awsq.qmx2az` artifact tree. Retain it as historical report, not an independently witnessed current full-spike PASS. Exact bundled dependency/unit checks are separate from genuine cross-driver/native catalog evidence.

No full review approval is issued. The outstanding findings above remain
mandatory FAIL/BLOCKED and require implementation or external evidence.

Final feature-run correction: the first `make test-builtin` failed ten TLS
fixture cases because enabling the SDK unifies both Rustls providers. Added
a direct dev-only Rustls edge (existing 0.23.45; no version changes) and built
each fixture server with an explicit ring provider. No process-global crypto
selection is changed. The repeated full builtin command exits 0, including
all 36 OCI harness cases (32 exercised checks and four external skip paths).
`rustls-lock-diff.txt` records the single added dependency edge. This does not
implement the missing mTLS-required real-service matrix in finding 14.

Additional source-level frontend concern (D-06 remains FAIL):
`src/frontend/api/serve.rs:150` still uses axum-server's automatic
`RustlsConfig::from_pem` provider selection. The feature fixture failure above
reproduced that constructor panicking when both Rustls providers are enabled;
the fixture correction does not change production API/squad TLS setup. No
production HTTPS startup regression was executed here. Audit/fix that path
and require an actual TLS startup/handshake test before claiming frontend
parity; local suite success does not close it.
