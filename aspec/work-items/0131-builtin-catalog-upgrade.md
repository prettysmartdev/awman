# Work Item: Feature

Title: upgrade the builtin state directory in place when the worker protocol changes
Issue: issuelink

## Summary:
- Today an awman whose worker protocol (`WORKER_PROTOCOL`, e.g. `0.7.8/18/awman-1`)
  differs from the one recorded in `<stateDir>/awman-runtime-version` refuses the whole
  state directory (`EngineError::BuiltinCatalogMismatch`). Every builtin command fails,
  including `awman agents status` and `awman agents kill`, and the only recovery is to
  move the directory aside by hand and re-import every image.
- This work item adds a one-way, allowlisted, backed-up upgrade. When the recorded
  protocol is a known predecessor of the current one and no older awman still runs
  sessions from the directory, awman migrates the catalog, removes the dead sandboxes
  left by the older build, rewrites the marker and continues with the command.
- Unknown, newer and unversioned catalogs keep the current non-destructive refusal.
- First allowlisted step: `0.7.2/18/awman-1` → `0.7.8/18/awman-1` (Microsandbox
  catalog schema 27 → 29, already exercised by `tools/test-msb-catalog-compat.sh`).

## User Stories

### User Story 1:
As a: user

I want to:
keep using my builtin state directory after upgrading awman

So I can:
skip re-importing images by hand and avoid a wall of protocol-mismatch errors that
no command can fix.

### User Story 2:
As a: user

I want to:
get a clear refusal, not a migration, while an older awman is still running agents
from the same directory

So I can:
finish or stop those agents first, so they aren't orphaned by a catalog they can no
longer read.

### User Story 3:
As a: user

I want to:
be able to get my previous catalog back if the upgrade goes wrong

So I can:
roll back by returning to the older awman binary and restoring the backup.

## Implementation Details:

### Layer 0: `src/data/builtin_runtime.rs`, `src/data/builtin_state.rs`
- Add `UPGRADABLE_FROM: &[&str]` beside `WORKER_PROTOCOL`, listing the exact protocol
  strings the current build may migrate from (initially `["0.7.2/18/awman-1"]`). Only
  exact matches qualify: no prefix or semver comparison, and never a protocol newer
  than the current one.
- Extend `RuntimeMarker` with `Upgradable { found }`, returned by
  `check_runtime_marker` when the marker is listed in `UPGRADABLE_FROM`. `Mismatch`
  stays for everything else, including the "unversioned existing catalog" case.
- Add `BuiltinState::backup_catalog(from: &str) -> Result<PathBuf, DataError>`. It copies
  `db/` to `db.pre-<sanitised from>/` with private permissions, refuses symlinks the
  same way `secure_create` does, and fails if the target already exists, so it never
  overwrites an earlier backup. The copy is made with the catalog lock held, before
  the SDK opens the database.
- Add `BuiltinState::replace_runtime_marker(expected)`, which writes a temporary file,
  fsyncs it and renames it over the marker. It is called only after a successful
  migration.

### Layer 1 (engine): `src/engine/container/builtin/catalog.rs`, `msb_driver.rs`
- `check_catalog` becomes `prepare_catalog`, returning `Ready` or `NeedsUpgrade { found }`.
  `MsbDriver::open_blocking` runs the upgrade when needed, then continues as today.
