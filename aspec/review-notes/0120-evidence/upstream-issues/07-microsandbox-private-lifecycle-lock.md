> Saved proposal only; not submitted. Reconciled 2026-11-19 against the exact base below. Upstream compilation and native acceptance remain required before submission.

# Create private Unix lifecycle locks independently of umask

**Repository:** `superradcompany/microsandbox`

**Base:** `main` @ `abff71505fc46416b3d704ee2787f8e55ae66efa` (0.7.16).

The Unix lifecycle guard creates an inherited worker lock using the default
`0666 & ~umask` mode. With a common developer umask of `0002`, the resulting
`0664` lock is group-writable. An embedding worker that validates inherited
descriptors as private correctly refuses to start. This caused actual Linux
KVM boot failure in awman's pinned 0.7.8 runtime.

Request `0600` when creating the lifecycle lock. Existing files are not chmodded,
the process umask is not changed, and the descriptor validation contract remains
strict. The lifecycle exclusivity/reuse regression also checks that newly
created locks have no group/other permissions.

Before submission, run the focused lifecycle tests with umask `0002` and `0077`,
including inherited-descriptor ownership, then boot native guests on Linux and
macOS. Verify migration expectations for pre-existing permissive lock files;
this patch deliberately does not take ownership of or alter existing files.

Removal gate: pin a published runtime with equivalent creation permissions and
pass awman's native boot and exact optimized artifact acceptance with umask
`0002`. No wire or persisted guest-storage format changes are proposed.
