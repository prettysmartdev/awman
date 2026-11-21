# awman

## Project Overview

awman runs coding agents in isolated containers and microVMs, with CLI, TUI,
API, web, and worker interfaces. These instructions apply throughout this
repository; a nested `AGENTS.md` takes precedence within its directory.

## Source of Truth

All decisions — architecture, behavior, security, UX, and implementation — are governed by the spec files in `aspec/`. Before writing or modifying code, consult the relevant spec document(s).

| Concern | Spec file |
|---|---|
| Project purpose, language, personas | `aspec/foundation.md` |
| Architecture and design principles | `aspec/architecture/design.md` |
| Security constraints | `aspec/architecture/security.md` |
| CLI commands, flags, config | `aspec/uxui/cli.md` |
| Local dev, build, test workflows | `aspec/devops/localdev.md` |
| Test tiers and coverage | `aspec/devops/testing.md` |
| CI/CD | `aspec/devops/cicd.md` |
| Operations | `aspec/devops/operations.md` |
| Subagents | `aspec/devops/subagents.md` |
| Work item template | `aspec/work-items/0000-template.md` |

## Tech Stack

- **Language**: Rust (idiomatic, async)
- **TUI framework**: Ratatui
- **Build tools**: `make`, `cargo`, `docker`
- **Output**: single statically-linked binary (`awman`)

## Build & Test Commands

```sh
make all      # build the awman binary
make install  # build + install to /usr/local/bin/
make test     # run the manifest local gate
make validate-native # Apple Silicon acceptance and evidence packet
```

## Architecture Constraints

- Single statically-linked binary for macOS (Apple Silicon only), Linux, Windows
- Two execution modes: interactive (TUI via Ratatui) and command (single invocation, stdout/stderr output)
- Interacts only with the active Git repo and the local Docker daemon
- Prefer simplicity over cleverness — readable by an intermediate Rust developer
- Small, modular crates

## Security Constraints (non-negotiable)

- **Never execute any agent/code assistant directly on the host machine**, with one
  accepted exception: the `HostAgentPinger` readiness/credential-refresh ping defined in
  `aspec/architecture/security.md`. It is the only type allowed to spawn an agent binary
  on the host, only for its listed triggers, with a fixed greeting, no task or repository
  content, an empty private working directory, and a first-party executable resolved from
  an installed location outside the checkout. Do not add triggers, a second host-agent
  mechanism, or a fallback to an unvalidated program name; do not remove the exception.
- All agent work (tasks, prompts, repository access) runs inside containers or microVMs
  (images built from `Dockerfile.dev`); the ping above never carries any of it
- Only mount the current directory or Git repo root to containers — never parent directories
- If a parent directory is the Git root, prompt the user to confirm the mount scope

## Testing

- Follow `aspec/devops/testing.md` and `ci/gates.toml`. Cover important user journeys and failure boundaries at the lowest useful tier; test counts and one-to-one replacement are not coverage goals.
- Use `awman-test-support` for private state, owned processes, deadlines and typed capabilities. Never depend on a developer's HOME or host agents.
- Keep pure tests small. Prove cross-component behavior through production interfaces and reserve real external systems for explicitly granted boundary and native tiers.
- Every suite and gate must be registered. Missing infrastructure is blocked evidence, never a passing skip. Do not reintroduce pre-WI144 harnesses or fallback workflows.

## Configuration

- Per-repo config: `GITROOT/.awman/config.json`
- Global config: `$HOME/.awman/config.json`
- Binary install path: `/usr/local/bin/`
- Data/storage: `$HOME/.awman/`

## Work Items

Work items live in `aspec/work-items/`. Use `aspec/work-items/0000-template.md` as the template for new work items.

After implementing a work item:
1. Review and update `docs/` to reflect the current state of the tool
   - **Documentation must be user-facing, NOT work-item-specific**
   - Do not create "WI 0XYZ implementation guide" docs
   - Instead, update existing user docs (e.g., `docs/08-headless-mode.md`) to describe current behavior
   - If a new feature warrants a new doc file, create it as a user guide (e.g., `docs/10-new-feature.md`), not as implementation/architecture notes
   - Implementation details, decisions, and technical notes belong in the work item spec or code comments, never in published docs
2. Follow the patterns, conventions, and architecture established in the `aspec/` spec

## Developer Skills

Repository development skills live in `.agents/skills/<name>/SKILL.md`. Read
the relevant skill when its workflow applies:

| Skill | When to use it |
|---|---|
| [blog-post](.agents/skills/blog-post/SKILL.md) | Write an awman blog post in the established style. |
| [release-prep](.agents/skills/release-prep/SKILL.md) | Prepare a release, including versions, docs, release notes, and a blog post. |
| [security-review](.agents/skills/security-review/SKILL.md) | Conduct a requested security review of awman and its repository. |

Customer-facing skills remain in `skills/` and are distributed by awman's
builtin skill library. Both collections use the [Agent Skills format](https://agentskills.io/specification):
a directory named after the skill, with a `SKILL.md` containing YAML `name` and
`description` fields followed by Markdown instructions. Supporting files use
paths relative to the skill directory.

`AGENTS.md` is the canonical repository instruction file, following the
[AGENTS.md standard](https://agents.md/) stewarded by the Linux Foundation's
Agentic AI Foundation.
