# Work Item: Task

Title: Bounded output_tx Channel with Lossy Overflow
Issue: issuelink

## Summary:

Replace the unbounded `tokio::sync::mpsc::unbounded_channel` used for `output_tx`/`output_rx` with a bounded channel to defend against unbounded message accumulation under backpressure. Use a lossy (drop-oldest) strategy on overflow so that senders are never blocked.

## User Stories

### User Story 1:
As a: user

I want to:
amux to remain stable under extreme output rates (e.g. a command that produces millions of lines per second)

So I can:
trust that amux will not run out of memory or deadlock even with pathological command output

## Implementation Details:

This work item is a direct result of performance audit findings in `aspec/work-items/plans/0033-performance-audit-findings.md` (Area 2.4).

### Current Behaviour

`TabState` uses `tokio::sync::mpsc::unbounded_channel()` (state.rs:443). Senders (`output_tx` clones) can push messages at any rate; if the TUI tick rate can't keep up, the channel queue grows without bound.

In practice the TUI drains all pending messages per tick (`while let Ok(line) = output_rx.try_recv()`), and ticks at ~60 Hz. However, if a command produces output significantly faster than the tick drain rate, the channel will grow.

### Proposed Change

Replace with a bounded channel (suggested capacity: 4096 messages):

```rust
let (output_tx, output_rx) = tokio::sync::mpsc::channel(4096);
```

To avoid blocking senders (which would stall the async task producing output), use a `try_send` wrapper that drops the **oldest** message on overflow:

```rust
fn lossy_send(tx: &Sender<String>, msg: String) {
    if tx.try_send(msg).is_err() {
        // Channel full: drain one message to make room, then retry once.
        // Or simply drop the new message (drop-newest strategy, simpler).
    }
}
```

The drop-newest strategy (simply discard the new message if the channel is full) is simpler and acceptable for output display — losing a few log lines during a burst is preferable to OOM.

### `OutputSink` impact

`OutputSink::Channel` wraps the sender. Update its `println` implementation to use `try_send` with drop-newest on overflow.

### Compatibility with `status --watch`

The `status --watch` path uses `CLEAR_MARKER` to clear the output window. Ensure this marker is not dropped on overflow — or redesign the clear mechanism to not go through the channel (e.g. a separate oneshot clear channel).

## Edge Case Considerations:
- Messages dropped during overflow will appear as missing lines in the output window. This is visible to the user but preferable to OOM.
- The `CLEAR_MARKER` used by `status --watch` must not be dropped. Consider giving it higher priority or routing it through a separate channel.
- `output_tx` clones are distributed to multiple tasks. A bounded channel means all senders share the same capacity limit.

## Test Considerations:
- Unit test: send 10,000 messages to a channel with capacity 4096 via `try_send`; verify `output_rx` contains at most 4096 messages and no panic occurred.
- Integration test: run `status --watch` while also sending output; verify CLEAR_MARKER is not dropped.

## Codebase Integration:
- Follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- Primary files: `src/tui/state.rs`, `src/commands/output.rs`.
- No new runtime dependencies.
