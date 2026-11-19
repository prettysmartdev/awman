#!/usr/bin/env bash
# Copy a user-built executable, escalating only the copy when needed.
set -euo pipefail
target_dir=${1:?usage: install.sh TARGET_DIR INSTALL_PATH}
install_path=${2:?usage: install.sh TARGET_DIR INSTALL_PATH}
root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"
binary="$target_dir/release/awman"
destination="$install_path/awman"
# Atomic replacement also works when a Linux worker is executing the previous
# binary (overwriting an executing inode would fail with ETXTBSY).
install_binary() (
    local binary=$1 install_path=$2 destination=$3 staged
    mkdir -p "$install_path"
    staged=$(mktemp "$install_path/.awman-install.XXXXXX")
    trap 'rm -f "$staged"' EXIT
    install -m 755 "$binary" "$staged"
    mv -f "$staged" "$destination"
    trap - EXIT
)
parent=$install_path
while [ ! -e "$parent" ]; do parent=$(dirname "$parent"); done
if [ -d "$parent" ] && [ -w "$parent" ]; then
    install_binary "$binary" "$install_path" "$destination"
else
    # Escalate only this copy, never Cargo or dependency build scripts.
    command -v sudo >/dev/null || {
        echo "Cannot write $install_path and sudo is unavailable; choose a writable INSTALL_PATH" >&2
        exit 1
    }
    sudo bash -c "set -euo pipefail; $(declare -f install_binary);"' install_binary "$@"' \
        -- "$binary" "$install_path" "$destination"
fi
if [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ]; then
    bash tools/macos-sign.sh --verify "$destination"
fi
