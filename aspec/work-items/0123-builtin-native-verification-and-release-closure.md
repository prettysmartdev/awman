# Work Item: Task

Title: Complete builtin native scenarios, clean-build verification and release acceptance
Issue: n/a

Current distribution policy (2026-09-28): signing and notarization are out of
scope by user instruction. No signing identity, Apple distribution account,
notarization credentials or explicit ad-hoc signing step is required. Native
boot and final-artifact checks still apply; failures must be reported honestly.
Status: Open — dispatch only to agents with the required runner/service access
Parent: [WI 0121](0121-complete-builtin-runtime-and-native-verification.md)
Dependency: [WI 0122 Apple-store implementation](0122-native-apple-image-store-bridge.md)

## Summary:

- Execute and close the original WI 0119/0121 mandatory acceptance gates that
  require resources unavailable to the Linux ARM64 development agent. The user
  authorized this follow-up split; none of the deferred gates becomes PASS.
- This includes authoring and validating remaining **native scenario code**,
  not merely clicking existing CI jobs. Inventory omissions explicitly before
  execution; a synthetic fixture or hardcoded capability is not native proof.
- Authoritative starting points: [evidence register](../review-notes/0121-evidence-register.md),
  its latest raw evidence, WI 0121 sections A–J, and WI 0120's patch inventory.

## User Stories

### User Story 1:
As a: maintainer

I want to:
Have resource-qualified agents execute the actual awman contracts and exact
release artifacts on every supported target and preserve reviewable evidence.

So I can:
Close the original runtime work honestly and release without relying on skipped
hardware tests, inaccessible historical artifacts or configured-but-unrun jobs.

## Implementation Details:

### Required agent assignments and preflight

No accessible remote runner connection was supplied to the
parent agent. These are capability-based assignments awaiting dispatch, not
claims that an external agent has already run:

| Assignment | Required access before accepting the task |
|---|---|
| Git/build coordinator | Accessible awman object database, authoritative WI 0119 base and reviewed head, branches/history for genuine old/new builds; clean checkout/export capability |
| Apple native agent | Apple Silicon macOS/HVF; Xcode/Swift; disposable Apple Containers and Docker services; approved native tracing |
| Linux ARM64 agent | Native ARM64 with usable `/dev/kvm`; disposable Docker Unix/TLS/mTLS and registry/proxy/CA services; strace; enough disk/RAM for bounded feature/release builds |
| Linux x86_64 agent | Native x86_64 with usable `/dev/kvm` and the same service/trace/build capabilities |
| Existing-platform agent | Windows and Intel Mac native build/regression runners with existing backend prerequisites |

At the first step each assigned agent must record native architecture, KVM/HVF
access, required service/fixture availability, Git identity and artifact paths.
Fail prerequisites explicitly and reassign to a capable runner. Never replay
this item on a resource-identical unavailable runner and call its skips success.
Use disposable secrets; redact evidence and never dump ambient credentials.

### Work packages and remaining scenario implementation

1. Restore the full base/head diff, including renames/deletions, staged/dirty and
   untracked inputs. Audit all patches and rebuild from a clean checkout with
   independently reconstructed payload/native inputs. Record the exact revision
   and any post-review patch separately. The previous missing `.git` pointer
   must not be repaired by inventing history.
2. Complete the actual-awman non-root agent × settings × prompt strategy matrix
   (WI 0121 C). Exercise every SettingsMount/prompt/overlay strategy, image
   defaults and explicit overrides, live atomic credential refresh/writeback,
   multiple consumers and onboarding. Select controlled synthetic guest agents
   through production descriptor/orchestration paths. Never substitute plan
   serialization or a fake driver for guest execution.
3. Complete actual CLI/TUI/API, workflow setup/teardown/headless/failure and squad
   dispatch/discovery/concurrency scenarios. Exercise PTY resize/interrupt,
   stdin/EOF/seeded input, ACP fragmentation/lag, startup/import/exec cancellation,
   owner/worker crash, orphan/stale-resource cleanup, keep/grace, multi-VM isolation
   and launcher-alive reattachment. Preserve explicit owner-exit refusal.
4. Produce two genuine old/new awman artifacts and test compatible/incompatible
   protocol/catalog transitions, Linux deleted executables and Mac replacement/
   translocation. Creating two copies of one binary is not upgrade coverage.
5. Use controlled reachable guest DNS/auth/MCP/proxy/private-CA endpoints and
   server-side observations. Prove permitted connections before asserting
   denial. Cover shared-IP wrong/missing SNI and UDP/QUIC bypass, denied egress,
   network isolation, memory pressure/guest OOM/host survival, CPU allocation and
   unsupported limits/socket bridges. TLS SNI enforcement cannot prove encrypted
   HTTP Host/:authority inspection; record and test the actual bounded contract.
