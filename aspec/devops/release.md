# Release operator runbook

A release is one command run from the Apple Silicon release Mac:

```sh
make release
```

It starts the hosted CI gate and the four asset builds together, runs the
required checks on exact attested bytes on this Mac and a Linux host (four
native checks and the [selected SBX smokes](#release-checks-on-real-hosts)),
seals one draft release, asks once for confirmation, then lands the release
commit on `main`, tags it, publishes the draft and verifies the public bytes
anonymously. After any stop, the recovery is the same command again:
completed work is kept and only unfinished work is repeated.

The target is publication in under one hour from the command, with no
runner registration, no organization setup and no manual merge. The target is
an engineering allocation, not a measured guarantee. Measured timings, how they
were taken and what remains unmeasured are recorded in
[release-measurements.md](release-measurements.md).

## Release invariants

- The tag is plain `vX.Y.Z`, matching `Cargo.toml`. Pre-release suffixes are
  refused.
- Four assets are built once each, by hosted `release.yml` jobs checked out at
  the release commit R: `awman-linux-amd64`, `awman-linux-arm64`,
  `awman-macos-arm64` and `awman-windows-amd64.exe`. Each carries a GitHub
  build-provenance attestation naming `release.yml` and R. The same bytes are
  smoke-tested on their hosted OS, uploaded to the draft, re-read before
  publication and downloaded anonymously afterwards.
- Every check on real hosts is required and has no waiver flag: the signed
  offline boot plus persisted-state upgrade of the macOS asset, the Apple
  Containers smoke on the Mac, the offline KVM boot and the Docker smoke of the
  Linux amd64 asset on the Linux host, and each selected SBX smoke. A missing,
  blocked, short, timed-out, stale, unbound or failed result prevents sealing.
  A blocked check never falls back to another runtime or another host.
- Publication evidence is **sealed** before confirmation: `SHA256SUMS`,
  `release-evidence.json` and `seal.json` freeze every asset digest, the CI run,
  the native results, the commit, tree and plan. The confirmation, the tag
  message and the published release all name the same seal digest.
- R is validated first and landed unchanged: fast-forward when `main` has not
  moved, otherwise a server-side merge commit. There is no squash, so the tag
  points at exactly the commit whose bytes were built and booted. Unrelated
  movement of `main` is tolerated; a merge conflict or a change to the
  publication policy blocks with a named reason.
- A published tag and published assets are never moved, replaced or deleted. A
  bad release is superseded by a new version.
- Nothing runs an agent on a host. The boot, Apple Containers and Docker checks
  run a synthetic agent only inside a guest or container started by the exact
  asset; every host mount lies in the check's private state and no parent of the
  private test repository is mounted. The SBX smoke runs a shell in a disposable
  sandbox and a preparation-only `awman ready`; it runs no agent or provider
  task. No GitHub token or provider key is sent to the Linux host. No
  self-hosted runner executes repository workflows.

## Prerequisites (the prepared environment)

Run `make release-doctor` at any time: it checks the prerequisites below for the
current commit with [read-only probes](#doctor-probes). It creates no sandbox,
container, claim or remote branch. `make release` runs the same checks first
and stops at the first failure, before anything is pushed or dispatched. A probe
does not replace the check: image loading, building and sessions are first
exercised by the check itself.

**Repository and GitHub**

- `gh` is authenticated as a maintainer who can push branches and tags,
  dispatch workflows, read Actions runs and artifacts, verify attestations and
  create and edit releases. Repository write is enough; organization
  administration is not needed. Branch and tag rules are the maintainer's
  choice; preflight reports a rule this login cannot satisfy
  (`publication-policy-denied`). The login must also be able to read the
  branch rules and tag rulesets: unreadable or hidden rule data is
  `publication-policy-unknown`, never treated as unrestricted.
- `origin` is the canonical repository. `release.yml` and the current `ci.yml`
  are active on the default branch. This is one-time rollout work, not a
  per-release step.
- The reviewed fixture pins in `ci/gates.toml` are published, including the
  architecture-specific amd64 guest fixture. A pin still marked pending blocks
  with `pin-pending`; nothing substitutes a locally built fixture. A maintainer
  can publish, verify and commit both architecture pins beforehand with
  `make fixtures-pin` on a clean branch based on `origin/main`.

**Release Mac (Apple Silicon)**

- Hypervisor.framework, `codesign`, `sandbox-exec`, Git, the pinned Rust
  toolchain, free disk for the expanded native kits, payload, fixtures and VM
  state, and a short private temp root under `/private/tmp` so guest socket
  paths stay under the 103-byte limit.
- Apple Containers: the `container` CLI on `PATH` and its system service
  running (`container system start`; `container system status` succeeds). See
  [docs/11-runtimes.md](../../docs/11-runtimes.md#apple-containers).
- Docker Sandboxes: `sbx` **0.54.0 or newer** on `PATH`, signed in with
  `sbx login` as the account running the release, so that
  `sbx --no-input ls --json` succeeds. The release uses this existing login; no
  dedicated identity, API key or environment export is needed.
- A Docker Engine and `python3` are not used on the Mac.

**Experimental Docker Sandboxes**

SBX is enabled in shipping builds on Apple silicon macOS and Linux x86_64
after the accepted native developer smoke. `make release` runs the
[release SBX smoke](#what-each-smoke-proves) on the selected hosts.
`make validate-sbx` remains the separate current-session developer diagnostic
(add `LINUX_HOST=user@host` to include Linux); its report is not release
evidence.

The deeper `artifact-sbx-smoke` provider suite and its native kit declaration
remain registered as opt-in diagnostics, outside the release plan and hosted
asset build. They retain their dedicated identity, provider-key, strict cleanup
and evidence requirements when selected. `make release` needs no API keys,
dedicated identity or credential provisioning.

**Linux host (x86_64 GNU)**

- Reachable over noninteractive SSH as `LINUX_HOST=user@host`, with read/write
  `/dev/kvm`, Git, a glibc compatible with the kit tools, `setsid`, `tar`,
  `curl`, `ps`, `timeout`, `sha256sum` and the other usual coreutils, working
  kernel seccomp offline enforcement and at least 4096 MB free under `/tmp`.
- A Docker Engine on that host, usable by the SSH account without a password:
  `docker` on that account's noninteractive `PATH` and `docker version`
  reaching a server. The Docker smoke uses `DOCKER_HOST` from the SSH account's
  environment, otherwise `unix:///var/run/docker.sock`. It does not select a
  Docker context, so an engine reachable only through a context is reported
  blocked by the check even if the probe passes.
- Only when [Linux SBX is selected](#sbx-host-selection): `sbx` 0.54.0 or newer
  on the SSH account's noninteractive `PATH`, signed in as that account, plus
  `base64` and `mktemp`.
- No Cargo, checkout or GitHub credential is needed on it.

`LINUX_HOST` is stored in `git config --local awman.release.linuxHost` after
the first successful preflight; later runs may omit it for the KVM and Docker
checks. The saved value never selects Linux SBX.

Run the release with no Git routing variables (`GIT_DIR`, `GIT_WORK_TREE` and
the like) set on the Mac or in the Linux SSH account's environment: the SBX
readiness repository setup does not clear them.

### Doctor probes

| Probe | What is checked | Bound | Stop code |
|---|---|---|---|
| Mac boot host | Hypervisor.framework, tools, socket path, 4096 MB free, pinned downloads | | `mac-host` |
| Apple Containers | `container system status` | 20 s | `apple-service-unavailable` |
| Linux boot host | architecture, `/dev/kvm`, tools, seccomp, free disk | SSH connect 8 s | `linux-host-unreachable`, `linux-host-prerequisite` |
| Linux Docker | architecture, tools, free disk, `docker version` reaching a server | 20 s | `docker-daemon-unavailable`, or the Linux host codes |
| SBX, each selected host | CLI fingerprint, `sbx --version` at least 0.54.0, `sbx --no-input ls --json`; Linux must be x86_64 | 20 s per command | `sbx-smoke-blocked`, with the reason code in the message |

**Release prep**

Version, `Cargo.lock`, documentation and `docs/releases/vX.Y.Z.md` (no
placeholder) are committed, by the `release-prep` skill, on a clean `main` or
`release/vX.Y.Z` checkout whose base `origin/main` is an ancestor of HEAD.
`VERSION` defaults to the `Cargo.toml` version at HEAD.

## Release checks on real hosts

| Check id | Host | Attested asset | Mechanism | Required |
|---|---|---|---|---|
| `native.macos-arm64` | release Mac | `awman-macos-arm64` | native kit: signed offline boot and upgrade | always |
| `native.macos-arm64-apple` | release Mac | `awman-macos-arm64` | native kit, suite `artifact-apple-smoke` | always |
| `native.linux-x64` | Linux host | `awman-linux-amd64` | native kit: offline KVM boot | always |
| `native.linux-x64-docker` | Linux host | `awman-linux-amd64` | native kit, suite `artifact-docker-smoke` | always |
| `sbx.macos-arm64` | release Mac | `awman-macos-arm64` | release-owned SBX smoke | always |
| `sbx.linux-x64` | the explicit `LINUX_HOST` | `awman-linux-amd64` | the same smoke over SSH | only when selected |

All of them are in addition to the hosted `release` gate and the hosted
exact-byte smoke of all four assets. The declarations live in `ci/gates.toml`
(`[release.native.*]` and `[release.sbx_smoke.*]`); the manifest policy rejects
any other host, asset, selection or step list for these checks.

### What each smoke proves

**Apple Containers and Docker.** The attested kit runs the attested asset with
`runtime.kind` set to `apple` or `docker`, in a private repository and private
awman state:

1. The pinned guest fixture is loaded from local bytes under a tag reserved for
   this run. Nothing is pulled from a registry.
2. `awman ready --prepare-only --json` prepares the project and agent images and
   reports ready.
3. `awman exec prompt -n --agent claude` starts a real container. A synthetic
   shell script, mounted read-only as the agent, must report the prompt and a
   Linux guest, write to stderr and exit 37.
4. While the container runs, its host mounts are inspected: the private
   repository is mounted, no parent of it is, and every source is inside the
   check's private state.
5. The run's containers and images are removed and verified absent.

It does not prove real agents or providers, authentication, registries,
networking policy or broader runtime behaviour.

**SBX.** With the host's existing login, the smoke records the `sbx` path,
version and digest, then runs the steps `version`, `session`, `create`,
`inspect`, `exec`, `run`, `stop`, `restart`, `remove` and `cleanup` on a new
shell sandbox named `awman-release-<random>`: Linux guest execution, a private
workspace round trip through both `exec` and `run`, restart, removal and
verified absence. It then runs
`awman ready --agent claude --prepare-only --json` through the exact asset, with
`runtime.kind = "sbx"`, authentication mode `none`, credential refresh
disabled, a new empty-commit repository and a private `AWMAN_HOME`. `AWMAN_*`
and a fixed list of provider and GitHub credential variables are removed from
its environment, and the fingerprinted `sbx` directory is first on `PATH`. The
result must be ready on the SBX runtime. The smoke never runs `sbx login` or
`sbx logout`, reads no provider key, passes no secret to a sandbox and removes
only names and directories it recorded as its own. The Linux host receives a
fixed script and the asset bytes.

Status, the confirmation summary and the sealed evidence state this limit:

> Release SBX smoke: logged-in CLI, shell sandbox lifecycle, guest execution,
> workspace sharing and exact-asset readiness. It does not prove provider
> authentication, effective agent policy or a successful provider task.

No Windows or Linux arm64 SBX support follows from it. The provider suite
`artifact-sbx-smoke` (`native.macos-arm64-sbx`) is a different, optional
diagnostic.

### SBX host selection

The Mac smoke is always selected. The Linux smoke is selected only when
`LINUX_HOST` is non-empty (a Make variable or an exported environment variable,
passed on as `--linux-host`) on the invocation that pins the attempt. The
selection and SSH destination are frozen in the journal, the remote claim and
the sealed evidence.

| Frozen at pin | Later invocation | Result |
|---|---|---|
| Mac only | `LINUX_HOST` unset | continues |
| Mac only | `LINUX_HOST=h` | BLOCKED `sbx-selection-changed` |
| Mac and Linux `h` | `LINUX_HOST=h` | continues |
| Mac and Linux `h` | unset, Linux smoke already passed | continues; before the seal the raw Linux evidence is re-verified and `h` must still be reachable for the `sbx` identity check |
| Mac and Linux `h` | unset, Linux smoke unfinished | BLOCKED `sbx-selection-changed` |
| Mac and Linux `h` | another host | BLOCKED `sbx-selection-changed` |

So a saved host with no `LINUX_HOST` runs the KVM and Docker checks on that host
and SBX on the Mac only. A first run on a clone with no saved host has to name a
host; passing `LINUX_HOST` then selects Linux SBX. To leave it out, save the
host first with `git config --local awman.release.linuxHost user@host` and omit
`LINUX_HOST`. To change a frozen selection before the seal, start a new attempt
with `SUPERSEDE=1`; after the seal, pass the frozen host or none. A selected
Linux host with a missing or signed-out `sbx` blocks the release; it never
degrades to Mac only. Even if a completed Linux SBX selection is omitted on
resume, its frozen destination is probed to recheck CLI identity before sealing.
If that host is unreachable, restore SSH access to the frozen destination and
resume; a fresh check is needed only if its identity changed.

The SBX selection and the KVM/Docker SSH destination are frozen at admission.
The saved `awman.release.linuxHost` is the KVM/Docker destination; it does not
select Linux SBX. A resume with a different saved or explicit destination stops
with `linux-host-changed`, including when a Docker result has already passed.
Restore the frozen destination to continue, or use `SUPERSEDE=1` before sealing
to start a new attempt on another host.

### Scheduling and deadlines

Each check starts once its asset (and, for a native check, its kit) is
verified, while hosted work continues. The controller runs at most two of these
checks at once per host, and starts Linux SBX only after Docker has passed or
failed. A retained BLOCKED Docker record is unfinished and does not permit
Linux SBX to start alongside its retry. Different hosts run in parallel.

| Check | Deadlines |
|---|---|
| Apple and Docker smoke | test suite 420 s, kit run 480 s, controller 780 s; kit service commands 20 s each; owned-object cleanup 120 s |
| SBX smoke, each host | whole check 1080 s; `create` 300 s and other lifecycle steps 60 s; readiness 420 s; on the Mac the lifecycle as a whole also has 600 s; on Linux the asset upload has 300 s and SSH connect 8 s |

An Apple or Docker run that exceeds its deadline after the service was admitted
is FAILED. An SBX readiness or whole-check deadline, or a lifecycle step
deadline from `create` on, is FAILED. A preflight probe, or an SSH stage or
upload that cannot reach the host, is BLOCKED, as is a lost SSH connection.
Cleanup steps keep their own deadline after the overall deadline or an
interrupt. Commands the controller and kit bound are killed with their process
groups. Linux SBX remote commands escalate from TERM to KILL after five
seconds. The remote worker records its process group and start identity in the
owned scratch root; reconciliation authenticates and stops that worker before
removing claimed sandboxes or the root.

For SBX, a prerequisite found before `create` is BLOCKED; a defect from `create`
on is FAILED, and a cleanup failure takes precedence. A final cleanup listing
that cannot be obtained before any resource was created is BLOCKED and keeps
the ownership record. Once creation has begun, an unverifiable cleanup is
FAILED and retains the intent for reconciliation.

### Evidence binding and the seal

- **Apple and Docker** use the native kit path: attested kit, exact test
  inventory, JUnit, runner log and command exit, bound to commit, tree, asset
  and kit digests. The result also records the service identity (CLI path,
  digest and version, service or server version, Docker endpoint) and a hashed
  host name, and binds by SHA-256 the ownership reservation
  (`runtime-owned.json`), the session and mount audit (`runtime-smoke.json`)
  and the independent cleanup audit (`runtime-reconcile.json`). A pass needs the
  attested asset, the expected guest output and exit, private mounts, an observed
  running container and zero leaked or remaining objects. A changed service CLI,
  or an object that survived its test (even if cleanup then removed it), is
  FAILED.
- **SBX** evidence is kept per host in `sbx-smoke/<id>/` in the private state.
  `binding.json` binds commit, tree, manifest, plan and policy digests, the
  asset and its attested digest, target, SSH destination, selection digest, step
  list, readiness agent, minimum CLI version and deadlines. `report.json` lists
  the digest of every other file. Verification re-derives the pass from the raw
  files: the exact step set with every exit zero, matching guest witnesses,
  readiness on the exact asset, an unchanged CLI, verified absence and a
  duration within budget. Anything missing, extra, modified or unbound is
  `sbx-smoke-invalid`. For Linux the retained lifecycle evidence is the receipt
  and the SSH progress log, not the full output of every remote command.
- The seal requires a passed record for every native check and every selected
  SBX host, re-verifies their raw files, applies the evidence age limit to each
  and re-reads each selected host's `sbx` version and digest.
  `release-evidence.json`, covered by the seal digest, carries the native
  records (with `service`), `sbx_selection`, `sbx_smoke`, `sbx_limits` and one
  gate line per check; an unselected Linux smoke reads `not-selected`. Raw
  directories stay in private state.

## The command

```sh
make release [VERSION=vX.Y.Z] [LINUX_HOST=user@host] [YES=1] \
             [SUPERSEDE=1] [TAKEOVER=<owner-id>] [REHEARSE=1] \
             [DECIDED_AT=<RFC 3339 time>] [REHEARSAL_REPO=<owner/repo>]
```

`DECIDED_AT` records when release preparation began; the one-hour target is
measured from it (otherwise from admission). Every invocation is kept in the
timing ledger with its monotonic duration and outcome, including interrupted
and failed ones, and the hosted runs' service creation and finish times are
recorded. A release that takes longer than 60 minutes still finishes, but
the output, the confirmation summary, `release-status` and `timings.jsonl`
mark the performance target **MISSED**.

| Phase | What happens |
|---|---|
| preflight | Admit the checkout (branch, clean, version, notes, no existing tag or release, base ancestry), then check tools, GitHub capabilities, workflows, both hosts, Apple Containers, Docker, the selected SBX hosts and pins. |
| pin | Pin R, its tree, the release plan bytes, policy digest and SBX host selection, then create the remote ownership claim. |
| push | Push R to `release/vX.Y.Z`, never forced. |
| dispatch | Start the CI `release` gate (or reuse authenticated passing evidence for exactly R) and the four asset builds, each with a durable idempotency key. |
| collect | Verify CI evidence; per asset verify the attested bytes, the build record and the native kit. One automatic rerun of a classified infrastructure failure per item: a lost runner, or a failed step that the workflow at R defines as transport (checkout, toolchain setup, artifact upload/download, attestation); any other failed step is a product failure. Native checks and SBX smokes start as soon as their inputs verify. |
| native | Finish the four native checks and the selected SBX smokes and verify their raw results. |
| seal | Re-authenticate the retained hosted evidence (verified CI record, every `build.json` and asset digest; missing files are recovered from the pinned run and artifact IDs, never rebuilt), the native results, the SBX evidence and the live SBX CLI identity, then create or adopt the owned draft, upload and reconcile the assets, write the seal. |
| confirm | Print the summary and ask for `y`. `YES=1` answers it. |
| land | Fast-forward or server-side merge R into `main`. |
| tag | Create the annotated tag `vX.Y.Z` at R carrying the seal digest. |
| publish | Turn the draft into the latest release. |
| verify | Download every asset anonymously, compare to the seal, `SHA256SUMS` and attestations, write the receipt. |
| cleanup | Remove kits, the controller checkout and verification copies. |

### The publication confirmation

Before the irreversible steps the command prints one summary: version, R,
landing mode, the four asset digests with attestation status, CI evidence (reused
or new), each native result, each selected SBX result with its CLI version and
digest, an explicit line when Linux SBX was not selected, the SBX limits
sentence, the seal digest, the age and result of the latest `main` and nightly
CI as **advisory** lines (they never block), and elapsed time. Type `y` to continue. Any other answer stops with `not-confirmed`; run
the command again to be asked again. `YES=1` skips the question for
unattended runs. The confirmation is recorded against the seal digest, so a
changed seal is asked about again.

### Output and exit codes

On any stop the command prints exactly two lines:

```
release: BLOCKED [linux-host-unreachable] ssh build@kvm1 timed out after 8 s
  recovery: start or reconnect the Linux KVM host, then run make release
```

| Exit | Meaning |
|---|---|
| 0 | Verified publication (with `REHEARSE=1`: a complete prepublication rehearsal) |
| 1 | FAILED: a check ran and found a defect |
| 2 | BLOCKED: a prerequisite or infrastructure condition |
| 130 | Interrupted |

Details go to `logs/<phase>.log` in the private attempt state. Every code and
its default recovery are in one table, `xtask/src/release/error.rs`.

## Recovery

`make release` is the only recovery command. It re-runs preflight for
unfinished work, re-probes completed phases (seconds) and continues at the
first incomplete phase, pinned to the same R, plan and evidence. Nothing is
rebuilt or re-run when its result is already valid.

| Situation | What `make release` repeats |
|---|---|
| Laptop slept, command interrupted or killed | Nothing; polling resumes against the pinned runs. |
| Classified hosted infrastructure failure | The failed job on retained inputs, automatically once. |
| Host unreachable or lost mid-check | Only that host's unfinished checks after the host is back; a passed result is reused. SBX cleanup authenticates and stops its recorded remote worker before removing its claimed resources. |
| A native check or SBX smoke is BLOCKED | That check, once the prerequisite is repaired. Its earlier evidence directory is kept (suffix `-rejected-` or `-blocked-`). |
| Interrupted during an Apple, Docker or SBX smoke | A native run that is still alive or has finished is adopted; otherwise the check's recorded objects are removed and verified before it runs again. An interrupted SBX smoke runs afresh with a new sandbox name (old directory kept with suffix `-interrupted-`). |
| `sbx` version or bytes changed before the seal | Only that host's SBX smoke; the old evidence is kept with suffix `-previous-`. |
| Upload error or lost response | Reconciles the draft; only an owned incomplete upload is retried before seal. |
| Tag created, publish failed | Publish only; the tag is untouched. |
| Published, verification failed on the network | Verification only. |
| `main` moved | Unrelated movement: fast-forward or merge. A conflict or publication-policy change stops with a single named blocker. |
| State directory lost | Rebuilt from the remote claim and seal; ownership is never invented. A sealed attempt recovers its evidence records and their original completion times from the sealed `release-evidence.json`, so their age is never reset. |
| Receipt, tag annotation or public assets lost after publication | Completed `tag`, `publish` and `verify` are re-probed: a bound receipt and the sealed public assets are required, otherwise read-only verification runs again or the command stops with the named defect. |

A **product failure** (`ci-failed`, `build-failed`, `native-failed`,
`sbx-smoke-failed`, invalid evidence, `verify-mismatch`) is final for that
evidence: fix the cause, commit and run `make release SUPERSEDE=1` to start a new **unsealed** attempt at the new
commit. Rerunning until green is not a policy. A sealed or tagged identity
cannot be superseded; choose a new version.

Resume removes only what an interrupted check recorded as its own. For Apple and
Docker these are the reserved image names, the containers linked to them or to
the run's owner label, and the private allocation. For SBX they are the two
sandbox names in `intent.json` and, on Linux, the remote directory whose owner
file names this check. Nothing is pruned, and Apple and Docker never claim an
object that existed before the run. If absence cannot be verified, the ownership
record is kept and nothing is discarded: repair the host or service and run the
same command again.
`SUPERSEDE=1` and `make release-abandon` perform the same cleanup before they
move or delete state, so they also need the affected host reachable.

An attempt pinned by release tooling that predates the Apple, Docker and SBX
checks is not carried over: its pinned plan is rejected with `manifest-stale`,
including by this tooling's supersede and abandon. Abandon it from a checkout of
the tooling that created it, then start again.

### Repairing a blocked check

A native check blocked while running is reported as `native-blocked` with the
cause in brackets or in its logs; an SBX smoke as `sbx-smoke-blocked` with the
reason code. Preflight reports the service codes directly. Repair the named
prerequisite, confirm with `make release-doctor` (pass `LINUX_HOST` only if the
attempt selected Linux SBX), then run the same `make release` command.

| Reported cause | Repair |
|---|---|
| `apple-service-unavailable` | Install Apple Containers if `container` is missing; run `container system start` until `container system status` succeeds. |
| `docker-daemon-unavailable` | On the Linux host install or start Docker, give the SSH account access to the socket (for example through the `docker` group, then a new login) and check `ssh user@host docker version`. If the engine is not on the default socket, set `DOCKER_HOST` for noninteractive SSH sessions. |
| `sbx-cli-missing`, `sbx-cli-too-old` | Install or upgrade `sbx` to 0.54.0 or newer on the named host's `PATH` (over SSH, the noninteractive `PATH`). |
| `sbx-not-signed-in` | Run `sbx login` on the named host as that account; `sbx --no-input ls --json` must succeed. |
| `sbx-name-occupied` | A sandbox or remote directory with a name the check needs already exists. A pre-existing lifecycle sandbox name or remote directory is refused before use; inspect it before removing it yourself. A sandbox already holding the readiness name is currently removed by the post-check cleanup and the check reports FAILED, so keep nothing under that name. |
| `linux-host-unreachable`, `linux-host-prerequisite` | Restore noninteractive SSH or install the named tool; the host must be Linux x86_64. |
| `sbx-selection-changed` | Restore the frozen `LINUX_HOST` choice (see [SBX host selection](#sbx-host-selection)). |
| a `fixture-ownership` message in the native `runner.log` | Names reserved for the run were already present, or the reservation did not match. Inspect the runtime before retrying. |

A FAILED check is not repaired by rerunning. Inspect its evidence, fix the
cause, commit and use `SUPERSEDE=1`. After a cleanup failure, check the named
host (`sbx ls`, `docker ps --all`, `container list --all`) for objects with the
names the evidence records (`awman-release-…` sandboxes, `awsmoke-…` images)
and remove only those.

If a second clone or machine holds the attempt (`attempt-locked`,
`state-foreign`), stop the other owner and run `make release TAKEOVER=<owner-id>`
using the id the message names. Operator-side release code can be repaired from
another checkout and resumed because the state refers to R by commit; bugs in
`release.yml` or the native kit are part of R and need a new commit.

Evidence is valid for 14 days (the plan may lower it). Every required item
must carry its completion time; expired or age-unknown evidence stops with
`evidence-expired` before sealing, landing, tagging and publication, including
a publication still pending after the tag exists. Before the seal, revalidate
with `SUPERSEDE=1`; once sealed or tagged, publish a new version. Only the
read-only verification of an already published release ignores expiry.

The publication-policy check fetches the current `main` into the controller
clone and compares the policy at exactly that commit, again after landing (a
server-side merge may absorb a concurrent policy change). A fetch failure is
infrastructure (`network`), not a policy verdict.

## Status, abandon and doctor

```sh
make release-status VERSION=vX.Y.Z    # read-only: phase table, first error and recovery, advisory CI
make release-doctor [LINUX_HOST=...]  # prerequisites only; changes nothing
make release-abandon VERSION=vX.Y.Z   # delete this attempt's own draft, claim and state
```

`release-status` shows each phase, the first error with its recovery, each
native and selected SBX outcome with its reason, the SBX hosts, whether Linux
SBX was selected, the SBX limits sentence, and the age and result of the latest
`main` and nightly gates. An Apple, Docker or SBX check with no result yet,
including one not started or still running, is also listed as BLOCKED; read the
first error and its logs before repairing a service. A published release without a
receipt is reported as published but unverified. `release-abandon` is refused
once the tag exists and never deletes a draft or claim it does not own.

Private attempt state lives under `$AWMAN_RELEASE_STATE`, or
`$(git rev-parse --git-common-dir)/awman-release/`. It is private to the clone,
survives `cargo clean`, and holds the journal, exact plan bytes, verified
downloads, native results, SBX evidence, release evidence and `timings.jsonl`.
A lost state directory recovers the SBX selection from the remote claim and,
once sealed, the native and SBX records from the sealed evidence. The journal is a cache,
never proof; the remote claim and the seal are the authority. The remote claim
is the branch `awman-release-state/<version>`, kept as an audit record after
publication.

## Rehearsal

```sh
make release-rehearse [REPEAT=N]   # layer 1: no network
make release REHEARSE=1            # layer 2: real GitHub and hosts, no publication
```

`release-rehearse` drives the production state machine against a directory
standing in for GitHub and scripted hosts, injecting one fault at a time:
unreachable host, killed command, transient and product CI failures, lost
responses, partial uploads, moved `main`, tag and verify retries, a second
owner and incomplete evidence, plus Apple, Docker and SBX cases such as blocked,
failed, timed-out, interrupted, tampered, short or stale results, selection
changes and a changed SBX CLI. Each scenario asserts one primary error, one
recovery, what was not repeated and the final published state. Reports are
written to `target/release-rehearsal/`. Its latencies are modelled; they are
behavioral evidence, not wall-clock evidence.

`make release REHEARSE=1` runs from any branch against real GitHub, both real
hosts and their Apple Containers, Docker and SBX services. Run it once without
`LINUX_HOST` (saved host, SBX on the Mac only) and once with it (Linux SBX
selected) to exercise both selections. A finished rehearsal keeps its local
state; remove it with `make release-abandon VERSION=vX.Y.Z REHEARSE=1`, after
copying any evidence you want, before starting the next one. It pushes to a `rehearsal/<nonce>` branch, uses a separate claim,
builds a draft, then stops before landing and deletes its draft. It can never
land, tag or publish (`rehearsal-refused`). It needs `release.yml` and the
current `ci.yml` on the default branch. It measures prepublication work only.

```sh
git config --local awman.release.rehearsalRepos <owner/repo>   # explicit authorization
make release REHEARSAL_REPO=<owner/repo> LINUX_HOST=user@host   # complete rehearsal
```

`REHEARSAL_REPO` runs the complete production path (land, tag, publish,
anonymous verification) from a clone whose `origin` is a **disposable**
repository holding a copy of the source and the one-time workflow rollout. It
is refused for the canonical repository, for a repository not listed in the
clone's `awman.release.rehearsalRepos`, and when `origin` differs. Its tag and
release are named `rehearsal-vX.Y.Z-<nonce>`, never a product version; its state
is `disposable-vX.Y.Z` and `make release-status VERSION=vX.Y.Z
REHEARSAL_REPO=<owner/repo>` reads it. Preparing the disposable repository is
environment setup; its policy, runner capacity and network differ from
production and must be disclosed with any measurement. Running it is a remote
write and needs a maintainer's explicit authorization. See
[release-measurements.md](release-measurements.md).

## Residual risks

Release is deliberately narrower than the broad gates. The following can reach
a release without blocking it:

- A macOS or Windows host-code regression, lint, licence or advisory finding,
  web, musl and dependency or helper-scan result in changes that never ran the
  broad `pr`/`main` gate. Those run on pull requests and `main` and nightly, and
  are shown as advisory lines at confirmation.
- Per-agent guest breadth, builder/strategy, soak, Docker, registry and Git
  boundaries, and Linux replacement and extended persisted-state upgrade
  coverage run only in the operator-run native diagnostic (`make validate-native`
  on the host being diagnosed) and have no schedule. The release Apple
  Containers and Docker smokes cover one synthetic session each, not those
  boundary suites.
- SBX provider, policy and credential coverage is opt-in through `boundary-sbx`
  or `artifact-sbx-smoke`. Provisioned nightly SBX runs are diagnostic. The
  release SBX smoke does not claim provider or effective-policy coverage, and
  Linux SBX is checked only when selected.
- Apple, Docker and SBX results describe the service versions the hosts ran. A
  Docker or Apple Containers service or CLI change after a passed result does not
  rerun it; the recorded CLI path, digest and version are shown in the
  confirmation and status output. `SUPERSEDE=1` before the seal starts a new
  attempt. A changed `sbx` CLI reopens its check before the seal.
- Reproducibility of the shipped bytes is observed only for Linux x64 default in
  the nightly; the four shipping target/feature combinations are not covered by
  a reproduction check.
- A historical Linux builtin upgrade is not claimed: no Linux predecessor
  exists, and a missing predecessor is blocked evidence, never a pass.
- The post-landing `main` gate does not prove R; the release gate does.
- Native results are produced on operator machines and are not Actions
  attestations; build provenance of every asset is. This matches the existing
  trust root: the operator's login already tags and publishes.
- Experimental SBX provider and policy breadth is opt-in and does not block release. The shipping runtime retains its unsupported-agent, option, host and sandbox-reuse refusals.
- The Linux host, Apple Containers, Docker and the SBX logins are required
  dependencies. An unavailable one retains valid work and publication waits.

Not weakened: no host agent execution, no credential sent to the Linux host, no
self-hosted runner, and the tag exists only at the commit every attestation
names.

## Verifying a published release

Anyone can verify an asset with
`gh attestation verify <file> --repo prettysmartdev/awman --signer-workflow prettysmartdev/awman/.github/workflows/release.yml`
and compare it with the release's `SHA256SUMS`; see
[docs/11-runtimes.md](../../docs/11-runtimes.md).
