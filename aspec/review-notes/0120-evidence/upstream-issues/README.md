# Upstream issue drafts (not filed)

Drafts for a maintainer to file by hand. Nothing here has been posted. Each issue
gives upstream the context for a change awman carries as a vendored patch today,
so that any later PR can link to an existing discussion.

Checked against upstream `main` on 2026-10-26: none of these changes has landed.

| File | Repository | Patch it would retire | Shape |
|---|---|---|---|
| [01-microsandbox-isolated-builder.md](01-microsandbox-isolated-builder.md) | `superradcompany/microsandbox` | `third_party/microsandbox-0.7.8/` | Additive SDK method |
| [02-microsandbox-artifacts-root.md](02-microsandbox-artifacts-root.md) | `superradcompany/microsandbox` | `third_party/microsandbox-filesystem-0.7.8/` | Additive build input |
| [03-microsandbox-strict-sni.md](03-microsandbox-strict-sni.md) | `superradcompany/microsandbox` | `third_party/microsandbox-network-0.7.8/` and `third_party/microsandbox-types-0.7.8/` (policy option) | Additive, default-off option |
| [04-microsandbox-suffix-sni-binding.md](04-microsandbox-suffix-sni-binding.md) | `superradcompany/microsandbox` | `third_party/microsandbox-network-0.7.8/` (policy tightening) | Behaviour change; asks a question |
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
- Ready-to-open PRs for all five live in [`../upstream-prs/`](../upstream-prs/). Each
  ZIP holds a `git diff` against upstream's default branch, plus a `PR.md` with the
  title, base commit and description. Fill in `#<issue-NN-number>` once the issues
  are filed. `../upstream-libkrun-docs.md` is the earlier 0.1.41-era draft of PR 05.
