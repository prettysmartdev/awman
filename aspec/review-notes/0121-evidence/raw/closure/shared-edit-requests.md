# WI 0121 shared edits (append-only under shared-edit-requests.lock)

Ownership and append protocol: ownership.md. The following planning records reserve required integration; they are requests, not applied patches or test evidence. Requestors must append exact signatures/diffs once interfaces are determined. Resolve every required request before final verification; no non-owner edits a requested file.

## REQ-plan-001
From: plan / all implementation steps
To: impl-cache-testinfra
Files/symbols: tests/builtin_runtime/main.rs module registry
Required edit: register modules acquisition, sdk_worker, network_resources, distribution, lifecycle, guest_compat, cache_testinfra when supplied; preserve platform/feature gating at actual boundaries. Do not register nonexistent files. Publish a --list inventory demonstrating discovery.
Reason / D-ID: all; autotests=false means missing registration is hidden coverage.
Status: OPEN; implementation owners supply concrete modules.

## REQ-plan-002
From: plan / impl-network-resources
To: impl-sdk-worker and impl-lifecycle (each only their own file)
Files/symbols: builtin/msb_driver.rs Sandbox::builder/create; builtin/backend.rs both SandboxSpec construction sites
Required edit: consume the explicit resolved network policy published by network owner; enforce it at SDK/VMM boundary in both foreground/background creation. Publish precise type/signature before implementation. Unsupported policy must refuse pre-launch; that alone does not complete denied egress.
Reason / D-ID: D-05; type-only policy is not enforcement.
Status: OPEN; network owner supplies exact contract/diff.

## REQ-plan-003
From: plan / impl-sdk-worker
To: impl-build-tooling
Files/symbols: Cargo.toml/Cargo.lock; third_party/<pinned SDK crate> if necessary; third_party/README.md; WI 0120 inventory
Required edit: receive SDK owner's exact isolated-config API patch, vendor only necessary immutable source with hash/license/upstream.diff/PATCH.md, register local override and update removal inventory. Do not independently guess a new runtime patch or edit Cargo caches. If existing SDK API suffices, resolve as unnecessary with evidence.
Reason / D-ID: D-07/D-11; isolated SDK coexistence.
Status: OPEN; sdk-worker supplies exact patch or proof no patch needed.

## REQ-plan-004
From: plan / impl-cache-testinfra
To: impl-acquisition, impl-sdk-worker, impl-lifecycle (each only their own file)
Files/symbols: engine/oci/mod.rs acquisition return/use; builtin/msb_driver.rs import_archive; builtin/backend.rs import/prune/use
Required edit: adopt cache owner's explicit archive/staging/installed-image lease lifetime API through entire materialization/publication/use interval; add tests at real owner boundaries. Cache owner supplies signatures/diff; preserve existing publication-window lock.
Reason / D-ID: D-09; validated pathname alone cannot protect subsequent import/use from prune.
Status: OPEN; likely owner follow-up/fix-area integration after wave 2.

## REQ-plan-005
From: plan / all steps
To: impl-cache-testinfra
Files/symbols: tools/isolated-test.sh, test gate helpers; Makefile after handoff
Required edit: implement plan.md gates with explicit disposable fixture inputs, narrow credential opt-in, ordinary HOME/CODEX_HOME/source/proxy/credential scrubbing and required-external failure semantics. Remove broad fast docker name suppression without enabling real stores. Coordinate exact commands with build owner before its completion.
Reason / D-ID: D-02/D-10/D-11; tests must remain isolated and visible.
Status: OPEN.

