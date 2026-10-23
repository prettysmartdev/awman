# Guest fixtures for the builtin hardware tests. Sourced (not run) by
# tools/test-builtin-hw.sh and tools/msb-payloads/payloads-verify.sh.
#
# prepare_builtin_hw_fixtures DIR exports AWMAN_TEST_BUILTIN_BASE_TAR and
# AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE, using the caller's values when set.
# Otherwise skopeo makes the docker-archive alpine base under the local
# container engine (Docker preferred, else Apple Containers), with only DIR
# mounted, so nobody installs skopeo; tools/oci-runtime-spike/fixture then
# builds the promoted fixture from it. Both are kept in DIR and reused: the
# fixture until its builder's sources change. On macOS the fixture is also
# loaded into Apple Containers (starting its service if needed) and the
# Apple-store test inputs are exported; release_builtin_hw_fixtures removes
# that image again.

builtin_hw_skopeo_image=quay.io/skopeo/stable:v1.22.3-immutable@sha256:c0ee1f4edca5c01cb8d5611124f92f3cc47196ecab68aee5d0f90834e00574d5
builtin_hw_apple_ref=

builtin_hw_blocked() { echo "BLOCKED: $*" >&2; exit 2; }

builtin_hw_sha256() {
    if command -v sha256sum >/dev/null; then sha256sum | awk '{print $1}'; else shasum -a 256 | awk '{print $1}'; fi
}

prepare_builtin_hw_fixtures() {
    local dir=$1 repo base_tar fixture engine key build
    repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
    mkdir -p "$dir"
    dir=$(cd "$dir" && pwd -P)
    base_tar=${AWMAN_TEST_BUILTIN_BASE_TAR:-}
    fixture=${AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE:-}
    if [ -z "$base_tar" ]; then
        base_tar=$dir/base.tar
        if [ ! -s "$base_tar" ]; then
            if docker info >/dev/null 2>&1; then engine=(docker run --rm --user "$(id -u):$(id -g)")
            elif container system status >/dev/null 2>&1; then engine=(container run --rm)
            else builtin_hw_blocked "start Docker or Apple Containers to build the guest fixture base (or set AWMAN_TEST_BUILTIN_BASE_TAR)"
            fi
            echo "using ${engine[0]} to run skopeo" >&2
            rm -f "$dir/base.tar.partial"
            "${engine[@]}" --volume "$dir:/out" "$builtin_hw_skopeo_image" \
                copy --override-os linux --format v2s2 docker://docker.io/library/alpine:3.22 docker-archive:/out/base.tar.partial
            mv "$dir/base.tar.partial" "$base_tar"
        fi
    fi
    if [ -z "$fixture" ]; then
        fixture=$dir/fixture-oci.tar
        # Rebuild when the base or the fixture builder changes.
        key=$( (cat "$base_tar"; find "$repo/tools/oci-runtime-spike/fixture" -type f -not -path '*/target/*' | LC_ALL=C sort | xargs cat) | builtin_hw_sha256)
        if [ ! -s "$fixture" ] || [ "$(cat "$dir/fixture.key" 2>/dev/null)" != "$key" ]; then
            build=$(mktemp -d "$dir/build.XXXXXX")
            cargo run --locked --quiet --manifest-path "$repo/tools/oci-runtime-spike/fixture/Cargo.toml" -- "$base_tar" "$build"
            mv "$build/fixture-oci.tar" "$fixture"
            rm -rf "$build"
            printf '%s' "$key" > "$dir/fixture.key"
        fi
    fi
    [ -f "$base_tar" ] && [ -f "$fixture" ] || builtin_hw_blocked "guest fixture archives missing"
    export AWMAN_TEST_BUILTIN_BASE_TAR=$base_tar AWMAN_TEST_BUILTIN_FIXTURE_ARCHIVE=$fixture
    if [ "$(uname -s)" = Darwin ]; then
        # The Apple-store guest tests import the fixture from a running Apple
        # Containers service (and stop and restart that service themselves).
        local apple_cli
        apple_cli=$(command -v container) || builtin_hw_blocked "install Apple Containers (https://github.com/apple/container/releases)"
        container system status >/dev/null 2>&1 || container system start
        container image load --input "$fixture"
        container image inspect awman-spike/fixture:latest >/dev/null ||
            builtin_hw_blocked "Apple Containers did not load the fixture as awman-spike/fixture:latest"
        builtin_hw_apple_ref=awman-spike/fixture:latest
        export AWMAN_TEST_APPLE_CONTAINER=1 AWMAN_TEST_APPLE_CLI=$apple_cli
        export AWMAN_TEST_APPLE_FIXTURE_REF=$builtin_hw_apple_ref AWMAN_TEST_APPLE_IMAGE=$builtin_hw_apple_ref
    fi
}

release_builtin_hw_fixtures() {
    if [ -n "$builtin_hw_apple_ref" ]; then
        container image delete "$builtin_hw_apple_ref" >/dev/null 2>&1 || true
        builtin_hw_apple_ref=
    fi
}
