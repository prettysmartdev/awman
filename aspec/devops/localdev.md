# Local Development

WI 0144 remains an incomplete shadow migration. Static checks passing do not
certify a working production evidence chain or complete legacy replacement;
see [replacement-ledger.md](replacement-ledger.md) for code blockers and cutover.

Development: local
Build tools: make, cargo, docker

Cargo workspace: the root `awman` package owns the Layer 0–3 library;
`crates/awman-cli` owns the single `awman` executable and the integration test
binaries; `crates/awman-host-shims` owns the builtin worker and native Apple XPC
transport; `crates/awman-wsp` owns the work-server protocol;
`crates/awman-test-support` is the test harness; `xtask/` is the repository's
task runner (`cargo xtask`). The worker remains a private re-exec route of the
same executable. Use `-p awman-cli --bin awman` when selecting the executable by
package. Static builds use
`cargo build --workspace --release --bins --target x86_64-unknown-linux-musl`.

## Workflows:

Developer Loop:
- running `make all` should build the aspec CLI binary using the local Rust/Cargo toolchain
- running `make install` should build and sign as the invoking user, then install the awman CLI to /usr/local/bin/, using sudo only for the copy if needed; the installed macOS signature is verified afterwards
- `make check` is the fast inner loop: every lint (`cargo xtask lint all`) and the unit tier of the crates you changed
- `make pre-push` is `make check` plus `make test`; run it before pushing

Local testing:

<!-- gates:begin contract -->
Generated from `ci/gates.toml` (schema 1).

| Gate | Budget (minutes) | Required status |
| --- | --- | --- |
| candidate | 20 | gate |
| main | 20 | gate |
| nightly | 60 | gate |
| pr | 10 | gate |
| promote | 20 | gate |

| Tier | Per-test deadline (seconds) | Guarantees |
| --- | --- | --- |
| artifact | 300 | tempdir, loopback, subprocess, release-artifact |
| boundary | 120 | tempdir, loopback, subprocess, awman-binary, fake-cli, local-git |
| component | 20 | tempdir, loopback, subprocess, awman-binary, fake-cli, local-git |
| guest | 300 | tempdir, loopback, subprocess, awman-binary, hypervisor, builtin-payload, guest-fixtures, previous-release |
| soak | suite-specific | tempdir, loopback, subprocess, awman-binary |
| unit | 1 | tempdir |

Unit, lint, doctest and parity suites always run. Unknown/shared paths select the full PR set. Main and merge-group run the full set. Required skips, missing evidence and expired quarantine fail the gate.
<!-- gates:end contract -->

Tests are organised in tiers and selected by suites defined in `ci/gates.toml`;
[testing.md](testing.md) explains the tiers, the support crate and how to write
and reproduce tests. Locally:

| Command | What it runs | Needs |
|---|---|---|
| `make check` | `cargo xtask lint all` and `cargo xtask test run --tier unit --changed` | pinned nextest; `cargo-deny` and `shellcheck` are skipped with a notice when absent (CI has neither exemption) |
| `make test` | `cargo xtask test run --gate local`: the `unit` and `component` tiers of this host's build, both backends (builtin and daemon) | pinned nextest |
| `make test-unit` / `make test-component` | one tier | pinned nextest |
| `make test-boundary AWMAN_TEST_HOST_RESOURCES=…` | the `boundary` tier against the real engines and services you name | the named resources |
| `make test-guest` | fetches the pinned guest fixtures, then the `guest` tier | Apple Silicon or KVM, verified payload (`make payloads`), Docker or Apple Containers for the fixtures |
| `make test-artifact ARTIFACT=/abs/path/awman` | the `artifact` tier against a release executable | the executable |
| `make gates` | `cargo xtask gates check`: manifest, generated files, Makefile contract and these documents | none |
| `make architecture-lint` | the layering lint, its own test, and `cargo xtask lint hygiene` | none |
| `make legacy-test` | the retained legacy integration targets through `tools/isolated-test.sh`; `LEGACY_DOCKER=1` adds their real-Docker cases | see the `[[legacy_fallback]]` rows |

- `cargo-nextest` is required and pinned. `cargo xtask test install-runner`
  installs the pinned, SHA-256-verified release into `target/xtask-tools/`;
  `make test` and `make check` stop with that command if it is missing or a
  different version. They use the same nextest profile as CI
  (`.config/nextest.toml`, generated from the manifest: per-tier timeouts,
  resource groups, zero retries). `cargo xtask test run --runner libtest` falls
  back to the plain harness for debugging only.
