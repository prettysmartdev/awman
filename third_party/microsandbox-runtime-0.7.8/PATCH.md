# Private kernel memory and lifecycle locks

Published crate: `microsandbox-runtime 0.7.8`, SHA-256
`b9748ae73d3b183f52f13bf1c41fe216268978ff24fbd8a3a628f14cf8cc874e`.
Upstream revision: `7b7b9dc89e9a1c77801918f7833c3381d1754579`.
License: Apache-2.0; LICENSE is the upstream text (SHA-256
`a276ca3381fefb9cde42fccae847856085c76027557d62eee83f057eb6c53433`).

`upstream.diff` (SHA-256
`ad1a9ff8973c5029b85d35ad715652d1605cc88d9e8b2eeddf70d77c8e77caa9`)
enables `Vm::set_private_memory_boot(true)` for fresh Linux x86_64 boots. The
published `msb_krun =0.1.41` private boot path copies the callback kernel into
independently owned guest RAM. This preserves the static callback's idempotence
without mapping its shared allocation writable into guests. Checkpoint restore
retains its existing memory ownership path; awman's worker refuses restore
invocations.

The Unix lifecycle lock is created with mode `0600`, instead of inheriting a
potentially group-writable `0666 & ~umask` mode. The private worker requires a
regular owner-writable lock with no group/other writers; common developer umask
`0002` otherwise prevents every guest from starting. Existing file permissions
and the worker's validation are unchanged. The upstream exclusivity/reuse test
also checks that newly created locks grant no group/other access.

Reproduce by verifying and extracting the published crate, then applying
`patch -p1 < upstream.diff`. No other upstream source is changed.

Validation: registered guest boot, concurrent guest lifecycle and artifact boot
suites on Linux KVM. Native evidence is recorded with WI 0124; compilation alone
is not guest isolation evidence. The host-shims descriptor regression preserves
rejection of group/other-writable locks. Repeat native boot with umask `0002`.

Removal gate: a pinned upstream runner must request private kernel memory on
x86_64 and create private lifecycle locks independently of umask; Linux KVM
concurrent boot and optimized artifact suites must pass with umask `0002`.
