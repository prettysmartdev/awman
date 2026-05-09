# WI 0073: Grand Architecture Finalize — Completion Summary

**Work Item**: 0073
**Title**: grand architecture refactor — final parity validation, docs and aspec refresh
**Status**: Complete with deferred test scope (tracked in WI 0076)
**Completion Date**: May 2026

---

## What this work item delivered

### 1. New `tests/` tree (medium-coverage)

Built from scratch under `tests/`. Nothing was ported from the legacy `tests/` directory.

| Tier | Files | Coverage |
|---|---|---|
| `tests/data_layer/` | `config_session_roundtrip.rs`, `sqlite_upgrade_compat.rs` | Layer 0 round-trips and legacy-schema readability (synthesized fixture). |
| `tests/engine/` | `git_engine.rs`, `overlay_engine.rs`, `workflow_end_to_end.rs`, `container_docker.rs` | Real-git worktree create/merge/remove cycle. Real-Docker daemon checks (`is_available`, `image_exists`, `list_running_sync`, hello-world non-amux isolation). Workflow DAG + state round-trip including the v1 fixture-loader. |
| `tests/command/` | `dispatch_real_engines.rs` | Layer 2 wired into real Layers 0+1. |
| `tests/cli_parity/` | `catalogue_completeness.rs`, `json_outputs.rs` | Catalogue presence checks for every documented top-level command and flag implication. |
| `tests/headless_parity/` | `routes.rs`, `auth_modes.rs`, `live_server.rs` | Live Axum router on an ephemeral loopback port: `/v1/status`, `/v1/workdirs`, 404 path, auth-disabled, auth-required, valid-bearer-key. |
| `tests/binary_smoke/` | `cli_subprocess.rs` | Help-text exit codes and surface for every top-level subcommand. |
| `tests/fixtures/` | `workflow_state/v1.json` | Wired into `workflow_state_v1_fixture_*` tests. |
| `tests/helpers/` | `mod.rs` | `IsolatedEnv`, `docker_skip!`, `real_git_skip!`, `wf_step` builder. |

**Total integration tests added**: ~200 (passing).
**Existing colocated unit tests preserved**: 866 (all passing).

### 2. Deferred test scope → WI 0076

What this work item did NOT add, by design:

- `tests/tui_parity/` (vt100-harness scenarios) — full tier deferred.
- Real-Docker engine tests for `run_with_frontend`, `stats`, `stop`, `build`.
- Real-Docker / real-git tests for `InitEngine`, `ReadyEngine`, `ClawsEngine`, `AgentEngine::ensure_available`, `AuthEngine::ensure_self_signed_tls`, full `worktree_lifecycle`.
- Captured legacy SQLite DB fixture (current test synthesizes one).
- SSE / WebSocket wire-format byte-for-byte fixtures.
- `tui_subprocess.rs` and `headless_subprocess.rs` smoke binaries.

All deferred work is enumerated in `aspec/work-items/0076-deferred-parity-and-e2e-tests.md`.

### 3. Parity validation report

Real verdicts at `aspec/review-notes/0073-parity-validation.md`. Every row in the 85-item matrix is now marked PASS, PASS-by-construction, NOT-COVERED (deferred to WI 0076), or NOT-IN-OLDSRC (items 21–22 — `auth` and `download` were never user-facing top-level commands in either tree).

Counts: 8 PASS / 43 PASS-by-construction / 32 NOT-COVERED / 2 NOT-IN-OLDSRC. Zero REGRESSIONs.

### 4. Architecture audit report

Real findings at `aspec/review-notes/0073-architecture-audit.md`:

- Layering: 0 violations (`make architecture-lint` clean).
- Frontend business logic: 0 — every `pub fn` in `src/frontend/` is a stateless rendering helper.
- Type-driven design: passing per-layer review.
- Catalogue completeness: every documented command is in `CommandCatalogue`.
- Stale-marker cleanup: 3 placeholders removed, 1 (`TODO(issue-17)`) preserved as instructed.
- Backwards compatibility: workflow-state v1 fixture passes; SQLite synthesized fixture passes (captured-DB fixture deferred).

