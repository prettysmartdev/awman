# awman 0.13: a microVM per agent, and any agent you like

A core rule of awman has always been that agents never run on your host. This meant installing something else first: Docker, Apple Containers, or Docker Sandboxes. On a Mac that's one more daemon to keep running and update, and second set of tools you need to reference to understand what's running.

v0.13 removes that extra install on Apple Silicon by adding a builtin microVM engine powered by macOS native virtualization. It also adds custom agent support so you can use absolutely any harness you prefer.

---

```sh
# install or upgrade
curl -s https://prettysmart.dev/install/awman.sh | sh
```

---

## A microVM that ships inside awman

The new `builtin-experimental` runtime runs every agent session in its own hardware-virtualized Linux microVM. The VM code, kernel and guest agent are compiled into the awman binary, so installing awman is all you need. Running agents require need Docker, `container` or `sbx`.

```sh
awman config set --global runtime builtin-experimental
awman ready
```

What I care about most is the isolation. Each agent gets its own kernel behind Apple's hypervisor instead of sharing one with every other container. The only host files it can see are your project and the overlays you configure, and there's no Docker socket to hand over. Network access is a per-session policy (`public`, `allowlist` or `none`), and a repo can narrow the global policy but never widen it. CPU, memory and disk are configurable per session too.


It's experimental, and for now it runs only on macOS on Apple Silicon, from the `awman-macos-arm64` release. Images come from a source you choose (a registry, an archive, or your local Docker or Apple Containers store), and `awman ready` imports them automatically. Its behaviour and config as subject to change, and I really hope you'll reach out to let me know how it's performing.

## Bring your own agent

New coding agents show up every few weeks, and until now each one had to wait for awman to add it. In v0.13 any agent can be added via a single TOML file. The file describes how to install it, how to launch it, and which flags it understands. awman then builds, launches and isolates it like the built-in agents.

```sh
awman new agent pi            # writes .awman/agents/pi.toml
awman ready --agent pi        # review, approve, build
awman chat --agent pi
```

A repo definition (or a global one that runs a host check) is reviewed and approved once per content hash before anything runs. If it changes, you're asked again, so a teammate's edit to a repo's agent file doesn't take effect without you seeing it. `crush` and `maki` now ship as example definitions. The new set of awman builtin skills (`awman new skill --pull builtin`) gives any code agent a skill for writing new agent types so you don't have to.

## Also in 0.13

`awman agents` is the place to see, attach to and stop every running agent on any runtime, with `ls`, `inspect` and `rm` behaving the way Docker habits expect. `awman status` became `awman agents status`.

Workflows now check their prerequisites before the first container starts. Commit, push, PR and CI steps need a git identity, an SSH key or a GitHub token, so the run asks up front instead of failing an hour in. It also asks before handing any host credential to a step, and only the steps that need a credential get it. The HTTP API gains cancellation, idempotency keys, one-shot runs, resumable event streams and structured results, so automation no longer has to scrape text.

Some commands changed behaviour, so read the breaking-changes section of the [release notes](https://github.com/prettysmartdev/awman/blob/main/docs/releases/v0.13.0.md) before you upgrade.

---

Source and issues at [github.com/prettysmartdev/awman](https://github.com/prettysmartdev/awman). More at [prettysmart.dev](https://prettysmart.dev). Feedback, issues, and contributions all welcome.
