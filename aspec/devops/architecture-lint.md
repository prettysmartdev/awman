# Architecture Lint — Layering Enforcement

**Make target**: `make architecture-lint`
**Implementation**: `tools/architecture-lint.sh` (shell + grep + awk).

## Purpose

The four-layer architecture only stays four layers if a tool enforces it. `make architecture-lint` scans every `.rs` file under `src/` and fails when a lower layer imports from a higher one.

## Constraints enforced

```
Layer 0 (data):     std + external crates + crate::data::*
Layer 1 (engine):   above + crate::engine::*
Layer 2 (command):  above + crate::command::*
Layer 3 (frontend): above + crate::frontend::*
Layer 4 (binary):   any (entry point)
```

The lint inspects `crate::*` paths only — `std::*` and external crates are always allowed.

## What it catches

- `use crate::engine::Foo;` (direct import) anywhere under `src/data/`.
- `use crate::engine;` (bare module import) — caught via word-boundary matching.
- `use crate::{ engine::Foo, data::Bar };` (nested-use block) — caught by collapsing multi-line `use crate::{ … };` blocks before grepping.
- `fn x(thing: crate::engine::Stuff)` — type references in function signatures.

## What it ignores by design

- Pure-comment lines (`//`, `#`, `*`).
- `std::*` imports.
- External-crate imports.
- Substring-only matches (`crate::engineering` is fine; the boundary requires a non-identifier character after the segment).

## What it does NOT yet catch

- `use crate as foo; foo::engine::stuff::doit()` — re-aliasing the crate root. No production code does this; we accept the gap until the lint moves to a `syn`-based implementation.
- Macros that synthesize `crate::engine::…` paths at expansion time. Procedural-macro authors must be careful here.

## `#[cfg(test)]` upward imports

By default, `#[cfg(test)]` test modules under a lower layer may NOT import from a higher layer. The lint does not distinguish `#[cfg(test)]` blocks — every match is a violation. Exceptions require explicit developer approval and a documented justification in the PR description.

## Output format

```
VIOLATION [Layer 0]: src/data/foo.rs:42    use crate::engine::stuff;

architecture-lint: 1 violation(s) found
```

Exit code: zero on no violations, non-zero otherwise.

## Performance

Sub-second on the current `src/` tree. The shell implementation walks files once per layer with `grep`, plus one awk pass per layer for nested-use collapsing.

## Future enhancement: syn-based binary

A Rust binary at `tools/architecture-lint/` using `syn` would survive renames more gracefully and produce richer diagnostics (file/line/column/spans). The shell script ships today because it has no extra build dependency and runs in <1s on a cold tree. Before replacing it:

- Confirm the parser can ingest every Rust file in `src/` (including conditional `cfg(...)` modules).
- Decide whether macro-expanded code should be checked (probably not — that requires `cargo expand`).
- Verify no regression in run time.

## Local usage

```sh
make architecture-lint   # scan
make pre-push            # fmt-check + clippy + test + architecture-lint
```

## CI integration

Wired into `.github/workflows/test.yml` as the first step of the `fast` job, before clippy and tests, so layering violations fail fast.
