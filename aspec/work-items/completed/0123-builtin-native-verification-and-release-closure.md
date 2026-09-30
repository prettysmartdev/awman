# Work Item: Task

Title: Ship the builtin runtime as `builtin-experimental` on Apple Silicon macOS, with ad-hoc signing
Issue: n/a

Distribution policy (2026-09-29, supersedes the 2026-09-28 "no ad-hoc signing"
statement for this item): the macOS Apple Silicon release artifact is **ad-hoc
signed** with exactly the `com.apple.security.hypervisor` entitlement. There is
no Developer ID identity, notarization, Apple distribution account or signing
secret. Native boot and final-artifact checks apply to the exact signed
artifact; failures must be reported honestly.

Status: Completed 2026-09-30. Evidence: [WI 0123 notes](../../review-notes/0123-evidence/notes.md).
Parent: [WI 0121](0121-complete-builtin-runtime-and-native-verification.md)
Dependency: [WI 0122 Apple-store implementation](0122-native-apple-image-store-bridge.md) (completed)
Closes: this item is the final item of the builtin-runtime work stream.
Completing it resolves [WI 0119](0119-strict-embedded-microsandbox-runtime.md)
and [WI 0121](0121-complete-builtin-runtime-and-native-verification.md) for the
scope below.
Optional future work: [WI 0124](../0124-builtin-linux-and-docker-gates.md) holds
Linux, Docker Engine and registry edge-case work. None of it blocks this item
or the resolution of WI 0119/0121.

## Summary:

- The next release ships the builtin runtime as an **experimental, macOS Apple
  Silicon–only** runtime named `builtin-experimental`, parallel to
  `docker-sbx-experimental`. Every other platform refuses it with a clear
  message, and the existing runtimes are unchanged.
- The release artifact is ad-hoc signed with the hypervisor entitlement (the
  "intermediate route"). User-facing docs explain the resulting Gatekeeper,
  quarantine and file-permission behaviour.
- Scope is Apple Silicon macOS only: native scenarios, clean build, the signed
  optimized artifact, and the Apple Containers, archive and registry image
  sources. Registry validation covers the normal cases against real
  registries. Linux (both KVM architectures), Docker Engine sources, Docker
  backend regressions and registry edge cases (private CAs, proxies, token
  expiry, fault injection) are optional future work in WI 0124.
- When this item's checks pass, the builtin-runtime work stream is complete:
  WI 0119 and WI 0121 are resolved, with their remaining criteria recorded as
  deferred to WI 0124 rather than open.
- Authoritative starting points: the [evidence register](../../review-notes/0121-evidence-register.md),
  the [WI 0122 evidence](../../review-notes/0122-evidence/notes.md) (first native
  macOS guest boots, four macOS fixes), WI 0121 sections A–J, and WI 0120's
  patch inventory.

## User Stories

### User Story 1:
As a: user on an Apple Silicon Mac

I want to:
Opt into the experimental builtin runtime by setting `"runtime":
"builtin-experimental"` in `~/.awman/config.json`, and run agents in awman's
embedded microVM without Docker, `container` or `sbx`.

So I can:
Use the builtin runtime early, knowing it is experimental, with clear guidance
on Gatekeeper and macOS privacy prompts.

### User Story 2:
As a: maintainer

I want to:
Release a signed, boot-tested macOS artifact with reviewable evidence, without
waiting for Linux or Docker validation.

So I can:
Ship the experimental runtime now and close the remaining platforms later in
WI 0124.

## Implementation Details:

### A. Runtime name, platform gate and experimental status

1. Rename the persisted runtime value `builtin` to `builtin-experimental`
   (`RuntimeSelection` in `src/data/config/runtime_selection.rs`, plus every
   user-visible string, catalogue entry, TUI label, error and test). No
   released version exposed `builtin`, so no migration is needed. A config
   value of plain `builtin` is refused with an error that names
   `builtin-experimental`; it never falls back to Docker.
2. The runtime is selectable only through the **global** config
   (`~/.awman/config.json`, `awman config set --global runtime
   builtin-experimental`), like every runtime today. A repo config cannot
   select it, and it is never chosen by default or auto-detection.
