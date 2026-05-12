# Project Foundation

Name: amux
Type: CLI
Purpose: A containerized code and claw agent manager.

amux is organized as a **four-layer architecture** to ensure clean separation between data persistence, business logic, command dispatch, and presentation frontends (CLI, TUI, Headless). See `aspec/architecture/design.md` for details.

# Technical Foundation

## Languages and Frameworks

### CLI
Language: Rust
Frameworks: Ratatui
Guidance:
- The `amux` CLI should compile to a single, statically linked binary for macOS, Linux, and Windows.
- Every function of the CLI should be accessible either in "interactive" mode (i.e. running `amux` with no arguments launches a TUI to interact with its features) or "command" mode, where `amux` is run with one or more arguments, executes a single function, and then exits, printing its output to stdout and stderr.
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
Purpose: user of the `amux` CLI tool in their macOS, linux, or Windows terminal.
Use-cases:
- executing `amux` interactive mode for ongoing sessions
- executing `amux <>` command mode for single-use commands
RBAC:
- allowed: all
- disallowed: none