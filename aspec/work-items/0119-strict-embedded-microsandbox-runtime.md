# Work Item: Feature

Title: Builtin OCI agent runtime using strictly embedded Microsandbox
Issue: n/a

## Summary:
- Implement a builtin, container-class backend using Microsandbox, with its VM
  code, Linux kernel and guest agent embedded in the single awman executable.
  Each VM re-executes that same executable as a private worker. Do not extract
  another host executable, firmware shared library or host helper script.
- Initially maintain the required dependency changes as a small, checked-in
  local Cargo patch. Delivery does not depend on upstream accepting a PR.
  [Work item 0120](0120-upstream-embedded-kernel-support.md) prepares the
  upstreamable API and eventual removal of the downstream patch.
- Required platforms: Apple Silicon macOS, Linux ARM64 and Linux x86_64 with
  accessible KVM. Intel Mac and Windows need not support this backend, but
  existing awman builds/backends on those targets must not regress.
- Consume self-contained Linux OCI images. Image building is out of scope;
  images built from existing awman Dockerfiles remain usable through import.
  Support remote/local registries, local/remote Docker Engine image stores,
  and the local Apple Containers image store on Mac.
- Preserve Docker/Apple overlay, settings, prompt-delivery, lifecycle and
  frontend behavior as closely as possible. All existing overlay types and
  agent config strategies are required. Do not adopt the experimental SBX
  execution/config model. Smolvm is discarded.

## User Stories

### User Story 1:
As a: user

I want to: install only awman and execute agents in Linux microVMs using
existing OCI images

So I can: get isolation without installing Docker, Apple Containers or msb
as the execution runtime.

### User Story 2:
As a: user

I want to: retain my current overlays, agent settings, credential refresh,
workflows and squads when selecting the builtin backend

So I can: change runtime without rebuilding my agent configuration or losing
live filesystem/config behavior.

### User Story 3:
As a: maintainer

I want to: own a small reproducible local dependency patch and a clear route
to upstream support

So I can: ship independently without taking on a new VMM implementation or
an unbounded permanent fork.

## Implementation Details:

### Final spike findings and evidence

- Reports: [runtime comparison](../../tools/oci-runtime-spike/README.md) and
  [strict-embedding spike](../../tools/oci-runtime-spike/strict-embed/README.md).
  Executable probes and raw outputs live in `tools/oci-runtime-spike/`.
- Baselines: Microsandbox `v0.7.2`, commit
  `60d4dc8a436fb9365491567ec21d073e924e3c6d`; `msb_krun 0.1.39`, originating
  from libkrun commit `2bd0f84ad0956f3032e0490d3b8512b6851eca12`;
  Rust 1.94.0. The kernel/agent payloads came from the verified matching release.
- The strict Apple Silicon probe compiled the full SDK/CLI/runner, embedded
  agent and kernel into one executable, re-executed itself, booted a guest,
  imported OCI, passed all **9 image assertions** and all **32 mount/config
  assertions**, and propagated exit code 37. Its image tests ran with an empty
  host executable search path; the mount harness used its normal orchestration
  PATH. Logs confirm the embedded kernel provider supplied 24,576,000 bytes.
- `otool -L` listed only macOS system frameworks/libraries; the state-directory
  helper scan was empty. Ad-hoc signing with only
  `com.apple.security.hypervisor` in the entitlement plist succeeded. This is
  not proof of notarization or hardened-runtime release compatibility.
- Strict Mac result archive SHA256:
  `aab74bec2a36e46c35eeaef45a2e2ffb5222a61e2bd5e284727aad3e42c54060`.
  Raw evidence is retained in
  `tools/oci-runtime-spike/strict-embed/results/mac-arm64/`.
- Linux ARM64 compiled the full strict executable, imported/inspected OCI and
  launched the same-executable worker with no extracted runtime helpers. Kernel
  initialization reached KVM and failed because `/dev/kvm` was absent. Linux
  guest execution and the Linux x86_64 build/boot remain unverified. Static
  cap-ng linking removed the release's extra `libcap-ng.so` dependency; only
  ordinary system libraries remained in the probe's dynamic dependency list.
