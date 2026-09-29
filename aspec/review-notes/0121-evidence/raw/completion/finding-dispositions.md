# Review dispositions after completion follow-up

2026-09-28. Read with the register and final command exits. This supersedes
implementation descriptions in `../final-dispositions.md`, without promoting
unavailable execution to PASS.

| Finding | Current disposition |
|---|---|
| 1 allowed HTTPS | FAIL: strict opaque TLS path still rejects allowed authorities. Needs an explicit reviewed SNI enforcement implementation plus positive/negative controlled guest tests. |
| 2 UDP/QUIC | Code fixed: hostname allows are TCP-only. New pinned SDK evaluator test exercises cached-name binding denial for UDP/443. Native packet regression remains BLOCKED. D-05 remains FAIL because of finding 1. |
| 3 acquisition lease | Code fixed: atomic lookup/publication returns a leased result; staging is leased; clone retains the lease; backend retains it through projection/import/identity publication. Child-process prune test covers before-return callback and cloned projection, fresh and cached hits. Native SDK import and installed-image use/prune remain unexecuted/incomplete. |
| 4 deadline | PARTIAL: rejects late success, checks local copy/read/decompression/hash loops, bounded cache lock polling and reference rename. Slow-reader, late-success, validation callback, cancelled-reference and lock-wait regressions added. Full phase-specific fault/clock coverage, long metadata/graph work and preemptible blocking waits are not proven. Remains open. |
| 5 cancellation | FAIL: ready does not pass a cancellation signal and blocking HTTP header waits remain. Concrete token and cooperative reader checks do not close this. |
| 6 environment/credentials | Code fixed: injected absence is authoritative, production captures explicit named inputs once, credentials/proxy prepared only after a cache miss and reused on retry. Hostile upper/lower proxy plus inherited Docker-config child test and credential-file-removal retry test added; cached hit works with source stopped and unavailable auth. |
| 7 module registration | Fixed in earlier verification; current full suites retain registered modules. |
| 8 fast filtering | Fixed in earlier verification; no broad Docker-name exclusion. |
| 9 hardware gate naming | Fixed in earlier verification; required-hardware execution still BLOCKED. |
| 10 Clippy | Earlier dead code fixed; current gates recorded separately. Initial follow-up enum/import/type-inference errors corrected without lint suppression. |
| 11 network probes | PARTIAL: shell invocation corrected earlier; controlled positive endpoint/CA/error classification still incomplete. |
| 12 strategy matrix | FAIL: expanded actual-awman non-root agent/settings/prompt/live-consumer matrix remains missing. |
| 13 Apple adapter | FAIL: explicit refusal stub remains; manual export is not completion. |
| 14 real matrices | FAIL: required live mTLS/auth/CA/proxy/retry/format-template scenarios incomplete; real execution separately BLOCKED. |
| 15 report ordering | Earlier source fix retained; real late-stage failing service run still BLOCKED. |
| 16 documentation | Updated network boundary, current acquisition limits, vendor inventory and evidence status; no strategy excluded and no acceptance box checked. |

Additional production TLS issue: shared API/squad HTTPS construction now
uses explicit per-server ring, preserves ALPN and validates the chain/key.
The trusted/untrusted HTTPS handshake regression runs with builtin features,
where both crypto providers are present. This does not establish the full
frontend/workflow/squad guest matrix.

Full-diff approval remains impossible: baseline/HEAD are unknown and Git
metadata is inaccessible. Missing review/fix handoffs remain explicitly noted.
