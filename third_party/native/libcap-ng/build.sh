#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")" && pwd -P)
triple=${1:-$(uname -m)-unknown-linux-gnu}
case "$triple" in aarch64-unknown-linux-gnu|x86_64-unknown-linux-gnu) ;; *) echo "unsupported native target: $triple" >&2; exit 2;; esac
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != "${triple%%-*}" ]; then
    echo "native Linux host matching $triple required" >&2
    exit 2
fi
archive="$root/libcap-ng-0.8.3.tar.gz"
if [ ! -f "$archive" ]; then
    curl -fsSL --retry 3 https://people.redhat.com/sgrubb/libcap-ng/libcap-ng-0.8.3.tar.gz -o "$archive"
fi
printf '%s  %s\n' bed6f6848e22bb2f83b5f764b2aef0ed393054e803a8e3a8711cb2a39e6b492d "$archive" | sha256sum -c -
work=$(mktemp -d "$root/build.XXXXXX")
trap 'rm -rf "$work"' EXIT
tar xf "$archive" -C "$work"
(cd "$work/libcap-ng-0.8.3" && ./configure --disable-shared --enable-static --without-python && make -j2)
mkdir -p "$root/lib" "$root/$triple"
cp "$work/libcap-ng-0.8.3/src/.libs/libcap-ng.a" "$root/$triple/libcap-ng.a"
cp "$root/$triple/libcap-ng.a" "$root/lib/libcap-ng.a"
sha256sum "$root/$triple/libcap-ng.a"
