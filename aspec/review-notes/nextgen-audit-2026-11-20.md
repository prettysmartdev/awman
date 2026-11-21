# nextgen prerelease audit — 2026-11-20

Reviewed commit: `49eff9f8b2cca1c7084a001c441840f31a0286f0` (`nextgen`).

This is a new audit, independent of the preexisting untracked
`nextgen-prerelease-review.md`. Sol reviewed architecture/backend parity and
security; Terra reviewed release, CI, documentation, frontend behavior and test
risks; the primary reviewer inspected the reported code paths and reviewed
runtime reliability, streaming and local verification. Production code was not
changed. Findings below are source-confirmed unless explicitly described as a
risk or an outstanding evidence requirement. Source confirmation is not native
runtime reproduction.

**Recommendation: do not treat this revision as ready for release.** There are
security-boundary, runtime-selection and data-integrity defects, as well as
explicit prerequisites that currently block release rehearsal. Passing local
tests cannot replace native acceptance or establish that these untested paths
are correct.

## Consolidated findings

Severity describes impact, not implementation effort. High findings should be
resolved before release; Medium findings need a fix or an explicit disposition.
No Critical finding was established. References use line numbers at the reviewed
commit.

| ID | Severity | Domain | Location | Finding |
|---|---|---|---|---|
| A01 | High | Mount security | `src/command/session_create.rs:103` | An exact allowlisted subdirectory can become a mount of its parent Git checkout. |
| A02 | High | Host execution | `src/engine/ready/host_agent.rs:224` | Ready pings resolve executables against the original checkout PATH before switching to scratch cwd. |
| A03 | High | Runtime parity | `src/command/daemon_runtime/bootstrap.rs:55` | Daemon `--set` overrides are applied after the actual runtime has already been selected. |
| A04 | High | Persistence/upgrade | `src/data/fs/data_paths.rs:179` | Legacy migration can delete a legitimately populated new database created by foreground automation. |
| A05 | High | Reliability/API | `src/command/automation/launch.rs:827` | Headless automation publishes completion despite failed durable writes. |
| A06 | Medium | Secret redaction | `src/engine/auth/redaction.rs:448` | Streaming redaction leaks the suffix of overlapping registered secrets. |
| A07 | Medium | Windows security | `src/data/fs/private_files.rs:104` | The shared private-file helper lacks equivalent Windows ownership, ACL and no-follow checks. |
| A08 | Medium | Builtin parity | `src/engine/container/builtin/exec_bridge.rs:331` | Builtin phase steps ignore full-transcript and startup-grace options. |
| A09 | Medium | Builtin lifecycle | `src/engine/container/builtin/backend.rs:670` | Explicit stop also removes a builtin instance, breaking stop-then-remove profile recovery. |
| A10 | Medium | Architecture/store | `src/frontend/api/command_frontend.rs:1896` | The API frontend opens a second database and silently falls back on errors. |
| A11 | Medium | API/TUI parity | `src/command/commands/agents/mod.rs:75` | Profile command results have a default no-op presenter and disappear outside CLI. |
| A12 | Medium | Architecture/lifecycle | `src/frontend/api/routes.rs:2091` | HTTP routes own session-setup scheduling and completion orchestration. |
| A13 | Medium | Wire correctness | `src/engine/remote/events.rs:69` | SSE decodes each HTTP chunk independently, corrupting split UTF-8 characters. |
| A14 | Medium | Cancellation UX | `src/command/commands/remote.rs:650` | Ctrl-C while following a quiet remote job does not interrupt the stream wait. |
| A15 | Medium | Stream reliability | `src/engine/remote/events.rs:113` | EOF without `Done` is reported as clean completion, bypassing reconnect. |
| A16 | High readiness blocker | Release | `ci/gates.toml:1088` | Required guest archives remain pending, so both candidate and rehearsal preflight refuse to proceed. |
| A17 | Medium | Release concurrency | `.github/workflows/candidate.yml:80` | A unique run ID defeats the promised same-version serialization. |
| A18 | Medium readiness blocker | CI | `.github/workflows/nightly.yml:144` | Required source seeded-defect patches are absent, so nightly cannot pass. |
| A19 | Medium | Test safety | `tests/boundary/conformance/container_world.rs:103` | Boundary cleanup deletes fixed-name and prefix-matched Docker resources without ownership proof. |
| A20 | Medium risk | Test flakiness | `tests/drivers/guest_driver.rs:613` | Native PTY resize validation relies on a 300 ms delay across independent channels. |
| A21 | Low | Test diagnostics | `tests/component/engine/git_credentials.rs:261` | A 30-second readiness wait exceeds its enclosing 20-second test deadline. |
| A22 | Medium | Authoritative specs/docs | `aspec/uxui/cli.md:413` | Runtime names, config shape and platform support contradict current code and architecture specs. |
| A23 | Low | Web recovery UX | `web/src/components/Squad.tsx:103` | Squad recovery instructions target a hardcoded listener and overwrite its feature list. |
| A24 | Low | API wire contract | `src/command/commands/api_server/admission.rs:640` | Admission omits the documented `flags_applied.automation` field. |
| A25 | Medium | Memory/backpressure | `src/engine/acp/client.rs:139` | ACP transport and update queues grow without bounds when the frontend stops consuming. |

