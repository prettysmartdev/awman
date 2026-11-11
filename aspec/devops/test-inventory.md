# Legacy test, CI, and release inventory (phase 0)

Inventory created from the checked-out source tree on 2026-11-11 and reconciled during adversarial design review. The CSV is the static baseline, whose per-test semantic dispositions remain unresolved. The file/target disposition ledger records retained and blocked work; a retained file is not evidence of a complete replacement. CSV data is in [`test-inventory/`](test-inventory/). Run `perl aspec/devops/test-inventory/reconcile.pl` from the repository root to reproduce the static reconciliation; its output is [`reconciliation.txt`](test-inventory/reconciliation.txt).

## Discovery and confidence

The original inventory step could not find cargo on PATH. Review found Rust 1.94.0 under `/usr/local/rustup/toolchains/1.94.0-aarch64-unknown-linux-gnu/bin`; `cargo build --workspace --locked` passes and `cargo metadata --no-deps --locked --format-version 1` succeeds. The latter is saved as `cargo-metadata-resolved.json` and normalized in `cargo-targets.csv`. Metadata is target discovery, not compiled test discovery. Full per-platform/per-feature test lists and native rustdoc results remain **pending**; static counts are not runnable counts. `.git` points at an unavailable macOS worktree path, so local history is blocked. `gh` is installed but unauthenticated. No Actions timings/history or hardware measurements were collected.

Static counts from `rg` on this checkout:

| Measure | Count | Reconciliation |
|---|---:|---|
| Plain test attributes in `src crates tests` | 6,316 | The old unanchored grep count of 6,318 includes a string at `tests/builtin_runtime/gates.rs:253` and a comment at `src/command/session_setup.rs:479`. Neither is a test. |
| Parameterized test attributes | 397 | Previously omitted `#[tokio::test(...)]` / `#[test(...)]` forms, now included. |
| Total static test-attribute rows | 6,713 | 6,316 + 397; all have source-location IDs in `rust-tests.csv`. Macros/cfg still require compiled reconciliation. |
| Inline `#[cfg(test)]` markers in `src crates` | 702 | `rg -n` marker count. |
| Source files with inline marker in `src crates` | 439 | `rg -l` file count; WI estimate was 382 at its evidence commit. This checkout has 57 more files than that estimate. |
| Files with marker including integration tests | 443 | `rg -l` across `src crates tests`. |
| Distinct `AWMAN_TEST_*` names | 95 | Static grep over source, tests, tools, scripts, workflows and Makefile; WI says approximately 90. |
| Test/example targets including autodiscovery | 43 | 40 explicit plus 3 omitted WSP auto-targets (`client`, `wire_conventions`, `wire_sync`). Cargo metadata resolves 41 test targets and 2 examples: root 8 tests, CLI 29 tests + 2 examples, WSP 4 tests. All 54 workspace targets, including lib/bin/build scripts, are in `cargo-targets.csv`. |
| Files under `tools/` and `scripts/` | 172 | File listing; `scripts.csv` includes every file there because executable/script extensions are inconsistent. |
| Make executable targets | 22 | `make-targets.csv`; the previous count of 24 included two `.PHONY` metadata declarations. |
| Legacy test/helper/fixture files | 183 | `test-files.csv` includes files with no test annotation, paths and SHA-256. |
| Documentation fences | 10 | 2 Rust `compile_fail` fences; 8 non-Rust examples (including a shell fence under tests). Native rustdoc discovery remains pending. |
| Jobs in the two scoped workflows | 13 | 9 in `test.yml`, 4 in `release.yml`; see `workflows.csv`. |

The static rows count test attributes, not compiled or runnable tests. Cargo can expand cfgs, examples, feature-dependent code, and doctest cases differently. Per-test fields that could not be established from the available source window are marked `unknown`; do not treat those as negative findings.

## Rust tests and tier assignment

[`rust-tests.csv`](test-inventory/rust-tests.csv) enumerates source annotations with a source location ID, inferred target and suggested layer, local-window environment/side-effect hints, skip-marker scan, and pending disposition. Layer classification is a path-based proposal for review, not a semantic determination. Current caller field records the broad suite callers visible in Make and Actions; exact target and filter membership is unknown because test executables were not compiled/listed. The CSV labels a discovered skip marker only when one occurs near the annotation; absence means unknown, not “never skips.” Regression commit hints are provisional and require semantic confirmation against the WI and slice contract map.

[`inline-test-modules.csv`](test-inventory/inline-test-modules.csv) contains all 702 `cfg(test)` source markers in `src` and `crates`, each with file and line; module name and layer are intentionally unknown because the first inventory pass was static. The 439-file count is the distinct source-file count. [`doctests.csv`](test-inventory/doctests.csv) lists rustdoc fenced-code block openings found in Rust source. Exact doctest IDs, cfg/feature availability, and test outcomes require `cargo test --doc` and remain unknown.

[`cargo-registrations.csv`](test-inventory/cargo-registrations.csv) enumerates explicit and automatic test/example targets resolved by Cargo metadata. [`cargo-metadata.csv`](test-inventory/cargo-metadata.csv) records the `test-support` feature, `[profile.test]` (`debug = 0`, `incremental = false`), discovery limitation for doctests and explicit registration context.

Suggested layer names follow WI terminology: `0 data`, `1 engine`, `2 command`, `3 frontend`, `daemon-squad-worker`, `guest`, `artifact`. Suggested target tier is provisional and remains pending.

## Environment variables

