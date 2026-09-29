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
hash_file() { if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }
archive=$(field archive); archive_hash=$(field archive_sha256)
firmware=$(field firmware); agent=$(field agent); agent_hash=$(field agent_sha256)
dest="$root/third_party/msb-payloads/$target"
mkdir -p "$dest" "$root/third_party/msb-payloads/$arch"
download="$dest/$archive"
if [ ! -f "$download" ]; then
    curl -fsSL --retry 3 "https://github.com/superradcompany/microsandbox/releases/download/v0.7.2/$archive" -o "$download"
fi
actual_archive_hash=$(hash_file "$download")
[ "$actual_archive_hash" = "$archive_hash" ] || { echo "archive hash mismatch: $actual_archive_hash" >&2; exit 3; }
tar xf "$download" -C "$dest" "$firmware"
cc_bin=${CC:-cc}
"$cc_bin" -Wall -Wextra -Werror "$root/tools/msb-payloads/extract-kernel.c" -o "$dest/extract-kernel" "${link_flags[@]}"
"$dest/extract-kernel" "$dest/$firmware" "$dest/kernel.bin" > "$dest/kernel.meta"
agent_path="$root/third_party/msb-payloads/$arch/agentd"
if [ ! -f "$agent_path" ]; then
    curl -fsSL --retry 3 "https://github.com/superradcompany/microsandbox/releases/download/v0.7.2/$agent" -o "$agent_path"
fi
actual_agent_hash=$(hash_file "$agent_path")
[ "$actual_agent_hash" = "$agent_hash" ] || { echo "agent hash mismatch: $actual_agent_hash" >&2; exit 3; }
actual_kernel_hash=$(hash_file "$dest/kernel.bin")
source_revision=$(awk -F'"' '/^microsandbox_revision =/ {print $2; exit}' "$manifest")
libkrun_revision=$(awk -F'"' '/^libkrun_revision =/ {print $2; exit}' "$manifest")
cat > "$dest/provenance.txt" <<EOF
target=$target
architecture=$arch
host_os=$(uname -s)
host_arch=$(uname -m)
archive=$archive
archive_sha256=$actual_archive_hash
microsandbox_revision=$source_revision
libkrun_revision=$libkrun_revision
firmware=$firmware
extractor_sha256=$(hash_file "$root/tools/msb-payloads/extract-kernel.c")
native_cc=$("$cc_bin" --version 2>&1 | head -1)
kernel_sha256=$actual_kernel_hash
agent=$agent
agent_sha256=$actual_agent_hash
EOF
cat "$dest/provenance.txt"
cat "$dest/kernel.meta"
if [ "$(field status)" != verified ]; then
    echo "Payload evidence was recorded. Review compatibility and evidence on this native target, update the manifest hashes/metadata and set status=verified only after matching verification." >&2
    exit 3
fi
bash "$root/tools/msb-payloads/verify.sh" "$target"
