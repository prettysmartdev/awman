# Project Foundation

Name: awman
Type: CLI
Purpose: A containerized code agent manager.

awman is organized as a **four-layer architecture** to ensure clean separation between data persistence, business logic, command dispatch, and presentation frontends (CLI, TUI, API). See `aspec/architecture/design.md` for details.

# Technical Foundation

## Languages and Frameworks

### CLI
Language: Rust
Frameworks: Ratatui
Guidance:
- The `awman` CLI ships as one executable for macOS (Apple Silicon only; Intel Macs are not supported), Linux (x86_64 and arm64), and Windows (x86_64). Application and builtin-runtime dependencies are statically linked or embedded. Platform system libraries are allowed: the Linux x86_64 builtin asset uses GNU/glibc with a maximum required glibc symbol version of 2.35; it must not require a separately installed VM runtime, kernel, guest agent, SQLite, or runtime helper. The separate musl build remains static and does not include builtin support. See the exact dependency and ABI checks in `aspec/devops/release.md`.
- Every function of the CLI should be accessible either in "interactive" mode (i.e. running `awman` with no arguments launches a TUI to interact with its features) or "command" mode, where `awman` is run with one or more arguments, executes a single function, and then exits, printing its output to stdout and stderr.
- Idiomatic, async Rust code
- Small, easily understood modules and crates
- Prefer simplicity (understandable by an intermediate Rust programmer) over complex code that is concise.

# Best Practices
- Organize code in small, simple, modular components
- Each component should contain unit tests that validate its behaviour in terms of inputs and outputs
- The overall codebase should contain integration tests that validate the interation between components that are used together

# Personas

### Persona 1:
Name: user
Purpose: user of the `awman` CLI tool in their macOS, linux, or Windows terminal.
Use-cases:
- executing `awman` interactive mode for ongoing sessions
- executing `awman <>` command mode for single-use commands
RBAC:
- allowed: all
- disallowed: none
