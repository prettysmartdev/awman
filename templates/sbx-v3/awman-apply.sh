#!/bin/bash
# Synchronous policy delivery. No agent process may start before its receipt.
set -euo pipefail
fail() { printf 'awman-apply: %s\n' "$2" >&2; exit "$1"; }
[[ $EUID == 0 ]] || fail 77 'must run as root'
for helper in base64 sha256sum install mv rm runuser id mktemp dirname chmod; do
    command -v "$helper" >/dev/null || fail 69 "required helper $helper missing"
done
[[ $(id -u agent) == 1000 ]] || fail 69 'agent must have uid 1000'

declare -A paths owners hashes bodies
while IFS='|' read -r name path owner; do
    [[ -n $name ]] || continue
    paths[$name]=$path
    owners[$name]=$owner
done <<'AWMAN_MANAGED_FILES'
{{AWMAN_MANAGED_FILES}}
AWMAN_MANAGED_FILES

token() { [[ $1 =~ ^[a-zA-Z0-9_-]+$ ]]; }
digest() { [[ $1 =~ ^[a-f0-9]{64}$ ]]; }
step=0 count=0 ended=0 launch='' kit=''
while IFS= read -r line || [[ -n $line ]]; do
    [[ $ended == 0 ]] || fail 64 'input after end'
    case $step in
        0) [[ $line == 'AWMAN-APPLY 2' ]] || fail 64 'unsupported protocol';;
        1) launch=${line#launch }; [[ $line == "launch $launch" ]] && token "$launch" || fail 64 'invalid launch id';;
        2) [[ $line == 'agent {{AWMAN_AGENT}}' ]] || fail 64 'agent mismatch';;
        3) kit=${line#kit }; [[ $line == "kit $kit" ]] && digest "$kit" || fail 64 'invalid kit digest';;
        *)
            if [[ $line == end ]]; then
                ended=1
            else
                read -r -a fields <<< "$line"
                [[ ${#fields[@]} == 4 && ${fields[0]} == file ]] || fail 64 'invalid file line'
                name=${fields[1]}
                token "$name" || fail 64 'invalid file id'
                [[ -v paths[$name] && ! -v hashes[$name] ]] || fail 64 'unknown or repeated file id'
                [[ $line == "file $name ${fields[2]} ${fields[3]}" ]] && digest "${fields[2]}" || fail 64 'invalid file digest'
                [[ ${fields[3]} == - || ${fields[3]} =~ ^[A-Za-z0-9+/]+={0,2}$ ]] || fail 64 'invalid base64'
                hashes[$name]=${fields[2]}
                bodies[$name]=${fields[3]}
                count=$((count+1))
            fi;;
    esac
    step=$((step+1))
done
[[ $ended == 1 ]] || fail 64 'incomplete document'
[[ -f /usr/local/lib/awman/kit-id && ! -L /usr/local/lib/awman/kit-id ]] || fail 73 'missing kit identity'
baked=$(</usr/local/lib/awman/kit-id)
if [[ $kit != "$baked" ]]; then
    printf 'AWMAN-APPLY-STALE %s\n' "$baked"
    exit 3
fi

umask 077
stage=$(mktemp -d /tmp/awman-apply.XXXXXXXX) || fail 73 'cannot stage policy'
trap 'rm -rf -- "$stage"' EXIT
for name in "${!hashes[@]}"; do
    if [[ ${bodies[$name]} == - ]]; then
        : > "$stage/$name"
    else
        printf '%s' "${bodies[$name]}" | base64 -d > "$stage/$name" || fail 65 'cannot decode policy'
    fi
    actual=$(sha256sum < "$stage/$name") || fail 65 'cannot hash policy'
    [[ ${actual%% *} == "${hashes[$name]}" ]] || fail 65 'policy digest mismatch'
done

# This fragment runs as uid 1000 for agent-owned destinations. Root opens only
# its private staged input; it never opens a destination under the agent home.
write_file='
set -euo pipefail
target=$1 expected=$2 previous=$3
parent=$(dirname -- "$target")
ancestor=$parent
while [[ $ancestor != / ]]; do
    [[ ! -L $ancestor ]] || exit 73
    ancestor=$(dirname -- "$ancestor")
done
[[ ! -L $target && ( ! -e $target || -f $target ) ]] || exit 73
if [[ $previous != root ]]; then
    if [[ -f $target ]]; then
        # Unowned files are preserved on omission; a supplied policy must
        # never overwrite one. Modified formerly owned content is refused.
        if [[ $previous == absent ]]; then
            [[ $expected == absent ]] && exit 0
            exit 73
        fi
        actual=$(sha256sum < "$target") || exit 65
        [[ ${actual%% *} == "$previous" ]] || exit 73
    fi
fi
if [[ $expected == absent ]]; then
    rm -f -- "$target" || exit 73
    exit 0
fi
if [[ ! -d $parent ]]; then
    install -d -m 0755 -- "$parent" || exit 73
fi
tmp=$(mktemp "$parent/.awman-policy.XXXXXXXX") || exit 73
trap '\''rm -f -- "$tmp"'\'' EXIT
install -m 0644 /dev/stdin "$tmp" || exit 73
mv -f -- "$tmp" "$target" || exit 73
actual=$(sha256sum < "$target") || exit 65
[[ ${actual%% *} == "$expected" ]] || exit 65
'
for parent in /var /var/lib /var/lib/awman; do
    [[ ! -L $parent ]] || fail 73 'symlink in ownership path'
done
install -d -o root -g root -m 0755 /var/lib/awman || fail 73 'cannot create ownership directory'
# A later file refusal may follow an earlier successful write. Invalidate the
# old receipt before the first destination mutation, so replaying its launch id
# cannot start an agent with partially applied, unacknowledged policy.
[[ ! -L /var/lib/awman/receipt && ( ! -e /var/lib/awman/receipt || -f /var/lib/awman/receipt ) ]] || fail 73 'invalid receipt destination'
rm -f -- /var/lib/awman/receipt || fail 73 'cannot invalidate old receipt'
for name in "${!paths[@]}"; do
    path=${paths[$name]}
    expected=${hashes[$name]-absent}
    input=/dev/null
    [[ $expected == absent ]] || input=$stage/$name
    if [[ ${owners[$name]} == agent ]]; then
        marker=/var/lib/awman/owned-$name
        [[ ! -L $marker && ( ! -e $marker || -f $marker ) ]] || fail 73 'invalid ownership record'
        previous=absent
        if [[ -f $marker ]]; then
            previous=$(<"$marker")
            digest "$previous" || fail 73 'invalid ownership digest'
        fi
        runuser -u agent -- /bin/bash -c "$write_file" awman-write "$path" "$expected" "$previous" < "$input" || {
            status=$?; fail "$status" 'cannot apply agent policy'
        }
        if [[ $expected == absent ]]; then
            rm -f -- "$marker" || fail 73 'cannot clear ownership'
        else
            printf '%s\n' "$expected" > "$marker" || fail 73 'cannot record ownership'
            chmod 0644 "$marker" || fail 73 'cannot protect ownership'
        fi
    else
        # Root-owned destinations must have no symlink ancestors either.
        parent=$(dirname -- "$path")
        while [[ $parent != / ]]; do
            [[ ! -L $parent ]] || fail 73 'symlink in root policy path'
            parent=$(dirname -- "$parent")
        done
        /bin/bash -c "$write_file" awman-write "$path" "$expected" root < "$input" || {
            status=$?; fail "$status" 'cannot apply root policy'
        }
    fi
done

for parent in /var /var/lib /var/lib/awman; do
    [[ ! -L $parent ]] || fail 73 'symlink in receipt path'
done
install -d -o root -g root -m 0755 /var/lib/awman || fail 73 'cannot create receipt directory'
[[ ! -L /var/lib/awman/receipt && ( ! -e /var/lib/awman/receipt || -f /var/lib/awman/receipt ) ]] || fail 73 'invalid receipt destination'
receipt=$(mktemp /var/lib/awman/.receipt.XXXXXXXX) || fail 73 'cannot stage receipt'
trap 'rm -rf -- "$stage"; rm -f -- "$receipt"' EXIT
printf 'launch %s\nkit %s\nagent {{AWMAN_AGENT}}\n' "$launch" "$kit" > "$receipt" || fail 73 'cannot write receipt'
chmod 0644 "$receipt" || fail 73 'cannot set receipt mode'
mv -f -- "$receipt" /var/lib/awman/receipt || fail 73 'cannot commit receipt'
printf 'AWMAN-APPLY-OK %s %s %s\n' "$launch" "$kit" "$count"
