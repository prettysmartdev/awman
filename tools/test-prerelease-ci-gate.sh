#!/usr/bin/env bash
# Verify the prerelease gate accepts only completed non-macOS CI coverage.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/awman-prerelease-ci.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin"
cat > "$scratch/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[ "$1 $2" = 'run view' ] || exit 2

emit_job() { printf 'JOB\t%s\t%s\t%s\n' "$1" "$2" "$3"; }
if [ "$FAKE_CI_SCENARIO" = pending_then_success ]; then
  calls=$(cat "$FAKE_CI_CALLS")
  echo "$((calls + 1))" > "$FAKE_CI_CALLS"
  if [ "$calls" = 0 ]; then
    printf 'RUN\tin_progress\tpending\trun\n'
    emit_job completed success 'Fast checks (lint, fmt, clippy, hermetic tests)'
    emit_job queued pending 'Build smoke (macOS)'
    exit 0
  fi
fi

printf 'RUN\tcompleted\tfailure\trun\n'
emit_job completed success 'Fast checks (lint, fmt, clippy, hermetic tests)'
emit_job completed success 'Full tests with Docker (Linux)'
emit_job completed success 'Builtin build (aarch64-unknown-linux-gnu)'
emit_job completed success 'Static workspace build (default)'
case "$FAKE_CI_SCENARIO" in
  missing_required) ;;
  required_skipped) emit_job completed skipped 'Static workspace build (builtin-runtime)' ;;
  linux_failure) emit_job completed failure 'Static workspace build (builtin-runtime)' ;;
  *) emit_job completed success 'Static workspace build (builtin-runtime)' ;;
esac
emit_job completed skipped 'Builtin hardware (${{ matrix.name }})'
if [ "$FAKE_CI_SCENARIO" = unrelated_failure ]; then
  emit_job completed failure 'Unexpected Linux gate'
fi
emit_job completed cancelled 'Build smoke (macOS)'
EOF
chmod +x "$scratch/bin/gh"

export PATH="$scratch/bin:$PATH"
export AWMAN_PRERELEASE_CI_POLL_SECONDS=0
export FAKE_CI_CALLS="$scratch/calls"
gate="$root/tools/prerelease-ci-gate.sh"

for scenario in mac_capacity pending_then_success; do
  export FAKE_CI_SCENARIO="$scenario"
  echo 0 > "$FAKE_CI_CALLS"
  bash "$gate" owner/repo 123 > "$scratch/out" 2>&1 || {
    cat "$scratch/out" >&2
    echo "prerelease CI gate rejected $scenario" >&2
    exit 1
  }
done
[ "$(cat "$FAKE_CI_CALLS")" = 2 ] || {
  echo 'prerelease CI gate did not wait for dependent jobs' >&2
  exit 1
}

for scenario in linux_failure missing_required required_skipped unrelated_failure; do
  export FAKE_CI_SCENARIO="$scenario"
  if bash "$gate" owner/repo 123 > "$scratch/out" 2>&1; then
    echo "prerelease CI gate accepted $scenario" >&2
    exit 1
  fi
done

echo 'test-prerelease-ci-gate: all cases passed'
