#!/usr/bin/env bash
# A checkout-local tool cache survives the fresh HOME used by workflow teardown.
set -euo pipefail
cd "$(dirname "$0")/.."
repo=$PWD
cache="$repo/target/web-test-tools"

if ! command -v npm >/dev/null || ! command -v node >/dev/null; then
    # Match Dockerfile.dev. Bootstrap only the toolchain, never application code.
    node_version=22.23.5
    case "$(uname -s):$(uname -m)" in
        Linux:x86_64)
            node_arch=x64
            node_sha=cdeb8670a0c81f51b4dc4ac61919f9b1ad917b304bf8221eb939570663d240f8 ;;
        Linux:aarch64)
            node_arch=arm64
            node_sha=2babb9eb5264860d540849f4b064a6fbdebcdc323657d9d382ce23d313fd5e09 ;;
        *) echo 'Install Node.js >=22.13 and npm before running the web gate.' >&2; exit 1 ;;
    esac
    if [[ ! -x "$cache/node/bin/node" || ! -f "$cache/node/bin/npm" ]]; then
        mkdir -p "$cache"
        staging=$(mktemp -d "$cache/node-install.XXXXXX")
        trap 'rm -rf "$staging"' EXIT
        curl -fsSL --retry 3 "https://nodejs.org/dist/v$node_version/node-v$node_version-linux-$node_arch.tar.xz" -o "$staging/node.tar.xz"
        echo "$node_sha  $staging/node.tar.xz" | sha256sum -c -
        mkdir "$staging/node"
        tar -xJf "$staging/node.tar.xz" --strip-components=1 --no-same-owner -C "$staging/node"
        rm -rf "$cache/node"
        mv "$staging/node" "$cache/node"
        rm -rf "$staging"
        trap - EXIT
    fi
    export PATH="$cache/node/bin:$PATH"
fi

# The lockfiles are produced by npm 11 (also pinned in CI). npm 10 rejects
# their platform-specific optional dependencies instead of selecting this host.
if [[ $(npm --version) == 10.* ]]; then
    if [[ ! -f "$cache/npm/node_modules/npm/bin/npm-cli.js" ]]; then
        npm install --prefix "$cache/npm" --ignore-scripts --no-audit --no-fund npm@11.17.0
    fi
    export PATH="$cache/npm/node_modules/.bin:$PATH"
fi

# Some agent shells reset PATH even though the image defines CARGO_HOME.
if ! command -v cargo >/dev/null && [[ -x "${CARGO_HOME:-$HOME/.cargo}/bin/cargo" ]]; then
    export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
fi

if [[ -d "$cache/browsers" ]]; then
    export PLAYWRIGHT_BROWSERS_PATH="${PLAYWRIGHT_BROWSERS_PATH:-$cache/browsers}"
fi

# Optional rootless Debian browser runtime, prepared by web/test-setup. Normal
# development images use their installed system libraries and shared browsers.
if [[ -f "$cache/linux/fonts.conf" ]]; then
    multiarch=$(dpkg-architecture -qDEB_HOST_MULTIARCH)
    export LD_LIBRARY_PATH="$cache/linux/root/usr/lib/$multiarch:$cache/linux/root/lib/$multiarch${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    export FONTCONFIG_FILE="$cache/linux/fonts.conf"
fi

exec "$@"