## Security and isolation

### A01 — exact workdir permission does not constrain the eventual mount

`SessionCreatePolicy::validate_workdir` accepts an exact canonical match and
returns before Git-root validation (`session_create.rs:103–108`). That is a
reasonable rule only if the eventual mount remains that exact directory.
`SessionSetup` subsequently opens the session with default options
(`src/command/session_setup.rs:657`), and `DirectoryGitRootResolver` walks upward
to the nearest `.git` marker (`src/data/session_manager.rs:35–48`). Agent launch
then constructs the workspace mount from `session.git_root()`
(`src/engine/agent/mod.rs:865–878`).

If the operator permits `/repos/project/allowed`, but the checkout root is
`/repos/project`, a client admitted for `allowed` gains the parent checkout in
the guest, read-write by default. This is distinct from the correctly stricter
`workdir_roots` direct-child rule. The existing tests exercise that rule and
interactive mount decisions, not this admission-to-launch composition.

Carry an authorized mount boundary through setup, persistence/recovery and every
launch, or reject an exact entry that would require mounting a different root.
Add a production-interface test that creates an exact-subdirectory session and
inspects the final runtime launch mounts, including after restart.

### A02 — scratch cwd does not constrain ready-ping executable selection

`HostAgentPinger` resolves `program_in(&snapshot, &cmd)` and only then sets the
child's cwd to its temporary directory (`host_agent.rs:224–229`). The snapshot
comes unchanged from the invoking session (`src/engine/ready/mod.rs:145–156`).
`find_program` explicitly uses that snapshot's original cwd and PATH
(`src/engine/host_cli.rs:51–61`); relative PATH directories resolve against the
checkout. An absolute PATH entry pointing into the checkout has the same issue.
First-party pings use ordinary executable names and do not pass through custom
definition executable approval.

The prerequisite is an operator PATH containing a checkout-local directory ahead
of the installed agent. In that case repository-controlled executable selection
occurs on the host despite the claimed scratch-directory isolation. This review
did not execute a host agent or a reproduction payload.

Use a dedicated installed-program resolver for first-party host checks. Refuse
checkout-local candidates after canonicalization, and keep explicitly approved
custom pings distinct. Test selection with fake executables and both relative
and absolute checkout-local PATH entries; changing cwd alone is insufficient.

### A06 — stream redaction fails to union overlapping matches

The nonstream implementation finds all occurrences and unions overlapping ranges.
`RedactionStream::drain` instead matches greedily at the current position and
advances by that secret's full length (`redaction.rs:448–462`). It therefore
never considers a second secret beginning inside the consumed match.

For synthetic registered values `abcde` and `defgh`, the text `abcdefgh` can
produce `***fgh`, while the nonstream implementation masks the union. This
exposes a suffix, not ordinarily the entire second credential. Streaming output
feeds terminal/status/automation consumers, making divergence material even
though ordinary non-overlapping secret tests pass.

Preserve enough pending context to union overlaps, including overlaps split
across feeds. Add equivalence properties between streaming and whole-buffer
redaction over arbitrary chunk partitions, overlapping values and retirement.

