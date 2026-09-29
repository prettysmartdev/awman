#!/usr/bin/env bash
# Record the source/build-input facts that can be established from a checkout.
# Never treats an unavailable Git object database as an empty/clean diff.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd -P)
out=${1:-"$root/reproducible-build-audit"}
mkdir -p "$out"
out=$(cd "$out" && pwd -P)
cd "$root"
git rev-parse --show-toplevel > "$out/git-root.txt"
git rev-parse HEAD > "$out/head.txt"
git status --short --untracked-files=all > "$out/status.txt"
git ls-files --stage .cargo/config.toml Cargo.toml Cargo.lock build.rs rust-toolchain.toml \
  third_party tools/msb-payloads tools/oci-runtime-spike/sqlite-resolution .github/workflows \
  > "$out/tracked-build-inputs.txt"
for f in .cargo/config.toml Cargo.toml Cargo.lock build.rs rust-toolchain.toml \
  third_party/msb-payloads/manifest.toml third_party/msb_krun-0.1.39/Cargo.toml \
  third_party/microsandbox-filesystem-0.7.2/Cargo.toml \
  third_party/sqlx-sqlite-0.9.0/Cargo.toml third_party/sqlx-sqlite-0.9.0/PATCH.md \
  third_party/sqlx-sqlite-0.9.0/upstream.diff third_party/native/libcap-ng/build.sh \
  tools/msb-payloads/fetch.sh tools/msb-payloads/verify.sh tools/msb-payloads/extract-kernel.c \
  .github/workflows/test.yml .github/workflows/release.yml; do
  git ls-files --error-unmatch "$f" >/dev/null || { echo "required build input is not tracked: $f" >&2; exit 1; }
done
if grep -nE '(/tmp/|/private/tmp/|/Users/[^/]+/\.cargo|\.cargo/registry/src)' \
  Cargo.toml Cargo.lock build.rs .cargo/config.toml .github/workflows; then
  echo "absolute scratch/developer Cargo-cache input found" >&2; exit 1
fi
git diff --check > "$out/diff-check.txt"
if command -v sha256sum >/dev/null; then hasher=(sha256sum); else hasher=(shasum -a 256); fi
"${hasher[@]}" Cargo.toml Cargo.lock build.rs .cargo/config.toml third_party/msb-payloads/manifest.toml \
  > "$out/input-sha256.txt"
printf 'PASS reproducible build input audit at %s\n' "$out"
