# Upstream issue drafts (not filed)

Drafts for a maintainer to file by hand. Nothing here has been posted. Each issue
gives upstream the context for a change awman carries as a vendored patch today,
so that any later PR can link to an existing discussion.

Reconciled on 2026-11-19 against Microsandbox `abff71505fc46416b3d704ee2787f8e55ae66efa` (0.7.16) and libkrun `35cf9e345365b9500e3ef1c9823c3edcfb35a52b` (0.1.46). None of the proposed APIs has landed in those sources. Awman still uses pinned 0.7.8/0.1.41; adoption requires separate compatibility and native acceptance.

| File | Repository | Patch it would retire | Shape |
|---|---|---|---|
| [01-microsandbox-isolated-builder.md](01-microsandbox-isolated-builder.md) | `superradcompany/microsandbox` | `third_party/microsandbox-0.7.8/` | Additive SDK method |
| [02-microsandbox-artifacts-root.md](02-microsandbox-artifacts-root.md) | `superradcompany/microsandbox` | `third_party/microsandbox-filesystem-0.7.8/` | Additive build input |
| [03-microsandbox-strict-sni.md](03-microsandbox-strict-sni.md) | `superradcompany/microsandbox` | `third_party/microsandbox-network-0.7.8/` and `third_party/microsandbox-types-0.7.8/` (policy option) | Additive, default-off option |
| [04-microsandbox-suffix-sni-binding.md](04-microsandbox-suffix-sni-binding.md) | `superradcompany/microsandbox` | `third_party/microsandbox-network-0.7.8/` (policy tightening) | Behaviour change; asks a question |
| [06-microsandbox-private-memory-boot.md](06-microsandbox-private-memory-boot.md) | `superradcompany/microsandbox` | `third_party/microsandbox-runtime-0.7.8/` | Fresh x86_64 per-VM memory |
| [05-libkrun-static-krunfw-macos-alignment.md](05-libkrun-static-krunfw-macos-alignment.md) | `superradcompany/libkrun` | None; documentation only | Docs |

Filing notes:

- Microsandbox's feature-request template is empty and applies the `💗 enhancement`
  label. `CONTRIBUTING.md` sends bugs and feature requests to issues and broader
  design to discussions or Discord. It requires no issue before a PR, but PRs must be
  scoped to one change, use conventional commits, and include a summary and test plan.
- File 03 before 04. Issue 04 changes existing behaviour and references 03, so its
  link to 03 has to be filled in after 03 is filed.
- The `host.microsandbox.internal` → `host.builtin.internal` alias rename in the
  network patch is awman-specific, so no issue is drafted for it. If upstream ever
  makes the alias configurable, that rename can go too.
- Saved proposals for all six live in [`../upstream-prs/`](../upstream-prs/). Each
  ZIP holds a `git diff` against upstream's default branch, plus a `PR.md` with the
  title, exact base revision, description and validation status. These are not submission-ready until the outstanding upstream tests and maintainer decisions in each bundle are resolved. Fill in `#<issue-NN-number>` once the issues
  are filed. `../upstream-libkrun-docs.md` is the earlier 0.1.41-era draft of PR 05.

Proposal 03 must ship with proposal 04 or equivalent exact-name binding behavior
before it can retire awman’s complete strict-SNI boundary. Filing order is not
a statement that either proposal independently replaces the whole network patch.
