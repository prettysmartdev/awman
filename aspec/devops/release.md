# Release operator runbook

How a maintainer releases awman, resumes an interrupted release, recovers from
each failure state and rolls back. The CI side (workflows, gates, runners) is in
[cicd.md](cicd.md); the tests that back this procedure are in `xtask/tests/pipeline/`
and `xtask/tests/release_contract/`.

## The model

A release is **built once, validated as those exact bytes, then published**:

1. **Candidate** (`candidate.yml`): builds the four executables and `SHA256SUMS`
   once, signs the macOS asset, attests every asset, and stores them as a **draft**
   GitHub Release (`vX.Y.Z (candidate N)`) with a machine-readable
   `release-state.json`. Nothing is public and no tag exists.
2. **Validation**: the `candidate` gate (the full `main` gate, the `artifact`
   tier against each exact executable, the signed offline boot of the macOS asset
   and the `guest` tier on the release Mac) runs against those bytes. Artifact and
   guest jobs download the recorded bytes from the run that built them, verify
   digest and attestation, and never rebuild.
3. **Promotion** (`promote.yml`): re-verifies everything under a publication lock,
   creates the annotated tag at the candidate commit with the release GitHub App,
   and publishes the draft. No asset is uploaded or changed at this step.
4. **Post-publish verification**: downloads the *published* assets, checks
   `SHA256SUMS`, runs `gh attestation verify`, smoke-tests each executable and boots
   the macOS binary offline, then writes a verification receipt binding release id,
   tag, candidate and every asset digest.

Rules that never change:

- A published tag is never moved or reused, and published assets are never
  updated or replaced. A bad release is superseded by a new version.
- Lost or changed candidate bytes are never rebuilt under the same candidate
  number. A changed input is a new candidate.
- A skipped local test run (`skipped by operator`) satisfies no gate. Local test
  results are advisory; only CI evidence counts.
- An override is a reviewed, expiring entry in `ci/gates.toml`, and never covers
  artifact integrity, the signed-binary boot, the upgrade scenario or required
  hardware coverage. Evidence expires after `release.evidence_valid_days` (14) and
  at the earliest quarantine or override deadline.
- Every failing job prints exactly one line starting with `BLOCKED:` (the
  infrastructure is at fault: runner, hypervisor, pins, credentials, service) or
  `FAILED:` (the product or evidence is at fault). Nothing is published on either.

## Which path is active

`release.operator_path` in `ci/gates.toml` selects the operator path.

- `"pipeline"`: `make release` runs the flow below and starts `candidate.yml`.
- `"legacy"` (the current value): `cargo xtask release run` replaces itself with
  `scripts/release.sh`, which pushes the tag and lets the tag-triggered
  `release.yml` build and publish. The pre-checks, prompts and runner handling
  below are the same; the differences are that the tag is created and pushed by
  the operator before the build, there is no draft, no candidate number, no
  receipt, and `release-status` and `release-abandon` have nothing to report.
  Recovery is `gh run rerun <run id> --failed` on the `Release` run, or re-running
  `make release`.