- Upgrade sequence. The catalog lock is held for the whole sequence; if any step
  fails, the marker and the original `db/` are left as they were:
  1. **Liveness gate.** Refuse when any session from the older build may still be
     alive. The primary check needs no knowledge of the old schema: each per-sandbox
     SDK lifecycle lock under `sandboxes/` is held by its running worker, so try to
     take each one; a lock that can't be taken means a live session. Report the
     result as a new `EngineError::BuiltinCatalogUpgradeBlocked { state_dir, found }`,
     whose message tells the user to quit the older awman and retry. *Verify during
     implementation that 0.7.2 workers hold that lock for the VM's whole lifetime; if
     they don't, fall back to a read-only `rusqlite` query of the pre-migration
     `db/msb.db` for `awman.owner.pid` labels, checked with
     `DaemonSupervisor::is_process_alive`, and record the decision here.*

     **Decision (implemented): the lifecycle-lock probe is sufficient, so no fallback was added.**
     The locks are not under `sandboxes/`. The SDK keeps them at
     `<state dir>/run/locks/<hash>.lock` (`microsandbox_runtime::ipc::lifecycle_lock_path`,
     identical in SDK 0.7.2 and 0.7.8; `run_dir()` is `home/run` in both). Evidence from the
     published 0.7.2 crates (`microsandbox`, `microsandbox-runtime`, `microsandbox-cli`):
     - the launcher (`runtime/spawn.rs`) takes the lock before touching any endpoint, and dups it
       into the child as fd 99 (`LIFECYCLE_LOCK_FD`). It also passes `--lifecycle-lock-fd`,
       because launch contract patch 18 is ≥ 9.
     - the worker (`machine_cmd::run`, which awman's same-executable worker route calls)
       adopts that fd as a `SandboxLifecycleGuard`. It stores the guard in
       `runner::vm::Config::lifecycle_guard` ("process-lifetime ownership"), which `vm::enter`
       moves into `run` and never drops; `run` only ends with process exit.

     The gate therefore probes every regular file in `run/locks/` with a non-blocking `flock`. It
     also probes `awman-import.lock`, so an import running from the older awman blocks the
     upgrade. A lock that can't be taken produces `BuiltinCatalogUpgradeBlocked`. Locks left
     by crashed workers are free and count as dead. Caveat: git history (the 0.7.2 tag)
     wasn't readable in the implementation environment. The evidence comes from the
     published SDK 0.7.2 crates that protocol `0.7.2/18/awman-1` was built against.
  2. **Backup:** `backup_catalog(found)`.
  3. **Migrate:** open the catalog through `isolated_backend(&paths)`, which runs the
     SDK's forward migrations. Opening must not boot or touch any VM.
  4. **Retire stale sandboxes:** every sandbox whose `awman.worker.protocol` label is
     not `PROTOCOL` belongs to a dead session (step 1 guarantees this). Remove its
     record and per-sandbox directories through the driver, using a removal path that
     skips `naming::check_protocol` and is limited to this step. Keep imported images
     and the OCI cache.
  5. **Commit:** `replace_runtime_marker(PROTOCOL)`.
- Print one line to stderr in command mode, and show the same text in the TUI status
  area: `upgraded builtin state <dir> from <found> to <expected>; previous catalog
  saved in <backup>`. Do not prompt: the upgrade is backed up and non-destructive,
  and command mode must stay scriptable.
- Concurrency: two awman processes that open at once are serialised by the catalog
  lock. The second one re-reads the marker after taking the lock and sees `Matches`.

### Errors and exit codes
- `BuiltinCatalogMismatch` (added alongside this work item) stays for anything that
  isn't upgradable. `BuiltinCatalogUpgradeBlocked` and failed migrations exit 1, like
  every other engine failure (`EngineError::exit_code`).

### Release process
- `tools/release-artifact-check.sh` and the `release-prep` skill gain a check: when
  `WORKER_PROTOCOL` changes, the previous released protocol must be added to
  `UPGRADABLE_FROM`, or the release notes must say why it can't be.

## Edge Case Considerations:
- **Unversioned catalogs** (a `db/` with no marker) are never migrated: they may
  belong to a standalone `msb` install or to a pre-marker awman.
- **Newer marker** (running a downgraded awman): refuse as today. There is no downgrade
  path; restoring the backup with the older binary is the rollback.
- **Backup already exists** from an earlier, interrupted attempt: refuse with a message
  that names it, rather than overwriting the only good copy.
- **Crash during the upgrade:** the marker still names the old protocol, so the next
  run tries the upgrade again. Migration must therefore be idempotent, or restart from
  the backup: if `db/` is partially migrated (schema newer than the marker says),
  restore `db/` from the backup before retrying. Cover this with a test.
- **Disk full while copying the backup:** remove the partial backup and refuse, leaving
  the catalog untouched.
- **Lock files left behind by crashed 0.7.2 workers** are free to take, so they count
  as dead, which is correct.
- **Repo-level `builtin.stateDir`** that differs from the process's is still rejected
  before this code runs (security.md: the state directory is process-wide).
- **Long-running import lock** (`IMPORT_LOCK`): the upgrade takes only the catalog
  lock, so it doesn't wait for an import. An import already running from an older
  awman makes the liveness gate refuse.

### Review follow-up decisions (implemented)
- **Backup manifest.** `backup_catalog(from, database, ledger)` folds the staged copy's WAL into
  its database, records its applied migrations and every file's size in
  `awman-catalog-backup.json`, and publishes under a name claimed with an exclusive `mkdir`.
  Recovery restores only a backup whose files match its manifest and whose `from` matches the
  marker; partial migration is judged against the manifest's migrations, never inferred from an
  absent database or ledger.
- **Journalled restore.** `restore_catalog` publishes `awman-catalog-restore` before touching
  `db/` and removes it once the restored `db/` is durable; a failed second rename puts the
  displaced `db/` back. While the journal exists the next attempt repeats the restore, even if
  `db/` is missing or empty, then migrates again.
- **Reversible retirement.** Each stale sandbox's `sandboxes/<name>` is moved to
  `.sandboxes.retired/` before the SDK removes it. Rollback (and the start of every attempt) puts
  them back; they are deleted only after the marker commits.
- **Fence, not only a probe.** Besides the lifecycle-lock probe, the upgrade takes every existing
  image lease exclusively and the import lock, both without waiting, and keeps them until it
  ends. An older awman's launch or image removal then fails at once and its import waits. The
  lifecycle locks are probed again just before the commit; a VM started meanwhile (for an image
  with no lease file yet) blocks and rolls back the upgrade. An older awman that is open but idle
  is otherwise not stopped; operations that take none of these locks are not fenced.
- **Durability.** Directory fsync errors are reported (only EINVAL/ENOTSUP count as
  unsupported). A failure after the new marker became visible is not rolled back: the new marker
  and the migrated catalog stay together, with the backup kept.
- **Release guard.** `tools/release-upgrade-policy.sh` fails on a shallow checkout, a git error
  or an unparseable previous protocol, and requires a reason after the release-note phrase.
  It is skipped only without an earlier release tag or when that release has no
  `builtin_runtime.rs`. The release job checks out full history and tags.

## Test Considerations:
- Layer 0 unit tests: `check_runtime_marker` returns `Upgradable` only for allowlisted
  strings; `backup_catalog` refuses symlinks and existing targets and sets private
  permissions; `replace_runtime_marker` is atomic (a crash before the rename leaves the
  old marker).
- Engine tests with the in-memory driver (`backend/lifecycle_tests.rs` rig): stale
  sandboxes with old protocol labels are removed; current-protocol sandboxes and images
  are kept; a held lifecycle lock blocks the upgrade with
  `BuiltinCatalogUpgradeBlocked` and changes nothing.
- Catalog compatibility: extend `tools/test-msb-catalog-compat.sh` and its fixture so a
  catalog written by 0.7.2 is upgraded through `prepare_catalog` (not only through raw
  migrations), with rows, labels and `strict_sni` preserved.
- Integration (`tests/builtin_runtime/sqlite.rs`): keep the unversioned-catalog
  refusal test unchanged; add a marker-plus-0.7.2-schema fixture that `awman agents
  status` upgrades and then lists. Assert that the backup exists and the marker was
  rewritten.
- Concurrency: two threads open the same state directory at once; exactly one upgrade
  runs and both end up `Ready` (mirror
  `concurrent_first_starts_agree_on_one_marker`).
- Crash recovery: interrupt after migration but before the marker is rewritten, then
  rerun and expect success from the backup.
- Hardware: one Apple Silicon run that boots a guest after upgrading a real 0.7.2
  directory (the old-to-new build test that WI 0120 dropped).

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Layering: protocol constants and the allowlist live in Layer 0 (`src/data`); the
  upgrade policy, liveness gate and SDK calls live in the engine
  (`src/engine/container/builtin`). No unsafe code; `awman-host-shims` is untouched.
- Add the on-disk catalog upgrade to the "On-disk data contracts" list in
  `aspec/architecture/design.md`, and a bullet to `aspec/architecture/security.md`
  under private runtime state: upgrades are allowlisted, backed up, refused while
  sessions are alive, and never applied to unversioned or newer catalogs.
- Update WI 0120's note that "no user holds a 0.7.2 catalog" to point at this work item.

## Documentation

After implementation is complete, update user-facing documentation in `docs/` to reflect the current state of the tool:

- `docs/11-runtimes.md`: replace the "move or delete the directory" guidance with the
  automatic upgrade, the stderr notice, the "quit the older awman" refusal and how to
  roll back (older binary plus the `db.pre-*` backup).
- `docs/13-cleaning-up.md`: list `db.pre-*` backups as removable once you are happy with
  the upgrade, and decide whether `awman clean` offers to remove them.
- Keep the manual move-aside steps only for catalogs that can't be upgraded.

See `CLAUDE.md` for more guidance on documentation standards.
