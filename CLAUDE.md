# awman

## Source of Truth

All decisions — architecture, behavior, security, UX, and implementation — are governed by the spec files in `aspec/`. Before writing or modifying code, consult the relevant spec document(s).

| Concern | Spec file |
|---|---|
| Project purpose, language, personas | `aspec/foundation.md` |
| Architecture and design principles | `aspec/architecture/design.md` |
| Security constraints | `aspec/architecture/security.md` |
| CLI commands, flags, config | `aspec/uxui/cli.md` |
| Local dev, build, test workflows | `aspec/devops/localdev.md` |
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

```
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

- **Never execute any agent/code assistant directly on the host machine**
- All agentic operations run inside Docker containers (built from `Dockerfile.dev`)
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
