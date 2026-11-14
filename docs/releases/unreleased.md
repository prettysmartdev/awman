# Unreleased

Changes on `main` that have not yet been cut into a release. Grouped by what
they mean for you, not by where they were made.

The worker API and squad scheduler now share one daemon, one database and one execution capacity limit. Use `awman daemon start|stop|status|logs` for its lifecycle; the separate `api` and `squad` lifecycle commands have been removed.

Local clients use an owner-authenticated socket or named pipe. The optional network listener uses TLS and named API keys, exposes only configured features, and never exposes ephemeral features. Feature switches and network exposure changes apply live.

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

---

[Back to contents](../contents.md)
