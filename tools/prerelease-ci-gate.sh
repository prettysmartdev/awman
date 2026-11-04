#!/usr/bin/env bash
# Wait for the push CI checks needed before a release. The hosted macOS smoke
# job is replaced by the native checks in apple-silicon-prerelease.sh; runner
# allocation failures there must not prevent those checks from running.
set -euo pipefail

repo=${1:?usage: prerelease-ci-gate.sh owner/repo run-id}
run_id=${2:?usage: prerelease-ci-gate.sh owner/repo run-id}
poll_seconds=${AWMAN_PRERELEASE_CI_POLL_SECONDS:-60}
[[ "$poll_seconds" =~ ^[0-9]+$ ]] || {
  echo "prerelease-ci-gate: invalid poll interval: $poll_seconds" >&2
  exit 1
}

die() { echo "prerelease-ci-gate: $*" >&2; exit 1; }

while :; do
  snapshot=$(gh run view "$run_id" --repo "$repo" --json status,conclusion,jobs \
    --jq '
      (["RUN", .status, (.conclusion | if . == null or . == "" then "pending" else . end), "run"] | @tsv),
      (.jobs[] | ["JOB", .status, (.conclusion | if . == null or . == "" then "pending" else . end), .name] | @tsv)
    ') ||
    die "could not read Tests run $run_id"

  run_status=unknown
  fast=missing
  docker=missing
  builtin=missing
  static_default=missing
  static_builtin=missing
  pending=false
  while IFS=$'\t' read -r kind status conclusion name; do
    if [ "$kind" = RUN ]; then
      run_status=$status
      continue
    fi
    [ "$kind" = JOB ] || die "invalid job data for Tests run $run_id"
    # This job is rerun locally on Apple Silicon below. It may still be
    # queued, or have a hosted-runner failure, while the release checks pass.
    [ "$name" = 'Build smoke (macOS)' ] && continue

    required=false
    case "$name" in
      'Fast checks (lint, fmt, clippy, hermetic tests)') fast=$conclusion; required=true ;;
      'Full tests with Docker (Linux)') docker=$conclusion; required=true ;;
      'Builtin build (aarch64-unknown-linux-gnu)') builtin=$conclusion; required=true ;;
      'Static workspace build (default)') static_default=$conclusion; required=true ;;
      'Static workspace build (builtin-runtime)') static_builtin=$conclusion; required=true ;;
    esac
    if [ "$status" = completed ]; then
      case "$conclusion" in
        success) ;;
        skipped) if $required; then die "required CI job was skipped: $name"; fi ;;
        *) die "CI job $name finished with $conclusion (run $run_id)" ;;
      esac
    else
      pending=true
    fi
  done <<< "$snapshot"

  [ "$run_status" != unknown ] || die "missing run status for $run_id"
  if [ "$fast" = success ] && [ "$docker" = success ] &&
     [ "$builtin" = success ] && [ "$static_default" = success ] &&
     [ "$static_builtin" = success ] && ! $pending; then
    echo "Prerelease CI checks passed for run $run_id; macOS checks run locally"
    exit 0
  fi
  [ "$run_status" != completed ] ||
    die "run $run_id completed without all required CI jobs passing (fast=$fast, docker=$docker, builtin=$builtin, static-default=$static_default, static-builtin=$static_builtin)"
  echo "Waiting for non-macOS CI jobs in run $run_id"
  sleep "$poll_seconds"
done
