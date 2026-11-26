# Release operator runbook

A release is one command run from the Apple Silicon release Mac:

```sh
make release
```

It starts the hosted CI gate and the four asset builds together, runs the two
required native checks on exact attested bytes (this Mac and a Linux KVM
host), seals one draft release, asks once for confirmation, then lands the
release commit on `main`, tags it, publishes the draft and verifies the public
bytes anonymously. After any stop, the recovery is the same command again:
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
- Both native checks are required and have no waiver flag: a signed offline
  boot of the macOS asset plus a persisted-state upgrade on the Mac, and an
  offline KVM boot of the Linux amd64 asset on the Linux host. A missing,
  blocked, short or failed native result prevents sealing.
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
- Nothing runs an agent on a host. The native checks run a synthetic agent
  inside a guest booted by the exact asset, with a private test repository as
  the only mount. No GitHub token is sent to the Linux host. No self-hosted
  runner executes repository workflows.

## Prerequisites (the prepared environment)

Run `make release-doctor` at any time: it checks everything below for the
current commit and changes nothing. `make release` runs the same checks first
and stops at the first failure, before anything is pushed or dispatched.

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
  toolchain, free disk for the expanded native kit, payload, fixtures and VM
  state, and a short private temp root under `/private/tmp` so guest socket
  paths stay under the 103-byte limit.
- Docker, Apple Containers and `python3` are not needed.

**Linux KVM host (x86_64 GNU)**

- Reachable over noninteractive SSH as `LINUX_HOST=user@host`, with read/write
  `/dev/kvm`, Git, a glibc compatible with the kit tools, `tar`, `curl`,
  SHA-256 tools, working kernel seccomp offline enforcement and enough free
  disk. No Docker, Cargo, checkout or GitHub credential is needed on it.
- Preflight probes these within a bounded time (about 8 seconds for an
  unreachable host) before any long work.

`LINUX_HOST` is stored in `git config --local awman.release.linuxHost` after
the first successful preflight; later runs may omit it.

**Release prep**

Version, `Cargo.lock`, documentation and `docs/releases/vX.Y.Z.md` (no
placeholder) are committed, by the `release-prep` skill, on a clean `main` or
`release/vX.Y.Z` checkout whose base `origin/main` is an ancestor of HEAD.
`VERSION` defaults to the `Cargo.toml` version at HEAD.

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
| preflight | Admit the checkout (branch, clean, version, notes, no existing tag or release, base ancestry), then check tools, GitHub capabilities, workflows, both hosts and pins. |
| pin | Pin R, its tree, the release plan bytes and policy digest, then create the remote ownership claim. |
| push | Push R to `release/vX.Y.Z`, never forced. |
| dispatch | Start the CI `release` gate (or reuse authenticated passing evidence for exactly R) and the four asset builds, each with a durable idempotency key. |
| collect | Verify CI evidence; per asset verify the attested bytes, the build record and the native kit. One automatic rerun of a classified infrastructure failure per item: a lost runner, or a failed step that the workflow at R defines as transport (checkout, toolchain setup, artifact upload/download, attestation); any other failed step is a product failure. Native checks start as soon as their asset and kit verify. |
| native | Finish both native checks and verify their raw results against the attested kit. |
| seal | Re-authenticate the retained hosted evidence (verified CI record, every `build.json` and asset digest; missing files are recovered from the pinned run and artifact IDs, never rebuilt) and the native results, then create or adopt the owned draft, upload and reconcile the assets, write the seal. |
| confirm | Print the summary and ask for `y`. `YES=1` answers it. |
| land | Fast-forward or server-side merge R into `main`. |
| tag | Create the annotated tag `vX.Y.Z` at R carrying the seal digest. |
| publish | Turn the draft into the latest release. |
| verify | Download every asset anonymously, compare to the seal, `SHA256SUMS` and attestations, write the receipt. |
| cleanup | Remove kits, the controller checkout and verification copies. |

### The publication confirmation

Before the irreversible steps the command prints one summary: version, R,
landing mode, the four asset digests with attestation status, CI evidence (reused
or new), both native results, the seal digest, the age and result of the latest
`main` and nightly CI as **advisory** lines (they never block), and elapsed
time. Type `y` to continue. Any other answer stops with `not-confirmed`; run
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
| Host unreachable or lost mid-check | Only that host's unfinished check after the host is back; a passed result is reused. |
| Upload error or lost response | Reconciles the draft; only an owned incomplete upload is retried before seal. |
| Tag created, publish failed | Publish only; the tag is untouched. |
| Published, verification failed on the network | Verification only. |
| `main` moved | Unrelated movement: fast-forward or merge. A conflict or publication-policy change stops with a single named blocker. |
| State directory lost | Rebuilt from the remote claim and seal; ownership is never invented. A sealed attempt recovers its evidence records and their original completion times from the sealed `release-evidence.json`, so their age is never reset. |
| Receipt, tag annotation or public assets lost after publication | Completed `tag`, `publish` and `verify` are re-probed: a bound receipt and the sealed public assets are required, otherwise read-only verification runs again or the command stops with the named defect. |

A **product failure** (`ci-failed`, `build-failed`, `native-failed`, invalid
evidence, `verify-mismatch`) is final for that evidence: fix the source, commit
and run `make release SUPERSEDE=1` to start a new **unsealed** attempt at the new
commit. Rerunning until green is not a policy. A sealed or tagged identity
cannot be superseded; choose a new version.

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

`release-status` shows each phase, the first error with its recovery, and the age
and result of the latest `main` and nightly gates. A published release without a
receipt is reported as published but unverified. `release-abandon` is refused
once the tag exists and never deletes a draft or claim it does not own.

Private attempt state lives under `$AWMAN_RELEASE_STATE`, or
`$(git rev-parse --git-common-dir)/awman-release/`. It is private to the clone,
survives `cargo clean`, and holds the journal, exact plan bytes, verified
downloads, native results, evidence and `timings.jsonl`. The journal is a cache,
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
owner and incomplete evidence. Each scenario asserts one primary error, one
recovery, what was not repeated and the final published state. Reports are
written to `target/release-rehearsal/`. Its latencies are modelled; they are
behavioral evidence, not wall-clock evidence.

`make release REHEARSE=1` runs from any branch against real GitHub and both
real hosts. It pushes to a `rehearsal/<nonce>` branch, uses a separate claim,
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
  on the host being diagnosed) and have no schedule.
- Reproducibility of the shipped bytes is observed only for Linux x64 default in
  the nightly; the four shipping target/feature combinations are not covered by
  a reproduction check.
- A historical Linux builtin upgrade is not claimed: no Linux predecessor
  exists, and a missing predecessor is blocked evidence, never a pass.
- The post-landing `main` gate does not prove R; the release gate does.
- Native results are produced on operator machines and are not Actions
  attestations; build provenance of every asset is. This matches the existing
  trust root: the operator's login already tags and publishes.
- The Linux KVM host is a required dependency. An unavailable host retains valid
  work and publication waits.

Not weakened: no host agent execution, no credential on the Linux host, no
self-hosted runner, and the tag exists only at the commit every attestation
names.

## Verifying a published release

Anyone can verify an asset with
`gh attestation verify <file> --repo prettysmartdev/awman --signer-workflow prettysmartdev/awman/.github/workflows/release.yml`
and compare it with the release's `SHA256SUMS`; see
[docs/11-runtimes.md](../../docs/11-runtimes.md).
