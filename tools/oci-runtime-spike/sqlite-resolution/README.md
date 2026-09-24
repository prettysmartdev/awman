# SQLite dependency alignment spike

Date: 2026-09-24. Decision for [WI 0119](../../../aspec/work-items/0119-strict-embedded-microsandbox-runtime.md).
This is a standalone combined-link/database probe, not a production backend change.

## Decision

**Keep awman's rusqlite stack unchanged. Locally backport the already-merged
SQLx dependency-range fix to the published `sqlx-sqlite 0.9.0` crate.**

The only dependency-source change for SQLite is:

```diff
 [dependencies.libsqlite3-sys]
-version = ">=0.30.1, <0.38.0"
+version = ">=0.30.1, <0.39.0"
```

This is the exact change in upstream [commit
94aafe3a68884d923b0798a767c8d7f6cfda89d2](https://github.com/transact-rs/sqlx/commit/94aafe3a68884d923b0798a767c8d7f6cfda89d2),
merged September 9, 2026. The crates.io sparse index still listed `0.9.0` as the
latest published `sqlx-sqlite` version when checked for this spike; its published
manifest still has the old bound. Do not switch the entire SQLx workspace to a
moving Git branch or open a duplicate upstream PR.

Vendor that published crate with its license and this manifest-only patch,
select it using root `[patch.crates-io]`, and preserve the checked-in application
lockfile. Tested versions are `rusqlite 0.40.2`, `sqlx/sqlx-sqlite 0.9.0` and a
single bundled `libsqlite3-sys 0.38.2` (SQLite 3.53.2). Keep application and msb
catalog files, schemas, migrations and connection policies separate; only their
native SQLite implementation is shared. No awman data-access rewrite or schema
migration is required for this dependency alignment.

Remove this local patch when a released, compatible SQLx package incorporates
the bound change and passes the same tests. SQLx's published policy explicitly
allows raising the upper bound in patch releases; its [SQLite documentation](https://docs.rs/sqlx/0.9.0/sqlx/sqlite/)
also explains the single-native-library constraint and default static bundling.
Retain the distinct embedded-kernel patch until its own upstream replacement is
ready; these patches have independent lifecycles.

## What was actually tested

Host: Linux ARM64, Rust/Cargo 1.94.0. Awman baseline
`8765a9b95db1f0b2444ee999e3b5908ac59b78ee`, including the current working tree;
no application source, root manifest or root lockfile was changed by this spike.
Microsandbox `v0.7.2` / `60d4dc8a436fb9365491567ec21d073e924e3c6d` and the
existing `msb_krun 0.1.39` strict-embedding patch were reused.

| Check | Result |
|---|---|
| Unpatched combined dependency graph | Expected failure: two incompatible `links = "sqlite3"` requirements |
| One-line SQLx backport with unchanged awman rusqlite | Resolves one `libsqlite3-sys 0.38.2` |
| Compile/link actual awman library plus full msb SDK/CLI/VM, embedded kernel and guest agent | PASS in one combined executable |
| Actual awman session CRUD and task-store migration | PASS |
| Existing awman session/schema and task-store/gateway regression modules | **33 passed**, including legacy-shaped DB compatibility |
| rusqlite/SQLx report identical SQLite version and source ID | PASS: SQLite 3.53.2 |
| Cross-driver reads/writes, binary BLOB binding and `RETURNING` | PASS |
| Foreign keys, WAL reader isolation, busy error, rollback, reopen, integrity check | PASS |
| Actual msb read/write pools and complete migration list | **27 migrations**, applied twice without duplication; integrity check passes |
| Existing msb catalog opened by patched binary | PASS; image inspection output unchanged |
| Image registered by patched binary then read by old strict probe | PASS |
| Runtime dependencies | System libraries only; no SQLite, cap-ng or krun DSO |
| Reproducible `checks.sh` run using empty execution PATH and disposable state | PASS |

The old strict binary uses `libsqlite3-sys 0.37.0` / SQLite 3.51.3. Its
old→new→old catalog round trip uses an actual fixture OCI import and a second
image reference, not just an empty SQL database. This does not prove every
historical catalog or mixed-version live-worker upgrade scenario.

The probe calls real awman persistence code and real msb command/runner code in
one executable. It is stronger evidence than a miniature rusqlite/SQLx build,
but does **not** implement awman's production worker dispatch or backend. No
guest execution was attempted here: this host still lacks KVM. Mac and Linux
x86_64 builds, final optimized/signature checks and the complete integration
suite remain implementation/release gates, not reasons to defer this dependency
decision. Existing upstream SQLx deprecation warnings were not patched.

Evidence: [results/linux-arm64](results/linux-arm64/), especially
`unpatched.stderr`, `sqlite-tree.txt`, `sqlite-features.txt`, `sqlite-check.log`,
`awman-stores.log`, `catalog-roundtrip.log` and `dynamic-dependencies.txt`.

## Alternatives considered

| Option | Assessment |
|---|---|
| **Backport the upstream SQLx bound change** | **Chosen.** One manifest-line change; tested against actual combined graph and database operations; preserves awman's shipped SQLite version. Small temporary vendoring burden. |
| Downgrade awman to rusqlite 0.39.0 | A minimal pair resolves against native binding 0.37.0, as recorded in `downgrade-resolution.log`. Not selected: regresses awman's dependency baseline and expands application validation unnecessarily. No full awman downgrade was implemented or validated. |
| Use SQLx upstream Git workspace | Contains the fix, but couples us to unreleased workspace changes and more provenance/version management than this one-line backport. Not built. |
| Wait for a SQLx release | Clean eventual removal path, not a delivery prerequisite. |
| Disable bundled SQLite or use system SQLite | Does not remove Cargo's conflicting version/`links` requirements; also weakens the self-contained packaging contract. |
| Rewrite msb persistence, replace awman's rusqlite, or isolate a renamed second SQLite copy | Unnecessary scope and maintenance/native-symbol risk for a manifest-bound mismatch. Not attempted. |
| Separate msb executable | Avoids the link graph, but violates the chosen strict same-executable model. |

## Reproduce

The probe has its own locked Cargo workspace and uses awman as a path
dependency. It never edits root `Cargo.toml` or `Cargo.lock`. The script downloads
and checks the published SQLx crate, patches a private copy, verifies the
negative unpatched case, builds, runs the database tests and performs the
old/new catalog round trip. It does not use real credentials or contact an
image registry. Build dependencies still require network/cache access.

Reuse a successful strict-embedding build and the original fixture archive:

```sh
bash tools/oci-runtime-spike/sqlite-resolution/checks.sh \
  /path/to/strict-build \
  /path/to/pinned-microsandbox-checkout \
  /path/to/fixture-oci.tar
```

For the user's existing Mac scratch directories, if still available:

```sh
bash tools/oci-runtime-spike/sqlite-resolution/checks.sh \
  /private/tmp/awe.uYAQHK \
  /private/tmp/awe.uYAQHK/microsandbox \
  /private/tmp/awsp.8uJ29q/fixtures/fixture-oci.tar
```

The Mac form is provided for the outstanding platform check, not represented
as already run. It requires the same Rust/compiler/signing prerequisites as
the strict probe. On Linux the script additionally needs the strict spike's
`static-lib/libcap-ng.a`, or `SQLITE_SPIKE_NATIVE_LIB_DIR` pointing to a directory
with that archive. `CARGO_TARGET_DIR` may reuse the prior target cache. The
script prints a reviewable results archive path; raw databases/builds stay local.

Published `sqlx-sqlite-0.9.0.crate` SHA256:
`488e99c397a62007e4229aec669a179816339afc6d2620ca6fa420dbee2e982c`.
The checked-in patch targets its Cargo-normalized manifest; upstream's source
manifest contains the same dependency stanza. `Cargo.toml.orig` remains the
unmodified provenance copy, not the manifest used for building.
