#!/usr/bin/env bash
set -euo pipefail

previous=${1:?Usage: mac-checks.sh PREVIOUS_MAC_SPIKE_DIRECTORY}
previous=$(cd "$previous" && pwd -P)
scripts=$(cd "$(dirname "$0")" && pwd)
if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
    printf 'Apple Silicon Mac required.\n' >&2
    exit 2
fi
for command in cargo cc git curl patch codesign otool shasum; do
    command -v "$command" >/dev/null || { printf 'Missing build tool: %s\n' "$command" >&2; exit 2; }
done
test -s "$previous/downloads/msb.tar.gz"
test -s "$previous/fixtures/fixture-oci.tar"
output=$(mktemp -d /tmp/awe.XXXXXX)
output=$(cd "$output" && pwd -P)
mkdir -p "$output"/{release,artifacts,runtime,empty-path,state,tmp,logs}
printf 'Build and results: %s\nBuilds a patched standalone probe, not awman. No sudo or global installation.\n' "$output"
rustc --version > "$output/logs/toolchain.stdout"
cargo --version >> "$output/logs/toolchain.stdout"
printf '%s  %s\n' 14a5910c6b395e9d50e001d81388cbe5366f18165c57ede4a5eda3e3315b7dbd "$previous/downloads/msb.tar.gz" | shasum -a 256 -c -
tar xf "$previous/downloads/msb.tar.gz" -C "$output/release"
firmware=$(find "$output/release" -type f -name 'libkrunfw*.dylib' | head -1)
test -n "$firmware"
cc -Wall -Wextra -Werror "$scripts/extract-kernel.c" -o "$output/extract-kernel"
"$output/extract-kernel" "$firmware" "$output/kernel.bin" > "$output/kernel.metadata"
git clone --quiet --depth 1 --branch v0.7.2 https://github.com/superradcompany/microsandbox.git "$output/microsandbox" 2> "$output/logs/source.log"
test "$(git -C "$output/microsandbox" rev-parse HEAD)" = 60d4dc8a436fb9365491567ec21d073e924e3c6d
curl --fail --location --retry 3 --max-time 300 https://static.crates.io/crates/msb_krun/msb_krun-0.1.39.crate -o "$output/krun.crate"
printf '%s  %s\n' 0944407a6ae125935e64dcf9be666e56d2cca0907b9e34be6648784db453490c "$output/krun.crate" | shasum -a 256 -c -
tar xf "$output/krun.crate" -C "$output"
patch -d "$output/msb_krun-0.1.39" -p1 < "$scripts/embedded-loader.patch" > "$output/logs/patch.log"
curl --fail --location --retry 3 --max-time 300 https://github.com/superradcompany/microsandbox/releases/download/v0.7.2/agentd-aarch64 -o "$output/artifacts/agentd"
printf '%s  %s\n' ca6dde7a3000d8e93d5dca87fcebf77ebb36ce83377385bae2be70dbead9a5e1 "$output/artifacts/agentd" | shasum -a 256 -c -
printf 'Building full SDK/CLI/VM probe; first build may take several minutes.\n'
env CARGO_HOME="$output/cargo" CARGO_TARGET_DIR="$output/target" SPIKE_KERNEL="$output/kernel.bin" SPIKE_KERNEL_METADATA="$output/kernel.metadata" MSB_EMBED_ARTIFACTS_DIR="$output/artifacts" cargo build --locked --manifest-path "$scripts/full-probe/Cargo.toml" --config "patch.crates-io.msb_krun.path=\"$output/msb_krun-0.1.39\"" --config "patch.crates-io.microsandbox-cli.path=\"$output/microsandbox/crates/cli\"" > "$output/logs/build.log" 2>&1 || {
    tail -40 "$output/logs/build.log"
    printf 'Build failed; share %s/logs/build.log\n' "$output"
    exit 1
}
cp "$output/target/debug/awman-msb-full-embed-probe" "$output/runtime/awman-probe"
codesign --force --sign - --entitlements "$scripts/hypervisor.plist" "$output/runtime/awman-probe" > "$output/logs/sign.stdout" 2> "$output/logs/sign.stderr"
otool -L "$output/runtime/awman-probe" > "$output/logs/dynamic-libraries.stdout"
mv "$output/release" "$output/build-only-release"
mv "$output/kernel.bin" "$output/build-only-kernel.bin"
mv "$output/artifacts" "$output/build-only-artifacts"
printf 'case\texpected_exit\tactual_exit\n' > "$output/summary.tsv"

runtime() {
    env -i PATH="$output/empty-path" TMPDIR="$output/tmp" MSB_HOME="$output/state" MSB_BACKEND=local "$output/runtime/awman-probe" "$@"
}

record() {
    local name=$1 expected=$2 status=0
    shift 2
    printf 'Running %s...\n' "$name"
    "$@" > "$output/logs/$name.stdout" 2> "$output/logs/$name.stderr" || status=$?
    printf '%s\t%s\t%s\n' "$name" "$expected" "$status" | tee -a "$output/summary.tsv"
}

cleanup() {
    runtime remove --force strict-image strict-exit > "$output/logs/cleanup.log" 2>&1 || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
record import 0 runtime image load -i "$previous/fixtures/fixture-oci.tar" -t awman-spike/strict:latest
record inspect 0 runtime image inspect --format json awman-spike/strict:latest
record strict-image 0 runtime run -n strict-image --cpus 1 --memory 512M --no-tty --no-net --timeout 30s --max-duration 60s awman-spike/strict:latest
record strict-image-marker 0 grep -q image-contract-pass "$output/logs/strict-image.stdout"
record strict-exit 37 runtime run -n strict-exit --cpus 1 --memory 512M --no-tty --no-net --timeout 30s --max-duration 60s --entrypoint /bin/sh awman-spike/strict:latest -- -c 'exit 37'
record strict-mounts 0 bash "$scripts/../mount-probe.sh" msb "$output/runtime/awman-probe" "$previous/fixtures/fixture-oci.tar" "$output/mounts"
find "$output/state" "$output/mounts/state" -type f -name 'runtime.log*' -exec grep -H 'embedded-kernel-provider-called' {} \; > "$output/logs/kernel-provider.stdout" 2>/dev/null || true
record embedded-kernel-observed 0 test -s "$output/logs/kernel-provider.stdout"
find "$output/state" "$output/mounts/state" -type f \( -name '*.dylib' -o -name '*.so*' -o -name msb -o -name agentd \) > "$output/logs/extracted-helper-candidates.stdout"
cleanup
trap - EXIT
mkdir "$output/share"
cp "$output/summary.tsv" "$output/share/"
cp -R "$output/logs" "$output/share/"
if [ -d "$output/mounts" ]; then
    mkdir "$output/share/mounts"
    for file in "$output/mounts"/*; do
        case "$file" in *.stdout|*.stderr|*.log|*.tsv) if [ -f "$file" ]; then cp "$file" "$output/share/mounts/"; fi ;; esac
    done
fi
COPYFILE_DISABLE=1 tar czf "$output/results.tar.gz" -C "$output/share" .
printf 'Review and share %s/results.tar.gz\n' "$output"
printf 'Raw build/state stays local. This is a feasibility probe, not a finished awman backend.\n'
cat "$output/summary.tsv"
