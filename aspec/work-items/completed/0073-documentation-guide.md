# WI 0073: Documentation Guide

**Purpose**: Navigate all documentation created/updated for the grand architecture refactor completion.

---

## For Users

Start here if you're **using amux** (CLI, TUI, or Headless):

- **[docs/00-getting-started.md](../../../docs/00-getting-started.md)** — Installation and first steps
- **[docs/01-using-the-tui.md](../../../docs/01-using-the-tui.md)** — TUI keyboard reference and layout
- **[docs/02-agent-sessions.md](../../../docs/02-agent-sessions.md)** — How to chat with agents
- **[docs/07-configuration.md](../../../docs/07-configuration.md)** — Configurable fields and their behavior
- **[docs/08-headless-mode.md](../../../docs/08-headless-mode.md)** — HTTP API usage

User-facing behavior is **unchanged** from the legacy version. The refactor is internal.

---

## For Contributors

Start here if you're **developing amux** (adding features, fixing bugs):

### Architecture Basics (Required Reading)

1. **[docs/10-architecture-overview.md](../../../docs/10-architecture-overview.md)** (newly created)
   - Four-layer architecture explained
   - Design principles
   - How to add a new feature
   - How layers communicate

2. **[aspec/architecture/2026-grand-architecture.md](../2026-grand-architecture.md)** (detailed spec)
   - Complete specification of each layer
   - Tenets and constraints
   - Trait-based delegation patterns

3. **[aspec/architecture/design.md](../design.md)** (updated for 4-layer model)
   - Layer responsibilities
   - Data flow diagrams
   - Session as the anchor

### Implementation Guides

- **[aspec/devops/localdev.md](../devops/localdev.md)** — Local build, test, install
- **[aspec/devops/cicd.md](../devops/cicd.md)** — CI/CD pipeline configuration
- **[aspec/devops/architecture-lint.md](../devops/architecture-lint.md)** (newly created)
  - How `make architecture-lint` enforces layering
  - Configuration and exceptions
  - Implementation details

### Validation Reports (Reference)

Created during WI 0073, these documents record the validation results:

- **[aspec/review-notes/0073-parity-validation.md](../../review-notes/0073-parity-validation.md)** (newly created)
  - Matrix of 85 parity behaviors
  - Status (PASS / MINOR-DRIFT / REGRESSION) for each
  - Test file locations
  - Sign-off checklist

- **[aspec/review-notes/0073-architecture-audit.md](../../review-notes/0073-architecture-audit.md)** (newly created)
  - Layering audit results
  - Business logic segregation check
  - Type-driven design validation
  - Catalogue completeness
  - Backwards compatibility verification

### Work Item Reference

- **[aspec/work-items/0073-grand-architecture-finalize.md](./0073-grand-architecture-finalize.md)** (original spec)
  - Full implementation details and requirements
  - 10 sections: tests, validation, cleanup, audits, docs refresh, lint, final checks

- **[aspec/work-items/0073-summary.md](./0073-summary.md)** (newly created)
  - Executive summary of what was completed
  - Key metrics (tests, assertions, files updated)
  - Sign-off status

---

## Documentation by Topic

### Architecture & Design
| Document | Location | Audience | Key Topics |
|----------|----------|----------|-----------|
| Architecture Overview | `docs/10-architecture-overview.md` | All devs | Four layers, design principles, adding features |
| Grand Architecture Spec | `aspec/architecture/2026-grand-architecture.md` | Advanced devs | Complete specification, tenets, trait patterns |
| Design Principles | `aspec/architecture/design.md` | All devs | Layer responsibilities, data flow, Session type |
| Security Constraints | `aspec/architecture/security.md` | Security-conscious | Container isolation, auth, TLS |
| Foundation | `aspec/foundation.md` | All | Project purpose, languages, best practices |

### Testing & Validation
| Document | Location | Audience | Key Topics |
|----------|----------|----------|-----------|
| Parity Validation | `aspec/review-notes/0073-parity-validation.md` | Auditors | 85 behavior assertions, sign-off |
| Architecture Audit | `aspec/review-notes/0073-architecture-audit.md` | Auditors | Layering, logic segregation, type design |
| WI 0073 Spec | `aspec/work-items/0073-grand-architecture-finalize.md` | Implementers | Full test suite requirements, audit checklist |

### Operations & Build
| Document | Location | Audience | Key Topics |
|----------|----------|----------|-----------|
| Local Development | `aspec/devops/localdev.md` | Devs | Build, test, install commands |
| CI/CD Pipeline | `aspec/devops/cicd.md` | DevOps | GitHub Actions, test matrix, lint in CI |
| Architecture Lint | `aspec/devops/architecture-lint.md` | Devs | Lint tool, implementation, enforcement |
| Operations | `aspec/devops/operations.md` | SRE | Running amux in production |

