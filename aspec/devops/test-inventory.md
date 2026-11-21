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

For a new behavior, update its contract test and manifest suite. For a new
runtime or platform, update the manifest and acceptance matrix, then obtain
execution evidence on that platform. A source-level coverage row alone is not
proof of execution. Native Apple Silicon evidence follows
[testing.md](testing.md#apple-silicon-acceptance-packet).
