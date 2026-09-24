#!/usr/bin/env bash
set -euo pipefail

msb=${1:?Usage: host-checks.sh MSB SMOLVM FIXTURES NEW_OUTPUT_DIRECTORY [CRANE]}
smolvm=${2:?smolvm binary required}
fixtures=$(cd "${3:?fixtures directory required}" && pwd)
output=${4:?new output directory required}
crane=${5:-}
mkdir "$output"
output=$(cd "$output" && pwd)
mkdir "$output/msb-state" "$output/smolvm-state" "$output/tmp"
printf 'case\texit_code\n' > "$output/results.tsv"

record() {
    local name=$1 status=0
    shift
    "$@" > "$output/$name.stdout" 2> "$output/$name.stderr" || status=$?
    printf '%s\t%s\n' "$name" "$status" | tee -a "$output/results.tsv"
}

msb_run() {
    env -i PATH=/usr/bin:/bin TMPDIR="$output/tmp" MSB_HOME="$output/msb-state" MSB_BACKEND=local "$msb" "$@"
}

smol_run() {
    env -i PATH=/usr/bin:/bin TMPDIR="$output/tmp" SMOLVM_DATA_DIR="$output/smolvm-state" "$smolvm" "$@"
}

record platform uname -a
record msb-version msb_run --version
record smolvm-version smol_run --version
record msb-doctor msb_run doctor
record msb-import-docker msb_run image load -i "$fixtures/fixture-docker.tar" -t awman-spike/docker:latest
record msb-import-oci msb_run image load -i "$fixtures/fixture-oci.tar" -t awman-spike/oci:latest
record msb-inspect msb_run image inspect --format json awman-spike/oci:latest
record msb-export msb_run image save --format oci -o "$output/roundtrip-oci.tar" awman-spike/oci:latest
record smolvm-file-mount smol_run machine create -n file-probe -I "$fixtures/fixture-docker.tar" --cpus 1 --mem 512 --storage 1 --overlay 1 -v "$fixtures/expected-config.json:/single:ro"
record smolvm-directory-mount smol_run machine create -n directory-probe -I "$fixtures/fixture-docker.tar" --cpus 1 --mem 512 --storage 1 --overlay 1 -v "$fixtures:/fixtures:ro"
record msb-fractional-cpu msb_run create -n cpu-probe --cpus 0.5 awman-spike/oci:latest
record smolvm-fractional-cpu smol_run machine create -n cpu-probe --cpus 0.5 -I "$fixtures/fixture-docker.tar"

if [ -n "$crane" ]; then
    record crane-version env -i PATH=/usr/bin:/bin "$crane" version
    record crane-docker-flatten env -i PATH=/usr/bin:/bin TMPDIR="$output/tmp" "$crane" export - - < "$fixtures/fixture-docker.tar"
    record crane-oci-flatten env -i PATH=/usr/bin:/bin TMPDIR="$output/tmp" "$crane" export - - < "$fixtures/fixture-oci.tar"
    record crane-flat-list tar tf "$output/crane-docker-flatten.stdout"
    record crane-retained-opaque-file tar xOf "$output/crane-docker-flatten.stdout" compat/opaque/old
fi

printf '%s\n' 'Exit codes are observations, not pass/fail verdicts. No VM was started.'
