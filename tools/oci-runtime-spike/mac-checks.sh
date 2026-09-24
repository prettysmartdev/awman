#!/usr/bin/env bash
set -euo pipefail

if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
    printf 'Run this script on an Apple Silicon Mac, outside Rosetta.\n' >&2
    exit 2
fi
for command in cargo curl tar shasum codesign perl; do
    command -v "$command" >/dev/null || { printf 'Required development tool missing: %s\n' "$command" >&2; exit 2; }
done
scripts=$(cd "$(dirname "$0")" && pwd)
output=$(mktemp -d /tmp/awsp.XXXXXX)
output=$(cd "$output" && pwd -P)
mkdir -p "$output"/{downloads,msb,smolvm,msb-state,tmp,logs}
printf 'Results directory: %s\nNo sudo, global installation, real agent credentials, or image builds.\n' "$output"
printf 'case\tstatus\texpected_exit\tactual_exit\tseconds\n' > "$output/summary.tsv"
sw_vers > "$output/logs/platform.stdout"
uname -m >> "$output/logs/platform.stdout"
printf 'msb=v0.7.2\nsmolvm=v1.18.1\n' > "$output/logs/versions.stdout"

download() {
    local url=$1 name=$2 checksum=$3
    curl --fail --location --retry 3 --max-time 300 "$url" -o "$output/downloads/$name"
    (cd "$output/downloads" && printf '%s  %s\n' "$checksum" "$name" | shasum -a 256 -c -)
}

download https://github.com/superradcompany/microsandbox/releases/download/v0.7.2/microsandbox-darwin-aarch64.tar.gz msb.tar.gz 14a5910c6b395e9d50e001d81388cbe5366f18165c57ede4a5eda3e3315b7dbd
download https://github.com/smol-machines/smolvm/releases/download/v1.18.1/smolvm-1.18.1-darwin-arm64.tar.gz smolvm.tar.gz c987d335d7e419ebc2f0dabc9882f9ee10030a5150ad4668a1b1b3c4223d02fe
tar xf "$output/downloads/msb.tar.gz" -C "$output/msb"
tar xf "$output/downloads/smolvm.tar.gz" -C "$output/smolvm"
msb=$(find "$output/msb" -type f -name msb | head -1)
smolvm=$(find "$output/smolvm" -type f -name smolvm | head -1)
smolvm_binary=$(find "$output/smolvm" -type f -name smolvm-bin | head -1)
test -x "$msb" && test -x "$smolvm" && test -x "$smolvm_binary"

run_case() {
    local name=$1 expected=$2 status=0 started=$SECONDS verdict=PASS
    shift 2
    printf 'Running %s...\n' "$name"
    "$@" > "$output/logs/$name.stdout" 2> "$output/logs/$name.stderr" || status=$?
    if [ "$status" -ne "$expected" ]; then verdict=FAIL; fi
    printf '%s\t%s\t%s\t%s\t%s\n' "$name" "$verdict" "$expected" "$status" "$((SECONDS - started))" | tee -a "$output/summary.tsv"
}

msb_run() {
    env -i PATH=/usr/bin:/bin TMPDIR="$output/tmp" MSB_HOME="$output/msb-state" MSB_BACKEND=local "$msb" "$@"
}

smol_run() {
    env -i PATH=/usr/bin:/bin TMPDIR="$output/tmp" SMOLVM_DATA_DIR="$output/smolvm-state" "$smolvm" "$@"
}