6. Execute the real Unix/TLS/mTLS Docker and authenticated CA/proxy/keychain
   registry suites, credential failure/expiry/disconnect/retry/deadline/cancel
   cases, late-failure report suppression, source shutdown and cached guest boot.
   Populate the full required format × shipped-template corpus, including Apple
   exports after WI 0122, and execute existing Docker/Apple backend regressions.
7. Stress acquisition/publication/materialization/use/prune across processes,
   interrupted commits/ENOSPC and private path/lock substitution. Verify both
   archive leases and the planned-launch tag lease→SDK rootfs retention handoff
   on native guests, including process death and another session's protection.
8. Run actual SQLite migration, genuine bidirectional transaction/locking/crash
   checks and old→new→old catalog scenarios on all three native targets. Rebuild
   the genuine old probe with `tools/oci-runtime-spike/sqlite-resolution/build-old-probe.sh`
   and run `checks.sh auto`; fix any unreproducible producer inputs. Preserve
   exact bundled libsqlite3-sys 0.38.2/rusqlite 0.40.2, 27 migrations and no DSO.
9. Build optimized/LTO/stripped artifacts and prepare the actual Mac awman
   executable for distribution without signing or notarization. Boot the exact
   distributed artifacts; trace executable/
   firmware access, dependency/helper/provider retention, minimal PATH and
   enforced-offline cached execution. Record ABI, size/build/boot/RSS/disk
   measurements, notices, corresponding source, relink/source-offer materials
   and distribution review. An example driver does not qualify.

### Mandatory command/evidence contract

Start with `make pre-push`,
`cargo clippy --all-targets --features builtin-runtime -- -D warnings`, and
`make test-builtin`. Use `tools/native-builtin-ci.sh TARGET_TRIPLE` for each
native runner. Run the explicit required-hardware/network/pressure gates, the
real-store gates with disposable inputs, the real corpus gate, and the final
artifact gate. Exact current commands and failure behavior are preserved in
`aspec/review-notes/0121-evidence/raw/closure/run-gates.sh` (or the latest linked
runner in the register). Required-hardware opt-outs are never successful runs.
Record all exit codes, test inventories, source/artifact/fixture hashes and
native/service/distribution logs. Failed attempts remain alongside successful reruns.

## Edge Case Considerations:

- No ambient runtime discovery, host agent execution, helper fallback, secret
  leakage, database policy change or unlisted parity exception.
- Missing code stays FAIL; unavailable resources stay BLOCKED until resolved.
  A follow-up assignment is not validation, and a CI definition is not a run.
- Reconcile docs/specs, the register and WI 0120 after every native fix; repeat
  affected local gates at the final reviewed revision before acceptance.

## Test Considerations:

- [ ] D-01/D-02: every mandatory source works through actual services, including
  strict Apple-store and local/remote Docker; real auth/CA/proxy/failure/retry;
  cached execution needs no source.
- [ ] D-03/D-11: clean-checkout actual awman builds and boots through the current
  provider on Apple Silicon and both Linux KVM architectures with reproducible
  payloads, tracked inputs, patches and native dependencies.
- [ ] D-04: every required strategy/default/override passes in non-root guests,
  including live atomic refresh/writeback and multiple consumers; no exemption.
- [ ] D-05: tested guest network/auth/DNS/MCP/proxy/CA/egress and memory/CPU/OOM
  contracts; unsupported limits/socket bridges explicitly fail.
- [ ] D-06/D-09: actual frontend/workflow/squad, PTY/ACP, cancellation/reattach,
  crash/upgrade/multi-VM and cache concurrency without cross-session damage or
  silent protocol truncation.
- [ ] D-07/D-08: native isolated SDK coexistence and worker validation without
  ambient discovery, host execution, secrets or silent fallback.
- [ ] D-10: hermetic tier plus feature/hardware tiers complete without known
  linker/disk failures or hidden skips.
- [ ] D-12: exact bundled SQLite, migrations, genuine bidirectional transactions
  and old/new catalog checks on every native target; no contract change or DSO.
- [ ] D-13/D-14: exact optimized/stripped/distributed artifacts boot and pass
  native scans/Mac distribution; ABI/license/materials/measurements recorded.
- [ ] D-11/D-15: Windows/Intel Mac builds and actual Docker/Apple regressions;
  onboarding/image policy/bounded parity implemented and validated in scope.
- [ ] All mandatory commands and actual CI/hardware/store jobs pass at the same
  reviewed revision. Required-hardware skips do not count.
- [ ] Every D-01–D-15 and all nine WI 0119 criteria have linked final evidence;
  no unresolved mandatory FAIL/BLOCKED row; original checkboxes/docs/inventory
  agree. Close WI 0119 only at this point.

## Codebase Integration:

Use the current registered test suites and production interfaces. Add missing
native scenario code alongside them. Coordinate with WI 0122 and WI 0120;
retain previous fixes and the four authorized HostAgentPinger triggers.

## Documentation

Update user-facing guides only for actual behavior, maintain exact evidence
links and patch inventory, and distinguish the completed local handoff from
full runtime acceptance. This item remains open until all checks above pass.
