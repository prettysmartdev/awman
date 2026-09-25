#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd -P)
target=${1:?usage: verify.sh TARGET_TRIPLE}
case "$target" in
    aarch64-unknown-linux-gnu|aarch64-apple-darwin) arch=aarch64 ;;
    x86_64-unknown-linux-gnu) arch=x86_64 ;;
    *) echo "unsupported payload target: $target" >&2; exit 2 ;;
esac
manifest="$root/third_party/msb-payloads/manifest.toml"
field() { awk -v section="[targets.\"$target\"]" -v key="$1" '$0 == section {inside=1;next} /^\[/ {inside=0} inside && $1 == key {gsub(/"/, "", $3); print $3; exit}' "$manifest"; }
[ "$(field status)" = verified ] || { echo "unverified payload: $target" >&2; exit 3; }
dest="$root/third_party/msb-payloads/$target"
printf '%s  %s\n' "$(field kernel_sha256)" "$dest/kernel.bin" | sha256sum -c -
printf '%s  %s\n' "$(field agent_sha256)" "$root/third_party/msb-payloads/$arch/agentd" | sha256sum -c -
[ "$(wc -c < "$dest/kernel.bin")" = "$(field size)" ]
for key in size guest_address entry_address; do
    grep -qx "$key=$(field "$key")" "$dest/kernel.meta"
done
case "$(od -An -tu2 -j18 -N2 "$root/third_party/msb-payloads/$arch/agentd" | tr -d ' ')" in
    183) [ "$arch" = aarch64 ] ;;
    62) [ "$arch" = x86_64 ] ;;
    *) echo "wrong agent architecture" >&2; exit 3 ;;
esac
echo "verified $target payload"