The switch to `"pipeline"` is a reviewed manifest change made once a rehearsal
(`make apple-silicon-prerelease`, or the nightly `rehearsal` job) has passed on
the release Mac and the repository settings in [cicd.md](cicd.md#required-checks-and-repository-settings)
are in place, including the release GitHub App and tag ruleset. The rest of this
document describes the pipeline path.

## Prerequisites

- A clean checkout, normally on `main`, in sync with `origin/main`, with a `gh`
  login (`gh auth login`) that can dispatch workflows and, when the Mac is managed
  as a runner, administer repository runners.
- Release notes: `docs/releases/vX.Y.Z.md`, containing no placeholder.
- For the Apple Silicon job, a bare-metal Apple Silicon Mac with
  Hypervisor.framework (`kern.hv_support=1`), Apple Containers installed, the
  pinned Rust toolchain and enough disk. Hosted Macs are VMs and cannot boot a
  guest. The first release on a Mac needs Docker or Apple Containers running to
  build the cached guest fixture.
- The pins that releases depend on are published: the guest fixture
  (`[fixtures.guest-oci-base]`), the previous release's asset digest
  (`[previous_release.*]`) and the macOS payload record. While any is `pending`,
  validation reports `BLOCKED: pins are not published yet` and nothing releases.
- The version has the form `vX.Y.Z`; pre-release suffixes are refused.

## Normal path: `make release VERSION=vX.Y.Z`

```sh
make release VERSION=v0.14.0                      # RELEASE_RUNNER=auto by default
make release VERSION=v0.14.0 RELEASE_RUNNER=none  # someone else provides the release Mac
```

`RELEASE_RUNNER` is `auto` (use an online `awman-release` runner if there is one,
otherwise register this Mac when it can serve), `local` (require this Mac) or
`none` (never register). Prompts read from `/dev/tty`; the default is shown in
capitals; answering *no* to a *Proceed* question prints `  Aborting.` and exits 1.

| Step | What happens | Prompts and refusals |
|---|---|---|
| **Pre-checks** | `gh` is authenticated; remote state of the version is read from GitHub; branch, sync and working-tree checks; tag checks; runner selection | `Proceed with release from '<branch>'?` [y/N] when not on `main`. `Proceed anyway (you may be ahead, or behind)?` [y/N] when not equal to `origin/main`. `Proceed anyway? (consider committing or stashing these first)` [y/N] for changes other than `Cargo.toml`, `Cargo.lock` and `docs/releases/`. If the version is already verified: `Release vX.Y.Z is already published`, exit 0. If a candidate exists: `Candidate N for vX.Y.Z exists (state …); resuming`. With no runner available: `Continue? (the Apple Silicon job will wait for a runner, and nothing publishes until it passes)` [y/N]. A malformed version, a missing `gh` login or an invalid `RELEASE_RUNNER` stops here |
| **Version in Cargo.toml** | bumps `Cargo.toml` and refreshes only awman's own `Cargo.lock` entry; verifies the result | warns if the installed `awman --version` disagrees with `Cargo.toml` |
| **Release notes** | creates `docs/releases/vX.Y.Z.md` from a template if missing | with the placeholder still present: `Launch 'awman chat' to write release notes now?` [Y/n]; *no* stops with instructions to edit and re-run |
| **Tests** | runs `make test` and records the result bound to the exact source tree, index, untracked build inputs, manifest, `Cargo.lock` and tool versions | on failure the output is copied to the clipboard (`pbcopy`, `xclip` or `xsel`); `Skip tests and continue the release anyway?` [y/N], then `Launch 'awman chat' to fix the failing tests?` [Y/n]. A skip is recorded as `skipped by operator` and authorises nothing. A record for an unchanged tree is reused: `Tests already passed for this source tree` |
| **Commit** | commits exactly `docs/releases/vX.Y.Z.md`, `Cargo.toml`, `Cargo.lock` as `Add release notes for vX.Y.Z`; the commit tree must equal the tested tree | `Nothing to commit` when already committed |
| **Tag** | creates the local tag `vX.Y.Z` (refuses a local tag on another commit). It is only a local marker: the public tag is created at publication | |
| **Release runner** | when this Mac serves: provisions `/opt/awman` and the guest fixture, registers a temporary runner labelled `self-hosted, macOS, ARM64, awman-release` (token only in the environment), keeps the Mac awake with `caffeinate` | lists queued or running workflow runs that wait for a release runner: `Cancel them?` [Y/n]. A runner that cannot register stops the release before anything is pushed |
| **Push** | pushes `main` only (never the tag) and starts the candidate: `gh workflow run candidate.yml --ref main -f version=… -f commit=… -f operator_record=…` | `Started the release candidate for vX.Y.Z; the tag is pushed when it is published` |
| **Release workflow** | finds the run (up to 5 minutes) and watches it with `gh run watch … --exit-status` through candidate, validation, promotion and verification (up to 3 hours) | an earlier unsuccessful run: `Re-run its failed jobs on this Mac?` [Y/n]. Success requires state `verified`, the matching commit and the tag on origin; then `Release vX.Y.Z published!` |

With `RELEASE_RUNNER=none` the command stops after starting the candidate:
`Candidate started; the Release workflow will publish vX.Y.Z after the build gates
pass`, followed by the `gh run watch` command to follow it and
`Release vX.Y.Z tagged!`.

An `EXIT` trap stops the temporary runner's process group (SIGINT, after 30 s
SIGKILL, only while the group still holds the runner's processes) and
deregisters it whether the run succeeded, failed or was interrupted. Ctrl-C
starts that teardown and the command exits 130; a second Ctrl-C is ignored until
the runner is gone.

### What a re-run resumes

Every state is read from GitHub, never from the local checkout, so re-running
`make release VERSION=vX.Y.Z` from any clone does the right thing:

| State at start | A re-run |
|---|---|
| `verified` | prints `Release vX.Y.Z is already published` and exits 0 |
| `promoted` | resumes the watch; post-publish verification is retried |
| `validated` | resumes promotion (the tag may or may not be on origin; it is never recreated or moved) |
| `candidate-built` | resumes the same candidate; failed jobs are re-run, passed cells are not |
| `prepared` | resumes the run, or re-runs its failed jobs; if the run or its artifacts are gone, a new candidate number is allocated |
| `abandoned` or `none` | starts a new candidate; abandoned numbers are never reused |

Answers given to the prompts are recorded with the dispatch (as advisory
evidence only), so an interrupted session does not ask twice. A candidate is
resumed only for the same commit and inputs; if the release notes, `Cargo.lock`,
the toolchain, the entitlement file, the pins or the manifest changed, the
re-run builds candidate N+1 and supersedes N.

## Inspecting state: `make release-status`

```sh
make release-status VERSION=vX.Y.Z      # same as: cargo xtask release status vX.Y.Z
cargo xtask release status vX.Y.Z --json
```

Read-only. Prints the state, the candidate number and commit, whether the tag is
on origin, the failing gate IDs with `blocked` or `failed` and the reason, and
links to the evidence.

| State | What exists | Next action |
|---|---|---|
| `none` | no draft, no published release, no candidate run | `make release VERSION=vX.Y.Z` |
| `prepared` | a candidate run is queued or running, or ended before a complete draft | re-run `make release`, or `gh run rerun <id> --failed` |
| `candidate-built` | draft with the four assets, `SHA256SUMS` and `release-state.json`; gate not passed yet | re-run the failed or blocked jobs (same bytes) |
| `validated` | candidate gate passed; the tag may or may not be on origin | promotion runs automatically; re-run `make release` to resume it |
| `promoted` | tag on origin at the candidate commit, release published | verification pending or failed; re-run `make release` |
| `verified` | an authenticated receipt binds release id, tag, candidate and asset digests | done |
| `abandoned` | draft deleted by `release-abandon`; evidence kept in the run's `candidate-record-…` artifact | `make release` starts the next candidate number |

## Abandoning a candidate: `make release-abandon`

```sh
make release-abandon VERSION=vX.Y.Z      # same as: cargo xtask release abandon vX.Y.Z
```

Deletes the newest **unpromoted** draft after copying `release-state.json`,
`SHA256SUMS` and the gate evidence to `target/release-abandoned/<version>-c<N>/`
(the run's `candidate-record-…` artifact keeps them for 90 days). It refuses once
the tag is on origin (resume instead) or the release is published. Abandoning
never reuses the candidate number.

## Recovery by failure state

Always start with `make release-status VERSION=vX.Y.Z`; the failing gate and the
`BLOCKED:`/`FAILED:` line say which of these applies. `<run id>` is the candidate
run (`gh run list --workflow candidate.yml`).

### Interrupted candidate build

*Symptom:* state `prepared` with no draft, or a `build` job cancelled, timed out
or failed.

1. `gh run rerun <run id> --failed`, or answer *yes* to `Re-run its failed jobs
   on this Mac?`. Only failed build jobs run again. A build job whose `build.json`
   exists for the same input digest refuses to rebuild (`already built … not
   rebuilding`).
2. A cancelled self-hosted job may have left processes, VMs or images. The next
   job's first step (`ci sweep`) fails with the leftover list. Clean the release
   Mac (`cargo xtask release runner down`, then `up`) and rerun.
3. If the run is gone or its artifacts have expired, re-run `make release`: a new
   candidate number is allocated.

### Runner lost mid-validation

*Symptom:* `candidate-built`; the failing gate shows `blocked … lost hypervisor
mid-run` or `no evidence` for a cell. A test whose capability vanished mid-run is
infrastructure-blocked, never a pass or a skip.

1. Bring the release runner back (`make release` with `RELEASE_RUNNER=auto` or
   `local` re-registers it; `cargo xtask release runner down` removes stale
   registrations).
2. `gh run rerun <run id> --failed`. Artifact and guest jobs download the exact
   recorded bytes, verify digest and attestation, and rerun; passed cells are not
   rerun and the gate re-aggregates.

### Failed suite

*Symptom:* `candidate-built`; the failing gate shows `failed <cell>`.

- Cause was flaky or infrastructure and is fixed: `gh run rerun <run id>
  --failed` reruns that cell against the same artifacts.
- Real product bug: fix it on `main`, then `make release` again. The new commit
  is a new identity, so candidate N+1 is built and N is superseded (its draft
  deleted, evidence kept in the run artifacts). Reproduce the failure from its
  record as described in [testing.md](testing.md#reproducing-a-ci-failure).

### Partial asset upload

*Symptom:* `prepared` with a draft that lacks some assets; `assemble` failed.

1. `gh run rerun <run id> --failed`. Assembly recognises its own draft (run
   marker and identity), uploads only the missing assets, then `SHA256SUMS` and the
   final `release-state.json`. An existing asset with other bytes is a `FAILED`
   (lost bytes are never rebuilt under the same candidate).
2. If the original run's build artifacts are gone, `make release` allocates a new
   candidate.

### Promotion interrupted between tag and publish

*Symptom:* `validated` with the tag on origin and the release still a draft.

Re-run `make release VERSION=vX.Y.Z` (or dispatch `promote.yml` with the version,
commit and candidate number). `release promote tag` sees the tag at the candidate
commit and prints `Tag vX.Y.Z already on origin`; `publish` re-verifies everything
under the publication lock and publishes the draft. A second publish prints
`Release vX.Y.Z is already published`. The tag is never recreated or moved.

### Tag exists on a different commit

*Symptom:* `FAILED: tag vX.Y.Z exists on origin at <sha>, not at the candidate
commit …; a tag is never moved`. Nothing was published.

- If nothing was ever published from that tag (a stray manual tag), a maintainer
  deletes it (a recorded ruleset bypass) and re-runs.
- Otherwise release the next patch version.

### Changed input after validation

A force-push, edited release notes, a changed entitlement file, lockfile,
toolchain, payload or fixture pin, or a manifest edit is a different candidate
identity. `make release` allocates candidate N+1. Promotion of N is refused (`…
changed on main since candidate N … start a new candidate`).

### Commit no longer an ancestor of main

*Symptom:* `FAILED: commit … is no longer the tip or an ancestor of main;
candidate N … is void`. Release from the current `main`.

### Expired evidence, quarantine or override

Evidence expires after 14 days and at the earliest quarantine or override
deadline; expiry during validation or while waiting for the publication lock
invalidates it, and age is never reset by copying. Re-run `make release` to build
and validate a new candidate, or renew the manifest entry through review first.

### Promotion refused: missing or red gate

*Symptom:* `FAILED: required gate main is missing …` or `… not validated`.
Promotion checks every required gate (`candidate`, `main`) for the commit and
manifest version, their digests on the draft, every candidate cell and every
release-critical scenario. Finish validation (rerun failed jobs). There is no flag
to skip it.

### Override attempted on a non-overridable cell

`cargo xtask gates check` and preflight refuse the manifest (`… override
forbidden`). Artifact integrity, the signed boot, upgrade, guest and other
hardware cells have no override path. Restore the hardware or fix the product.

### Two operators start candidates for the same version

Candidate runs for one version are serialised and never cancelled. If two
dispatches still allocate the same number, the second assembly ends `BLOCKED:
candidate N … was allocated concurrently by another run; nothing was published`.
Re-run `make release` on the commit you want: it allocates the next number and
supersedes the other draft under the publication lock.

### Blocked before any build

| Message | Fix |
|---|---|
| `BLOCKED: pins are not published yet …` | publish the guest fixture and record the previous release digest in `ci/gates.toml` (`cargo xtask fixtures publish-plan` lists what is missing), commit the pins |
| `BLOCKED: no Rust toolchain …` or `runner toolchain … is not the pinned` | provision the runner (the first release on a new Mac) |
| runner preflight `BLOCKED:` (disk, `kern.hv_support`, Apple Containers) | free disk, or use real Apple Silicon (hosted Macs cannot boot a guest) |
| `BLOCKED: the release GitHub App … is not configured` | install the promotion App and its secrets (`AWMAN_RELEASE_APP_CLIENT_ID`, `AWMAN_RELEASE_APP_PRIVATE_KEY`); nothing was tagged |
| registry offline or rate-limited during a test | the cell is `blocked`; rerun when reachable |

## Rollback

A published release is never removed, re-tagged or re-uploaded.

1. **Supersede, do not delete.** Edit the release notes on GitHub to point at the
   fix (text only; assets untouched), and say so in `docs/releases/`.
2. **Fix forward.** Fix on `main`, then `make release VERSION=vX.Y.Z+1`.
3. **Failed verification.** If post-publish verification failed (`promoted`, no
   receipt), the published bytes are still the validated bytes, because digests
   are checked at every hop. Re-run the failed verify job (`make release` resumes
   it). If the defect is real, follow steps 1 and 2.
4. **Before publication** a bad candidate is simply abandoned
   (`make release-abandon`) and superseded; nothing public exists to roll back.
5. **A compromised or mis-signed release** is handled the same way: announce the
   superseding version in the release notes. Never re-upload an asset under the
   same tag.

## Rehearsal and pre-release validation

The nightly `rehearsal` job calls `candidate.yml` with `dry_run: true`: a
namespaced draft (`rehearsal/<run id>/vX.Y.Z`), a dry promotion (no tag, nothing
published, verification against the run's own bytes), and then the draft is
deleted. A rehearsal never reads, changes or deletes a production draft, and its
evidence can never promote production.

`make apple-silicon-prerelease` starts the same rehearsal for the current `main`
commit from your terminal and watches it. It creates no tag and publishes
nothing. Run it on the release Mac before the first release on the pipeline path
and after any change to the release inputs.

## The release runner

`cargo xtask release runner <check [--hardware] | provision | busy <owner/repo> |
verify-archive | up <owner/repo> <tag> | down>` manages the temporary runner that
`make release` uses; you rarely call it directly.

- `provision` installs what the job expects in `/opt/awman` (creating it with
  `sudo` the first time and handing it to the invoking user), including the guest
  fixture, which it fetches by pinned digest into `~/.awman/release-runner/fixtures`.
- `up` downloads the latest `actions/runner` osx-arm64 archive, refuses it unless
  its SHA-256 matches the checksum in that runner release's notes, registers it
  with a registration token passed only in `ACTIONS_RUNNER_INPUT_TOKEN`
  (labels `self-hosted, macOS, ARM64, awman-release`; name
  `awman-release-<tag>-<host>`), and starts it under `caffeinate`.
- Jobs run under `/tmp/awr` (mode 0700; refused if it is a symlink or someone
  else's directory; removed afterwards only if created here), because macOS caps
  Unix socket paths at 103 bytes.
- `down` is idempotent: it stops the process group, deregisters the runner, and
  retains repair state if deregistration fails so the next `down` can finish it.
- Overrides: `AWMAN_RELEASE_RUNNER_HOME`, `AWMAN_RELEASE_OPT_DIR`,
  `AWMAN_RELEASE_RUNNER_WORK`, `AWMAN_RELEASE_RUNNER_ONLINE_TIMEOUT`.
- The runner executes jobs as the invoking user and accepts any job whose labels
  match while it is online. Require approval for fork pull-request workflows, and
  do not approve fork runs during a release.
- Legacy: `tools/release-runner.sh` (label `hypervisor`) serves the legacy
  `release.yml` path until that path is retired.

## What users verify

Every release publishes `SHA256SUMS` next to the four executables, and each
executable has a build provenance attestation. Users verify them as described in
`docs/11-runtimes.md`:

```sh
shasum -a 256 -c SHA256SUMS --ignore-missing
gh attestation verify awman-macos-arm64 --repo prettysmartdev/awman \
  --signer-workflow prettysmartdev/awman/.github/workflows/candidate.yml
```

Releases published on the legacy path carry `SHA256SUMS` but no build provenance
attestation, so `gh attestation verify` finds nothing for them.

## Review hardening and cutover limits

The retained publisher is dispatch-only. A tag created by promotion cannot
start it. Fallback activation requires `AWMAN_LEGACY_RELEASE_ENABLED=true`, the
protected `release` environment, and an explicit `release.yml` dispatch on main
with `legacy_tag=vX.Y.Z`. Its checked-out tag must be an ancestor of main.
Disable that variable before enabling production candidate promotion. The
legacy operator now dispatches this workflow explicitly after pushing the tag;
its publication and runner must remain mutually exclusive with the successor.

Temporary runners require `AWMAN_RELEASE_RUNNER_GROUP_ID`. The successor queries
that organization group's policy and requires exactly candidate.yml and
promote.yml at `refs/heads/main`; the retained fallback requires a separate group
restricted to release.yml at main. Repository-scoped registration is refused.
The group must also be allocated to this repository and protected-main and
release-environment settings must be API-verified by the administrator (P1/P6).
The group identifier alone, a label, or a local hardware check proves none of
those external settings. Runner-home paths are canonicalized; teardown retains
state and fails when it cannot establish that the owned process is gone.

Candidate fetch exports an absolute, hash-verified executable path. On Unix it
restores owner execution and removes write permission before exporting that
path, including when artifact transport has discarded its executable mode.

The trusted candidate gate writes `validation.json` containing the candidate
record and gate digest. Only a successful gate step requests its attestation.
Promotion requires that attestation and independently binds candidate identity,
namespace, release ID, shipped byte digests, validation run and gate digest.
The candidate gate's plan digest must equal the recomputed candidate digest.
The draft journal remains advisory. Policy is reread from the live protected
main tip through authenticated API reads before tagging and publication, and a
tip change during verification requires a retry.

These checks do not finish cutover: exact compiled expected-set reconciliation
and the real producer/controller evidence transport still require integration.
Published recovery and receipts/harness retention beyond Actions artifact expiry
also remain blockers. Do not perform the first real release until those code
obligations and the administrator/native rehearsal rows in
[replacement-ledger.md](replacement-ledger.md) are closed.

The Fixtures dispatch is available on main under the `fixtures` environment. It
accepts a reviewed HTTPS arm64 Docker-save archive and its SHA-256, builds the
offline fixture using `fixtures build guest-oci-base`, and publishes a new
`fixtures-*` release without replacing an existing tag. P7 remains open until a
maintainer verifies the public fixture and previous-release assets, commits all
digests, and runs the native guest/artifact checks. Merely running the fixture
builder does not establish that its guest boots.
