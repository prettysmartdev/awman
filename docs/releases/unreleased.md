# Unreleased

Changes on `main` that have not yet been cut into a release. Grouped by what
they mean for you, not by where they were made.

The worker API and squad scheduler now share one daemon, one database and one execution capacity limit. Use `awman daemon start|stop|status|logs` for its lifecycle; the separate `api` and `squad` lifecycle commands have been removed.

Local clients use an owner-authenticated socket or named pipe. The optional network listener uses TLS and named API keys, exposes only configured features, and never exposes ephemeral features. Feature switches and network exposure changes apply live.

---

[Back to contents](../contents.md)