## REQ-plan-006
From: plan / impl-guest-compat
To: impl-build-tooling
Files/symbols: tools/oci-runtime-spike/fixture/ and image-checks.sh/guest-checks.sh if extensions are required
Required edit: preserve original 9/32 assertion identities; guest owner supplies any concrete new fixture/tool changes for actual-awman strategy matrix. Put new synthetic guest scripts in guest-owned test directories when no existing tool change is needed.
Reason / D-ID: D-04/D-15; fixture ownership remains build-tooling even when guest work begins.
Status: OPEN; resolve as unnecessary if owned test fixtures suffice.
## REQ-impl-sdk-worker-001
From: impl-sdk-worker
To: impl-build-tooling
Files/symbols: `third_party/microsandbox-0.7.2/lib/backend/local/mod.rs` (new vendored pinned crate), `Cargo.toml`, `Cargo.lock`, `aspec/work-items/0120-upstream-embedded-kernel-support.md`
Required edit: Vendor pinned microsandbox 0.7.2 preserving its license and add one `LocalBackendBuilder::build_lazy_isolated(self) -> LocalBackend` method whose body is `self.build_lazy_from(GlobalConfig::default())`. Document that it never reads `load_persisted_config_or_default`, `MSB_CONFIG_PATH`, HOME or XDG config. Keep existing builder methods unchanged. Register `[patch.crates-io] microsandbox = { path = "third_party/microsandbox-0.7.2" }`, resolve lock, record exact upstream/source hash, minimal diff and removal gate in WI 0120. The awman caller will use `.build_lazy_isolated()` and set all owned runtime paths explicitly.
Reason / D-ID: D-07; SDK 0.7.2 `try_build_lazy` always merges installed config before awman overrides; `GlobalConfig::default()` and private `build_lazy_from` make this a narrow backward-compatible API addition.
Tests / dependencies: `sdk_isolated_config_ignores_installed_msb_config`; needed before feature build/clippy. Please notify impl-sdk-worker when available.
Status: OPEN
## REQ-impl-sdk-worker-002
From: impl-sdk-worker
To: impl-build-tooling
Files/symbols: new `tests/builtin_runtime/distribution.rs` or owned release artifact scanner
Required edit: Assert the exact final release/LTO/stripped `awman` artifact retains the read-only `.msbver` (Linux) or `__TEXT,__msbver` (macOS) bytes `0.7.2`, and that SDK bounded version reader accepts that executable. Run after stripping and, on macOS, after signing; record exact artifact pre/post-sign hashes. A debug `--version` or fixture driver check is insufficient. Use `AWMAN_TEST_BUILTIN_ARTIFACT` and fail missing prerequisites under the distribution opt-in gate.
Reason / D-ID: D-08/D-13 version-section exception review; worker.rs keeps `#[used]` metadata and a narrow unsafe link_section attribute. Final artifact production/signing belongs to build-tooling.
Tests / dependencies: `builtin_final_artifact_retains_worker_provider`; P-DIST and native target runners.
Status: OPEN

## REQ-acquisition-001
From: impl-acquisition
To: impl-build-tooling
Files/symbols: `Cargo.toml` `[dev-dependencies]`; `Cargo.lock` (dependency edge only, `rustls 0.23.45` is already resolved in the lock via reqwest/axum-server)
Required edit: add `rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }` under `[dev-dependencies]` and refresh the lock with `cargo update -p awman --offline`-equivalent minimal resolution (no version bumps). Optional but preferred alongside: `tokio-rustls = "0.26"` (also already in the lock) so a test can accept raw TLS streams.
Reason / D-ID: D-02. The hermetic Docker TLS tests (`tests/oci_import/tls_docker.rs`) can prove server-auth TLS, private CA, expired/mismatched certificates and `verify:false` with `axum_server::tls_rustls::RustlsConfig::from_pem`, but a **mutual-TLS** server (client certificate required) needs `rustls::ServerConfig::builder().with_client_cert_verifier(..)` and `axum_server::tls_rustls::RustlsConfig::from_config`, which cannot be named without a direct `rustls` dependency. Cargo.toml/Cargo.lock are build-tooling-owned.
Tests / dependencies: `tests/oci_import/tls_docker.rs::docker_mtls_export_presents_the_client_certificate` and `docker_mtls_without_client_certificate_is_refused_by_the_engine` (the file is written to compile once the dependency exists; registration in `tests/oci_import/main.rs` is gated by impl-acquisition/fix-claude-area). Until then those two cases are absent (mTLS positive path: FAIL-by-absence in the register, not SKIP).
Status: OPEN
## REQ-impl-network-resources-001
From: impl-network-resources
To: impl-cache-testinfra
Files/symbols: `src/engine/container/builtin/mod.rs` (module registry + `tests::settings`), `tests/builtin_runtime/main.rs`
Required edit:
1. Register the new pure policy compiler UNGATED so its hermetic tests run in the default tier (D-10/G-3): add
   ```rust
   #[cfg_attr(not(awman_builtin), allow(dead_code))]
   mod network;
   ```
   next to `mod driver;` (it depends only on `crate::data::config::builtin_network` + serde/serde_json; no SDK). `driver.rs` imports it as `crate::engine::container::builtin::network::NetworkPlan`.
