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
version=$(awk -F'"' '/^version =/ {print $2; exit}' "$manifest")
field() { awk -v section="[targets.\"$target\"]" -v key="$1" '$0 == section {inside=1;next} /^\[/ {inside=0} inside && $1 == key {gsub(/"/, "", $3); print $3; exit}' "$manifest"; }
hash_file() { if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }
archive=$(field archive); archive_hash=$(field archive_sha256)
firmware=$(field firmware); agent=$(field agent); agent_hash=$(field agent_sha256)
dest="$root/third_party/msb-payloads/$target"
mkdir -p "$dest" "$root/third_party/msb-payloads/$arch"
# Downloads are cached and pinned by the manifest. A cached file whose hash no
# longer matches (e.g. left over from a previously pinned version) is discarded
# and fetched again; a fresh download that still mismatches is an error.
fetch_pinned() {
    local name=$1 path=$2 expected=$3 actual
    if [ -f "$path" ] && [ "$(hash_file "$path")" != "$expected" ]; then
        echo "discarding stale cached $name: $path" >&2
        rm -f "$path"
    fi
    if [ ! -f "$path" ]; then
        curl -fsSL --retry 3 "https://github.com/superradcompany/microsandbox/releases/download/v$version/$name" -o "$path.part"
        mv "$path.part" "$path"
    fi
    actual=$(hash_file "$path")
    [ "$actual" = "$expected" ] || { echo "$name hash mismatch: $actual (expected $expected)" >&2; exit 3; }
}
download="$dest/$archive"
fetch_pinned "$archive" "$download" "$archive_hash"
actual_archive_hash=$archive_hash
tar xf "$download" -C "$dest" "$firmware"
cc_bin=${CC:-cc}
"$cc_bin" -Wall -Wextra -Werror "$root/tools/msb-payloads/extract-kernel.c" -o "$dest/extract-kernel" ${link_flags[@]+"${link_flags[@]}"}
"$dest/extract-kernel" "$dest/$firmware" "$dest/kernel.bin" > "$dest/kernel.meta"
agent_path="$root/third_party/msb-payloads/$arch/agentd"
fetch_pinned "$agent" "$agent_path" "$agent_hash"
actual_agent_hash=$agent_hash
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
    # Exit 4 (not 3) so `make payloads-verify` can tell "recorded, awaiting
    # verification" apart from a download or hash failure.
    echo "Payload evidence was recorded. Run \`make payloads-verify\` on this native host to run the required gates and mark the payload verified." >&2
    exit 4
fi
bash "$root/tools/msb-payloads/verify.sh" "$target"