### A07 — private-file guarantees are Unix-only

In `src/data/fs/private_files.rs`, directory mode/owner checks and file
`O_NOFOLLOW`, link-count, owner and mode checks are all under `cfg(unix)`
(`:40–62,104–141`). The remaining Windows path opens normally and checks only
that the resulting object is a regular file. Creation inherits the surrounding
ACL. These helpers back API key hashes and TLS material, not just test files.

On a normally private Windows profile the inherited ACL mitigates exposure. On
a shared/custom data root, the helper's advertised owner-only guarantee is not
enforced; file symlinks/reparse behavior also lacks an equivalent boundary.
Implement native secure opens and ACL validation through the shim boundary, or
refuse stores whose privacy cannot be established. Validate on Windows; this
Linux review does not establish a native exploit or default-profile exposure.

## Runtime, persistence and frontend contracts

### A03 — configured runtime and actual runtime can disagree

Daemon bootstrap calls `Engines::for_env` before resolving `b.set`
(`bootstrap.rs:55–68`). `Engines::for_env` resolves file and environment layers,
then creates the runtime and credential monitor
(`src/command/dispatch/engines.rs:143–174`). Bootstrap installs those already
created handles into `Core` (`bootstrap.rs:102–113`) while exposing config that
includes `--set`.

With Docker as the file/default setting, starting a daemon with
`--set runtime.kind=builtin` can therefore leave execution on Docker. The
inverse can also select an unintended backend. This is more serious than stale
display text because runtime selection controls isolation and capabilities.

Command-local TUI/API `--set` has a related issue: dispatch applies overrides to
a session clone but passes unchanged engines into the command
(`src/command/dispatch/mod.rs:475–493,516–525`). Such process-scoped changes must
be rebound safely or explicitly refused, not accepted and ignored.

Resolve process config once before constructing engines. Add backend-identity
tests for daemon startup overrides and explicit rejection/rebinding tests for
command-local runtime changes. Check process-wide builtin state settings and
credential-refresh settings through the same path.

### A04 — foreground records can be mistaken for an interrupted migration

`ForegroundRecord::create` directly opens `<data_home>/data/awman.db`
(`src/command/automation/launch.rs:108–125`) without running legacy migration.
If an upgraded installation still has `<data_home>/api/awman.db`, a foreground
automation run creates and populates a second valid database. Later daemon
startup invokes `migrate_legacy_db` (`src/engine/daemon/core.rs:46–50`). When
both paths exist, migration assumes the destination is incomplete and removes
it and its WAL/SHM (`src/data/fs/data_paths.rs:179–192,281–286`). Newly recorded
automation is lost.

Provide one migration-aware store-opening path for every producer. Use an
explicit migration journal/identity to distinguish an interrupted copy from an
independently populated destination. Preserve both on an unexplained conflict.
Regression coverage should create a legacy DB, run a foreground producer, start
the daemon and assert that neither set of records is silently discarded.

### A05 — completion events can get ahead of failed persistence

`HeadlessRun::finish_observed` discards errors from delivery/observation writes,
`complete_turn`, execution transition and `finish_execution`
(`src/command/automation/launch.rs:774–804,827–865`). It then emits
`AgentTurnCompleted`/ended state and returns the successful native result.
A failed SQLite transaction can leave the durable turn unfinished while clients
have already received verified completion. This contradicts the durable-before-
publication contract in `aspec/architecture/design.md:369–373`.

Propagate persistence failure, and commit the related turn, execution and
command projection before publishing success. Test an injected store-write
failure and restart/reconnect, asserting that replay agrees with the original
response. The store also rejects serialized turn results above 8 MiB; this audit
does not rely on an unverified provider-output route to that bound.

### A08 — builtin drops two phase-step options

`PhaseStepContainerSpec::options` requests `StartupGrace(Duration::MAX)` and
`FullTranscript` (`src/engine/container/phase_step.rs:71–72`). The CLI-backed
container path honors them (`src/engine/container/process.rs:277–301`). Builtin
launch planning does not carry either into `Instance`/`ExecBridge`; the bridge
always creates the default 100-line tail and uses `frontend.grace_timeout()`
(`src/engine/container/builtin/exec_bridge.rs:331,355`).