3. Platform gate: on anything other than macOS on Apple Silicon (Linux of any
   architecture, Intel macOS, Windows), selecting `builtin-experimental` fails
   fast with an explicit "only supported on macOS Apple Silicon in this
   release" error, before any image acquisition or VM work, mirroring
   `docker-sbx-experimental`'s platform refusals. Linux support is re-enabled
   only by WI 0124.
4. Mark the runtime experimental wherever runtimes are listed or described:
   the valid-values list, `awman status`/TUI runtime labels, command help,
   `awman ready` output and the docs.

### B. Ad-hoc signing and the release artifact

1. Commit an entitlements file (for example `packaging/macos/awman.entitlements`,
   promoted from `tools/oci-runtime-spike/strict-embed/hypervisor.plist`)
   containing exactly `com.apple.security.hypervisor`. Do **not** add
   `com.apple.security.cs.disable-library-validation`: awman loads no dylib.
2. `release.yml` `release-macos-builtin`: after the optimized build, run
   `codesign --force --sign - --entitlements <file> -i awman`. Then:
   - verify with `codesign --verify --strict`;
   - fail if the embedded entitlements differ from the committed file;
   - record SHA-256 of both the unsigned and signed binaries (reproducibility
     compares the unsigned build; signing changes the bytes);
   - boot-test the **signed** artifact. Signing after testing produces a new,
     untested artifact and is not allowed.
3. Replace the "no signing or notarization" wording in the release job and
   spec text with the ad-hoc policy above. Linux and Windows release assets
   are built **without** the `builtin-runtime` feature for this release;
   Intel macOS stays without it.
4. Source builds: document the local `codesign` step and consider a
   `make sign-macos` helper so `make install` produces a runnable binary on
   Apple Silicon.

### C. Native macOS scenario work (Apple Silicon)

Run everything on a native Apple Silicon runner with HVF, disposable Apple
Containers services, and the signed artifact where a package requires the
distributed binary.

1. Clean-checkout build with independently reconstructed payload inputs;
   record the exact revision (`tools/native-builtin-ci.sh aarch64-apple-darwin`).
2. The actual-awman non-root agent × settings × prompt strategy matrix
   (WI 0121 C): every SettingsMount/prompt/overlay strategy, image defaults and
   overrides, live atomic credential refresh/writeback, multiple consumers and
   onboarding. Guest execution only; plan serialization and fake drivers do
   not count.
3. Actual CLI/TUI/API, workflow and squad scenarios: PTY resize/interrupt,
   stdin/EOF/seeded input, ACP fragmentation/lag, startup/import/exec
   cancellation, owner/worker crash, orphan/stale cleanup, keep/grace,
   multi-VM isolation, launcher-alive reattachment and explicit owner-exit
   refusal.
4. Two genuine old/new awman artifacts: compatible/incompatible
   protocol/catalog transitions and macOS binary replacement/translocation,
   including re-signing an upgraded binary.
5. Guest network contracts with controlled endpoints: DNS/auth/MCP/proxy/CA,
   SNI enforcement, UDP/QUIC bypass, denied egress, isolation, memory
   pressure/OOM/host survival, CPU allocation, unsupported limits/socket
   bridges. The WI 0122 `only_lo` probe semantics apply.
6. Image sources on macOS: Apple Containers store (done in WI 0122) and
   archive, including the Apple-export and OCI/docker-save archive formats of
   the shipped templates, with cached execution needing no source. Docker
   Engine sources are WI 0124.
7. Registry source, normal cases, against real registries with standard
   public TLS and no proxy:
   - anonymous pull of a small public image from Docker Hub and from GHCR,
     including their foreign-host Bearer token realms and blob redirects to
     CDN/storage hosts (credentials must never be sent to the redirect target);
   - multi-platform index selecting `linux/arm64`;
   - authenticated pull of a private repository (a disposable GHCR package or
     Docker Hub private repo with a throwaway, read-only token), with
     credentials from each supported source: environment variables, the
     macOS keychain, and Docker `config.json`;
   - wrong or missing credentials refused with a clear error that contains no
     secret; a nonexistent reference or tag reported as not found;
   - digest-pinned references (`name@sha256:…`) verified;
   - cached reuse (`CachedOnly`, then `IfMissing` without contacting the
     registry), and the pulled image boots in a guest with the network off.
   Private CAs, proxies, insecure registries, token expiry mid-pull,
   disconnects, stalls and 5xx fault injection are WI 0124. Their existing
   hermetic tests keep running.
