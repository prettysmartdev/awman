#!/usr/bin/env bash
# Build the real-image corpus consumed by `real_corpus_*` tests.
#
# Usage: tests/oci_import/corpus/build-corpus.sh <out-dir> [<git-root>]
#
# For every shipped agent template (`templates/Dockerfile.<agent>`) this
# builds the project image and the agent image with Docker for the host
# platform, then exports each agent image three ways:
#
#   <agent>.docker-save.tar   docker save
#   <agent>.oci.tar           docker buildx build -o type=oci (buildx required)
#   <agent>.apple.tar         container image save   (macOS with Apple Containers only)
#
# and writes `manifest.json` with the exporter's config digest for each
# archive. The tests never build anything; they only read this directory.
# This script is developer tooling: it runs docker/buildx/container on the
# host, which awman itself never does.
set -euo pipefail

out="${1:?usage: build-corpus.sh <out-dir> [<git-root>]}"
root="${2:-$(git rev-parse --show-toplevel)}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
# Never leave an earlier complete inventory after a failed rebuild.
rm -f "$out/manifest.json"
cd "$root"

command -v docker >/dev/null || { echo 'BLOCKED: Docker is required' >&2; exit 2; }
docker buildx version >/dev/null || { echo 'BLOCKED: buildx OCI export is required' >&2; exit 2; }
if [ "$(uname -s)" = Darwin ]; then
    command -v container >/dev/null || { echo 'BLOCKED: Apple container export is required on macOS' >&2; exit 2; }
fi

arch="$(uname -m)"
case "$arch" in
    x86_64) platform="linux/amd64" ;;
    aarch64 | arm64) platform="linux/arm64" ;;
    *) echo "unsupported host architecture $arch" >&2; exit 1 ;;
esac

project_tag="awman-corpus-project:latest"
docker build --platform "$platform" -t "$project_tag" -f templates/Dockerfile.project . >&2

entries=()
add_entry() {
    # template format archive reference platform config_digest
    [[ "$6" =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "invalid config digest for $1/$2" >&2; exit 1; }
    entries+=("{\"template\":\"$1\",\"format\":\"$2\",\"archive\":\"$3\",\"reference\":\"$4\",\"platform\":\"$5\",\"config_digest\":\"$6\"}")
}

for dockerfile in templates/Dockerfile.*; do
    agent="${dockerfile#templates/Dockerfile.}"
    [ "$agent" = "project" ] && continue
    tag="awman-corpus-${agent}:latest"
    # Shipped templates carry the {{AWMAN_BASE_IMAGE}} placeholder that
    # `awman ready` substitutes; do the same substitution here.
    rendered="$out/Dockerfile.$agent"
    sed "s|{{AWMAN_BASE_IMAGE}}|$project_tag|g" "$dockerfile" > "$rendered"
    docker build --platform "$platform" -t "$tag" -f "$rendered" . >&2
    config="$(docker image inspect --format '{{.Id}}' "$tag")"

    docker save "$tag" -o "$out/$agent.docker-save.tar"
    add_entry "$agent" docker-save "$agent.docker-save.tar" "$tag" "$platform" "$config"

    docker buildx build --platform "$platform" --provenance=false \
        -f "$rendered" -o "type=oci,dest=$out/$agent.oci.tar,name=$tag" . >&2
    oci_config="$(tar -xOf "$out/$agent.oci.tar" index.json | \
        perl -0777 -ne 'print $1 if /"digest"\s*:\s*"(sha256:[0-9a-f]{64})"/' | head -1)"
    # The exporter records the manifest digest in the index; the config
    # digest is read from the selected manifest.
    oci_config="$(tar -xOf "$out/$agent.oci.tar" "blobs/sha256/${oci_config#sha256:}" | \
        perl -0777 -ne 'print $1 if /"config"\s*:\s*\{[^}]*"digest"\s*:\s*"(sha256:[0-9a-f]{64})"/')"
    add_entry "$agent" oci-layout "$agent.oci.tar" "$tag" "$platform" "$oci_config"

    if [ "$(uname -s)" = "Darwin" ]; then
        container image load -i "$out/$agent.docker-save.tar" >&2
        container image save "$tag" --platform "$platform" -o "$out/$agent.apple.tar" >&2
        add_entry "$agent" apple-export "$agent.apple.tar" "$tag" "$platform" "$config"
    fi
done

{
    echo '{"entries":['
    first=1
    for e in "${entries[@]}"; do
        [ $first = 1 ] || echo ','
        first=0
        printf '%s' "$e"
    done
    echo ']}'
} > "$out/manifest.json.new"
mv "$out/manifest.json.new" "$out/manifest.json"
echo "corpus written to $out" >&2