An attended quiet setup command can consequently be interrupted after the normal
30-second grace period, while the same Docker command remains valid. A failed
setup/teardown command producing over 100 lines loses earlier diagnostic context
before the remediation agent receives its transcript.

Carry these resolved options through the backend-neutral launch contract. Cover
a quiet phase past normal grace and a failure with more than 100 lines in backend
conformance. Full transcripts also deserve bounded disk-backed storage rather
than unlimited host memory; preserve the complete-output contract when doing so.

### A09 — builtin stop is removal

Builtin `stop` calls `stop_owned` (`builtin/backend.rs:670–688`); that default
calls `finish_owned(..., true)` (`builtin/driver.rs:165–167`). Docker stop and
remove are separate operations. Profile orphan recovery calls stop followed by
remove (`src/command/commands/launch_policy/settings_profile.rs:525–532`); builtin's second lookup fails
because stop already deleted the instance, leaving the profile Closing with a
runtime-unavailable result despite successful cleanup.

Make explicit stop preserve the stopped instance and reserve removal for cleanup
paths that request it. Test stop/get/remove and profile close across backends.
`agents kill` already reconciles an absent instance, so it is not claimed to
misreport this particular failure.

### A10 — API automation bypasses the shared store

`ApiDispatchFrontend::automation_host` opens `SqliteSessionStore` by path and
constructs a new `AutomationStore` (`command_frontend.rs:1896–1902`). This
violates the shared-connection architecture and makes embedded/in-memory core
stores diverge from the frontend's store. Its `.ok()?` conversions additionally
turn storage errors into `None`; automation interprets `None` as foreground
execution and creates different command/session records
(`src/command/automation/launch.rs:221–227,509–515`).

Inject the existing Layer 2 automation host/store. Distinguish an actual
foreground invocation from failed API setup in the type/signature. Cover shared
in-memory stores and a store-opening failure without fallback or identity change.

### A11 — profile commands have no API/TUI result projection

Profile commands call `show_profile_result` (`agents/profile.rs:91,195`), whose
trait default is empty (`agents/mod.rs:75`). The API implementation supplies
status/list/detail/kill callbacks but no profile callback
(`src/frontend/api/command_frontend.rs:1792–1811`); the queue's alternate
`catalogue_json` path excludes agent outcomes (`queue_worker.rs:2013–2018`).
The builtin TUI also lacks this presenter. These leaves are advertised for local
owner frontends, including API, and declare JSON output.

CLI output therefore does not establish parity: local API profile list has no
profile result data, and a typed close failure can disappear from presentation.
Add the shared result projection and TUI presenter; test list, successful close,
and typed failed close through all admitted frontends.

### A12 — session setup orchestration remains in HTTP routes

The HTTP create route validates labels, registers cancellation, spawns setup,
reconciles state and sequences finalization (`src/frontend/api/routes.rs:2037,
2091–2105,2167–2185`). The typed `WorkerApi::sessions_create` only admits and
returns a plan (`src/command/daemon_api/worker.rs:142–161`). A caller of that
lower-level API must reproduce lifecycle business logic to obtain the same
behavior as HTTP.

Move ownership of scheduling/cancellation/finalization into Layer 2 and accept
presentation/event adapters there. Keep HTTP parsing/status mapping in the
frontend. The import lint can pass while this semantic architecture violation
remains; add a typed-API lifecycle test, not just another import pattern.

## Streaming and UX

### A13 — HTTP chunk boundaries corrupt text

`stream_job_logs` calls `String::from_utf8_lossy` on each network chunk before
appending it to its SSE buffer (`src/engine/remote/events.rs:69`). TCP/HTTP chunk
boundaries need not align with a UTF-8 character. A split multibyte character is
replaced by multiple replacement characters even when the complete event is
valid UTF-8 JSON. Unicode output, messages and step names can be changed.

Frame bytes first and decode complete event data, or use an incremental UTF-8
decoder. Cover every byte split of an event containing multibyte text, with
multiple events per chunk. Consider a shared bounded SSE decoder so newline,
size, EOF and reconnect semantics are not separately implemented by each client.