2. `BuiltinRuntimeSettings` gained a required field `pub network: crate::data::config::BuiltinNetworkSettings`. In `mod tests { fn settings(..) }` add `network: Default::default(),` to the struct literal. **This literal is compiled in the default (non-feature) build, so `cargo test --lib` fails to compile until this line is added.**
3. `tests/builtin_runtime/main.rs`: add `mod network_resources;` (file exists: `tests/builtin_runtime/network_resources.rs`; it compiles in the default build and gates itself).
Reason / D-ID: D-05 (network policy/resources), D-10 (pure tests without payload).
Tests / dependencies: `engine::container::builtin::network::tests::*` (6 tests), `network_resources::*` hermetic + `builtin_hw_*` gated tests.
Status: OPEN

## REQ-impl-network-resources-002
From: impl-network-resources
To: impl-sdk-worker
Files/symbols: `src/engine/container/builtin/msb_driver.rs` `impl SandboxDriver for MsbDriver::create`; `tests/builtin_runtime/runtime_refusal.rs::settings`
Required edit:
1. At the start of `create`, before any SDK call: `spec.check()?;` (new `SandboxSpec::check` in driver.rs: refuses sockets/FIFOs/devices as mounts, zero vCPUs, memory < 128 MiB).
2. Apply the compiled network plan (new `spec.network: NetworkPlan`, see `src/engine/container/builtin/network.rs`) to the builder; **fail closed**, never keep SDK defaults:
   ```rust
   let plan = spec.network.clone();
   let policy: microsandbox::sandbox::NetworkPolicy =
       serde_json::from_value(plan.policy_json()).map_err(|_| failure("network policy"))?;
   let mut builder = Sandbox::builder(spec.name) /* existing .image/.cpus/.memory/... */;
   builder = if plan.enabled {
       builder.network(|n| {
           n.policy(policy)
               .strict(plan.strict)
               .trust_host_cas(plan.trust_host_cas)
               .dns(|d| d.rebind_protection(true).nameservers(plan.nameservers.clone()))
               .tls(|t| t.enabled(false))
       })
   } else {
       builder.disable_network()
   };
   ```
   Do not add ports (`.port*`), secrets, or an outbound proxy. `builder.build()` errors already map to `failure("create configuration")` before launch — keep that.
3. Feature-only test (msb_driver tests, SDK present, no VM needed): `sdk_accepts_every_compiled_network_policy` — for each of `NetworkMode::{None, Public, Allowlist}` (allowlist with `Domain`, `Suffix` and `host_ports: vec![8765]`) compile with `network::compile`, `serde_json::from_value::<microsandbox::sandbox::NetworkPolicy>` must succeed, re-serialising must equal `plan.policy_json()`, and the built `Sandbox::builder(..)...build()` config's network must have `enabled == plan.enabled`, `strict == plan.strict`, `tls.enabled == false`, `ports.is_empty()`, `outbound_proxy.is_none()`.
4. `tests/builtin_runtime/runtime_refusal.rs::settings`: add `network: Default::default(),` (the struct gained a field; this file compiles in the default build).
Reason / D-ID: D-05 — a policy type is not enforcement; the SDK user-space netstack in the worker is the enforcement point.
Tests / dependencies: above; guest tests `builtin_hw_network_*` in tests/builtin_runtime/network_resources.rs depend on this.
Status: OPEN