- The tested image assertions cover USER, primary GID, WORKDIR, HOME, ENV,
  ordinary and opaque whiteouts, upper-layer contents and executable mode;
  image ENTRYPOINT/CMD also ran. Mount checks include file/dir ro/rw, nested
  mounts, config strategy families, prompts, live atomic refresh and synthetic
  escape/remount checks. They are not a complete security audit or actual
  awman-agent/frontend integration tests.
- The initial Mac socket-path failures were a harness issue: derived Unix
  socket paths exceeded macOS's 104-byte bound. A short private state path
  fixed them. Budget full derived paths, not just the configured state root.
- Existing awman uses `rusqlite 0.40` / `libsqlite3-sys ^0.38`. The pinned msb
  stack uses `sqlx-sqlite 0.9` / `libsqlite3-sys >=0.30.1, <0.38`, producing a
  real Cargo native-links conflict in the unpatched graph. The subsequent
  [SQLite resolution spike](../../tools/oci-runtime-spike/sqlite-resolution/README.md)
  resolves it by backporting an already-merged one-line SQLx manifest change.
  It links actual awman library code and the full strict runtime in one Linux
  ARM64 executable without downgrading rusqlite; 33 existing awman store tests,
  all 27 msb migrations, cross-driver checks and old/new catalog reads pass.
  This is not yet awman's production entrypoint/backend or a Mac link result.
- The proof patches only the libkrun firmware loader, exports a getter from
  the application, and presents the executable itself as the SDK's firmware
  path. It uses an embedded msb-version section. The upstream msb SDK/runtime
  source was unchanged. These path/symbol tricks demonstrate feasibility;
  they are not the desired production API.
- Smolvm was rejected after directory-only sharing, OCI-layout import failure,
  opaque-whiteout failure and local-archive image-default failures. Do not
  continue integrating or benchmarking it in this item.
- Real Docker/Apple store round trips, authenticated/local registries, Linux
  boot, complete PTY/ACP/lifecycle/squad behavior, resource enforcement,
  optimized size/performance and production signing were not established.

### Chosen model and explicit exclusions

- One host executable, not one process. Dispatch an internal worker before
  normal frontend, Tokio and TUI initialization. The runner takes over its
  process and exits on VM termination; never host it in a thread in awman's
  main process or use a fork-only child of the multithreaded application.
- Kernel/guest-agent bytes are embedded build inputs. OCI caches, writable
  guest disks, databases, sockets and logs are allowed on disk. Executables
  inside a guest are not extracted host helpers.
- OS frameworks, Linux KVM and the supported platform ABI remain prerequisites.
  Do not interpret “strict” as eliminating macOS frameworks or requiring a
  libc-free executable. Match and document the existing Linux release ABI.
- No silent fallback to a host agent, SBX, installed msb, or extracted helper
  bundle. Retain Docker/Apple as explicit independent backend choices; leave
  existing default selection behavior unchanged unless separately specified.
- Compared with bundled msb+firmware, strict packaging removes executable-cache
  installation/version-pair handling and ties runtime updates to awman releases.
  It does not inherently improve VM isolation or runtime performance. The spike
  estimated 2–4 engineer-weeks of incremental strict packaging/integration work
  versus 3–7 engineer-days for a bundle, excluding dependency alignment and
  common backend/importer work. These are planning estimates, not commitments.

### Phase 1: dependency and release foundation

1. **SQLite decision: keep awman's rusqlite and backport the upstream SQLx
   bound fix.** Vendor `sqlx-sqlite 0.9.0` and change only its
   `libsqlite3-sys` requirement from `>=0.30.1, <0.38.0` to
   `>=0.30.1, <0.39.0`, matching upstream commit
   `94aafe3a68884d923b0798a767c8d7f6cfda89d2`. Select it through the root
   `[patch.crates-io]` table. Preserve the tested `rusqlite 0.40.2` and bundled
   `libsqlite3-sys 0.38.2` lockfile baseline, with exactly one native SQLite
   package. Do not downgrade rusqlite, use system SQLite, rewrite persistence
   or adopt the whole unreleased SQLx workspace. Keep database paths, schemas,
   migrations and pool policies unchanged. Track this manifest-only backport
   separately from the kernel patch. Remove it once a compatible SQLx release
   contains the fix and passes the recorded combined-link, store, migration
   and old/new catalog checks. No new SQLite upstream PR is required.
