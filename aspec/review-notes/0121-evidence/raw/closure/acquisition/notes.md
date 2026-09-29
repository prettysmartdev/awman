# Acquisition / ready cancellation closure (2026-09-28)

Implementation (shared working tree; Git metadata inaccessible):
- Replaced OCI Docker and registry blocking reqwest clients plus detached body-reader threads with a synchronous facade over a private current-thread asynchronous HTTP executor. The same acquisition cancellation/deadline control covers request creation, connection/TLS/header waits, Bearer token requests and body chunks. Cancellation checked every 20 ms, executor shutdown has a 100 ms wait cap. Docker metadata is capped at 1 MiB; registry existing metadata caps preserved. HTTP read stalls remain bounded by per-read and overall acquisition timeouts.
- Transport futures are dropped on cancellation. No HTTP transfer worker remains blocked for the old one-hour timeout. Platform getaddrinfo may finish later under the OS resolver's own bounds; shutdown does not wait indefinitely for this uncancellable platform call. Filesystem/keychain calls likewise remain cooperative safe points.
- Production retry backoff now checks cancellation and acquisition deadline in <=20 ms slices. Errors returned by a request at the deadline normalize to the acquisition deadline rather than becoming retryable late successes.
- AgentRuntimeEngine and ContainerBackend now expose import_image_cancellable. ContainerRuntime forwards the caller token into BuiltinBackend, then into the actual cache acquirer, import lock, and archive projection. Existing import_image delegates with a fresh token.
- ReadyEngine imports run on a blocking worker with a bounded progress channel, keeping its async caller responsive. Ready exposes a cancellation handle; dropping the ready import future cancels its worker (including the existing session setup timeout). `awman ready` Ctrl-C signals cancellation and awaits cleanup. Both ready command and session setup now forward configured image sources.
- Archive projection uses controlled reads/hashing/validation and refuses expired/cancelled output. Import acquires the root agent's per-image exclusive lease before checking/replacing SDK tags.
- SDK import is a cancellation safe point: once its mutating transaction starts, finish SDK import plus identity publication before reporting cancellation. Arbitrarily dropping its extraction future could leave untracked writes. Cached valid image bytes may remain after cancellation that races this atomic phase. Native validation must exercise this contract; it is not an immediate-abort promise inside SDK extraction.
- Both acquisition and ready progress channels are bounded and drain without silently truncating messages.

Tests added:
- tests/oci_import/cancellation.rs: actual loopback sockets withholding Docker headers/body, registry manifest headers, auth token headers/body; cancellation must close the peer socket promptly, finish the worker, and leave cache images/refs/tmp empty. Deadline-only withholding-header cases too.
- Ready production caller path tests on a current-thread Tokio runtime: explicit caller signal reaches in-flight import; dropping ready on timeout reaches worker and terminates it.
- Real retry sleep cancellation regression.

Executed so far:
- ready_caller_token_cancels_an_in_flight_import PASS (ready-1.log).
- First loopback cancellation run FAIL: request.send() eagerly constructed a Tokio timer outside the executor. Backtrace recorded; fixed by constructing/polling send inside an async block. Replacement run pending.
- OCI/unit runs pending artifact locks. Do not treat pending logs as PASS.

Final local results:
- `PATH=/usr/local/cargo/bin:$PATH CARGO_BUILD_JOBS=2 cargo test --lib engine::oci:: -- --test-threads=2`: PASS, 107 passed, no ignored, no failures; `unit-final.log`. Includes actual retry sleep cancellation, late-success/deadline/cache lock and no-publication checks.
- `target/debug/deps/awman-074c8bb12127b45b engine::ready::tests --test-threads=2` (binary built by the command above): PASS, 16 passed; `ready-final.log`. Both production ready caller-token and dropped-future/current-thread regressions executed.
- `PATH=/usr/local/cargo/bin:$PATH CARGO_BUILD_JOBS=2 cargo test --test oci_import -- --test-threads=2`: exit0, harness 46 passed; `oci-final.log`. This includes gated source/corpus/native-keychain tests that return without execution when resources are absent; do NOT treat all 46 as real-service/native evidence. The three new cancellation tests executed their seven live loopback scenarios and peer-EOF/cache-cleanup assertions. The matrix agent's separately filtered hermetic run reports39 actual hermetic tests passed.
- Earlier failing reactor test is superseded by these reruns, retained as diagnostic evidence.

Source stable for root broad gates. No Cargo.toml/Cargo.lock edits owned by this subtask. Follow-up resource obligations remain: real Docker TLS/mTLS/private registry/keychain/proxy services, native VM ready/cancellation race validation, and the safe SDK mutation commit behavior on supported hosts. Kernel filesystem/keychain/DNS calls are cooperative, not force-preemptible. A cancellation during SDK mutation waits for its coherent transaction; this is explicit and must not be represented as immediate SDK-abort evidence.

Root final evidence audit correction: the unfiltered 46-test OCI run has **six**
explicit resource opt-outs, not seven. Root's final focused run executes40
fixture/loopback tests. The agent's filtered39 run additionally excluded the
pure `archive_real_corpus_preserves_identity_and_links` test because its filter
matched `real_corpus` anywhere in the name. The final unfiltered run executes
that test. No external opt-out is counted as a pass.