## REQ-impl-network-resources-003
From: impl-network-resources
To: impl-lifecycle
Files/symbols: `src/engine/container/builtin/backend.rs` (both `SandboxSpec` literals: foreground `plan()` ~L267 and background ~L612); `backend/lifecycle_tests.rs` (`settings()`, `new_spec()`); `exec_bridge/bridge_tests.rs` (SandboxSpec literal)
Required edit:
- backend.rs both sites: `network: network::compile(&self.settings.network),` (import `super::network` / `crate::engine::container::builtin::network`). Same policy for foreground and background VMs; there is no per-launch network override.
- lifecycle_tests.rs `settings()`: `network: Default::default(),`; `new_spec()` and bridge_tests.rs literal: `network: crate::engine::container::builtin::network::NetworkPlan::default(),`.
- Guest OOM: keep the agent's real exit status (137 after the guest OOM killer) and the normal remove-on-exit/keep cleanup; do not treat it as a host failure. Please add a fake-driver test that an exec ending `Exited(137)` is reported as 137 and the sandbox is still finished/removed.
Reason / D-ID: D-05.
Tests / dependencies: lifecycle-owned fake-driver tests; `builtin_hw_memory_oom_preserves_host_and_peer`.
Status: OPEN

## REQ-impl-network-resources-004
From: impl-network-resources
To: impl-guest-compat
Files/symbols: `tests/builtin_runtime/hw_driver.rs` `BuiltinRuntimeSettings` literal in `run()`
Required edit: add `network: Default::default(),` (new required field). Feature/example build only.
Reason / D-ID: D-05 interface change.
Status: OPEN

## REQ-impl-network-resources-005
From: impl-network-resources
To: impl-build-tooling (Cargo.toml, CI); Makefile part to whoever owns it at the time
Files/symbols: `Cargo.toml` new `[[example]]`; `.github/workflows/*` hardware jobs; Makefile `test-builtin`
Required edit:
```toml
[[example]]
name = "builtin_net_driver"
path = "tests/builtin_runtime/network_resources/driver.rs"
required-features = ["builtin-runtime"]
```
Build it wherever `builtin_hw_driver` is built (`make test-builtin`, hardware CI). Hardware CI jobs additionally set `AWMAN_TEST_BUILTIN_NETWORK=1` (needs outbound network from the runner; the tests start their own loopback HTTP/DNS-free fixtures) and `AWMAN_TEST_BUILTIN_PRESSURE=1` (bounded guest OOM: 256 MiB VM) alongside `AWMAN_TEST_BUILTIN_REQUIRE_HW=1`.
Reason / D-ID: D-05 guest tests need a same-executable worker host (test executables are refused as worker hosts).
Tests / dependencies: all `builtin_hw_network_*`, `builtin_hw_memory_*`, `builtin_hw_vcpu_*` tests report BLOCKED until the example exists.
Status: OPEN

