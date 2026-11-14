#!/usr/bin/env bash
# Copy a user-built executable, escalating only the copy when needed.
set -euo pipefail
target_dir=${1:?usage: install.sh TARGET_DIR INSTALL_PATH}
install_path=${2:?usage: install.sh TARGET_DIR INSTALL_PATH}
root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"
binary="$target_dir/release/awman"
destination="$install_path/awman"
if [ -w "$install_path" ] && { [ ! -e "$destination" ] || [ -w "$destination" ]; }; then
    install -m 755 "$binary" "$destination"
else
    sudo install -m 755 "$binary" "$destination"
fi
if [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ]; then
    bash tools/macos-sign.sh --verify "$destination"
fi
