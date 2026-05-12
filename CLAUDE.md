# amux

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
- **Output**: single statically-linked binary (`amux`)

## Build & Test Commands

```
make all      # build the amux binary
make install  # build + install to /usr/local/bin/
make test     # run all tests
```

## Architecture Constraints

- Single statically-linked binary for macOS, Linux, Windows
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

- Every module/function: unit tests validating inputs and outputs
- Cross-component flows: integration tests
- Full CLI behavior: end-to-end tests

## Configuration

- Per-repo config: `GITROOT/aspec/.amux.json`
- Global config: `$HOME/.amux/config.json`
- Binary install path: `/usr/local/bin/`
- Data/storage: `$HOME/.amux/`

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