[`test-environment.csv`](test-inventory/test-environment.csv) contains one row per occurrence for all 95 distinct `AWMAN_TEST_*` names, with file and line, with a heuristic purpose class and pending disposition. Classes are `tier gate`, `fixture path`, `credential`, `fault injection`, `re-exec marker`, and `shipped-binary switch`; class `unknown` means static naming did not prove intent. A filename hit may be a read, write, documentation mention, or shell assignment. Read-vs-set and exact purpose must be checked at each location during slice work.

## Make, scripts and workflows

[`make-targets.csv`](test-inventory/make-targets.csv) lists Make targets and marks each pending. [`scripts.csv`](test-inventory/scripts.csv) lists files under both `tools/` and `scripts/`; its role suggestions need source review; `script-callers.csv` contains direct in-tree references with line numbers, while unmatched entries remain unknown. [`workflows.csv`](test-inventory/workflows.csv) lists all 13 jobs in `test.yml` and `release.yml`; each row maps to the matching workflow ledger row and stays pending. Workflow job names and scopes:

- `test.yml`: `fast`, `full-linux-docker`, `web`, `build-macos`, `static-workspace-build`, `builtin-build`, `builtin-hardware`, `native-typed-provider`, `existing-backend-unsupported-targets`.
- `release.yml`: `preflight`, `build`, `release-macos-builtin`, `release`.

The current Make callers include `test` (builtin suite, then daemon parity/TUI/binary smoke), `test-fast` (exclusions by test-name substring), `test-full` (Docker-enabled full suite), `test-builtin` (feature build, registered targets, prior binary, network check), `test-builtin-hw`, `pre-push`, `native-builtin-ci`, `payloads-verify`, `apple-silicon-prerelease`, `architecture-lint`, and `release`. CI directly invokes the corresponding targets from the jobs above. Specific per-test caller assignments are unknown until Cargo list output and per-target selectors are available. `test.yml` has no job-level timeout on several hosted jobs; only hardware jobs declare timeouts. No job sets `AWMAN_TEST_BACKEND`; WI diagnoses that daemon-backend local/release tests do not run in ordinary CI.

The seven caller-less checks named in the WI are: `tools/test-build-previous-builtin-awman.sh`, `tools/test-kernel-callback-smoke.sh`, `tools/test-msb-isolated-builder.sh`, `tools/test-release-provider-metadata.sh`, `tools/test-release-upgrade-policy.sh`, `tools/test-review-build-inputs.sh`, and `tools/test-static-krunfw.sh`. These are retained as entries in `scripts.csv` with pending disposition. WI evidence also names uncalled protocol/catalog compatibility scripts and `probe-keychain-stdin.sh`; direct Make/Actions references need verification.

## Ledger coverage

[`ledger-crosswalk.csv`](test-inventory/ledger-crosswalk.csv) maps every row of WI section 9 to at least one inventory dataset; all rows are pending. Documentation-only ledger row is represented by `documentation.csv` as a scope entry. Key dispositions must be completed by later slice steps.

## Failure history and timing baseline

`gh run list` is **blocked: gh is installed but unauthenticated**. Git history fallback is **blocked** because the `.git` worktree pointer references `/Users/cohix-studio/Workspaces/prettysmart/awman/.git/worktrees/0144`, unavailable here. WI section “Evidence and current-state diagnosis” provides the available commit-to-class evidence: process inheritance/races (`2110d050`, `585680c3`, `e6a2d429`, `e6322996`, `f5b724fc`, `39fe0e05`, `72537ed5`, `d5043695`, `8b4d5273`, `4aaef99f`); hangs/synchronization (`776e56d6`, `58a1dadb`, `e1f959c0`, `204cea73`, `4d3ba70f`, `3e8d7e6c`, `8cf45de1`); platform/native build (`e28d35a1`, `02fe8c41`, `9f63d915`, `32a8e59a`); cache staleness (`f462fa2e`); policy/gate drift (`824aeda9`, `d0a29ec2`); release/candidate sequence (`9d4fec2b`, `146510ec`, `edd9dc9a`, `4a2853d7`, `f23e75a4`, `9eac9cda`). These are classifications reproduced from the WI, not independently confirmed with `git log`; exact failed run, OS, backend, gate and rerun are unknown.

Apple Silicon timing baseline is **blocked pending native Apple Silicon hardware** with Hypervisor.framework, verified payload and fixture, and access to cold/warm caches. Required measurements per WI section 2: compile, link, cache restore, fixture setup, previous-version build, VM boot, test run, signing and upload; repeated PR-path samples sufficient to calculate p50/p95. No timings are fabricated here.

## Dispositions after adversarial review

`test-inventory/dispositions.tsv` now uses real TSV delimiters and lists every
baseline file alongside the target-level records. `pending-cutover` means the
file is retained until semantic parity and native/shadow evidence are established;
`blocked` means the semantic mapping is still incomplete. Nothing in this ledger
claims that retained files have been deleted.

The integration proposed 149 named successors among 1,228 legacy integration
tests. Review disputed the strength of about 30 of those proposals, and 1,079
still have no complete replacement. These are incomplete implementation
obligations, not evidence of passing coverage. Per-test CSV dispositions remain
the unresolved static baseline until each exact mapping is reviewed.

Retained inline tests remain executable. The resource-using awman and xtask
harnesses are routed to component; only the explicitly selected pure awman
value-model subset keeps the one-second unit budget. The compatibility suite IDs
`unit-xtask`, `unit-awman-builtin` and `unit-host-shims-builtin` now declare their
actual component tier in the manifest. Native platform coverage is pending P9.
The detailed branch and cutover state is in `replacement-ledger.md`.
