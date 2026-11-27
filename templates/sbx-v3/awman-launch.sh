#!/bin/bash
set -euo pipefail
refuse() {
    printf '%s\n' 'awman-launch: no verified awman policy for this launch; start this sandbox through awman' >&2
    exit 78
}
[[ -n ${AWMAN_LAUNCH_ID:-} && -f /var/lib/awman/receipt && -f /usr/local/lib/awman/kit-id ]] || refuse
launch='' kit=''
while IFS=' ' read -r key value; do
    case $key in
        launch) launch=$value;;
        kit) kit=$value;;
    esac
done < /var/lib/awman/receipt
[[ $launch == "$AWMAN_LAUNCH_ID" && $kit == "$(</usr/local/lib/awman/kit-id)" ]] || refuse
exec "$@"
