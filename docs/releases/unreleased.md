# Unreleased

Changes on `main` that have not yet been cut into a release. Grouped by what
they mean for you, not by where they were made.

The worker API and squad scheduler now share one daemon, one database and one execution capacity limit. Use `awman daemon start|stop|status|logs` for its lifecycle; the separate `api` and `squad` lifecycle commands have been removed.

Local clients use an owner-authenticated socket or named pipe. The optional network listener uses TLS and named API keys, exposes only configured features, and never exposes ephemeral features. Feature switches and network exposure changes apply live.

`awman edit spec|workflow|skill|agent <ref>` changes an existing artifact with an agent. It supports `--interview` (the agent questions you first) and `--amend` (the agent reconciles the artifact with the evidence) for every kind, `--global` for workflows, skills and agents, and `--headless`/`--json` with `--prompt`. Repo artifacts are edited in place, global and library artifacts on a staged copy that is validated and published back, and remote work items through a draft that is saved while the agent runs. Cancellation is not rollback, retained drafts can be recovered, and editing an agent definition does not approve or run it. See [Editing existing artifacts](../03-agent-sessions.md#editing-existing-artifacts).

**Removed: `awman specs amend`.** Use `awman edit spec <ref> --amend` instead (for example `awman edit spec 0042 --amend`, or `awman edit spec github:84 --amend`). The `specs` command group is gone and no alias is provided.

`awman link <server-url>` saves a work server link using device approval or a supplied token. `workServers.*` config manages the link, and unsetting the server also removes its stored token. Linked servers supply manual queue items and automatic pull work. `awman link web-<code>` approves a browser on the daemon's engine listener.

Work item input now uses the shared `--work-item <REF>` flag. The old
`--issue` flag has been removed. Migrate as follows:

| Old form | Use instead |
|---|---|
| `--issue 84` for a GitHub issue | `--work-item github:84` (or set `workItems.source = "github"` and keep the bare number) |
| `--issue owner/repo#84` | `--work-item owner/repo#84` |
| `--issue https://github.com/owner/repo/issues/84` | `--work-item https://github.com/owner/repo/issues/84` |
| `--work-item 42` for a local work item | Keep `--work-item 42` with `workItems.source = "local"` |

References are resolved strictly: a missing, ambiguous, inaccessible, or
unfetchable work item is now an error instead of continuing with empty input
or falling back to another source. Dynamic workflows also require a work
item. A regular workflow may still run without one when its template does not
need work item values.

## Canonical checkout names and paths — breaking beta change

awman is beta software and does not guarantee compatibility between versions.
Branches, worktrees and clones now have one naming rule per checkout type.
Creation, resume and recreation use the same canonical location; historical
names are no longer generated, searched or used as fallbacks.

| Retired case | Current behavior |
|---|---|
| Unhashed `~/.awman/worktrees/<repo>/` directories for work items, workflows and session worktrees | Only `~/.awman/worktrees/<repo>-<path-hash>/` is used. Name lookup and missing-name suggestions no longer inspect the unhashed directory. |
| GitHub issue worktrees named `wf-<provider-slug>-<item-key>/`, with `awman/workflow-<provider-slug>-<item-key>` branches | GitHub issues use `<number>-<title-slug>/` and `awman/<number>-<title-slug>`. Resume and recreate no longer retain the old issue naming scheme. Non-GitHub remote items continue to use their provider slug and stable key. |
| Unencoded workflow names, including raw Unicode or names containing unsafe characters | The directory `wf-<encoded-name>/` and branch `awman/workflow-<encoded-name>` share one safe name, with a hash suffix when encoding or shortening changes it. Previous spellings are not discovered automatically. |
| Fixed remote-session `<session>/repo/` clone slot | Remote clones use `<session>/<repository-folder>/`, derived from the repository URL by the shared clone naming policy. There is no alternate `repo/` slot. |
| Per-session `<session>/worktree/` placeholder | Session initialization no longer creates this unused directory. Managed linked worktrees use the canonical repository-keyed worktree directory. |
| Branch-shaped fallback PR titles derived from arbitrary work-item keys | A work item with no title uses `Implement work item <key>`; awman no longer invents a legacy `awman/work-item-<key>` branch name for that title. |

Compatibility helpers and exports for the retired naming schemes have been
removed. Workflow `clone_repo` steps and remote sessions use the same default
repository-folder rule, and clone steps pass that destination explicitly to
Git. Explicit `into` paths are unchanged.

No automatic migration, deletion or branch renaming is performed. Review and
preserve any outstanding work with Git before moving or removing old worktrees.
An old worktree that still has the requested branch checked out can prevent
Git from creating a new worktree for that branch. Explicit paths to existing
linked worktrees remain subject to the normal repository ownership checks;
they do not trigger legacy-name discovery.

Every command that supports `--work-item` now also accepts `-w`, including
remote execution commands.

---

[Back to contents](../contents.md)
