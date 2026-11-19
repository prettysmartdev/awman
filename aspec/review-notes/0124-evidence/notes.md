# Linux x86_64 native acceptance

The builtin runtime passed native Linux x86_64 acceptance on 2026-11-19.
Linux ARM64 remains gated. This record validates the source and executable below;
subsequent changes require the relevant gates again.

## Identity and retained evidence

| Item | Value |
|---|---|
| Source commit | `50cb794ed6a613fefbe114caf8648e0a00db346e` |
| Source tree | `0f07fbb76f12a5ac8293ed042cd5ac46572f4572` |
| PR / base | [#75](https://github.com/prettysmartdev/awman/pull/75), `nextgen` |
| Packet | `20261119T172839Z-159092.tar.gz` |
| Packet SHA-256 | `2f49431756528d8226e4769de5d3a6a1d89919a38d4b7f52842e9e6b49abcbfe` |
| Candidate and installed SHA-256 | `f927569ed069ffc073f4e1a7dafb9317caf419ce8219323e4b02117bd1c85386` |
| OCI fixture SHA-256 | `c86291072784402551a5a223eaca257e3affd7ef37b6bb5bd2402f494f353f6f` |
| Docker-save fixture SHA-256 | `2dde9194d583e4fe853aa7a1089cc78a30388b61faf7b78e31d2373b910a3faf` |
| Started / finished (UTC) | `2026-11-19T17:28:39Z` / `2026-11-19T17:44:11Z` |

The archive is retained on the supplied KVM host under
`/home/debian/Workspaces/prettysmart/awman/target/native-validation/`, and copied
to `/workspace/target/linux-native-evidence/`. These are retained workspace
artifacts, not published release assets. The extracted `evidence/` directory
contains the candidate, fixture archives, source identity, independently compiled
inventories, command logs, per-suite JUnit, ABI/measurement records and packet
file digests. Installation evidence is retained separately as
`target/linux-install-verification.log` on the host and
`/workspace/target/linux-native-evidence/linux-install-verification.log` locally.

## Native execution and independent review

Host: Debian 13, x86_64, four CPUs and 8 GiB RAM, ordinary `debian` user with KVM
and Docker access, Rust 1.94.0 and Docker 26.1.5. Command:

```sh
CARGO_BUILD_JOBS=2 make linux-prerelease
```

The recipe sets umask `0002` in its subprocess. Offline tests enforce kernel
seccomp restrictions while preserving the user's identity and Unix IPC.

All 19 suites passed, with exactly 86 JUnit cases:

| Coverage | Passing cases |
|---|---:|
| Offline enforcement / runtime conformance / Docker sources / registry / templates | 1 / 3 / 8 / 10 / 17 |
| Guest boot / Docker builder / execution / image import | 1 / 1 / 12 / 2 |
| Guest executable replacement / networking / agent strategies | 1 / 2 / 16 |
| Optimized artifact suites | 12 |

The exact published libkrun private-memory mapping checks passed five additional
tests. The guest cases cover independently owned concurrent VMs, non-root image
identity, real mounts and output receipts, CPU/memory configuration, lifecycle,
source deletion and cached execution. Builder coverage includes automatic
preparation, forced rebuild, a failed read-only source inspection with an enabled
builder, and cached execution with the builder disabled and no CLI probes.

The optimized candidate passed dependency/provider checks, entrypoints, enforced
offline boot and cached executable replacement, measurements, smoke, prohibited
switches and upgrade-policy checks. The first Linux builtin release has no
published predecessor: replacement is not claimed as historical cross-release
migration. Pinning a predecessor automatically adds the genuine old/new suites.

The official packet verifier passed on the native host and on the copied packet
in a clean checkout of the recorded commit. An independent audit also reconciled
every nonempty compiled inventory with its passing JUnit results, checked all
183 file hashes and 59 successful command exits, verified umask `0002`, and
matched registry creation/removal identity. No required test was missing,
ignored, skipped or duplicated. Filtered tests belonging to other suites do not
count as required cases.

## Installation, artifact and ABI

Actual normal-user installation passed:

```sh
umask 0002
CARGO_TARGET_DIR=target/native-validation-build CARGO_BUILD_JOBS=2 make install
/usr/local/bin/awman --version
/usr/local/bin/awman __awman-builtin-kernel-check
```

The installed executable is root-owned mode `0755`, reports `awman 0.13.0`,
validates the embedded kernel and agent without host helpers, and matches the
candidate byte-for-byte. Runtime dependencies are embedded; the documented OS
C ABI and unwinder remain dynamic.

The candidate is 124,844,216 bytes. Its recorded maximum required glibc symbol
version is **2.39**, reflecting the Debian build host. This is **not** evidence
for a glibc 2.35 distribution artifact. Published Linux assets are separately
built on Ubuntu 22.04 and checked against the glibc 2.35 ceiling. Native-host
measurements are informational, not release performance thresholds.

The normal installation build exposed inherited `cargo run` package metadata
invalidating dependency fingerprints. The native runner now removes that parent
metadata while retaining build configuration. The final candidate build reused
the normal installation cache in 1.085 seconds. The earlier full optimized
native build took 24m04s; the normal installation rebuild took 26m10s.

## Other validation and upstream work

The full local manifest gate passed on `5a8b2218`, which has the same runtime
sources as the recorded native commit. Later changes corrected native tests,
evidence collection, Cargo environment handling and documentation. All 202
xtask unit tests passed after the tooling fixes; xtask Clippy passed with warnings
denied after the test-module placement correction.

[CI on the runtime implementation](https://github.com/prettysmartdev/awman/actions/runs/44565928200)
passed every required job on `a5f79344`. The
[recorded native commit's CI run](https://github.com/prettysmartdev/awman/actions/runs/44577503333)
and [PR checks](https://github.com/prettysmartdev/awman/pull/75/checks) track later
validation. Current PR readiness requires its latest-head checks; this record
does not predeclare future CI outcomes.

Earlier rejected packets are not acceptance evidence. In particular, the first
complete runtime-test run passed its tests but omitted JUnit copies because the
collector incorrectly derived nextest's report directory from Cargo's build
directory. The successful packet above includes fresh XML and passed the strict
inventory reconciliation after that collector was fixed.

Saved upstream proposals 01–07 were reconciled with current upstream source,
including private x86 kernel memory and private Unix lifecycle locks. Their
publication/removal gates remain in WI 0120 and `third_party/` provenance. The
newer upstream versions were not adopted or claimed as native-validated here;
no upstream branches, commits, issues or PRs were submitted.
