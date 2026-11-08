# Engine conformance gate (WI 0142)

`make test` runs this target along with the existing test targets. The daemon
fixtures create a real in-process `DaemonRuntime` and core on a temporary data
root, enable engine ephemerally, claim the owner identity, and bind the production
router through `local::start_claimed` on a private Unix socket. The fixture's
instance opener injects the same engines as the builtin run; the transport,
feature gate, admission, dialog arbitration, terminal hub and codec are production
code.

Run just the conformance target with:

```
bash tools/isolated-test.sh -p awman-cli --test engine_parity -- --nocapture
```

TUI fixtures select the engine with `AWMAN_TEST_BACKEND=builtin|daemon`. The
same existing test functions run in both processes; no copies of their assertions
are maintained. Pure Tab rendering tests have no backend and run unchanged.

Latency uses 25 warmup iterations followed by 300 recorded iterations for each
measurement. Every Submit runs real Dispatch, and must finish before the next
Submit; stale output cannot satisfy a timer. Dialog response timing ends at the
engine-side response receiver, starting before the engine publishes its request.
The actual RemoteInstanceBackend adapter also measures 325 commands and 975
dialog answers. The Makefile runs these two benchmarks serially, apart from
other test targets, so parallel fixtures do not skew the timing. No samples are
discarded. The normal p95 limit is
strictly below 5 ms. On an explicitly designated slow or oversubscribed CI host,
set **`AWMAN_TEST_SLOW_HOST=1`** to retain the measurements and functional checks
while disabling only the strict latency assertions. No hardware detection,
retry-until-fast logic or implicit CI opt-out is used.

Outcome traces are in memory on the engine, never persisted. They record session
creation, canonical command paths/arguments, complete ordered dialog requests,
statuses, output bytes, container lifecycle and command results. Filesystem
snapshots compare the complete repo tree byte for
byte. Frame snapshots compare Ratatui cell buffers including style. Runtime and
terminal scenarios compare bytes and lifecycle effects through the same hub as
0141. Secret-containment tests scan persisted files, output frames, Debug and a
child test process's panic output.

Set `AWMAN_TEST_PARITY_ARTIFACT_DIR=/absolute/path` to retain JSON artifacts:
engine-side outcome traces, full frame snapshots, and each CLI invocation's
argv, fake-container command log, stdout/stderr bytes, exit code, Git status
and worktree listing. Hello and environment snapshots are never archived.

`make test` runs the library and maintained CLI targets with builtin selected,
then repeats all existing TUI/Tab tests and the existing CLI smoke/parity
harnesses with daemon selected. The smoke executable wrapper forwards the
selection even through fixture `env_clear()` calls; ApiHome applies it after
clearing the child environment. CLI conformance also compares both backends
inside one named harness process hosting the daemon core in process (the name
preserves the real PID identity check rather than bypassing discovery).
