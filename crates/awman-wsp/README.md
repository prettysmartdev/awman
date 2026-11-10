# awman-wsp

Work Server Protocol v1 wire types, outbound client and test tools. Execution,
worker admission policy and awman's HTTP routes belong to awman, outside this crate.

The default library includes protocol types, affinity/label helpers, `Client`,
`mock_worker::{MockWorker, MockOutcome, LeaseReport}` and the reusable
`conformance::{run, cases, Report, ConformanceAdmin}` runner.

Features:

- `mock`: `mock::MockWorkServer`, with `Arc<MockWorkServer>::serve()` opening an
  ephemeral loopback HTTP listener. Keep the returned `ServingMock` alive;
  dropping it stops the listener. Administration happens through the Rust API.
- `terminal`: `Client::open_terminal()` and `terminal::TerminalSocket`, a
  `Stream` and `Sink` of `LeaseTerminalFrame`. TLS uses native certificate roots.
  Frame types and `terminal::SUBPROTOCOL` are available without this feature.

Tests enable both features through a development dependency, so plain
`cargo test -p awman-wsp` runs the HTTP and WebSocket conformance tests.

## In-process harness with real HTTP

```rust,no_run
use std::{collections::BTreeMap, sync::Arc};
use awman_wsp::{Client, conformance, mock::MockWorkServer};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let server = Arc::new(MockWorkServer::new());
let serving = server.clone().serve().await?;
let report = conformance::run(&serving.base_url, server.as_ref()).await;
assert!(report.is_success(), "{report:?}");

// Connect an independently implemented worker to serving.base_url.
// Its POST /v1/link gives a link_id; the test administrator approves it:
let linker = Client::new(&serving.base_url, String::new())?;
let start = linker.start_link().await?;
server.approve_link(&start.link_id, BTreeMap::new());
let token = linker.poll_link(&start.link_id).await?.unwrap();
let client = Client::new(&serving.base_url, token.refresh_token)?;
// Register this client, enqueue fixtures with server.enqueue(item, timestamp),
// then run a MockWorker or your own implementation.
# let _ = client;
# Ok(())
# }
```

To test another server, implement `ConformanceAdmin` with its fixture provisioning
and link approval APIs, then call `conformance::run(base_url, &admin)`. The admin adapter supplies explicit fixture timestamps through `enqueue_at` and
expires a lease while retaining its ID through `expire_lease_reusing_id`; the
next claim must increase its epoch. These hooks make age/preference ordering and
same-ID epoch fencing deterministic. `event_count` inspects stored events and is required when the server returns an
empty acknowledgement, so deduplication is still verified. Each mutation method
returns `conformance::AdminFuture<'_>` (a boxed async future) so the trait is object
safe. Use an isolated queue. The runner returns individual passed/failed cases;
it does not panic on protocol failures. The integration suite additionally tests
mock administration, terminal grants and serialized wire conventions.

`MockWorker::run_once` claims, heartbeats, sends supplied events and posts a
supplied `CommandResult` JSON value, releases a lease, or vanishes so it expires.
It runs no commands and needs no agent or container runtime.

`MockWorkServer` exposes deterministic clock parameters on its in-process lease
operations, plus `expire_leases`, `request_cancel`, `set_credentials`,
`set_terminal`, `send_terminal`, `result_for`, `release_for`, `events_for`,
`heartbeat_for` and `terminal_frames` for inspecting tests. HTTP leases last three
seconds and registrations advertise a one-second heartbeat interval. Events
return `{accepted}` counting newly stored `(attempt, sequence)` pairs.

Link polling returns `202 {}` while pending (optionally `interval_ms` to slow
polling), `200 {refresh_token}` on approval, `400 {error, code: "access_denied"}`
on denial and `410 {error, code: "expired"}` on expiry. `Client` respects the
advertised polling interval. Registering again updates the same linked worker.
Server-assigned label keys are refused in worker registrations; reported `awman/`
facts are allowed while configured `awman/` labels are rejected by `WorkerLabels`.
`WorkerLabels::with_server` returns a `Result` and refuses worker keys owned
by the server. The mock retains server ownership after labels are removed and
removes conflicting claims from older registrations when ownership is assigned.
Plain keys such as `pool` are accepted; bare dotted keys such as `infact.kind`
are rejected on submission (use `infact.example/kind`).

Executable lease requirements fail closed: unknown fields in the work item,
repository, credentials, execution, push, outputs or affinity are refused. The
client preserves the claim ID and epoch before decoding those requirements,
then releases unsupported claims as `ineligible`. A release failure is surfaced;
it is never treated as successful refusal. HTTP and terminal-handshake
`lease_lost` responses fence the same lease owner.

Queue items may additionally carry `body` (Markdown preview) and `source`
(`{url}`), so a picker can show the work before claiming it. The mock server
supplies both from the queued item. They carry no executable repository or
credential fields; the claimed `WorkItem` remains authoritative. Older servers
that omit the preview remain readable and show their summary instead.

Every lease-scoped request carries `x-wsp-epoch: <epoch>`, including the terminal
WebSocket handshake. The mock requires it and returns `409 {error, code:
"lease_lost"}` for missing, invalid or stale epochs. IDs may be reused after
expiry; the epoch must increase. The client remembers epochs returned by claims.
Engine handles should use `Client::with_lease_epoch(id, epoch)` to pin the
claim-time epoch even if another run reclaims that ID on the same client.
Unknown lease IDs send epoch zero and are fenced. The mock normally issues fresh
opaque IDs; `expire_lease_reusing_id` exercises same-ID fencing. Completed items
cannot be claimed again. Events, releases and results remain inspectable after
lease end.

`LeaseResult<R>` embeds the application's own `CommandResult` without converting
it. Affinity operators
retain WSP's explicit Kubernetes spellings (`In`, `NotIn`, `Exists`,
`DoesNotExist`); other enum values use snake_case. Timestamp and list helpers
use a vendored copy of 0139's helpers, guarded by a byte-identity test against
`src/data/wire.rs` when built in this repository. The Cargo manifest uses explicit
dependency versions, so the crate can also build outside the workspace. Millisecond duration
fields retain their existing integer Rust types, and the shared duration serde
adapters are also available through `wire`.