cleanup() {
    for name in image-probe exit-probe stream-probe; do
        msb_run remove --force "$name" >> "$output/logs/cleanup.log" 2>&1 || true
    done
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

run_case msb-signature 0 codesign --verify --deep --strict "$msb"
run_case smolvm-signature 0 codesign --verify --deep --strict "$smolvm_binary"
run_case msb-entitlements 0 codesign -d --entitlements :- "$msb"
run_case smolvm-entitlements 0 codesign -d --entitlements :- "$smolvm_binary"
run_case msb-doctor 0 msb_run doctor
run_case registry-pull 0 msb_run pull docker.io/library/alpine:3.22
run_case base-save 0 msb_run image save --format docker -o "$output/base.tar" docker.io/library/alpine:3.22
if [ ! -s "$output/base.tar" ]; then printf 'Base preparation failed; share %s\n' "$output/logs"; exit 1; fi
run_case fixture-build 0 env CARGO_HOME="$output/cargo" CARGO_TARGET_DIR="$output/target" cargo run --locked --manifest-path "$scripts/fixture/Cargo.toml" -- "$output/base.tar" "$output/fixtures"
test -s "$output/fixtures/fixture-oci.tar"
run_case host-checks 0 bash "$scripts/host-checks.sh" "$msb" "$smolvm" "$output/fixtures" "$output/host-checks"
run_case msb-load 0 msb_run image load -i "$output/fixtures/fixture-oci.tar" -t awman-spike/fixture:latest
run_case msb-image-contract 0 msb_run run -n image-probe --cpus 1 --memory 512M --no-tty --no-net --timeout 30s --max-duration 60s awman-spike/fixture:latest
run_case msb-image-marker 0 grep -q image-contract-pass "$output/logs/msb-image-contract.stdout"
run_case smolvm-image-default-command 0 smol_run machine run --cpus 1 --mem 512 --storage 1 --overlay 1 --timeout 30s -I "$output/fixtures/fixture-docker.tar"
run_case smolvm-image-contract 0 smol_run machine run --cpus 1 --mem 512 --storage 1 --overlay 1 --timeout 30s -I "$output/fixtures/fixture-docker.tar" -- /compat/check-image
run_case smolvm-image-marker 0 grep -q image-contract-pass "$output/logs/smolvm-image-contract.stdout"
run_case msb-exit-code 37 msb_run run -n exit-probe --cpus 1 --memory 512M --no-tty --no-net --timeout 30s --max-duration 60s --entrypoint /bin/sh awman-spike/fixture:latest -- -c 'exit 37'
run_case smolvm-exit-code 37 smol_run machine run --cpus 1 --mem 512 --storage 1 --overlay 1 --timeout 30s -I "$output/fixtures/fixture-docker.tar" -- /bin/sh -c 'exit 37'
run_case msb-streams 0 msb_run run -n stream-probe --cpus 1 --memory 512M --no-tty --no-net --timeout 30s --max-duration 60s --entrypoint /bin/sh awman-spike/fixture:latest -- -c 'printf stdout-marker; printf stderr-marker >&2'
run_case smolvm-streams 0 smol_run machine run --cpus 1 --mem 512 --storage 1 --overlay 1 --timeout 30s -I "$output/fixtures/fixture-docker.tar" -- /bin/sh -c 'printf stdout-marker; printf stderr-marker >&2'
for candidate in msb smolvm; do
    run_case "$candidate-stdout-marker" 0 grep -q stdout-marker "$output/logs/$candidate-streams.stdout"
    run_case "$candidate-stderr-marker" 0 grep -q stderr-marker "$output/logs/$candidate-streams.stderr"
done
run_case msb-mounts 0 bash "$scripts/mount-probe.sh" msb "$msb" "$output/fixtures/fixture-oci.tar" "$output/msb-mounts"
run_case smolvm-directory-mounts 0 bash "$scripts/mount-probe.sh" smolvm "$smolvm" "$output/fixtures/fixture-docker.tar" "$output/smolvm-mounts"

if [ -n "${APPLE_IMAGE:-}" ]; then
    run_case apple-export 0 container image save --output "$output/apple.tar" "$APPLE_IMAGE"
    if [ -s "$output/apple.tar" ]; then
        run_case apple-to-msb 0 msb_run image load -i "$output/apple.tar" -t awman-spike/apple:latest
        run_case apple-archive-layout 0 tar tf "$output/apple.tar"
    fi
fi
if [ -n "${DOCKER_IMAGE:-}" ]; then
    run_case docker-export 0 docker image save --output "$output/docker.tar" "$DOCKER_IMAGE"
    if [ -s "$output/docker.tar" ]; then
        run_case docker-to-msb 0 msb_run image load -i "$output/docker.tar" -t awman-spike/docker:latest
        run_case docker-archive-layout 0 tar tf "$output/docker.tar"
    fi
fi
cleanup
trap - EXIT
mkdir "$output/share"
cp "$output/summary.tsv" "$output/share/"
cp -R "$output/logs" "$output/share/"
for directory in host-checks msb-mounts smolvm-mounts; do
    if [ -d "$output/$directory" ]; then
        mkdir "$output/share/$directory"
        for file in "$output/$directory"/*; do
            case "$file" in
                *.stdout|*.stderr|*.tsv|*.log) if [ -f "$file" ]; then cp "$file" "$output/share/$directory/"; fi ;;
            esac
        done
    fi
done
COPYFILE_DISABLE=1 tar czf "$output/results.tar.gz" -C "$output/share" .
printf '\nReview then share: %s/results.tar.gz\n' "$output"
printf 'Raw state/images stay in %s; the share archive excludes them.\n' "$output"
printf 'Smolvm directory-only success is NOT file-mount compatibility. Lifecycle, PTY, and security review remain separate gates.\n'
cat "$output/summary.tsv"