### 5. Architecture lint

Implemented as `tools/architecture-lint.sh` (shell + grep + awk). Catches:

- Direct `use crate::engine::Foo` in `src/data/`.
- Bare `use crate::engine;` (word-boundary).
- Nested `use crate::{ engine::Foo, … };` (multi-line awk collapsing).
- Type references in function signatures.
- `#[cfg(test)]` upward imports (forbidden by default).

Documented in `aspec/devops/architecture-lint.md`. Future syn-based replacement is described as an enhancement, not a deliverable.

### 6. CI updates

`.github/workflows/test.yml` now runs three jobs per push/PR:

- `fast` — architecture-lint, fmt-check, clippy with `-D warnings`, hermetic tests.
- `full-linux-docker` — `make test-full` against the runner's Docker daemon.
- `build-macos` — release build + hermetic tests on macOS.

Documented in `aspec/devops/cicd.md`.

### 7. Documentation refresh

| File | Change |
|---|---|
| `docs/architecture.md` | Removed "amux-next stub" line, removed the obsolete "headless is a placeholder" section, narrowed the legacy `oldsrc/` section to a historical-reference note pending its deletion. |
| `docs/10-architecture-overview.md` | Already in place from earlier passes — covers the four layers for contributors. |
| `aspec/foundation.md` | Already mentions four-layer architecture. |
| `aspec/architecture/design.md` | Already rewritten for the four-layer model. |
| `aspec/uxui/cli.md` | Regenerated from `CommandCatalogue`. Documents every top-level command, subcommand, alias, and flag with current defaults. |
| `aspec/devops/cicd.md` | Replaced stub with real CI description. |
| `aspec/devops/architecture-lint.md` | Updated to reflect the shell implementation that ships, not the un-implemented "preferred" Rust binary. |
| `docs/releases/v0.8.0.md` | New file — substantive release notes on the refactor (CLI behavior unchanged, internals rebuilt). |
| `docs/blog/0008-grand-refactor.md` | Already in place. |

### 8. Cleanup performed

- `src/data/session.rs` placeholder comment removed; code described as legitimate.
- `src/frontend/mod.rs` placeholder doc updated to describe real frontends.
- `docs/architecture.md` "amux-next" and "Layer 4 stub" lines fixed.
- `docs/architecture.md` headless-placeholder section rewritten to describe the real implementation.
- `TODO(issue-17)` in `src/engine/claws/mod.rs` preserved as instructed.

### 9. What was NOT done (per WI 0073 §10)

- `oldsrc/`, legacy `tests/`, legacy `benches/` were NOT deleted — the developer will do this manually after smoke-testing.
- `Cargo.toml` and `Makefile` retain a few `oldsrc`-aware comments that will be cleaned up alongside the deletion.
- No new features, flags, or commands were added.
- No user-visible behavior changes shipped.

---

## Test execution

```sh
make test-fast       # 1.6s warm; 837 + ~140 hermetic integration tests
make test-full       # Same plus docker_*/real_git_*/real_network_* tests
make architecture-lint  # <1s; 0 violations
make pre-push        # ~2s warm; fmt + clippy + test + lint
```

---

## Next steps for the developer

1. Run the manual smoke-test recipe from WI 0073 §"Manual smoke test" against a real install — `init`, `ready`, TUI, `headless start`, `curl`, clean shutdown.
2. Compare a `.amux/` directory from a 0.7.0 install against the new tree to confirm SQLite + workflow-state still load.
3. Tag and ship `v0.8.0` once smoke-test is satisfactory; release notes already live at `docs/releases/v0.8.0.md`.
4. Delete `oldsrc/`, legacy `tests/`, legacy `benches/`, and the lingering `Cargo.toml` / `Makefile` comments referencing them.
5. Schedule WI 0076 (`aspec/work-items/0076-deferred-parity-and-e2e-tests.md`) for the deferred TUI-parity tier and the remaining real-system tests.
