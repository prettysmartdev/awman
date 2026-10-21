#!/usr/bin/env bash
# Regression checks for stale caches and completeness of the source audit.
set -euo pipefail
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
repo=$(cd "$(dirname "$0")/.." && pwd)
scratch=$(mktemp -d /tmp/awman-input-regressions.XXXXXX)
trap 'rm -r -- "$scratch"' EXIT
fixture="$scratch/repo"
mkdir -p "$fixture/tools" "$fixture/third_party/msb-payloads"
cp "$repo/tools/reproducible-build-audit.sh" "$fixture/tools/"
files=(.cargo/config.toml Cargo.toml Cargo.lock build.rs rust-toolchain.toml
    crates/awman-cli/Cargo.toml crates/awman-cli/build.rs
    crates/awman-host-shims/Cargo.toml crates/awman-host-shims/build.rs
    third_party/msb-payloads/manifest.toml
    tools/msb-payloads/fetch.sh tools/msb-payloads/verify.sh tools/msb-payloads/extract-kernel.c
    .github/workflows/test.yml .github/workflows/release.yml)
for tree in microsandbox-0.7.8 microsandbox-network-0.7.8 microsandbox-types-0.7.8 microsandbox-filesystem-0.7.8 sqlx-sqlite-0.9.0; do
    for file in Cargo.toml upstream.diff PATCH.md; do files+=("third_party/$tree/$file"); done
done
for file in "${files[@]}"; do
    mkdir -p "$fixture/$(dirname "$file")"
    cp "$repo/$file" "$fixture/$file"
done
git -C "$fixture" init -q
git -C "$fixture" add .
git -C "$fixture" -c user.name=Fixture -c user.email=fixture@invalid commit -qm fixture
bash "$fixture/tools/reproducible-build-audit.sh" "$scratch/audit" >/dev/null
for tree in microsandbox microsandbox-network microsandbox-types microsandbox-filesystem; do
    for file in Cargo.toml upstream.diff PATCH.md; do
        path="third_party/$tree-0.7.8/$file"
        grep -Fq "$path" "$scratch/audit/input-sha256.txt"
        git -C "$fixture" rm -q --cached "$path"
        if bash "$fixture/tools/reproducible-build-audit.sh" "$scratch/rejected" > "$scratch/error" 2>&1; then
            echo "FAIL: audit accepted untracked $path" >&2; exit 1
        fi
        grep -Fq "required build input is not tracked: $path" "$scratch/error"
        git -C "$fixture" add "$path"
    done
done
printf 'path: /tmp/untrusted-cargo-input\n' > "$fixture/.github/workflows/scratch.yml"
if bash "$fixture/tools/reproducible-build-audit.sh" "$scratch/rejected" > "$scratch/error" 2>&1; then
    echo 'FAIL: audit accepted absolute scratch input in a workflow' >&2; exit 1
fi
grep -Fq 'absolute scratch/developer Cargo-cache input found' "$scratch/error"
rm "$fixture/.github/workflows/scratch.yml"
echo 'PASS audit hashes/requires all four patch trees and rejects workflow scratch inputs'
# An archive left over from an older version must tell the user what to remove.
target=$(rustc -vV | sed -n 's/^host: //p')
case "$target" in aarch64-apple-darwin|aarch64-unknown-linux-gnu|x86_64-unknown-linux-gnu) ;; *) exit 0;; esac
mkdir -p "$fixture/third_party/msb-payloads/$target"
cat > "$fixture/third_party/msb-payloads/manifest.toml" <<EOF
version = "0.7.8"
[targets."$target"]
archive = "stale.tar"
archive_sha256 = "wrong-hash"
firmware = "firmware"
agent = "agentd"
agent_sha256 = "wrong-agent"
EOF
archive="$fixture/third_party/msb-payloads/$target/stale.tar"
printf stale > "$archive"
if bash "$fixture/tools/msb-payloads/fetch.sh" "$target" > "$scratch/error" 2>&1; then exit 1; fi
grep -Fq "remove stale cache $archive and rerun" "$scratch/error"
echo 'PASS stale archive gives actionable cache path'

# Exercise the agent cache diagnostic after a valid archive/extraction.
printf firmware > "$scratch/firmware"
tar cf "$archive" -C "$scratch" firmware
expected=$(shasum -a 256 "$archive" | awk '{print $1}')
sed "s/wrong-hash/$expected/" "$fixture/third_party/msb-payloads/manifest.toml" > "$scratch/manifest.toml"
mv "$scratch/manifest.toml" "$fixture/third_party/msb-payloads/manifest.toml"
cat > "$scratch/extractor" <<'EXTRACTOR'
#!/usr/bin/env bash
printf fixture-kernel > "$2"
printf 'size=14\nguest_address=0\nentry_address=0\n'
EXTRACTOR
cat > "$scratch/cc" <<'COMPILER'
#!/usr/bin/env bash
while [ "$1" != -o ]; do shift; done
shift
cp "$EXTRACTOR_FIXTURE" "$1"
chmod +x "$1"
COMPILER
chmod +x "$scratch/cc" "$scratch/extractor"
agent="$fixture/third_party/msb-payloads/${target%%-*}/agentd"
mkdir -p "$(dirname "$agent")"
printf stale-agent > "$agent"
if CC="$scratch/cc" EXTRACTOR_FIXTURE="$scratch/extractor" bash "$fixture/tools/msb-payloads/fetch.sh" "$target" > "$scratch/error" 2>&1; then exit 1; fi
grep -Fq "remove stale cache $agent and rerun" "$scratch/error"
echo 'PASS stale agent gives actionable cache path'
