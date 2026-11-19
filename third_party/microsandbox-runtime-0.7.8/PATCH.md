# Private kernel memory for Linux x86_64

Published crate: `microsandbox-runtime 0.7.8`, SHA-256
`b9748ae73d3b183f52f13bf1c41fe216268978ff24fbd8a3a628f14cf8cc874e`.
Upstream revision: `7b7b9dc89e9a1c77801918f7833c3381d1754579`.
License: Apache-2.0; LICENSE is the upstream text (SHA-256
`a276ca3381fefb9cde42fccae847856085c76027557d62eee83f057eb6c53433`).

`upstream.diff` (SHA-256
`c0026a1efc4f5710f53a1222024bfd166f22474c2a27ea65c0acadaeb05b6003`)
enables `Vm::set_private_memory_boot(true)` for fresh Linux x86_64 boots. The
published `msb_krun =0.1.41` private boot path copies the callback kernel into
independently owned guest RAM. This preserves the static callback's idempotence
without mapping its shared allocation writable into guests. Checkpoint restore
retains its existing memory ownership path; awman's worker refuses restore
invocations.

Reproduce by verifying and extracting the published crate, then applying
`patch -p1 < upstream.diff`. No other upstream source is changed.

Validation: registered guest boot, concurrent guest lifecycle and artifact boot
suites on Linux KVM. Native evidence is recorded with WI 0124; compilation alone
is not guest isolation evidence.

Removal gate: a pinned upstream runner must request private kernel memory on
x86_64, and Linux KVM concurrent boot and optimized artifact suites must pass.