### A14 — Ctrl-C requires a later typed event

The follow task's signal handler only flips an atomic flag
(`src/command/commands/remote.rs:650–656`). The flag is tested by the event sink,
but `stream_job_logs` waits for bytes and ignores keepalive comments. A quiet
job can therefore keep follow blocked after Ctrl-C; the request timeout is one
day (`src/engine/remote/events.rs:52`).

Select the cancellation token against the stream future (and reconnect delay)
so cancellation does not depend on another event. Test a healthy SSE connection
that sends only comments and verify prompt local detachment without cancelling
the server-side job.

### A15 — premature EOF is treated as successful follow completion

When the response stream ends without `EventPayload::Done`, `stream_job_logs`
still invokes `on_stream_end` and returns `Ok(())` (`events.rs:113–114`). The
caller retries only `Err` (`remote.rs:667–692`), so a cleanly closed connection
from a proxy or server can silently end follow while the command is running.

Distinguish terminal `Done`, deliberate user detachment and incomplete EOF.
Reconnect incomplete streams using the last acknowledged event cursor to avoid
replaying output. Test EOF before Done, after Done, and mid-frame.

### A25 — ACP line limits do not bound queued host memory

ACP transport uses unbounded stdout/stderr/stdin channels
(`src/engine/acp/client.rs:43–45`) and unbounded permission/update channels
(`:139–140`). The session driver calls synchronous frontend `next_prompt` and
`request_permission` methods while its update receiver is not being drained
(`src/engine/acp/session.rs:327–346,406–420`). The independent reader can keep
accepting and queueing valid agent updates during those waits. The 8 MiB
per-line framing limit does not bound the sum of queued lines or decoded updates.

A noisy or malfunctioning container can therefore consume growing host memory
while the UI awaits a person or is otherwise slow. Introduce bounded queues and
an explicit overflow policy that does not deadlock response/permission traffic;
coalesce appropriate state updates and separate protocol control from bulk
output. Exercise a stalled frontend with sustained synthetic output and verify
bounded memory and responsive cancellation. No host-memory exhaustion experiment
was run during this review.

## Release and test readiness

### A16 — candidate preflight is intentionally blocked

Both `guest-oci-base` and `guest-docker-base` remain `pending` in
`ci/gates.toml:1088–1098`, with no immutable published archive pins.
`xtask/src/release/candidate.rs:223–229` rejects this, including dry-run
candidates through `xtask/src/release/state/inputs.rs:90–97`.

Publish reviewed fixtures, verify their digests/provenance and commit the pins;
then obtain the required native evidence and run rehearsal. Do not remove the
guard or call the missing infrastructure a passing skip. This is an outstanding
cutover prerequisite, not an accidental fail-open vulnerability.

### A17 — every candidate gets a different concurrency group

The candidate workflow promises one run per version/mode, but the group always
includes `github.run_id` (`candidate.yml:76–82`). Simultaneous runs can select
the same next candidate number, duplicate all builds and collide at assembly
(`xtask/src/release/state/decide.rs:98–124,161–176`). Assembly fails closed, so
this finding is wasted work/recovery and broken serialization, not unguarded
publication.

Use a stable normal-candidate group while keeping journal-only dispatches
independent. Exercise two same-version candidates and journal dispatch while one
candidate is active; do not assume pending-run behavior provides a durable queue.

### A18 — source defect detection is required but has no inputs

Nightly requires `ci/seeded-defects` (`nightly.yml:144–145`), which does not
exist. The helper refuses zero patches (`xtask/src/ci/seeded.rs:130`) and the
nightly verdict marks this job blocking (`nightly.yml:365–374`). Supply reviewed
source defect patches and demonstrate they are detected. This is documented
unfinished infrastructure, not proof of mutation coverage.

### A19 — Docker boundary tests do not own everything they remove

Conformance setup removes the fixed `boundary-foreign-conformance` name before
creating a fixture (`container_world.rs:103–104`). Cleanup removes all containers
matching `awman-conformance-` and fixed image tags (`:125–134`). Template boundary
tests also build over and remove `awman-boundary-project:latest`
(`tests/boundary/templates/mod.rs:173–179`).

