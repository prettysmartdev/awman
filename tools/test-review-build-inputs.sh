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
# fetch.sh discards a cached download whose hash no longer matches the
# manifest (one left over from an older pinned version) and downloads it
# again; a fresh download that still mismatches is an error. A fake curl on
# PATH serves the "release assets" from $scratch/assets, so no network is used.
target=$(rustc -vV | sed -n 's/^host: //p')
case "$target" in aarch64-apple-darwin|aarch64-unknown-linux-gnu|x86_64-unknown-linux-gnu) ;; *) exit 0;; esac
hash_file() { if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }
mkdir -p "$scratch/bin" "$scratch/assets" "$fixture/third_party/msb-payloads/$target"
cat > "$scratch/bin/curl" <<'CURL'
#!/usr/bin/env bash
url= out=
while [ $# -gt 0 ]; do case "$1" in -o) out=$2; shift 2 ;; -*) shift ;; *) url=$1; shift ;; esac; done
echo "curl $url" >> "$CURL_LOG"
cp "$ASSETS/${url##*/}" "$out"
CURL
cat > "$scratch/extractor" <<'EXTRACTOR'
#!/usr/bin/env bash
printf fixture-kernel > "$2"
printf 'size=14\nguest_address=0\nentry_address=0\n'
EXTRACTOR
cat > "$scratch/cc" <<'COMPILER'
#!/usr/bin/env bash
[ "$1" != --version ] || { echo fake-cc 1.0; exit 0; }
while [ "$1" != -o ]; do shift; done
shift
cp "$EXTRACTOR_FIXTURE" "$1"
chmod +x "$1"
COMPILER
chmod +x "$scratch/bin/curl" "$scratch/cc" "$scratch/extractor"
printf firmware > "$scratch/firmware"
tar cf "$scratch/assets/stale.tar" -C "$scratch" firmware
printf good-agent > "$scratch/assets/agentd"
cat > "$fixture/third_party/msb-payloads/manifest.toml" <<MANIFEST
version = "0.7.8"
[targets."$target"]
archive = "stale.tar"
archive_sha256 = "$(hash_file "$scratch/assets/stale.tar")"
firmware = "firmware"
agent = "agentd"
agent_sha256 = "$(hash_file "$scratch/assets/agentd")"
MANIFEST
archive="$fixture/third_party/msb-payloads/$target/stale.tar"
agent="$fixture/third_party/msb-payloads/${target%%-*}/agentd"
mkdir -p "$(dirname "$agent")"
fetch() { # expected-status
    local status=0
    : > "$scratch/curl.log"
    PATH="$scratch/bin:$PATH" CURL_LOG="$scratch/curl.log" ASSETS="$scratch/assets" \
        CC="$scratch/cc" EXTRACTOR_FIXTURE="$scratch/extractor" \
        bash "$fixture/tools/msb-payloads/fetch.sh" "$target" > "$scratch/out" 2>&1 || status=$?
    [ "$status" = "$1" ] || { cat "$scratch/out" >&2; echo "FAIL: fetch.sh exited $status, expected $1" >&2; exit 1; }
}
url=https://github.com/superradcompany/microsandbox/releases/download/v0.7.8

# A stale cached archive and agent are both replaced by fresh downloads.
# The manifest is not `verified`, so a complete fetch ends with exit 4.
printf stale > "$archive"
printf stale-agent > "$agent"
fetch 4
grep -Fq "discarding stale cached stale.tar: $archive" "$scratch/out"
grep -Fq "discarding stale cached agentd: $agent" "$scratch/out"
grep -Fxq "curl $url/stale.tar" "$scratch/curl.log"
grep -Fxq "curl $url/agentd" "$scratch/curl.log"
cmp -s "$archive" "$scratch/assets/stale.tar"
cmp -s "$agent" "$scratch/assets/agentd"
grep -Fq 'Payload evidence was recorded' "$scratch/out"
echo 'PASS stale cached archive and agent are discarded and downloaded again'

# Matching cached files are reused without a download.
fetch 4
[ ! -s "$scratch/curl.log" ] || { echo 'FAIL: fetch.sh downloaded files it had cached' >&2; exit 1; }
if grep -Fq discarding "$scratch/out"; then echo 'FAIL: fetch.sh discarded a matching cache' >&2; exit 1; fi
echo 'PASS matching cached downloads are reused'

# A fresh download that still mismatches is an error, not a retry loop.
printf stale > "$archive"
printf tampered > "$scratch/assets/stale.tar"
fetch 3
grep -Fq "discarding stale cached stale.tar: $archive" "$scratch/out"
grep -Fq "stale.tar hash mismatch" "$scratch/out"
echo 'PASS a mismatched fresh archive download is refused'

tar cf "$scratch/assets/stale.tar" -C "$scratch" firmware
printf tampered > "$scratch/assets/agentd"
printf stale-agent > "$agent"
fetch 3
grep -Fq "discarding stale cached agentd: $agent" "$scratch/out"
grep -Fq "agentd hash mismatch" "$scratch/out"
echo 'PASS a mismatched fresh agent download is refused'
