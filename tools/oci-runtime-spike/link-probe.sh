#!/usr/bin/env bash
set -euo pipefail

candidate=${1:?Usage: link-probe.sh msb-or-smolvm source-directory output-directory}
source_dir=$(cd "${2:?source directory required}" && pwd)
output_dir=${3:?output directory required}
mkdir -p "$output_dir"
output_dir=$(cd "$output_dir" && pwd)
case "$candidate" in
    msb) dependency="microsandbox = { path = \"$source_dir/sdk/rust\", default-features = false, features = [\"local\", \"net\"] }" ;;
    smolvm) dependency="smolvm = { path = \"$source_dir\" }" ;;
    *) exit 2 ;;
esac
printf '%s\n' '[package]' 'name = "awman-runtime-link-probe"' 'version = "0.0.0"' 'edition = "2021"' '[workspace]' '[lib]' 'path = "lib.rs"' '[dependencies]' 'rusqlite = { version = "=0.40.0", features = ["bundled"] }' "$dependency" > "$output_dir/Cargo.toml"
printf '%s\n' 'pub fn probe() {}' > "$output_dir/lib.rs"
cargo generate-lockfile --manifest-path "$output_dir/Cargo.toml" > "$output_dir/resolution.log" 2>&1