On a developer/native granted daemon these can remove another run's or an
existing user's resources. A private hosted CI daemon reduces that exposure.
Allocate unique run IDs, label owned resources and delete only recorded IDs.
Test two concurrent fixture worlds and preserve a preexisting similarly named
container/image.

### A20/A21 — timing assumptions weaken native and component diagnostics

The guest PTY resize scenario sends resize, sleeps 300 ms, then sends stdin that
causes `stty size` (`tests/drivers/guest_driver.rs:613–622`). Resize and stdin
travel independently; load can reverse effective processing order. The test
then waits for a size the guest will never print again. This is a source-visible
race risk, not a reproduced flake in this environment. Synchronize on an
observable resize acknowledgement or repeat the query within a deadline.

The Git credential component test waits 30 seconds for an authenticated request
(`tests/component/engine/git_credentials.rs:261`), while its nextest suite kills
tests at 20 seconds (`.config/nextest.toml:166–169`). Align setup/cleanup budgets
with the tier, so failures reach the owned diagnostic path before nextest kills
the test. Do not simply increase every suite's timeout.

## Documentation, formats and maintainability

### A22 — conflicting runtime contracts

The authoritative CLI spec says runtime is a string, uses
`apple-containers`/`docker-sbx-experimental`/`builtin-experimental`, rejects
`builtin`, and restricts builtin to macOS (`aspec/uxui/cli.md:413–417`). Its
example also has the obsolete layout (`:446–453`). The current implementation
uses `runtime.kind` with `docker`, `apple`, `sbx`, `builtin`
(`src/data/config/runtime.rs:75–98`); current architecture and configuration
documentation include Linux x86_64 builtin support. The grand-architecture
document also retains an older runtime-selection paragraph and image-build
policy description while the newer design describes the updated behavior.

User-facing instructions repeat the obsolete names, notably the builtin-init
workaround in `docs/00-getting-started.md:112` and squad runtime instructions in
`docs/12-squad.md:920–927`. Following the former produces rejected config.
Reconcile authoritative specs first, then current guides and examples. Keep
historical release notes historical. Add validation of active configuration
examples against the current decoder where it protects a real user journey.

### A23 — web Squad recovery command changes the wrong settings

The prerelease web Squad view instructs users to set
`daemon.network.listeners.loopback.features engine,squad`, regardless of the
connected listener (`web/src/components/Squad.tsx:103`). It can change an unrelated
listener, and on loopback it replaces a list that may already include `worker`.
Use the reported listener identity and preserve its existing feature set.
`web/src/lib/connect/model.ts:248` already uses the reported name elsewhere.

### A24 — accepted automation settings are missing from admission output

`flags_applied` starts from frontend-profile defaults and copies selected fields
(`src/command/commands/api_server/admission.rs:640–682`). The contributing
profile table does not include automation
(`src/command/dispatch/projections/raw_args.rs:233–258,399–419`), so the response
omits the documented `flags_applied.automation` value
(`docs/09-api-and-remote-mode.md:2935–2941`). Clients cannot inspect the accepted
mode through the advertised field. Define and test the exact response projection
for absent, bare and explicit automation options; update web consumers if the
wire projection changes.

### Smaller observations

- `README.md:4` has malformed badge Markdown pointing at the removed `test.yml`
  workflow; current CI is `ci.yml`.
- `aspec/devops/architecture-lint.md` also refers to the removed workflow.
- Several vendored `third_party/sqlx-sqlite-0.9.0/src/*.rs` files have executable
  Git modes. This is low-impact repository hygiene, not an execution finding.
- Cursor and Cline image recipes still execute mutable downloaded installer
  scripts (`templates/Dockerfile.cursor:57`, `templates/Dockerfile.cline:37`).
  Builds run inside images; this is supply-chain/reproducibility exposure, not
  proof of a compromised endpoint. Where available, prefer verified installation
  artifacts. Preserve the spec's distinction between installation defaults and
  user-controlled agent version selection.
- API routes/presenters and the main agent module remain large (3,444, 2,134 and
  3,057 lines respectively). Size alone is not a defect. A10–A12 identify actual
  responsibilities to extract: shared store ownership, typed result projection
  and lifecycle orchestration. Prefer those changes over cosmetic module splits.