### User Documentation
| Document | Location | Audience | Key Topics |
|----------|----------|----------|-----------|
| Getting Started | `docs/00-getting-started.md` | New users | Install, concepts, first session |
| Using the TUI | `docs/01-using-the-tui.md` | TUI users | Keyboard shortcuts, layout, tab management |
| Agent Sessions | `docs/02-agent-sessions.md` | Users | Chat, implement, authentication |
| Security & Isolation | `docs/03-security-and-isolation.md` | Security-conscious | Containers, worktrees, SSH, Docker |
| Workflows | `docs/04-workflows.md` | Advanced users | Multi-step workflows, control, persistence |
| Yolo Mode | `docs/05-yolo-mode.md` | Advanced users | Autonomous operation, restrictions, countdown |
| Configuration | `docs/07-configuration.md` | All users | Config files, all settings, defaults |
| Headless Mode | `docs/08-headless-mode.md` | API users | HTTP server, sessions, endpoints |
| Remote Mode | `docs/09-remote-mode.md` | Advanced users | Running agents remotely, streaming logs |

---

## Reading Order for Different Roles

### New Contributor
1. `docs/10-architecture-overview.md` — understand the 4-layer model
2. `aspec/architecture/2026-grand-architecture.md` — deep dive into tenets and traits
3. `aspec/devops/localdev.md` — build and test locally
4. Pick a layer and dive into `src/*/mod.rs` files

### Code Reviewer (PR Review)
1. Skim `aspec/architecture/design.md` to recall the four layers
2. Check `make architecture-lint` passes (layering validation)
3. Review against tenets:
   - Does this layer import from lower layers only? ✓
   - Is frontend code business-logic-free? ✓
   - Are types preferred over free functions? ✓
4. If new command, verify it's in `CommandCatalogue` and all three frontends implement identical flags

### Maintainer (Planning Next Work Item)
1. **[aspec/work-items/0073-summary.md](./0073-summary.md)** — understand the refactor scope and completion
2. **[aspec/review-notes/0073-parity-validation.md](../../review-notes/0073-parity-validation.md)** — verify all behaviors are PASS or approved
3. Decide on next feature or bugfix
4. Check `aspec/architecture/2026-grand-architecture.md`, section "Edge Case Considerations" for warnings

### Security Auditor
1. `aspec/architecture/security.md` — know the security constraints
2. `docs/03-security-and-isolation.md` — understand user-visible security
3. `src/engine/container/` code review — verify no host execution
4. Check: is every agent invocation routed through `ContainerRuntime`?

---

## Key Changes from Legacy Code

The refactor is **internal only**. User-facing behavior is unchanged. Key changes for developers:

| Aspect | Legacy | New |
|--------|--------|-----|
| **Command logic location** | Scattered across frontend crates | Centralized in `src/command/` |
| **Container execution** | Dense `run_with_*` functions | `ContainerRuntime` builder with `Vec<ContainerOption>` |
| **Frontend communication** | Direct calls into engines | Trait-based delegation |
| **Session state** | Per-frontend (TabState, etc.) | Unified `Session` type |
| **Config merging** | Decentralized per-command | Centralized in `src/data/config/` |
| **Test organization** | All tests in `tests/` | Unit tests colocated in `src/`; integration tests in `tests/` |
| **Layering enforcement** | None (spaghetti code) | `make architecture-lint` in CI |

---

## Validation Checklist

Before claiming WI 0073 complete:

- [ ] All parity tests (85 assertions) are PASS or approved MINOR-DRIFT
- [ ] No architecture-lint violations
- [ ] All documentation updated (aspec + docs)
- [ ] Tests run locally: `make test-fast` and `make test-full`
- [ ] CI passes on at least one Docker-enabled runner per OS
- [ ] `oldsrc/` remains untouched (ready for manual deletion)
- [ ] Review notes are signed off by developer

---

## Quick Links

**Build & Test**:
```bash
make all              # Build amux
make test-fast        # Hermetic tests
make test-full        # All tests (needs Docker)
make architecture-lint # Lint layering
make pre-push         # Pre-commit checks (fmt + clippy + test + lint)
```

**Inspect Architecture**:
```bash
ls src/data/          # Layer 0: Session, config, persistence
ls src/engine/        # Layer 1: Container, workflow, git, overlay, auth
ls src/command/       # Layer 2: Dispatch, command implementations
ls src/frontend/      # Layer 3: CLI, TUI, Headless
cat src/main.rs       # Layer 4: Entry point
```

**Read Specs**:
```bash
cat aspec/architecture/2026-grand-architecture.md  # The master spec
cat aspec/architecture/design.md                    # Design overview
cat aspec/devops/architecture-lint.md               # Lint tool spec
```

---

**Status**: Complete ✓  
**Date**: May 8, 2026  
**Next Step**: Manual testing and deletion of `oldsrc/`, legacy `tests/`, legacy `benches/`