8. Cache stress across processes: acquisition/publication/materialization/use/
   prune, interrupted commits/ENOSPC, private path/lock substitution, archive
   leases and the planned-launch tag lease→SDK rootfs retention handoff.
9. SQLite on macOS: migration, genuine bidirectional transaction/locking/crash
   checks and old→new→old catalog scenarios (`build-old-probe.sh`,
   `checks.sh auto`); exact bundled SQLite, 27 migrations, no DSO.
10. The optimized/LTO/stripped **signed** artifact: boot it, trace
   executable/firmware access and helper/provider retention, minimal PATH and
   enforced-offline cached execution. Record ABI, size/build/boot/RSS/disk
   measurements, notices and corresponding source materials.
11. Existing macOS backends (`apple-containers`, `docker-sbx-experimental`)
    still pass their regressions with the renamed runtime present. Intel macOS
    and Windows assets still build and refuse `builtin-experimental`.

### D. Documentation (user-facing)

State that `builtin-experimental` is **experimental and Apple Silicon macOS
only** in every place runtimes are described: `README.md`,
`docs/00-getting-started.md`, `docs/01-concepts.md`,
`docs/03-agent-sessions.md`, `docs/04-security-and-isolation.md`,
`docs/05-workflows.md`, `docs/07-configuration.md`, `docs/11-runtimes.md`,
`docs/12-squad.md`, `docs/13-cleaning-up.md` and the generated command
reference. Do the same in `aspec/uxui/cli.md`, `aspec/architecture/design.md`,
`aspec/architecture/security.md` and `aspec/devops/{cicd,localdev}.md`, plus
the release notes and blog post for the release. Remove or reword claims of
Linux support for this release.

Include plain explanations of:

- **How to enable it:** only by setting `"runtime": "builtin-experimental"` in
  `~/.awman/config.json` (or with `awman config set --global`), and what
  other platforms report.
- **The signature:** the binary is ad-hoc signed, not Developer ID signed or
  notarized, because macOS only lets a program start a VM when its signature
  carries the hypervisor entitlement.
- **Gatekeeper and quarantine:** copies that arrive via a browser, AirDrop,
  Mail or Messages are quarantined and blocked. Explain how to install
  without quarantine (the install script or `curl`), and how to clear
  quarantine for a verified download, with a checksum-verification step
  first.
- **Integrity:** verifying the published SHA-256 is the only way to confirm
  origin. Anything that modifies the binary (strip, patching, relinking)
  invalidates the signature and stops it launching; re-sign after such
  changes.
- **Privacy (TCC) prompts:** interactive runs are attributed to the terminal
  app. The squad daemon runs under launchd and is attributed to awman itself,
  so repos under `~/Desktop`, `~/Documents` (and likely Downloads, iCloud
  Drive and removable volumes) prompt again after **every** awman upgrade,
  and background work waits until the prompt is answered. The keychain is
  unaffected because awman reads it through Apple's `security` tool. Point to
  the System Settings → Privacy & Security → Files and Folders entry.
- **Source builds:** the `codesign` step.

## Edge Case Considerations:

- `builtin` (old spelling), a mistyped value, or `builtin-experimental` on a
  non-Apple-Silicon host: explicit error, never a fallback to Docker or
  another runtime.
- A binary modified after signing (strip, patch, package-manager relink):
  documented; the error the user sees is described.
- Repos in TCC-protected folders under the squad daemon: prompts after each
  upgrade are documented. Consider surfacing denied folder access as a clear
  squad error instead of a silent stall; if implemented, test it.
- Quarantined downloads: documented.
- No ambient runtime discovery, host agent execution, helper fallback, secret
  leakage or unlisted parity exception (unchanged from WI 0121).

## Test Considerations:

Start with `make pre-push`,
`cargo clippy --all-targets --features builtin-runtime -- -D warnings` and
`make test-builtin` on Apple Silicon, then
`tools/native-builtin-ci.sh aarch64-apple-darwin` and the required
hardware/network/pressure/real-Apple-store/corpus/final-artifact gates
(`aspec/review-notes/0121-evidence/raw/closure/run-gates.sh` or its
successor). Required-hardware opt-outs never count as passes. Keep failed
attempts alongside successful reruns.

