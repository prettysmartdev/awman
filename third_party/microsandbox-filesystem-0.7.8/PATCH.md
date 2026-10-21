Published crate: `microsandbox-filesystem 0.7.8`, archive SHA-256
`e890749fd6c87b7966865a4f7a0add888926adce222fdd4525eb64e047fee30b`.
Upstream revision: `7b7b9dc89e9a1c77801918f7833c3381d1754579` (`crates/filesystem`).
The manifest declares Apache-2.0; `LICENSE` carries the same Apache-2.0 license text
from the upstream repository at the revision above, SHA-256 `a276ca3381fefb9cde42fccae847856085c76027557d62eee83f057eb6c53433`. Neither published filesystem crate includes a LICENSE file.

Apply `patch -p1 < upstream.diff` to the published crate. The patch changes only
`build.rs` and has SHA-256
`b87c1e5f67fe0b9b3044bc43ff332601233280012bd35ea4145b6b75b9feab61`.
This diff is byte-identical to the previous 0.7.2 carried diff (also recorded in the WI 0121 closure source hashes).
The unpatched upstream `build.rs` SHA-256 is
`22a3297cdb5f9261960b8b24867cfa5fcf80c28d122217ebc68122ae6291e390`; patched
`build.rs` SHA-256 is
`b517cd7532576194f00f30e73fdb00ad9e3dcfc200a8cf33127c9d84f036f77e`.

When `MSB_EMBED_ARTIFACTS_ROOT` is set, the build script selects
`<root>/<CARGO_CFG_TARGET_ARCH>/agentd`, validates it for the target, and stages it.
The existing `MSB_EMBED_ARTIFACTS_DIR` single-directory input and workspace-local
or download paths continue to work when the root variable is unset. The root lookup
is the awman target-aware guest-agent input; it does not enable host-bundle extraction.

Verification: workspace builtin builds and `make test-builtin` exercise target-aware agent staging; `tools/msb-payloads/verify.sh` verifies the selected Linux ARM64 bytes. Native Darwin/x86_64 gates remain unpassed.

Removal gate: pin an upstream release with equivalent architecture-aware `MSB_EMBED_ARTIFACTS_ROOT` handling, then pass payload verification, clean builtin builds and native guest/artifact tests before dropping this patch.
