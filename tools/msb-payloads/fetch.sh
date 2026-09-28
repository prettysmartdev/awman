#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd -P)
target=${1:?usage: fetch.sh TARGET_TRIPLE}
case "$target" in
    aarch64-unknown-linux-gnu) arch=aarch64; os=Linux; link_flags=(-ldl) ;;
    x86_64-unknown-linux-gnu) arch=x86_64; os=Linux; link_flags=(-ldl) ;;
    aarch64-apple-darwin) arch=aarch64; os=Darwin; link_flags=() ;;
    *) echo "unsupported payload target: $target" >&2; exit 2 ;;
esac
if [ "$(uname -s)" != "$os" ] || [ "$(uname -m | sed 's/^arm64$/aarch64/')" != "$arch" ]; then
    echo "native $target host required to extract firmware" >&2; exit 2
fi
manifest="$root/third_party/msb-payloads/manifest.toml"
field() { awk -v section="[targets.\"$target\"]" -v key="$1" '$0 == section {inside=1;next} /^\[/ {inside=0} inside && $1 == key {gsub(/"/, "", $3); print $3; exit}' "$manifest"; }
archive=$(field archive); archive_hash=$(field archive_sha256)
firmware=$(field firmware); agent=$(field agent); agent_hash=$(field agent_sha256)
dest="$root/third_party/msb-payloads/$target"
mkdir -p "$dest" "$root/third_party/msb-payloads/$arch"
download="$dest/$archive"
if [ ! -f "$download" ]; then
    curl -fsSL --retry 3 "https://github.com/superradcompany/microsandbox/releases/download/v0.7.2/$archive" -o "$download"
fi
printf '%s  %s\n' "$archive_hash" "$download" | sha256sum -c -
tar xf "$download" -C "$dest" "$firmware"
cc -Wall -Wextra -Werror "$root/tools/msb-payloads/extract-kernel.c" -o "$dest/extract-kernel" "${link_flags[@]}"
"$dest/extract-kernel" "$dest/$firmware" "$dest/kernel.bin" > "$dest/kernel.meta"
agent_path="$root/third_party/msb-payloads/$arch/agentd"
if [ ! -f "$agent_path" ]; then
    curl -fsSL --retry 3 "https://github.com/superradcompany/microsandbox/releases/download/v0.7.2/$agent" -o "$agent_path"
fi
printf '%s  %s\n' "$agent_hash" "$agent_path" | sha256sum -c -
sha256sum "$dest/kernel.bin"
cat "$dest/kernel.meta"
if [ "$(field status)" != verified ]; then
    echo "Record the extracted kernel hash and metadata in $manifest after review, then set status=verified." >&2
    exit 3
fi
bash "$root/tools/msb-payloads/verify.sh" "$target"