- [x] Runtime rename, experimental labelling and the platform gate: unit and
  CLI tests cover `builtin-experimental`, the refused `builtin`, and every
  non-Apple-Silicon refusal. ([hermetic](../../review-notes/0123-evidence/final/hermetic.log),
  [Intel refusal](../../review-notes/0123-evidence/final/intel-refusal.log))
- [x] D-01 (macOS): Apple store and archive sources work on the real service;
  cached execution needs no source. (WI 0122 evidence; the
  [real corpus](../../review-notes/0123-evidence/final/image-sources.log) of all nine templates; the
  offline hardware tests)
- [x] D-02 (normal cases): every registry case in C.7 passes against real
  Docker Hub/GHCR repositories, with no secret in any error or log. Public
  cases in [image-sources](../../review-notes/0123-evidence/final/image-sources.log) plus a
  registry-pulled image running offline in [hardware](../../review-notes/0123-evidence/final/hardware.log);
  authenticated pulls from a private GHCR package with environment,
  Docker-config and keychain credentials, and wrong and missing credentials
  refused, in [private-registry](../../review-notes/0123-evidence/final/private-registry.log).
- [x] D-03/D-11 (macOS): clean-checkout actual awman builds and boots through
  the current provider with reproducible payloads and tracked inputs.
  ([clean-checkout CI](../../review-notes/0123-evidence/final/clean-checkout-ci.log))
- [x] D-04: every required strategy/default/override passes in non-root
  guests on Apple Silicon. ([hardware](../../review-notes/0123-evidence/final/hardware.log): strategy
  matrix, overlays, image defaults and overrides, credential refresh)
- [x] D-05 (macOS): tested guest network and resource contracts; unsupported
  limits/socket bridges explicitly fail. ([hardware](../../review-notes/0123-evidence/final/hardware.log);
  IPv6 denial is proven only on hosts with IPv6)
- [x] D-06/D-09 (macOS): actual frontend/workflow/squad, PTY/ACP,
  cancellation/reattach, crash/upgrade/multi-VM and cache concurrency.
  ([hardware](../../review-notes/0123-evidence/final/hardware.log): workflow, API, squad, owner crash and
  clean, old/new upgrade and binary replacement, PTY, ACP, cancel, reattach,
  concurrent VMs; cache invariants in [hermetic](../../review-notes/0123-evidence/final/hermetic.log))
- [x] D-07/D-08 (macOS): worker descriptor validation runs in every native
  guest boot; ambient `MSB_*` overrides and an installed Microsandbox
  configuration are refused (`runtime_refusal`, hermetic tier).
- [x] D-10 (macOS): hermetic, feature and hardware tiers complete without
  hidden skips; `make pre-push` is green on macOS.
- [x] D-12 (macOS): exact bundled SQLite, migrations and old/new catalog
  checks. ([sqlite](../../review-notes/0123-evidence/final/sqlite.log))
- [x] D-13/D-14 (macOS): the exact ad-hoc-signed optimized artifact boots and
  passes native scans; entitlements equal the committed file;
  ABI/materials/measurements recorded.
  ([release artifact](../../review-notes/0123-evidence/final/release-artifact.log))
- [x] Existing macOS backends and Intel macOS builds are unaffected
  ([Apple backend](../../review-notes/0123-evidence/final/apple-backend-regression.log),
  [Intel](../../review-notes/0123-evidence/final/intel-refusal.log)). Windows is built only in CI; the
  live `docker-sbx-experimental` regression needs a Docker account (WI 0124).
- [x] All docs listed in section D updated; the Gatekeeper, quarantine,
  integrity and TCC explanations are present. Release notes text is below,
  for `release-prep`.
- [x] All of the above pass at the same reviewed revision: every log records
  the same code-patch hash ([notes](../../review-notes/0123-evidence/notes.md)).
- [x] Resolution: WI 0119's acceptance criteria, WI 0121 and the evidence
  register record every criterion and D-row as PASS for this scope or
  **Deferred (WI 0124, optional)**. WI 0119, WI 0121 and this item are
  completed; WI 0124 stays open as optional future work.

