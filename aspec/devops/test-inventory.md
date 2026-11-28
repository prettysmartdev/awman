# Executable test inventory

The inventory is derived from Cargo targets, Rust discovery and the suites in
`ci/gates.toml`. Run `cargo xtask test list` to inspect selection and
`cargo xtask gates check` to reject unregistered tests, empty required suites,
missing contract IDs and stale generated files. CI records compiled inventories
alongside JUnit; the controller requires the selected tests to actually pass.

Coverage is assessed by important behavior and risk, as described in
[replacement-ledger.md](replacement-ledger.md), rather than by matching individual
historical tests. Do not reconstruct the deleted pre-WI144 CSV inventory or use
its counts as an acceptance threshold. The Git history preserves that history.

Some registered suites belong to no gate cell because only a real host can run
them. `artifact-apple-smoke` and `artifact-docker-smoke` are release-critical
and run through `make release` native kits; `artifact-sbx-smoke` is the optional
provider diagnostic. The release SBX smoke is declared in the manifest's
`[release.sbx_smoke.*]` table rather than as a suite. See
[testing.md](testing.md#required-release-checks).

For a new behavior, update its contract test and manifest suite. For a new
runtime or platform, update the manifest and acceptance matrix, then obtain
execution evidence on that platform. A source-level coverage row alone is not
proof of execution. Native evidence follows
[testing.md](testing.md#native-validation).
