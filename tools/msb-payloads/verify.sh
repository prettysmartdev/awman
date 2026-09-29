#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd -P)
target=${1:?usage: verify.sh TARGET_TRIPLE}
case "$target" in
    aarch64-unknown-linux-gnu|aarch64-apple-darwin) arch=aarch64; elf_machine=183 ;;
    x86_64-unknown-linux-gnu) arch=x86_64; elf_machine=62 ;;
    *) echo "unsupported payload target: $target" >&2; exit 2 ;;
esac
manifest="$root/third_party/msb-payloads/manifest.toml"
field() { awk -v section="[targets.\"$target\"]" -v key="$1" '$0 == section {inside=1;next} /^\[/ {inside=0} inside && $1 == key {gsub(/"/, "", $3); print $3; exit}' "$manifest"; }
fail() { echo "payload verification failed ($target): $*" >&2; exit 3; }
[ "$(field status)" = verified ] || fail "manifest status is not verified"
arch_manifest=$(field architecture)
[ "$arch_manifest" = "$arch" ] || fail "manifest architecture '$arch_manifest' does not match target architecture '$arch'"
dest="$root/third_party/msb-payloads/$target"
kernel="$dest/kernel.bin"
agent="$root/third_party/msb-payloads/$arch/agentd"
meta="$dest/kernel.meta"
provenance="$dest/provenance.txt"
for f in "$kernel" "$agent" "$meta" "$provenance"; do [ -f "$f" ] || fail "missing $f"; done
hash_file() { if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }
kernel_hash=$(hash_file "$kernel")
agent_hash=$(hash_file "$agent")
[ "$kernel_hash" = "$(field kernel_sha256)" ] || fail "kernel hash differs from manifest"
[ "$agent_hash" = "$(field agent_sha256)" ] || fail "agent hash differs from manifest"
[ "$(wc -c < "$kernel" | tr -d ' ')" = "$(field size)" ] || fail "kernel size differs from manifest"
for key in size guest_address entry_address; do
    expected=$(field "$key")
    grep -qx "$key=$expected" "$meta" || fail "$key differs from manifest metadata"
done
grep -qx "target=$target" "$provenance" || fail "provenance target mismatch"
grep -qx "architecture=$arch" "$provenance" || fail "provenance architecture mismatch"
grep -qx "archive_sha256=$(field archive_sha256)" "$provenance" || fail "archive provenance mismatch"
grep -qx "microsandbox_revision=$(awk -F'"' '/^microsandbox_revision =/ {print $2; exit}' "$manifest")" "$provenance" || fail "source revision provenance mismatch"
grep -qx "kernel_sha256=$kernel_hash" "$provenance" || fail "kernel provenance hash mismatch"
grep -qx "agent_sha256=$agent_hash" "$provenance" || fail "agent provenance hash mismatch"
machine=$(od -An -tu2 -j18 -N2 "$agent" | tr -d ' ')
[ "$machine" = "$elf_machine" ] || fail "agent ELF e_machine=$machine, expected $elf_machine"
magic=$(od -An -tx1 -N4 "$agent" | tr -d ' \n')
[ "$magic" = 7f454c46 ] || fail "agent is not ELF"
size=$(field size); address=$(field guest_address); entry=$(field entry_address)
(( size > 0 && address > 0 && entry >= address && entry < address + size )) || fail "kernel address/entry bounds invalid"
alignment=$(field kernel_alignment)
(( alignment > 0 && (alignment & (alignment - 1)) == 0 && address % alignment == 0 )) || fail "kernel load address does not meet manifest alignment"
printf 'PASS payload %s arch=%s archive_sha256=%s kernel_sha256=%s agent_sha256=%s size=%s load=0x%x entry=0x%x\n' \
    "$target" "$arch" "$(field archive_sha256)" "$kernel_hash" "$agent_hash" "$size" "$address" "$entry"
