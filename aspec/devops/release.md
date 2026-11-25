# Release operator runbook

The release entry point is `make release` on the Apple Silicon release Mac.
It waits for the maintainer to squash merge the release PR into `main`, then
builds and validates a candidate from that exact merge commit. The command
creates the version tag and publishes the validated draft using the
maintainer's authenticated `gh` session. No GitHub App, branch protection,
tag ruleset bypass, or approving PR review is required by this procedure.

## Release invariants

- The release tag is plain `vX.Y.Z`, matching the version in `Cargo.toml`.
  Pre-release suffixes are refused.
- A candidate is built once for four assets: `awman-linux-amd64`,
  `awman-linux-arm64`, `awman-macos-arm64`, and `awman-windows-amd64.exe`.
  `SHA256SUMS`, attestations, and `release-state.json` bind those exact bytes.
- The candidate gate must pass before the command creates a tag. It includes
  the authenticated main gate, artifact tests against those bytes, native
  guest validation, and the signed offline boot of the macOS asset. Local
  prerelease packets alone cannot authorize publication.
- Immediately before tagging and publishing, `cargo xtask release promote`
  re-downloads the draft assets and verifies hashes, attestations, gate
  evidence, expiry, version inputs, and the current `main` tip. It creates an
  annotated tag at the merged PR commit and publishes the existing draft;
  it does not rebuild, sign, or replace assets.
- A published tag is never moved or reused. Published assets are never
  replaced. A bad release is superseded with a new version.
- The `promote.yml` workflow only verifies public bytes after the local
  publication. It downloads every published asset, checks `SHA256SUMS` and
  attestations, smoke tests the binaries, boots the macOS binary offline,
  and records a receipt. Rehearsals use it to verify candidate bytes without
  creating a tag or publishing anything.
- Failed or blocked validation stops before tagging. Once a tag exists, a
  retry resumes the same candidate and tag; it never silently switches
  commit or bytes.

## Prerequisites

- Prepare and commit version, notes, and documentation on
  `release/vX.Y.Z`. The release notes at `docs/releases/vX.Y.Z.md` must have
  no placeholder.
- Authenticate `gh` as a maintainer with permission to merge the PR, run
  workflows, create tags, and edit releases. If `make release` registers this
  Mac as a runner, the login also needs organization runner-group access and
  runner registration rights. `apple-prerelease` does not need those rights.
- Use a bare-metal Apple Silicon Mac with Hypervisor.framework and the pinned
  toolchain. Native Linux acceptance requires an x86_64 KVM host reached
  through `REMOTE_LOGIN=user@host`. The Linux host needs Docker with a local
  Unix-socket context. Apple prerelease and the candidate workflow need Apple
  Containers, not Docker or `sbx` on the release Mac. The `sbx` runtime is
  disabled in v0.14 pending [issue #86](https://github.com/prettysmartdev/awman/issues/86).
- Keep the required fixture, prior-release, runner-image, and macOS payload
  pins published and recorded in `ci/gates.toml`. The candidate gate reports
  missing pins as blocked evidence.
- Keep the `release` environment available to workflows on `main` and the
  `awman-release` runner group restricted to `candidate.yml` and
  `promote.yml`. Branch rules and PR review settings are the maintainer's
  choice, not release prerequisites.

The GNU `awman-linux-amd64` asset is built on Ubuntu 22.04. The candidate
checks the exact executable with `readelf` and records `abi.json`; native
Linux packets from newer developer hosts do not prove the published ABI.

## Normal path

```sh
make linux-prerelease REMOTE_LOGIN=user@host
make apple-prerelease
make release
```

The two prerelease targets may run in either order. They create or resume the
release branch, pin inputs, run native acceptance, and store packets for the
same release branch commit. The Apple target opens the PR and reuses a
successful full main CI gate when its authenticated run and downloaded
evidence prove that exact commit; otherwise it dispatches a new main gate.
The Apple target runs native acceptance before waiting for that main gate, so
a native failure returns its packet without first waiting for a full CI run.
It stores a verified native packet before starting CI and reuses that packet
on a retry at the same commit. A later CI failure therefore does not require
another native run; changed source or invalid packet evidence forces one.
The Mac archive omits AppleDouble metadata. If extraction of a newly produced
archive fails but the runner's saved evidence independently verifies at the
same commit, Apple prerelease may stage that evidence without rerunning native
execution; the archive problem remains visible for independent review.
Neither target checks GitHub branch rules,
approval counts, tag rulesets, or App credentials.

`make release` checks both native packets and the authenticated main gate,
then waits for the PR to merge. Merge the PR using **Squash and merge** while
the command waits. It reads the PR's merge commit from GitHub, requires that
commit to be the tip of `origin/main` with one parent, and compares its tree
to the validated release branch tree. If the PR head or merged tree changed,
rerun both prerelease targets. If `main` moved after the PR merge, stop and
inspect before retrying.
If a post-merge workflow defect requires a source fix, create a new reviewed
release-branch commit and an open follow-up PR. The old packets cannot be
relabeled for that commit; both prerelease targets must validate it before
the follow-up PR is squash merged.

The command then starts `candidate.yml` for that merge commit with automatic
promotion disabled. It waits for the candidate gate, rechecks the validated
draft, creates the annotated tag, publishes the draft, dispatches
`promote.yml` for public asset verification, and waits for the receipt.
`make release-status VERSION=vX.Y.Z` shows the current state. `VERSION` is
optional for `make release`; if supplied, it must match `Cargo.toml`.

`RELEASE_RUNNER=auto` (default) uses an online native release runner or
registers this Mac temporarily. `RELEASE_RUNNER=local` always registers this
Mac. `RELEASE_RUNNER=none` relies on an already managed runner, but the
command still waits for validation and publication. When this command starts
a local runner, an exit trap stops and deregisters it after success, failure,
or interruption. Runner-group discovery happens only when registration is
needed; an existing runner or `RELEASE_RUNNER=none` does not need that lookup.

## Recovery

Re-run `make release` from the clean release branch or the merged `main`
checkout. The command reads release state from GitHub and resumes the same
candidate. A failed candidate run can be rerun from the Actions page after
the underlying cause is fixed. A `validated` candidate is ready for local
tagging and publication; `promoted` needs public verification; `verified` is
complete. The command refuses a candidate for another commit or a tag at
another commit. A draft can be abandoned with
`make release-abandon VERSION=vX.Y.Z` before publication; its candidate
number remains reserved.

If public verification fails after publication, fix the runner or external
condition and rerun `make release`; the tag and assets are not changed.
If the published bytes are wrong, create a new version.

## Rehearsal and status

`cargo xtask release validate` runs a dry-run candidate for current `main`.
It calls `promote.yml` to verify the candidate bytes and deletes the rehearsal
draft. It never creates a public tag or release.

`make release-status VERSION=vX.Y.Z` is read-only. It reports the candidate,
commit, state, tag presence, failing gate IDs, and evidence links. Candidate
and post-publication verification evidence is retained by GitHub Actions.
