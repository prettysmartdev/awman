**Title:** `fix(runtime): isolate embedded kernel memory for fresh x86_64 VMs`

Reviewed against Microsandbox `abff71505fc46416b3d704ee2787f8e55ae66efa`
(0.7.16). Local proposal only; not filed.

The runtime's fresh Linux x86_64 boot path currently builds a libkrun VM and
enters it without requesting private-memory boot. An embedding application's
`static-krunfw` accessor can be idempotent and process-owned; mapping its buffer
as writable guest RAM allows one VM to alter the image reused by another VM.

Call the existing `Vm::set_private_memory_boot(true)` immediately after building
fresh Linux x86_64 VMs. Leave checkpoint restores on their own memory-installation
path. This requires no libkrun fork or new kernel-provider API.

Acceptance must cover libkrun's independent sibling mappings and immutable backing,
concurrent real guests, repeated boots, and optimized/LTO/stripped artifacts.
Awman's pinned 0.7.8 downstream patch and native Linux validation are separate
from validating this 0.7.16 proposal. Remove the downstream runtime patch only
after a published equivalent passes the same behavior contracts.
