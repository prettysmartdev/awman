# Work Item: Task

Title: Add tokio-console Instrumentation (Debug Feature Flag)
Issue: issuelink

## Summary:

Add opt-in `tokio-console` instrumentation behind a Cargo feature flag so developers can visualise async task lifetimes, channel backpressure, and task wakeup latency during debugging or performance investigation. The instrumentation must not affect the release binary in any way.

## User Stories

### User Story 1:
As a: developer

I want to:
enable `tokio-console` by running `cargo run --features tokio-console` and connect the console to see all spawned tasks and their states

So I can:
identify stuck tasks, orphaned futures, and channel backpressure issues during development

## Implementation Details:

This work item is a direct result of performance audit findings in `aspec/work-items/plans/0033-performance-audit-findings.md` (Instrumentation Recommendation).

### Changes

#### `Cargo.toml`

```toml
[features]
tokio-console = ["dep:console-subscriber"]

[dev-dependencies]
console-subscriber = { version = "0.4", optional = true }
```

Wait — `console-subscriber` must be a full dependency (not dev-only) because it initialises the Tokio runtime subscriber at startup. But it must only be compiled when the feature is enabled:

```toml
[dependencies]
# ... existing dependencies ...
console-subscriber = { version = "0.4", optional = true }

[features]
default = []
tokio-console = ["dep:console-subscriber", "tokio/tracing"]
```

#### `src/main.rs`

```rust
fn main() {
    #[cfg(feature = "tokio-console")]
    console_subscriber::init();

    // existing tokio runtime setup
}
```

#### Tokio runtime

`tokio-console` requires the Tokio runtime to be built with `--cfg tokio_unstable`. Add to `.cargo/config.toml`:

```toml
[build]
rustflags = ["--cfg", "tokio_unstable"]
```

This is only needed when `tokio-console` is used; for normal builds it is harmless.

### Usage

```bash
# Terminal 1: run amux with console support
RUSTFLAGS="--cfg tokio_unstable" cargo run --features tokio-console

# Terminal 2: connect the console
tokio-console
```

### tracing spans to add

Once `tokio-console` is wired up, add `tracing::instrument` or manual spans around:
- `terminal.draw()` in `src/tui/mod.rs` — labels each frame render
- `tab.tick()` in `tick_all()` — labels per-tab tick
- `spawn_stats_poller` task — labels the polling loop

## Edge Case Considerations:
- `tokio_unstable` is required for `tokio-console` but is not a stable Tokio flag. It must NOT be enabled in release builds or CI (unless explicitly opted in).
- `console-subscriber` has a non-trivial overhead when enabled. Document clearly that it is for development only.
- The `[build] rustflags` in `.cargo/config.toml` affects all crates in the workspace. Document this caveat.

## Test Considerations:
- CI should build with `--features tokio-console` at most as a compile-check (`cargo build --features tokio-console`), not as a default.
- Verify that `cargo build` (no features) produces a binary of the same size as before.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Primary files: `Cargo.toml`, `src/main.rs`, optionally `.cargo/config.toml`.
- The `console-subscriber` dependency must be optional and must not appear in the release binary.
