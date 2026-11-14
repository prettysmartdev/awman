#!/usr/bin/env bash
# Explicit setup for minimal Debian containers without root access. Run through
# web-test-env.sh so setup and the gate resolve exactly the same tool cache.
set -euo pipefail
cd "$(dirname "$0")/.."
cache="$PWD/target/web-test-tools"

npm --prefix web ci --ignore-scripts
npm --prefix web/edge ci --ignore-scripts
npm --prefix web/e2e ci --ignore-scripts

export PLAYWRIGHT_BROWSERS_PATH="${PLAYWRIGHT_BROWSERS_PATH:-$cache/browsers}"
npm --prefix web/e2e exec -- playwright install chromium firefox

if [[ ${1:-} != --portable-linux ]]; then
    echo 'Browsers installed. System libraries must be installed in the image (playwright install --with-deps).'
    echo 'For a rootless Debian 12 container, use: make web/test-setup WEB_TEST_SETUP_ARGS=--portable-linux'
    exit 0
fi

# APT verifies repository metadata and package hashes as usual. Download and
# unpack into the checkout; do not install packages or run maintainer scripts.
source /etc/os-release
[[ "$ID:$VERSION_ID" == debian:12 ]] || { echo 'Portable browser libraries require Debian 12.' >&2; exit 1; }
runtime="$cache/linux"
mkdir -p "$runtime/lists/partial" "$runtime/archives/partial" "$runtime/root"
apt_options=(-o "Dir::State::lists=$runtime/lists" -o "Dir::Cache::archives=$runtime/archives" -o Debug::NoLocking=1)
apt-get "${apt_options[@]}" update
# Use the dependency list shipped by the pinned Playwright package, keeping the
# portable runtime aligned with the same browsers the gate actually executes.
mapfile -t packages < <(node <<'NODE'
const { deps } = require('./web/e2e/node_modules/playwright-core/lib/server/registry/nativeDeps.js');
const arch = process.arch === 'arm64' ? 'arm64' : 'x64';
const selected = deps[`debian12-${arch}`];
if (!selected) throw new Error('Unsupported Playwright platform');
console.log([...new Set([...selected.chromium, ...selected.firefox, 'fonts-liberation', 'fonts-noto-color-emoji'])].join('\n'));
NODE
)
[[ ${#packages[@]} -gt 0 ]]
apt-get "${apt_options[@]}" --download-only --no-install-recommends -y install "${packages[@]}"
for package in "$runtime/archives/"*.deb; do
    dpkg-deb -x "$package" "$runtime/root"
done
cat > "$runtime/fonts.conf" <<'XML'
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig>
  <dir prefix="relative">root/usr/share/fonts</dir>
  <cachedir prefix="relative">font-cache</cachedir>
</fontconfig>
XML