2. Vendor only the necessary patched dependency source in an owned repository
   directory, with upstream revision/version, original license, patch rationale,
   reproducible diff and removal instructions. Use `[patch.crates-io]` with a
   repository-relative path and commit the lockfile. Never depend on `/tmp`,
   mutable fork branches, generated Cargo-cache edits or a developer's machine.
3. Start from the proven loader boundary but make embedded-kernel ownership,
   validation and selection explicit. Prefer a typed process-lifetime provider.
   Keep it small enough to carry locally until WI 0120 lands; do not ship the
   fake firmware-file convention as a public user configuration contract.
4. Own target-specific kernel/agent builds or checksum-verified build inputs.
   Verify architecture, provenance, payload bounds and compatibility; never
   fetch/install host runtime code at application startup. Guest-agent embedding
   is distinct from the SDK feature that embeds an extractable host bundle.
5. Statically link or otherwise remove non-system native runtime dependencies
   such as cap-ng on Linux. Review source/notice/distribution obligations for
   embedded kernel and native libraries; retain a documented reproducible build
   pipeline. No claim that the implementation is exclusively Rust is required.
6. Feature/target-gate the builtin backend so unsupported targets still build
   existing functionality and return a precise unsupported-backend diagnostic.

### Phase 2: runtime ownership and awman integration

1. Implement an engine-owned builtin container backend behind the existing
   `AgentRuntimeEngine` / container option contract. Keep image, config and
   lifecycle policy out of frontends and the binary entrypoint.
2. Add narrow same-executable worker dispatch and explicit embedded-runtime
   resolution. Reuse msb's private configuration transport, inherited descriptor
   ownership, startup handshake, watchdogs and lifecycle locks. Do not send
   secrets in argv or serialize host pointers in worker messages.
3. Preserve awman's public `--version`; expose the embedded msb version and
   capabilities separately. Support the capability handshake actually needed
   by the runtime rather than spoofing every msb CLI command. Disable upstream
   self-install/self-update and ambient executable/library discovery for this
   backend. Any necessary msb-side glue patch must be recorded separately.
4. Refactor Docker-shaped `cli_binary()`, exec/background/stop/remove defaults
   and build-image assumptions into explicit backend operations. Do not simply
   substitute the awman executable into Docker argv builders.
5. Provide create/start/exec/attach/stop/remove, discovery, labels/ownership,
   stats, cancellation and background lifecycle behavior. Protect other
   sessions during cleanup. Define recovery when an old worker survives an
   awman update, including catalog/protocol compatibility and reattachment.
6. Allocate short, user-private socket/runtime locations securely with
   collision-safe ownership. Handle long/non-ASCII paths, concurrent starts,
   stale locks, permissions and cross-user isolation without predictable
   world-writable endpoints.

### Phase 3: OCI acquisition and image readiness

- Model source type explicitly: registry, Docker Engine store, Apple store or
  archive. A registry reference must not accidentally select a same-named image
  from a daemon, or vice versa. Resolve platform and content identity explicitly.
- Remote and localhost registries: implement authentication, digest verification,
  native-platform selection, private CA/proxy handling, caching and offline
  reuse. Credential-helper configurations must not introduce an undocumented
  mandatory executable dependency; provide an explicit native credential path.
- Local and remote Docker Engine stores: use the supported image-export API
  over the selected Unix socket, TLS or supported authenticated remote transport.
  Do not scrape Docker's private storage directories. Source daemon access is
  needed only for acquisition, never for running cached images.
- Apple Containers store: validate actual exports, then implement a versioned
  native/in-process bridge or supported store API that does not require a
  separately executed `container` helper. The source store/service must exist
  to export its images; it must not be needed after import. Resolve this adapter
  early. If it cannot meet the strict requirement, report the blocker instead
  of silently reducing scope or treating manual archive export as completion.
