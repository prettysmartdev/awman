# Work Item: Task

Title: Skipped dependency upgrades — tracking note
Issue: n/a

## Summary:
Two direct dependencies were evaluated for upgrade during the 2026-05-28 dependency maintenance pass but could not be updated without introducing instability or requiring nightly Rust. This note records the reason for each skip so future maintainers can revisit when the blockers are resolved.

## Skipped Packages

### rusqlite 0.39 → 0.40

**Reason:** `rusqlite 0.40.0` (and its transitive dependency `libsqlite3-sys`) requires the `cfg_select` feature, which is an unstable Rust library feature not yet available on the stable toolchain pinned in `rust-toolchain.toml` (`1.94.0`). Attempting to build fails with:

```
error[E0658]: use of unstable library feature `cfg_select`
error: could not compile `libsqlite3-sys` (build script)
```

**Resolution path:** Once `cfg_select` is stabilised in a future Rust release, or `libsqlite3-sys` is updated to avoid the unstable feature, re-attempt `rusqlite = { version = "0.40", features = ["bundled"] }` and run the test suite.

### libc 0.2 → 1.0.0-alpha.3

**Reason:** The `1.0.0-alpha.3` release is a pre-release (alpha). Adopting an alpha release in a production binary carries undefined stability risk — the API surface could change in subsequent alpha/beta/rc releases before the 1.0 stable lands.

**Resolution path:** Once `libc 1.0.0` stable is released, evaluate the migration guide for breaking API changes against the code in `src/` (primarily unix-specific signal/process handling), make required changes, and run the full test suite.

## Implementation Details:
- No code changes required — these are deferred, not rejected.
- All other available minor/breaking-version upgrades (sha2 0.10→0.11, rcgen 0.13→0.14) were applied successfully on the same date.

## Edge Case Considerations:
- n/a (no code changes)

## Test Considerations:
- n/a (no code changes)

## Codebase Integration:
- n/a (no code changes)

## Documentation

No user-visible behaviour changed; no doc updates required.
