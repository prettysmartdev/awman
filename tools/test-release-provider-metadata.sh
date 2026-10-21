#!/usr/bin/env bash
# Exercise the real artifact checker up to its hardware/offline-session gate,
# with deterministic inspection-tool fixtures. Does not boot a VM.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
binary=${1:-$repo/target/debug/awman}
scratch=$(mktemp -d /tmp/awman-release-metadata.XXXXXX)
trap 'rm -r -- "$scratch"' EXIT
mkdir "$scratch/bin"
env -i "$binary" __awman-builtin-kernel-check > "$scratch/provider.json"
cat > "$scratch/bin/ldd" <<'TOOL'
#!/bin/sh
printf 'libc.so.6 => /lib/libc.so.6\n'
TOOL
cat > "$scratch/bin/readelf" <<'TOOL'
#!/bin/sh
printf '.msbver GLIBC_2.17\n'
TOOL
cat > "$scratch/bin/otool" <<'TOOL'
#!/bin/sh
printf '__msbver\n'
TOOL
cat > "$scratch/bin/fs_usage" <<'TOOL'
#!/bin/sh
exit 0
TOOL
cat > "$scratch/bin/strace" <<'TOOL'
#!/bin/sh
exit 0
TOOL
# The checker copies the fixture artifact, so keep metadata next to the output.
cat > "$scratch/artifact" <<'ARTIFACT'
#!/bin/sh
# awman @VERSION@ msb
if [ "$1" = --version ]; then printf 'awman @VERSION@\n'; else /bin/cat "$0.json"; fi
ARTIFACT
version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$repo/Cargo.toml" | head -1)
sed "s/@VERSION@/$version/g" "$scratch/artifact" > "$scratch/versioned"
mv "$scratch/versioned" "$scratch/artifact"
chmod +x "$scratch/artifact" "$scratch/bin/"*
target=$(rustc -vV | sed -n 's/^host: //p')
for mode in current old; do
    output="$scratch/$mode"
    mkdir "$output"
    cp "$scratch/provider.json" "$output/awman.json"
    if [ "$mode" = old ]; then
        sed 's/0.7.8/0.7.2/g' "$scratch/provider.json" > "$output/awman.json"
    fi
    if PATH="$scratch/bin:$PATH" bash "$repo/tools/release-artifact-check.sh" "$scratch/artifact" "$target" "$output" > "$scratch/check.log" 2>&1; then
        echo 'FAIL: artifact checker silently bypassed offline-session gate' >&2; exit 1
    fi
    if [ "$mode" = current ]; then
        grep -q 'BLOCKED: pass an offline' "$scratch/check.log"
    else
        grep -q 'provider metadata mismatch' "$scratch/check.log"
    fi
done
echo 'PASS current provider metadata reaches hardware gate; old metadata is refused'