## REQ-impl-network-resources-006
From: impl-network-resources
To: impl-cache-testinfra
Files/symbols: `tools/isolated-test.sh`
Required edit: pass through (do not scrub) `AWMAN_TEST_BUILTIN_NETWORK`, `AWMAN_TEST_BUILTIN_PRESSURE` and `AWMAN_TEST_BUILTIN_NET_PUBLIC_URL` (optional override of the public HTTPS probe URL); scrub host proxy variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`, lower-case forms) for builtin runs so the tests prove guest traffic does not depend on them.
Reason / D-ID: D-05 gates.
Status: OPEN
## REQ-impl-build-tooling-001
From: impl-build-tooling
To: impl-guest-compat / impl-lifecycle
Files/symbols: tests/builtin_runtime/hardware.rs and/or tests/builtin_runtime/lifecycle.rs (owned outside build-tooling)
Required edit: Add a native gated scenario that launches current `awman` entrypoint with the typed builtin provider and synthetic guest, proves same-executable worker launch, guest identity, exit 37, no host helper/library extraction, and x86 writable kernel RAM. Consume `AWMAN_TEST_BUILTIN_ARTIFACT` so Apple release validation can boot the exact signed artifact; never substitute builtin_hw_driver as the artifact under test.
Reason / D-ID: D-03/D-13 require an actual-awman typed-provider boot and final artifact boot; CI/tooling cannot claim those from the existing standalone driver alone.
Tests / dependencies: Native KVM/HVF; `AWMAN_TEST_BUILTIN=1 AWMAN_TEST_BUILTIN_REQUIRE_HW=1`; fixture archive; Mac entitlement on actual awman.
Status: OPEN
## RESOLUTION-REQ-impl-network-resources-002
By: impl-sdk-worker
Files changed: `src/engine/container/builtin/msb_driver.rs`, `tests/builtin_runtime/runtime_refusal.rs`
Status: CODE APPLIED; `spec.check()` runs before SDK call. Every compiled network plan is deserialized into SDK `NetworkPolicy`; enabled and disabled branches explicitly apply policy, strictness, CA trust, DNS rebinding/nameservers and disabled TLS. Disabled branch calls `disable_network()` first. `sdk_accepts_every_compiled_network_policy` covers None/Public/Allowlist including host port, but cannot execute until verified payload/static native inputs and the vendored isolated SDK patch are supplied. Default-build Clippy `-D warnings` and six runtime-refusal tests pass.
Remaining dependency: impl-build-tooling REQ-impl-sdk-worker-001; P-INPUT feature build.
## DETAIL-REQ-impl-sdk-worker-001
By: impl-sdk-worker
Artifact: `/awman/context/workflow/0121/sdk-isolated-config-upstream.patch` is the exact one-method diff against pinned crates.io `microsandbox 0.7.2` source. `patch --dry-run -p1` succeeded against `/usr/local/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/microsandbox-0.7.2` without modifying the Cargo cache. Build-tooling should copy/apply it inside the owned vendored crate, preserve license, and record this diff in WI 0120. No root/third_party file was edited by impl-sdk-worker.
## REQ-impl-network-resources-007
From: impl-network-resources
To: impl-cache-testinfra
Files/symbols: `tests/builtin_runtime/gates.rs::builtin_hardware_tests_are_named_so_the_fast_tier_skips_them`
Required edit: the policy currently asserts `builtin_hw_*` tests live only in `hardware.rs` and that each body calls `scenario(`. With `mod network_resources;` registered it FAILS (verified in a scratch build: "network_resources.rs: builtin_hw_network_dns_auth_ca_proxy_mcp: only real-guest tests (and all of them) may be named builtin_hw_*"). Generalise it to: every `builtin_hw_*` test in any `tests/builtin_runtime/*.rs` (and subdirectories) must begin with a gate call — accept `scenario(` (hardware.rs) and `gate(test, Need::` (network_resources.rs; other new modules will add their own gate helpers) within its first 3 lines; non-`builtin_hw_` tests must not boot guests. Keep `bodies.len() >= 10` for hardware.rs.
Reason / D-ID: D-05/D-10 — new guest test modules are required by plan; the fast tier still skips them by the `builtin_hw` name prefix.
Tests / dependencies: after REQ-impl-network-resources-001 item 3.
Status: OPEN

## REQ-impl-network-resources-006 (amendment)
From: impl-network-resources
To: impl-cache-testinfra
Required edit: replace `AWMAN_TEST_BUILTIN_NET_PUBLIC_URL` in REQ-006 with the two names the tests actually read: `AWMAN_TEST_BUILTIN_NET_ALLOWED` (default `example.com`) and `AWMAN_TEST_BUILTIN_NET_DENIED` (default `example.org`). Also pass through `AWMAN_TEST_BUILTIN_REPORT` if not already.
Status: OPEN

## REQ-acquisition-002
From: impl-acquisition
To: impl-cache-testinfra
Files/symbols: `tests/builtin_runtime/main.rs` module registry (resolves REQ-plan-001 for the acquisition module)
Required edit: add `mod acquisition;` next to the existing `mod binary;` … `mod state_dir;` lines. The file `tests/builtin_runtime/acquisition.rs` exists now; it `#[path = "../oci_import/support.rs"] mod oci_support;` (public awman API only, no feature cfg) and uses `crate::gate::hardware_or_skip` and `crate::hardware::{driver_path, fixture_archive}` (both already `pub`). No new cfg: the file compiles on the default build and its single test `builtin_hw_cached_execution_after_sources_stop` reports SKIP/BLOCKED through the existing gate. Please confirm it appears in `cargo test --test builtin_runtime -- --list`.
Reason / D-ID: D-02 (cached execution after the source stops, empty PATH), D-15 evidence row. `autotests=false` means an unregistered file is zero coverage.
Tests / dependencies: none beyond the existing hardware gate. `make test-builtin` / `AWMAN_TEST_BUILTIN_REQUIRE_HW=1 make test-builtin` pick it up once registered.
Status: OPEN

## RESOLUTION-REQ-impl-network-resources-003
By: impl-lifecycle
Files changed: `src/engine/container/builtin/backend.rs`, `src/engine/container/builtin/backend/lifecycle_tests.rs`, `src/engine/container/builtin/exec_bridge/bridge_tests.rs`
Status: CODE APPLIED. Both foreground and background `SandboxSpec` now use `network::compile(&self.settings.network)`. Test literals carry default network plans/settings. `guest_oom_exit_137_is_preserved_and_cleanup_still_runs` asserts exit 137 and owned removal. Native network/OOM execution remains BLOCKED by P-INPUT/P-LA/P-LX/P-MAC; feature compilation is not yet possible here.

## REQ-impl-lifecycle-001
From: impl-lifecycle
To: impl-guest-compat
Files/symbols: `tests/builtin_runtime/hardware.rs::Run::facts_for`
Required edit: `cargo clippy --locked --bin awman --lib --tests -- -D warnings` currently fails because `facts_for` is dead code in this foreign-owned file. Use it in a registered test, remove it if unnecessary, or otherwise resolve the warning without suppressing unrelated diagnostics.
Reason / D-ID: D-06/D-10; all-tests clippy gate for lifecycle handoff.
Tests / dependencies: default-feature all-tests clippy. Last observed 2026-09-28 15:12 UTC.
Status: OPEN

## PROGRESS-REQ-plan-004 (impl-lifecycle portion)
By: impl-lifecycle
Files changed: `src/engine/container/builtin/backend.rs`
Status: PARTIAL. After `acquire`, builtin import now takes `OciCache::lease_archive(&acquired.archive)` and holds it through projection, SDK import and identity publication; a concurrent prune before lease acquisition yields an explicit retry error, never use of a stale path. The cache owner has not yet published an atomic acquisition-return lease API or installed-image use lease, so the pre-lease window and import/use/remove races remain open. `msb_driver.rs` and `engine/oci/mod.rs` are owned by other steps. Do not close REQ-plan-004 from this record.

## REQ-impl-lifecycle-002
From: impl-lifecycle
To: impl-cache-testinfra
Files/symbols: `tests/builtin_runtime/main.rs`; `tests/builtin_runtime/gates.rs` if its hardware naming policy needs extension
Required edit: register `mod lifecycle;` (new owned `tests/builtin_runtime/lifecycle.rs`). Its `builtin_hw_actual_awman_cli_synthetic_agent_exit_37` body calls local `gate(TEST)` after the TEST constant. The gate runs the exact `AWMAN_TEST_BUILTIN_ARTIFACT` if set, otherwise `CARGO_BIN_EXE_awman`; requires `AWMAN_TEST_BUILTIN=1`, hypervisor, promoted fixture archive and builtin info route. It performs actual `awman ready --json` and `awman exec prompt` with a synthetic guest `claude` file mount, asserts guest UID 1234, prompt, stderr, exit 37, no host container helper. No SDK driver is invoked. Ensure `--test builtin_runtime -- --list` discovers it, and the fast tier skips it by name. Do not count a gate-off run as execution evidence.
Reason / D-ID: D-03/D-06/D-13 entrypoint proof gap. The file exists but `autotests=false` makes it zero coverage until registered.
Tests / dependencies: native feature build, KVM/HVF, promoted fixture archive; `AWMAN_TEST_BUILTIN_ARTIFACT` for exact signed artifact. This container cannot run it; runtime SDK patch and native hosts remain prerequisites.
Status: OPEN

## PROGRESS-REQ-impl-build-tooling-001 (impl-lifecycle portion)
By: impl-lifecycle
Files changed: `tests/builtin_runtime/lifecycle.rs`
Status: PARTIAL. A real `awman ready` + `awman exec prompt` synthetic guest test body now exists and selects `AWMAN_TEST_BUILTIN_ARTIFACT` when provided. It asserts UID 1234, prompt, stderr, exit 37 and no host container helper. It cannot run here: no KVM, SDK isolated-config patch absent, and `tests/builtin_runtime/main.rs` registration is pending REQ-impl-lifecycle-002 with cache-testinfra. No x86 writable-RAM tracing or exact final signed Mac artifact has been exercised. Keep this request open until native artifact evidence is recorded.

## REQ-impl-lifecycle-003
From: impl-lifecycle
To: fix-codex-area / final-verification
Files/symbols: lifecycle-owned `tests/builtin_runtime/lifecycle.rs` and related `src/command/`, `src/frontend/`, builtin lifecycle files
Required edit: D-06 mandatory coverage remains absent after this step: actual-awman synthetic-guest TUI/API/workflow setup/teardown/failure and squad dispatch/discovery/concurrency; startup/import/exec cancellation and parent/worker crash/stale-orphan recovery; two actual-awman build replacement on Linux and Mac, including protocol/catalog compatibility and deleted/replaced executable. Implement and execute on native hosts with genuine old/new artifacts. Preserve the strict owner-exit reattachment refusal. Do not convert fake-driver or gate-off tests into PASS. Native CLI smoke added by this step needs REQ-impl-lifecycle-002 registration and execution first.
Reason / D-ID: D-06 remains FAIL by absent mandatory end-to-end scenarios; D-03/D-13 contribution remains absent evidence.
Tests / dependencies: P-GIT/P-OLD/P-INPUT/P-LA/P-LX/P-MAC and signed final artifact for release case.
Status: OPEN

## REQ-fix-codex-area-001
From: fix-codex-area
To: fix-claude-area (impl-cache-testinfra and impl-guest-compat ownership)
Files/symbols: `tests/builtin_runtime/gates.rs::builtin_hardware_tests_are_named_so_the_fast_tier_skips_them`; `tests/builtin_runtime/hardware.rs::Run::facts_for`
Required edit: Generalize the hardware naming gate so registered `acquisition.rs`, `network_resources.rs`, and `lifecycle.rs` guest tests that invoke their required gate are accepted, while preserving the fast-tier skip/name invariant. Make `facts_for` used by a meaningful assertion or remove the unused helper; do not suppress Clippy.
Reason / D-ID: `PATH=/usr/local/cargo/bin:$PATH make test-fast` currently fails in `gates.rs` on `builtin_hw_cached_execution_after_sources_stop`; `cargo clippy --all-targets -- -D warnings` fails on unused `facts_for`. Both are outside fix-codex-area allowlist. Earlier REQ-impl-network-resources-007 and REQ-impl-lifecycle-001 cover related issues, but these are current rerun results.
Tests / dependencies: both exact commands above must pass before final verification.
Status: OPEN

## REQ-fix-codex-area-002
From: fix-codex-area
To: fix-claude-area (impl-network-resources ownership)
Files/symbols: `src/data/config/env.rs`; consumed by `src/engine/container/builtin/embedded/mod.rs` (fix-codex-area owned)
Required edit: Add a typed Layer 0 helper such as `pub fn host_var_os(name: &str) -> Option<std::ffi::OsString>` that preserves non-UTF-8 presence and daemon overlays, with a test for non-UTF-8 process input. The SDK resolver must refuse every ambient `MSB_*` override even when its value is non-UTF-8. The current `std::env::var_os(variable)` in `embedded/mod.rs:23` preserves that security property but fails `make architecture-lint`; `host_var` alone loses non-UTF-8 values. I will change the SDK resolver to call the helper when available.
Reason / D-ID: D-07/D-10; pre-push architecture lint fails now. Do not weaken the override check or bypass the linter with alternate direct process-env iteration.
Tests / dependencies: `make architecture-lint`; non-UTF-8 override regression in the owned SDK module.
Status: OPEN

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
| REQ-acquisition-001 | FAIL: requested tls_docker.rs and required-client-certificate scenarios are absent; no speculative dependency added for nonexistent code. |
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

## Completion follow-up — 2026-09-28

This supersedes earlier descriptions of the implementation, without closing
unexecuted mandatory gates:

- REQ-plan-004: atomic archive handoff implemented. `ImageAcquirer::acquire`
  returns `LeasedImage`; lookup, publication and staging use leased APIs. The
  backend holds the result through projection/import/identity publication.
  Clones retain the lease. The child-process prune regression covers the
  callback before acquisition returns, cached hits, clone handoff and actual
  projection. Installed-image use/removal stress and native import remain open.
- REQ-acquisition-001: Rustls is now an explicit normal dependency, because
  the shared production API/squad HTTPS server also needs provider selection.
  Both fixture and production servers select ring per server; production keeps
  axum-server's h2/http/1.1 ALPN. No process-global provider override is added.
  The missing real mTLS/service matrices remain FAIL/BLOCKED independently.
- No fifth vendor patch was introduced. The direct feature-gated network
  dependency uses the already-pinned 0.7.2 SDK evaluator to test UDP denial.
  WI 0120 records the two manifest integration changes.

Execution logs and final results for this follow-up are in `completion/` and
will be linked from the durable evidence register. No unavailable native,
real-service or signing execution is resolved as PASS by this note.

## Final resource-qualified continuation — 2026-09-28

All requests have an explicit applied or follow-up disposition; none is silently
closed. This section supersedes earlier statements that the new code is absent.

- REQ-plan-001/-006: guest_compat is now implemented/registered (actual awman,
  non-root nine-agent named/all-skills scenarios). Remaining artificial strategy
  combinations/live multi-consumer refresh/default-override coverage is assigned
  to WI 0123, FAIL until implemented and BLOCKED until native execution.
- REQ-plan-002 and network-002/-003: explicit strict_sni plus network/types SDK
  patches now allow bound visible SNI without interception; positive TLS/data and
  negative no-dial SDK tests execute. Native network/auth/MCP/resource matrix is
  WI 0123. Native unverified is not PASS.
- REQ-plan-003/-004: isolated builder retained; archive leases plus per-image
  plan/import/remove leases are wired. Cross-process locks and plan-vs-remove
  regression execute. Native coexistence/crash/stress is WI 0123.
- REQ-plan-005 and acquisition-001: real Docker Unix/TLS/mTLS scenarios and
  required-auth/private-CA/observed-proxy registry cases now exist. Existing
  Rustls dependency serves real fixture and API/squad code. No separate file
  named tls_docker.rs is needed: cases are in real_stores.rs. Remaining real
  expiry/disconnect/retry scenarios and actual services are WI 0123.
- REQ-impl-lifecycle-003: broader actual frontend/workflow/squad/crash/upgrade
  scenario implementation remains FAIL and is explicitly assigned WI 0123.
- SDK-worker-002/build-tooling-001 and all native-execution portions: BLOCKED,
  assigned WI 0123's capable runners, with release signing/tracing/old-new builds.
- All previous APPLIED registration/config/gate/lint/environment requests remain
  applied. Missing Apple adapter is WI 0122, with native implementation required.
- The manifest now carries six vendor patches. Network and types are separately
  scoped and recorded in WI 0120; the direct SQLx feature edge supports genuine
  transaction tests. Normal Rustls and direct dependencies are not extra patches.

See closure/finding-dispositions.md and closure/command-exits.tsv. Creating these
follow-ups resolves request ownership, not their mandatory runtime acceptance.
No remote resource-qualified agent has run from this container.
