# Final review dispositions — resource-qualified continuation

2026-09-28. Supersedes prior implementation descriptions; original mandatory
acceptance remains open. Local executed checks are linked by the evidence
register. `command-exits.tsv` is authoritative for this snapshot's final gates.

| Review finding | Current resolution and follow-up |
|---|---|
| 1 allowed HTTPS | Implemented explicit strict_sni, retained through SDK config conversion; actual proxy TLS/application data and missing/wrong/unbound/sibling no-dial tests execute. Native routing/enforcement is WI 0123 BLOCKED, not inferred from proxy tests. |
| 2 UDP/QUIC | TCP-only name rules plus SDK evaluator denial regression retained. Native packet validation WI 0123 BLOCKED. |
| 3 acquisition lease | Atomic leased lookup/publication/staging and cloned handoff retained; child-process prune executes. Shared planned-image lease now excludes tag mutation across processes; SDK rootfs references take over after creation. Native handoff/crash stress WI 0123. |
| 4 deadline | Acquisition control covers HTTP request/header/body/token, retry sleep, copy/read/decompression/hash/projection/cache lock/publication and late success. Withheld-header deadline and slow-reader/no-publication regressions execute. Filesystem/keychain/DNS syscalls are cooperative, not preemptible. SDK mutation finishes coherently. Full native fault-phase scenario coverage remains WI 0123. |
| 5 production cancellation | Ready caller/drop/Ctrl-C now reaches acquisition via a bounded-progress blocking worker; session setup forwards image sources. Seven loopback socket cancellation/deadline cases assert peer EOF and no partial cache. Ready explicit-token and dropped-future tests execute. Native SDK commit/cancel race remains WI 0123. |
| 6 environment/credentials | Authoritative captured inputs, source-free cache lookup and retry credential freezing retained and executed. No ambient proxy/helper fallback. |
| 7 module registration | All supplied modules, including new guest_compat, registered. Native script requires actual-awman smoke and strategy-matrix names. |
| 8 fast filtering | Broad Docker-name filter removed; pure source tests remain in default/fast tiers. |
| 9 required hardware names | Native gate requires and runs builtin_hw tests; absent hardware fails. Zero guest executions here. |
| 10 Clippy | Final root commands record current result; no warning-suppression workaround. Dependency SQLx deprecations remain under Cargo's dependency warning cap. |
| 11 network probes | Controlled endpoints with positive baseline and TLS/tool/HTTP failure classification implemented; shell regression executes. Guest execution WI 0123 BLOCKED. |
| 12 strategy matrix | Actual-awman nine-agent named/all-skills guest test added. Artificial descriptor combinations, live atomic refresh with multiple consumers and complete overrides remain missing: FAIL, carried to WI 0123. Existing guest scenario remains BLOCKED until native run. |
| 13 Apple adapter | Missing implementation remains FAIL. WI 0122 explicitly assigns native bridge implementation and validation to an Apple Silicon agent with service/Swift/signing access. Manual export is not completion. |
| 14 real matrices | Explicit Unix/TLS/mTLS with rejected absent identity, required authenticated private-CA proxy observation and full format × template corpus scaffolding added. Real expiry/retry/failure and guest source-stop scenarios remain incomplete: FAIL, WI 0123. Existing service runs BLOCKED without disposable inputs. |
| 15 PASS report ordering | ScenarioReport marks PASS only after complete success; late-stage failure regression executes and asserts no PASS record. Live service execution remains BLOCKED. |
| 16 documentation | Current boundary, cancellation safe points, six-patch inventory and deferred work agree with the register. No mandatory box checked; no missing strategy called an exception. |

Additional local checks: genuine SQLx/rusqlite bidirectional transactions,
rollback/uncommitted visibility/write contention on a real SDK catalog and
actual awman reopen; API/squad TLS per-server provider regression retained.
These do not establish old/new binaries, every native architecture or full
frontend/workflow/squad guest parity.

The full base/head diff and missing paired handoffs remain BLOCKED. Git points
to an unavailable Mac worktree and baseline has no recoverable commit here.
No fake Git history or external agent execution has been created. WI 0123
requires the capable coordinator to restore and review the complete identity.