- Archive support must preserve OCI and Docker-save configuration, layers,
  whiteouts, ownership/modes and platform metadata. Do not flatten away defaults.
- `ready` should prepare/import an existing image, not build it. Explain how to
  build an image externally from the existing awman Dockerfiles when missing.
  Do not make ordinary startup depend on a builder or start a source daemon
  merely to run an already cached image.

### Phase 4: required compatibility

- Reuse awman's overlay resolver and `ResolvedContainerOptions`. Support
  Directory, Skill, Env and Context overlays, all global/repo/workflow scopes,
  files/directories, ro/rw, nested destinations and conflict/precedence rules.
- Reuse every `SettingsMount` family: None (Maki/Copilot), Direct
  (Codex/OpenCode/Gemini/Crush/Cline), Claude and Antigravity. Derive mount
  destinations from effective image HOME, not assumptions about the Dockerfile.
- Preserve Claude's sanitized staged directory, individual config file,
  refresh-token-free mode-0600 credentials and atomic refresh. Preserve existing
  Antigravity staging/keychain rules. Never mount host credential directories
  wholesale or broaden the authorized `HostAgentPinger` behavior.
- Cover every `SystemPromptMode`: Append/file, AppendInline, Replace, AgentsMd,
  EnvFile, AddDir and deliberately Unsupported. Preserve argv boundaries,
  environment precedence, file contents and non-root access.
- No whole-parent-directory exposure to simulate a file mount, and no one-time
  copies to simulate live sharing. Explicitly test host-side atomic replacement
  and guest writeback, including multiple simultaneous credential consumers.
- Preserve interactive PTY resize/interrupt/detach/reattach and noninteractive
  stdin/EOF, exit codes, stdout/stderr separation and binary-clean ACP streams.
  Preserve CLI/TUI/API, workflow and squad dispatch/discovery behavior.
- Define and test memory/CPU enforcement rather than assuming vCPU count equals
  Docker's fractional CPU quota. Do not silently round or ignore limits. For
  unavoidable Docker/Apple semantic differences, explicitly document and reject
  unsupported requests; a missing required overlay/config strategy is not an
  acceptable compatibility exception.
- Exercise network/API authentication, DNS, host-local MCP services, proxy/CA
  behavior and denied egress. Any host Docker-socket bridge must remain explicit
  opt-in with an updated security-spec boundary; never treat a Unix socket as
  an ordinary file mount or enable host-daemon access by default.

### Acceptance criteria

- [ ] Actual awman links and ships as a single host executable on required
  targets; no extracted executable/firmware DSO or separately installed runtime
  is used. Unsupported platforms retain their previous builds/backends.
- [ ] Reproducible local dependency patch, pinned payloads and native dependency
  handling are owned by the repository; clean-checkout builds work.
- [ ] The selected SQLx backport (or its validated released replacement)
  resolves exactly one bundled native SQLite package without changing awman's
  rusqlite baseline or database contracts; the recorded SQLite spike checks
  pass on supported targets.
- [ ] Native guest execution succeeds on Apple Silicon and Linux ARM64/x86_64
  with KVM; source inspection or blocked hardware tests are not reported as pass.
- [ ] All mandatory image-source adapters work, including actual Apple-store
  and local/remote Docker-store round trips; cached execution works without them.
- [ ] All overlay/config/prompt strategies and agreed runtime/frontend/squad
  contracts pass automated tests; known differences are explicit and bounded.
- [ ] Final optimized/LTO and signed artifacts preserve the embedded provider,
  boot successfully, and pass the relevant distribution/security checks.
- [ ] Documentation/spec changes describe the builtin backend accurately;
  existing Docker/Apple behavior is regression-tested; no SBX model is adopted.
- [ ] WI 0120 has a precise patch inventory and replacement/removal path;
  upstream acceptance is not a completion dependency for this item.

## Edge Case Considerations:
- Reject wrong-platform images/payloads, malformed or oversized archives,
  digest mismatches, extraction traversal/symlink/hardlink attacks, truncated
  transfers and insufficient disk space before exposing partial state.