## Authenticated registry check (how it was run)

Done 2026-09-30 against the private package `ghcr.io/cohix/awman-probe`
([log](../../review-notes/0123-evidence/final/private-registry.log)). To repeat it:

1. **A private image.** Push the fixture (or any small `linux/arm64` image)
   to a private GitHub package, for example with Apple Containers:
   `container registry login ghcr.io -u <user>`, then
   `container image tag awman-spike/fixture:latest ghcr.io/<user>/awman-probe:latest`
   and `container image push ghcr.io/<user>/awman-probe:latest`. Packages
   pushed to a personal namespace start private.
2. **A disposable read-only token.** A classic personal access token with
   only `read:packages`, short expiry, used only for this run.
3. **Run:**
   ```sh
   export AWMAN_TEST_REGISTRY_PRIVATE_REFERENCE=ghcr.io/<user>/awman-probe:latest
   export AWMAN_TEST_REGISTRY_PRIVATE_USER=<user>
   export AWMAN_TEST_REGISTRY_PRIVATE_TOKEN=<token>
   AWMAN_TEST_IMAGE_STORES=1 AWMAN_TEST_REGISTRY_PRIVATE=1 \
     cargo test --test oci_import store_registry_private_normal_cases -- --nocapture
   AWMAN_TEST_IMAGE_STORES=1 AWMAN_TEST_REGISTRY=1 AWMAN_TEST_NATIVE_KEYCHAIN=1 \
     AWMAN_TEST_REGISTRY_HOST=ghcr.io \
     AWMAN_TEST_REGISTRY_REFERENCE=<user>/awman-probe:latest \
     AWMAN_TEST_KEYCHAIN_USERNAME=<user> AWMAN_TEST_KEYCHAIN_PASSWORD=<token> \
     cargo test --test oci_import native_keychain_disposable_registry_entry -- --nocapture
   ```
   The first test covers environment-variable and Docker `config.json`
   credentials plus wrong and missing credentials; the second covers the macOS
   keychain (it stores and then removes a disposable keychain item). Revoke
   the token afterwards.

## Release notes (for `release-prep`)

- **Experimental builtin runtime for Apple Silicon.** Set
  `"runtime": "builtin-experimental"` in `~/.awman/config.json` to run each
  agent session in a microVM embedded in awman itself: no Docker, `container`
  or `sbx` needed at run time. `awman ready` imports agent images from the
  Apple Containers image store, a registry, a Docker Engine or an archive.
  Workflows, squad, the API server, ACP and PTY sessions, network policies
  (`public`, `allowlist`, `none`) and resource limits work in real guests.
  macOS on Apple Silicon only in this release; see
  [Runtimes](../../../docs/11-runtimes.md#builtin-microvm-experimental).
- **The Apple Silicon binary is ad-hoc signed.** It carries the hypervisor
  entitlement the builtin runtime needs, but it is not notarized: a copy
  downloaded with a browser is quarantined by Gatekeeper, and background squad
  tasks in `~/Desktop`/`~/Documents` re-prompt for folder access after each
  upgrade. See
  [Gatekeeper and permissions](../../../docs/11-runtimes.md#the-macos-binary-gatekeeper-and-permissions).
- **Fixes:** `awman config set` rejects unknown runtime values, and a bad
  stored value no longer blocks `awman config`; images without an explicit
  `HOME` fall back to the user's `/etc/passwd` entry (builtin runtime); the
  API documentation now shows the real command and status routes.

## Codebase Integration:

- Use the current registered test suites and production interfaces; add
  missing native scenario code alongside them.
- Coordinate with WI 0120 (patch inventory) and WI 0124.
- Registry tests use disposable, read-only tokens supplied through
  environment variables at run time. They are never committed, and must be
  gated so ordinary test runs never contact a real registry.
- Retain the WI 0122 fixes and the four authorized HostAgentPinger triggers.
- Keep Linux code paths compiling and unit-tested, but gated off at runtime
  for this release; WI 0124 enables them.

## Documentation

As section D. Docs describe current behaviour for end users; implementation
detail stays in this item, the register and code comments.
