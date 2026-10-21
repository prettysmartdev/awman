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
  crates/awman-cli/Cargo.toml crates/awman-cli/build.rs \
  crates/awman-host-shims/Cargo.toml crates/awman-host-shims/build.rs \
  third_party tools/msb-payloads tools/oci-runtime-spike/sqlite-resolution .github/workflows \
  > "$out/tracked-build-inputs.txt"
for f in .cargo/config.toml Cargo.toml Cargo.lock build.rs rust-toolchain.toml \
  crates/awman-cli/Cargo.toml crates/awman-cli/build.rs \
  crates/awman-host-shims/Cargo.toml crates/awman-host-shims/build.rs \
  third_party/msb-payloads/manifest.toml \
  third_party/microsandbox-0.7.8/Cargo.toml \
  third_party/microsandbox-0.7.8/upstream.diff \
  third_party/microsandbox-0.7.8/PATCH.md \
  third_party/microsandbox-network-0.7.8/Cargo.toml \
  third_party/microsandbox-network-0.7.8/upstream.diff \
  third_party/microsandbox-network-0.7.8/PATCH.md \
  third_party/microsandbox-types-0.7.8/Cargo.toml \
  third_party/microsandbox-types-0.7.8/upstream.diff \
  third_party/microsandbox-types-0.7.8/PATCH.md \
  third_party/microsandbox-filesystem-0.7.8/Cargo.toml \
  third_party/microsandbox-filesystem-0.7.8/upstream.diff \
  third_party/microsandbox-filesystem-0.7.8/PATCH.md \
  third_party/sqlx-sqlite-0.9.0/Cargo.toml third_party/sqlx-sqlite-0.9.0/PATCH.md \
  third_party/sqlx-sqlite-0.9.0/upstream.diff \
  tools/msb-payloads/fetch.sh tools/msb-payloads/verify.sh tools/msb-payloads/extract-kernel.c \
  .github/workflows/test.yml .github/workflows/release.yml; do
  git ls-files --error-unmatch "$f" >/dev/null || { echo "required build input is not tracked: $f" >&2; exit 1; }
done
if grep -rnE '(/tmp/|/private/tmp/|/Users/[^/]+/\.cargo|\.cargo/registry/src)' \
  Cargo.toml Cargo.lock build.rs .cargo/config.toml .github/workflows \
  crates/awman-cli/Cargo.toml crates/awman-cli/build.rs \
  crates/awman-host-shims/Cargo.toml crates/awman-host-shims/build.rs; then
  echo "absolute scratch/developer Cargo-cache input found" >&2; exit 1
fi
git diff --check > "$out/diff-check.txt"
if command -v sha256sum >/dev/null; then hasher=(sha256sum); else hasher=(shasum -a 256); fi
"${hasher[@]}" Cargo.toml Cargo.lock build.rs .cargo/config.toml \
  crates/awman-cli/Cargo.toml crates/awman-cli/build.rs \
  crates/awman-host-shims/Cargo.toml crates/awman-host-shims/build.rs \
  third_party/msb-payloads/manifest.toml \
  third_party/microsandbox-0.7.8/Cargo.toml \
  third_party/microsandbox-0.7.8/upstream.diff \
  third_party/microsandbox-0.7.8/PATCH.md \
  third_party/microsandbox-network-0.7.8/Cargo.toml \
  third_party/microsandbox-network-0.7.8/upstream.diff \
  third_party/microsandbox-network-0.7.8/PATCH.md \
  third_party/microsandbox-types-0.7.8/Cargo.toml \
  third_party/microsandbox-types-0.7.8/upstream.diff \
  third_party/microsandbox-types-0.7.8/PATCH.md \
  third_party/microsandbox-filesystem-0.7.8/Cargo.toml \
  third_party/microsandbox-filesystem-0.7.8/upstream.diff \
  third_party/microsandbox-filesystem-0.7.8/PATCH.md \
  > "$out/input-sha256.txt"
printf 'PASS reproducible build input audit at %s\n' "$out"
