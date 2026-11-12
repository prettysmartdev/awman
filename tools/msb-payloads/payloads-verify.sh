#!/usr/bin/env bash
# Verify published payload bytes against the reviewed manifest. Changing a
# payload's verification status requires reviewed native acceptance evidence;
# this command never promotes an unverified record automatically.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd -P)
cd "$root"
target=${1:?usage: payloads-verify.sh TARGET_TRIPLE}
bash tools/msb-payloads/fetch.sh "$target"
bash tools/msb-payloads/verify.sh "$target"
printf '%s\n' 'Payload bytes verified. Native behavior is certified separately by the manifest guest/artifact suites.'
