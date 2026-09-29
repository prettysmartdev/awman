#!/usr/bin/env bash
# Rebuild the historical libsqlite3-sys 0.37.0 strict CLI probe from its
# checked-in manifest/lock and current native payload inputs. This is a real
# older CLI catalog writer; it does not relabel a new catalog as old.
set -euo pipefail
root=$(cd "$(dirname "$0")/../../.." && pwd -P)
target=${1:-$(rustc -vV | sed -n 's/^host: //p')}
output=${2:?usage: build-old-probe.sh TARGET_TRIPLE OUTPUT_DIRECTORY}
case "$target:$(uname -s):$(uname -m)" in
  aarch64-unknown-linux-gnu:Linux:aarch64|x86_64-unknown-linux-gnu:Linux:x86_64|aarch64-apple-darwin:Darwin:arm64) ;;
  *) echo "BLOCKED: old probe must be built natively on a supported target" >&2; exit 2 ;;
esac
bash "$root/tools/msb-payloads/verify.sh" "$target"
payload="$root/third_party/msb-payloads/$target"
if [[ "$target" == *linux* ]]; then
  native="$root/third_party/native/libcap-ng/$target"
  test -f "$native/libcap-ng.a" || { echo "BLOCKED: build pinned static cap-ng first" >&2; exit 2; }
else native=; fi
lock="$root/tools/oci-runtime-spike/strict-embed/full-probe/Cargo.lock"
sed -n '/^name = "libsqlite3-sys"$/,/^$/p' "$lock" | grep -q '^version = "0.37.0"$' || { echo "old probe lock no longer pins libsqlite3-sys 0.37.0" >&2; exit 1; }
mkdir -p "$output/runtime" "$output/artifacts" "$output/static-lib"
output=$(cd "$output" && pwd -P)
agent_arch=aarch64
if [[ "$target" == x86_64-* ]]; then agent_arch=x86_64; fi
cp "$root/third_party/msb-payloads/$agent_arch/agentd" "$output/artifacts/agentd"
export SPIKE_KERNEL="$payload/kernel.bin"
export SPIKE_KERNEL_METADATA="$payload/kernel.meta"
export MSB_EMBED_ARTIFACTS_DIR="$output/artifacts"
if [ -n "$native" ]; then export LIBRARY_PATH="$native${LIBRARY_PATH:+:$LIBRARY_PATH}"; fi
export CARGO_TARGET_DIR="$output/cargo-target"
cargo build --locked --manifest-path "$root/tools/oci-runtime-spike/strict-embed/full-probe/Cargo.toml" \
  --target "$target" > "$output/build.log" 2>&1 || { tail -80 "$output/build.log" >&2; exit 1; }
cp "$CARGO_TARGET_DIR/$target/debug/awman-msb-full-embed-probe" "$output/runtime/awman-probe"
cp "$SPIKE_KERNEL" "$output/kernel.bin"
cp "$SPIKE_KERNEL_METADATA" "$output/kernel.metadata"
cp -a "$root/third_party/msb_krun-0.1.39" "$output/msb_krun-0.1.39"
if [ -n "$native" ]; then cp "$native/libcap-ng.a" "$output/static-lib/libcap-ng.a"; fi
if command -v sha256sum >/dev/null; then sha256sum "$output/runtime/awman-probe" "$output/kernel.bin" "$output/kernel.metadata" "$output/artifacts/agentd"; else shasum -a 256 "$output/runtime/awman-probe" "$output/kernel.bin" "$output/kernel.metadata" "$output/artifacts/agentd"; fi > "$output/inputs.sha256"
printf 'PASS rebuilt genuine old-probe binary (sqlite native binding 0.37.0): %s\n' "$output/runtime/awman-probe"
