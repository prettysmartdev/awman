# Release timing measurements

## Status

The release path has passed fault-injected state-machine rehearsals, but the
decision-to-verified-publication target is **UNMEASURED**. This workspace did
not contact GitHub, use a Mac or Linux KVM host, create a tag, or publish a
release. Layer 1 proves state-machine behavior and measures its local
orchestration overhead; virtual release timelines are estimates based on the
design inputs. Neither is a measured publication-time pass.

Percentiles use nearest rank, `ceil(p*N)`. Sample counts are small engineering
samples, not confidence bounds.

## MEASURED: fault-world orchestration

`cargo xtask release rehearse --repeat 10 --scenario <name>` drives the
production release state machine against `DirForge`, fake source/tools/hosts,
and a virtual clock. `orchestration_wall_ms` is monotonic wall time spent in
the harness after world setup while invoking the driver. It excludes Cargo
startup, report writing, GitHub/runner work, actual SSH/native work, host
recovery and human pauses. `recovery_wall_ms` measures only driver time in
resumed invocation(s) after the scripted stop. These figures are measured in
this Linux container and do not measure release latency.

| Rehearsal | Runs | Driver wall p50 / p95 | Resume driver wall p50 / p95 | Virtual critical path p50 / p95 | Result |
|---|---:|---:|---:|---:|---|
| Clean | 10 | 2,070 / 2,219 ms | n/a | 24.2 / 37.5 min | Passed |
| Transient CI infrastructure failure | 10 | 2,116 / 2,239 ms | n/a; retry is inside the same invocation | 26.0 / 37.5 min | Passed; one failed-job rerun, no extra release-build dispatch. Modelled retry wait/run: 12.2 / 13.5 min |
| Linux host unavailable at preflight | 10 | 23 / 26 ms | n/a; preflight refuses before starting work | 0 / 0 min | Passed; no push, claim or dispatch |
| Linux host lost during native work | 10 | 3,355 / 3,442 ms | 1,470 / 1,484 ms | 28.7 / 41.2 min | Passed; completed Mac result retained |
| Interrupted Mac native work | 10 | 3,389 / 3,515 ms | 1,497 / 1,510 ms | 28.8 / 42.5 min | Passed; orphan stopped and hosted/Linux work retained |
| Promotion retry after tag creation | 10 | 3,299 / 3,445 ms | 1,280 / 1,291 ms | 24.2 / 37.5 min | Passed; no CI/build/native rerun and tag retained |

The transient CI model includes a recovery attempt in its virtual critical path.
Its additional virtual recovery duration is recorded as the second CI attempt's
scripted queue plus run time in `recovery_modelled_minutes`; it is a model, not
a measured wall-time recovery. For the host, native and promotion retries, the
resume wall values above measure only fake-driver execution after the fault.
The time to restore a real host, rerun a real native test, or complete real
hosted work is **UNMEASURED**.

The transient CI retry model's 12.2 / 13.5 minute p50/p95 exceeds the design's
estimated <=8 minute recovery allocation. This is a modelled miss, not a real
timing result; the fake runner models one CI attempt duration rather than the
individual failed jobs' true production retry timing. Keep the recovery target
open until real hosted retry samples establish the duration.

Each stopped invocation reported exactly two formatted lines: one primary
error and one recovery instruction. Rehearsals also compare the annotated tag
target, seal message and release asset names/sizes/digests across invocations;
the tag remained at the validated commit and the sealed asset identity did not
change. Clean completion verified four assets and two native passes in the
fake release evidence. These assertions do not establish external byte
identity; the real release verification must read the published asset hashes
back and bind them to the seal and tag.

## ESTIMATED: decision to verified publication

The reviewed design allocates the following cold, prepared-environment serial
critical path. Independent hosted asset work overlaps; parallel phase durations
contribute their maximum, not their sum. These allocations total 49 minutes,
leaving 11 minutes under the 60-minute objective. All values in this table are
**ESTIMATES**, not measured timings or acceptance evidence.

| Critical-path phase | ESTIMATED allocation |
|---|---:|
| Decision, version/notes/docs preparation and routine review | 8 min |
| Controller bootstrap | 3 min |
| Preflight and required pinned-input downloads | 2 min |
| Dispatch and cumulative critical-path queue waits | 4 min |
| Longest parallel hosted chain: cold asset/kit builds, signing, smoke and transfer | 18 min |
| Native tail after assets arrive, including SSH and result collection | 6 min |
| Seal and draft reconciliation | 3 min |
| Routine operator confirmation | 2 min |
| Land, tag and publish | 1 min |
| Anonymous download and verification | 2 min |
| **ESTIMATED total** | **49 min** |

The design's transient CI recovery target is at most 8 additional minutes,
including queue time. A lost long build can push the run past 60 minutes. The
49-minute allocation is not p50 or p95; only complete real runs can establish
those percentiles. Real decision-to-verified-publication p50/p95: **UNMEASURED,
n=0**.