- Validate non-root UID/GID/supplementary groups, executable modes, file
  replacement, hardlinks, watchers, xattrs and Mac case sensitivity. Basic
  root-mode fixture success is not sufficient for real agent settings access.
- Handle guest failure, OOM, worker crash, parent crash, interrupted startup,
  orphan cleanup, same-name collisions and version upgrades without killing or
  corrupting another session. Do not corrupt terminal state on detach/failure.
- Do not leak secrets into logs, process arguments, persisted configuration,
  image metadata, support bundles or worker-version probes.
- KVM missing/denied and Mac entitlement failures must be clear errors, not
  host execution, runtime downloads or weaker sandbox fallbacks.
- Preserve agent-internal namespace/seccomp/sandbox behavior where required;
  do not disable safeguards merely to make compatibility tests pass.

## Test Considerations:
- Keep fast unit/command/frontend tests hermetic using runtime/store/transport
  doubles. Add an explicit opt-in builtin-runtime integration gate consistent
  with existing Docker/Apple test gates. Never repurpose real HOME/CODEX_HOME,
  contact real credential stores, or run real paid agents during ordinary tests.
- Promote the fixture image and its individual assertions into automated
  integration tests. Cover both immutable image defaults and explicit runtime
  overrides, all overlay/config/prompt variants and non-root execution.
- Re-run `tools/oci-runtime-spike/sqlite-resolution/` checks when changing
  SQLx/rusqlite/native SQLite: combined linking, existing awman persistence
  regressions, msb migrations, cross-driver transactions and old/new catalog
  round trips. Assert one native binding and no SQLite DSO dependency.
- Add real-hardware jobs for Apple Silicon and both Linux architectures, with
  KVM/entitlement prerequisites checked explicitly. Missing hardware is SKIP or
  BLOCKED, never a passing execution test.
- Exercise minimal-PATH/offline cached execution, dependency listings, executable
  and firmware access tracing, and final artifact helper-extraction scans.
- Test private/local registries and local/remote image-store adapters with
  disposable stores, archives and credentials, including failure/retry paths.
- Add PTY/ACP framing, cancellation, reattachment, upgrade recovery, multi-VM,
  workflow/squad concurrency, quotas, network and opt-in socket-bridge tests.
- Run architecture lint, existing fast/full backend regressions and configured
  formatting/lints. Measure optimized binary size, cold/warm startup, RSS,
  disk growth and realistic git/build workloads without inventing performance
  targets or treating unoptimized probe size as a release estimate.

## Codebase Integration:
- Follow `aspec/architecture/design.md`, `aspec/architecture/security.md`,
  `aspec/devops/localdev.md` and release/CI conventions. Update their
  Docker-only/CLI-runtime assumptions
  deliberately as part of implementation; preserve execution-isolation policy.
- Core integration points: `src/engine/agent_runtime/`,
  `src/engine/container/backend.rs`, `runtime.rs`, `options.rs`,
  `src/engine/overlay/agent_settings.rs`, `src/engine/agent/agent_matrix.rs`,
  `src/engine/ready/`, and runtime configuration/detection/capabilities.
- Data-layer types own persisted source/runtime configuration. Engine owns
  runtime/import/overlay behavior. Command layer owns user orchestration;
  frontends render shared outcomes. The binary contains only minimal internal
  worker-mode routing before normal startup.
- Spike paths in this item refer to repository-root `tools/oci-runtime-spike/`.
  Keep evidence and reproducible probes available, but do not make production
  runtime execution depend on developer scripts or their temporary artifacts.

## Documentation

- Update existing user installation, runtime selection, ready/image handling,
  overlay/config, headless/workflow/squad and troubleshooting guides after
  implementation. Document required hosts, image-source selection, external
  build workflow, KVM/entitlements, cache cleanup and explicit parity limits.
- Update architecture, security, CI/release and local-test specs for the new
  worker boundary, backend capabilities and hardware integration gates.
- Keep dependency patches, provenance, spike results and upstream migration
  details in work-item/developer material, not a work-item-specific user guide.
