# Work Item: Task

Title: Docker Sandbox Runtime
Issue: issuelink

## Summary:
- # Docker Sandbox Runtime

This work item will add a new runtime to awman: Docker sbx (`docker-sbx`).

Docker Sandbox is a new offering from Docker which runs code agents within MicroVMs instead of containers for increased isolation and better performance.

This work item will require deep web research to determine how docker sbx works, how it differs from Docker and Apple Containers, how to manage the use of multiple agents, how to manage config and auth auto-passthrough, how to handle overlays of all types, how to manage prompt and system prompt insertion, etc.

The purpose of this work item is not to guess or make assumptions about how sbx works, it is to do deep research about its capabilities, do a thorough comparison with the other two runtimes about how it differs in operation, and if all of awman's functionality can be satisfied by iy. It is a new offering from Docker and therefore may not support every single feature awman needs. This is OK, but should be clearly documented in the work item and deeply understood for both integration and user experience effects.

Do the deep research, determine how docker sbx could be added as a third runtime for awman, and create a plan for how to implement it with as much feature parity as possible while retaining the most similar user experience possible.

## User Stories

### User Story 1:
As a: [admin | user | other]

I want to:
description of task

So I can:
description of result


## Implementation Details:
- details


## Edge Case Considerations:
- considerations

## Test Considerations:
- considerations

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- **Update existing feature docs** (e.g., if implementing headless features, update `docs/08-headless-mode.md`)
- **Create new user guides only if a new user-visible feature warrants it** (e.g., `docs/10-my-feature.md`)
- **Never create work-item-specific docs** (e.g., no "WI 0123 implementation guide" in published docs)
- **Keep all technical/implementation details in work item specs or code comments**, not in `docs/`
- **Docs are for end users**, not for developers trying to understand implementation

See `CLAUDE.md` for more guidance on documentation standards.