- `make test` and `make check` never touch a real container engine, guest,
  keychain, clipboard, daemon manager, Git configuration, home directory or the
  network. Isolation is a property of the tests themselves
  (`crates/awman-test-support`: a private `HOME`, XDG and `AWMAN_HOME`, supervised
  children, owned ports), so a bare `cargo test` or `cargo nextest run` is
  equally safe; there is no wrapper script and no `AWMAN_TEST_*` opt-in. Real
  resources are used only when named.
- Run one suite or one test: `cargo xtask test run --suite component-engine`, or
  `cargo nextest run -p awman-cli --test component -E 'test(=engine::git::clone_over_ssh)'`.
  `cargo xtask test list` prints the expected test set.
- `AWMAN_TEST_HOST_RESOURCES` is a comma-separated list of `docker=<DOCKER_HOST>`,
  `local-registry=<ip:port>`, `git-transports`, `docker-base-image=<reference>`,
  `apple-containers`, `docker-sbx`, `hypervisor`, `builtin-payload=<dir>`,
  `offline`, `public-network`, `registry-credentials=<file>` and
  `private-registry-reference=<repository>:<tag>`. Example:
  `make test-boundary AWMAN_TEST_HOST_RESOURCES=docker=unix:///var/run/docker.sock,git-transports`.
  A test whose capability is not named reports `BLOCKED`; it never passes and
  never skips.
- The boundary job contract: `docker` is the engine the tests may create, inspect
  and remove containers and images on; `local-registry` is a loopback OCI
  registry started outside the test; `git-transports` uses the host `sshd` and
  `git http-backend`; `docker-base-image` names the base image the build tests
  use.
- Fixtures the guest and artifact tiers need are content-addressed:
  `cargo xtask fixtures fetch [--tier guest]` downloads and verifies them into
  the fixture store and `cargo xtask fixtures verify` re-checks the store. A
  fixture whose pin is still `pending` in `ci/gates.toml` fails closed as
  `BLOCKED`.
- The workspace test profile disables debug information and incremental
  artifacts to keep the linked test binaries within small runners' disk budgets.
  Tests remain unoptimised with debug assertions enabled. For debugger symbols,
  use `CARGO_PROFILE_TEST_DEBUG=2 make test`.
- Property tests use `proptest` and snapshot tests use `insta`; regenerate
  snapshots and golden files with `cargo xtask test update-golden --suite <id>`.
  `make docs-reference` regenerates `docs/14-command-reference.md` from the
  command catalogue.