## Residual risks and limits

- No real GitHub dispatch, artifact/attestation transfer, draft upload,
  anonymous download, Mac HVF boot, SSH transport, Linux KVM boot, or operator
  confirmation was timed here.
- `make release REHEARSE=1` stops before landing, tagging and publication, so
  it cannot produce an end-to-end decision-to-verified-publication sample. Do
  not treat it as one. The disposable-repository `REHEARSAL_REPO` mode can;
  it is exercised only against the fault world here (layer 1) and has never
  been run against real GitHub: n=0 real samples.
- Coverage moved out of the release blocking path includes native guest
  `boot`, `exec`, `import`, `network`, `builder`, `strategy`, and test-build
  `upgrade` checks; builtin unit suites on the Mac; Apple Containers boundary
  checks; Linux executable-replacement checks; and artifact dependency scan,
  upgrade-policy and size/measurement checks. These remain diagnostic,
  scheduled or on-demand coverage as defined by the release design. Their
  absence can delay detection of runtime, adapter, dependency-policy and size
  regressions until those checks run.
- Blocking security/isolation evidence remains the hosted
  `component-security` and parity checks, exact-asset switch smoke, and native
  offline session boot on the shipped Mac and Linux assets. This evidence
  preserves the agent-host isolation requirement; it does not replace the
  moved guest strategy suite's per-agent descriptor coverage.
- Host-loss and interruption tests use fake hosts and a virtual clock. They
  verify retained work and recovery decisions, but not real supervisor cleanup,
  process containment or SSH reconnection behavior.
- The measured samples are ten runs per selected scenario on one Linux
  container. They are useful for code-path overhead and retry logic only.

## Maintainer procedure on the prepared Mac and Linux host

### Available today: real-host prepublication rehearsal

This exercises real hosted workflows and both native hosts but intentionally
stops before landing, tagging and publication. It does **not** measure the
complete release objective.

1. Prepare the Apple Silicon release Mac and reachable Linux x86_64 KVM host
   under the documented prerequisites. Finish one-time workflow/fixture setup
   before the timed sample; do not repair pins or workflow configuration
   inside a sample.
2. Check out the exact candidate commit with a clean worktree. Start the timer
   when the maintainer makes the release decision, before routine version,
   notes and documentation preparation. Record UTC start, commit, host image
   identifiers, run generation, and whether this is cold or warm.
3. From the Mac, run the single command below and record UTC finish when the
   command has verified the rehearsal draft and printed its result:

   ```sh
   make release REHEARSE=1 LINUX_HOST=user@linux-host
   ```

4. Preserve the command output, private timing ledger, CI/release workflow run
   IDs, native result digests and operator pause/retry intervals with the
   sample. Resume faults with the same command and include the interruption in
   total elapsed time. Classify a resumed/reuse run separately from a cold
   sample. This rehearsal deletes its owned draft and does not tag or publish.
5. Record the duration as **prepublication only**. It cannot be entered into
   the decision-to-verified-publication distribution.

### Complete end-to-end samples: disposable-repository mode

`REHEARSAL_REPO` completes the production path in an allowlisted disposable
repository and publishes only `rehearsal-vX.Y.Z-<nonce>` tags and releases,
verifies every public download and writes a receipt. No remote write has been
authorized or performed by this work item; a maintainer must authorize the
disposable repository explicitly. Do not substitute the production repository
or a product version.

One-time environment setup (not part of any sample): create the disposable
repository, push a copy of the source with `release.yml`, `ci.yml` and the
fixture pins active on its default branch, clone it on the Mac, and authorize
it with `git config --local awman.release.rehearsalRepos <owner/repo>`.

For each sample, use the Mac as controller and the Linux host as its native
worker, and pass the decision time so the ledger measures the whole hour:

```sh
make release REHEARSAL_REPO=<owner/repo> LINUX_HOST=user@linux-host \
  DECIDED_AT=2026-12-01T09:00:00Z
```

Each invocation (including interrupted and failed ones) is recorded with its
monotonic duration in the private journal, and `timings.jsonl` holds the
phase, service-run and `target missed` events. For each sample, time from the decision before routine release
preparation through anonymous verification and receipt creation; record the
same command's durable timestamps, GitHub run IDs, immutable seal digest,
published rehearsal tag and downloaded hashes. Use a unique rehearsal version
and generation per cold sample; run at least ten complete samples (prefer
twenty, including at least three cold prepared runs). For fault recovery,
record total decision-to-verification elapsed time including the fault and
recovery, plus the interval from the error to verified completion. Compute
nearest-rank p50/p95, maximum, sample count, failures and cold/warm/retry
classification. Keep all failures in the dataset. Until those samples exist,
the under-60-minute end-to-end criterion remains **UNMEASURED**.