- Raw DTO construction, `.ok()?` at ownership boundaries and default no-op
  frontend methods make omitted behavior compile successfully. Strengthen those
  interfaces where missing behavior changes outcomes; do not add wrappers merely
  to satisfy a style preference.

## Verification and coverage

Host: `aarch64-unknown-linux-gnu`, Rust 1.94.0. The local runner selects the
`linux-x64-default` manifest recipe through its same-OS fallback, but compiles
for the actual ARM64 host. These results are not native x86_64 or builtin-VM
acceptance.

| Check | Result |
|---|---|
| `make test` | Passed: 1,912 tests across 30 nextest invocations; all selected suites passed, exit 0. This was one complete pass, not a measured flake-rate study. |
| Architecture rules, formatting, hygiene, NOTICE, workflow/matrix generation, manifest checks | Passed through `cargo xtask lint all`; manifest reported zero structural gaps. |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Initial default-parallel invocation was killed by SIGKILL with no compiler finding. Repeated with `CARGO_BUILD_JOBS=1`: passed. |
| Shell syntax | 24 scripts passed `bash -n`; ShellCheck was unavailable. |
| Dependency licenses/bans/sources and advisory scan | Not run: cargo-deny and cargo-audit were unavailable. The repository has a nightly advisory job, which was not executed here. |
| README/top-level docs relative file targets | No missing file targets found by the review helper; anchors were not validated. |
| Native/boundary/artifact/release acceptance | Not run on this host; prerequisite and platform limits remain open. |

The original combined lint command exits nonzero because of its first Clippy
SIGKILL. The separate successful serial Clippy result resolves that check; it
does not turn omitted cargo-deny or ShellCheck checks into passes. Logs for this
session are `/tmp/awman-review-tests.log`, `/tmp/awman-review-lint.log` and
`/tmp/awman-review-clippy-serial.log`.

Coverage included the architecture/spec contracts; runtime launch and lifecycle
translation; API/daemon admission and ownership; automation persistence and
upgrade; credential redaction and private state; wire streaming and cancellation;
release workflows/manifests and promotion preconditions; test supervision,
selection and timing; current runtime/readiness documentation; and a light web
API/recovery consistency pass. The webapp was not deeply reviewed, as requested.

Confirmed good practices in inspected paths:

- API keys use 32 bytes from the OS-backed secure random source, hashed storage,
  fixed-size constant-time comparison, and rejection with no configured keys.
- Unix private state helpers enforce owner/mode/link checks and private atomic
  publication; the Windows gap is explicitly separated above.
- ACP filesystem access defaults to denial; container paths are not implicitly
  interpreted as host paths.
- The library forbids unsafe code; native shim code inspected here documents
  its FFI safety invariants. This was not an exhaustive audit of vendored VMM code.
- Cargo.lock is tracked; workflow action references inspected are commit-pinned;
  release preflight and assembly collision checks fail closed.
- The manifest runner uses owned fakes/private test state; missing native
  resources are not accepted as passing evidence. Source changes select the
  full PR set, ruling out the suspected cross-layer change-selection gap.
- No tracked credential files matching the inspected common secret-file names
  were found. This is not a historical secret-scanner attestation.

Remaining evidence: native Apple Silicon, Linux x86_64/KVM, Windows and real
Docker/Apple boundary execution; exact release artifact and offline acceptance;
GitHub-side protection/runner permissions and actual rehearsal; current locked
dependency advisory scan; and stress measurements of bounded buffering and
concurrent sessions. Local source inspection does not certify those properties.

## Recommended sequence

1. Fix A01–A05 before release, with regression coverage at the responsible
   command/runtime/persistence boundary. Include A06–A12 in that correction pass
   because they share those contracts.
2. Resolve release prerequisites A16/A18 and the candidate serialization defect;
   obtain native evidence without weakening gates.
3. Repair streaming cancellation/decoding/backpressure, test ownership/timing and the current
   runtime documentation. Keep web clients aligned if wire types change.
4. Run the manifest local/PR gates, targeted failure-injection tests, native
   acceptance and candidate rehearsal against the corrected commit. Review that
   evidence before deciding release readiness.