- On macOS, Hypervisor.framework refuses unsigned processes. `make build`
  ad-hoc signs the Apple Silicon release binary with
  `packaging/macos/awman.entitlements` (`tools/macos-sign.sh`); `make sign-macos
  BIN=<path>` re-signs a copied or modified binary. The guest tier needs signed
  test executables and drivers (`tools/macos-sign-test-artifacts.sh`, which also
  signs cargo's `deps/` originals because cargo re-copies them on every build);
  a missing entitlement is reported as `BLOCKED` with that instruction.
- Guest scenarios (boot, exec, import, network, upgrade, builder, strategy) run
  only in the `guest` tier on a host that can boot a guest. A missing
  `/dev/kvm`, denied KVM access, missing Apple hypervisor entitlement,
  unverified target payload, unpinned fixture or unsupported host is `BLOCKED`
  with the concrete prerequisite recorded; it is never a passing boot test. The
  upgrade scenario needs the pinned previous release (`previous-release`);
  nobody builds an older awman by hand.
- SQLite and native dependency changes are covered by the guest-tier tests, the
  catalog compatibility check (`tools/test-msb-catalog-compat.sh`) and the
  release artifact dependency scan (`artifact-deps-scan`). Inspect a final
  feature artifact for dynamic dependencies and extracted-helper candidates with
  `bash tools/release-artifact-check.sh` and
  `bash tools/measure-release-artifact.sh`. The runtime must work offline with a
  minimal executable search path after build; no production runtime path may
  depend on `tools/` scripts, Cargo caches or temporary spike artifacts.
- The builtin driver uses the vendored `build_lazy_isolated()` entry point on
  pinned `microsandbox 0.7.8`. It starts from independent defaults and
  awman-owned paths without merging installed SDK config.
  `third_party/microsandbox-0.7.8/upstream.diff` contains the one-method patch
  selected in Cargo.toml and Cargo.lock; WI 0120 records its removal gate. The
  guest kernel comes from the published `msb_krun =0.1.41` `static-krunfw`
  feature: awman defines the `krunfw_get_kernel` callback in
  `crates/awman-host-shims/src/kernel.rs`, and no `msb_krun` patch is vendored.
  In the final binary `krunfw_get_kernel` is visible to `nm` in debug and plain
  release builds but not after `strip` or under fat LTO, so artifact scans must
  not require the symbol name. `make payloads` fetches and checksum-verifies the
  host's payload (`tools/msb-payloads/fetch.sh`); `make payloads-verify` is the
  promotion path for a new payload pin and needs a clean native checkout. Do not
  replace this with process-global environment mutation.
- Known issue: `tools/msb-payloads/payloads-verify.sh` still drives its gate
  through the retired `test-builtin` Make target, so `make payloads-verify` fails
  at that step until the script moves to `cargo xtask test run --tier guest,artifact`.
  Use `make test-guest` and `make test-artifact` directly meanwhile.

### Pinned tooling

| Tool | Version | Used for | Installed by |
|---|---|---|---|
| Rust toolchain | 1.94.0 (`rust-toolchain.toml`) | everything | `rustup` |
| `cargo-nextest` | 0.9.150 | every test run, local and CI | `cargo xtask test install-runner` (verified release archive) |
| `cargo-llvm-cov` | 0.6.21 | nightly diagnostic coverage per layer | `cargo install --locked --version 0.6.21 cargo-llvm-cov` |
| `cargo-mutants` | 25.3.1 | nightly sampled mutation testing of `src/data` and `src/command` | `cargo install --locked --version 25.3.1 cargo-mutants` |
| `cargo-deny` | 0.19.0 | licences, bans, sources (`deny.toml`) and nightly advisories | `cargo install --locked --version 0.19.0 cargo-deny` |
| `actionlint` | 1.7.7 | workflow lint | `cargo xtask ci install-tool actionlint` (verified archive) |
| `shellcheck` | 0.11.0 | shell lint | `cargo xtask ci install-tool shellcheck` (verified archive) |
| `proptest` | 1.12.0 | property tests (workspace dev-dependency, exact pin) | `Cargo.toml` |
| `insta` | 1.49.0 | snapshot tests (workspace dev-dependency, exact pin) | `Cargo.toml` |

The manifest's `[tools.*]` tables are the source of these versions. Any change
to a pin is a manifest change reviewed together with its digest.

Version control:
- Git is used for this project

Documentation:
- After every work item is implemented, documentation should be written within the docs/ folder. Do not create one document per work item, but instead author a comprehensive set of documentation that explains to the user how to use the aspec tool in its entirety. Each work item should trigger an inspection of the entire docs/ folder to update and/or add relevant usage information.

## Profiling

### tokio-console (task lifetime visualisation)

> **Unavailable.** The `tokio-console` feature and `console-subscriber` dependency are not present in the current manifests. Work item 0040 describes a proposal, not an implemented feature.

`tokio-console-subscriber` will be gated behind a `tokio-console` Cargo feature flag so it is never compiled into release builds.

If the feature is added in a future change, enable it with:

```sh
cargo run --features tokio-console
```

Then in a separate terminal:

```sh
tokio-console
```

This shows all live Tokio tasks, their poll counts, and idle/busy times — useful for diagnosing task starvation or orphaned tasks.

**Install tokio-console CLI:**

```sh
cargo install tokio-console
```

### Flamegraph profiling

Install `cargo-flamegraph`:

```sh
cargo install flamegraph
```

Produce a CPU flamegraph for the binary (requires sudo on Linux for perf):

```sh
sudo cargo flamegraph --bin awman -- exec workflow path/to/workflow.yaml --non-interactive
```

The output is `flamegraph.svg` in the current directory.

### Heap profiling

On Linux, use `heaptrack` to measure heap allocations:

```sh
# Install heaptrack (Ubuntu/Debian)
sudo apt install heaptrack

# Profile awman
heaptrack ./target/release/awman implement 0001 --non-interactive
heaptrack_gui heaptrack.awman.*.zst
```
