#!/usr/bin/env bash
# architecture-lint.sh — enforce the four-layer import rule.
#
# Layers:
#   0  src/data/       → may only import crate::data::*
#   1  src/engine/     → may import crate::data::* and crate::engine::*
#   2  src/command/    → may import crate::data::*, crate::engine::*, crate::command::*
#   3  src/frontend/   → may import crate::data::* and crate::engine::* for types and frontend traits only (no engine behavior calls: see the engines.<field>. rule), crate::command::*, crate::frontend::*
#   4  src/main.rs, src/lib.rs → any
#
# Only inspects `crate::` paths. Ignores std::* and third-party crates.
#
# Exit status:
#   0  every scan ran and found no violation
#   1  at least one rule violation
#   2  a scanner could not run (missing tool, unsupported regex capability,
#      missing or unreadable input, grep/awk/find error). A rule that cannot be
#      evaluated is never treated as a pass; a scanner error outranks a
#      violation count.
#
# Every scan distinguishes grep's "no match" (exit 1) from an execution error
# (exit >= 2), uses only POSIX ERE (no `grep -P`, no `\b`), and is exercised on
# fixture trees by tools/test-architecture-lint.sh. The scanned tree defaults
# to the repository and can be redirected with ARCH_LINT_ROOT (used by the
# fixtures).

set -uo pipefail

REPO_ROOT="${ARCH_LINT_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
SRC="$REPO_ROOT/src"
SHIMS="$REPO_ROOT/crates/awman-host-shims/src"
SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/architecture-lint.XXXXXX") || {
    echo "architecture-lint: SCANNER ERROR: cannot create a scratch directory under ${TMPDIR:-/tmp}" >&2
    exit 2
}
trap 'rm -rf "$SCRATCH"' EXIT
trap 'exit 2' TERM
VIOLATION_FILE="$SCRATCH/violations"
SCANNER_FILE="$SCRATCH/scanner-errors"
if ! : > "$VIOLATION_FILE" || ! : > "$SCANNER_FILE"; then
    echo "architecture-lint: SCANNER ERROR: cannot create result files under $SCRATCH" >&2
    exit 2
fi

# record RESULT-FILE: count one result. Scans run in subshells, so a result that
# cannot be written ends the whole lint rather than reading as a clean scan.
record() {
    if ! { echo "1" >> "$1"; } 2>/dev/null; then
        echo "architecture-lint: SCANNER ERROR: cannot record a result in $1; the result is NOT a pass" >&2
        kill -TERM $$
        exit 2
    fi
}

scanner_fail() {
    echo "architecture-lint: SCANNER ERROR: $*" >&2
    record "$SCANNER_FILE"
}

# new_errf: allocate a diagnostic file for one scan. A scan that cannot have one
# does not run, so its caller must return; the failure is already recorded.
new_errf() {
    mktemp "$SCRATCH/err.XXXXXX" || {
        scanner_fail "cannot create a diagnostic file under $SCRATCH; the scan did not run"
        return 1
    }
}

# grep_scan ARGS...: run grep (files named in ARGS, or stdin). Prints matches;
# no match is empty output and success; any other failure is a scanner error.
grep_scan() {
    local errf out rc
    errf=$(new_errf) || return 0
    out=$(grep "$@" 2>"$errf")
    rc=$?
    if [ "$rc" -gt 1 ]; then
        scanner_fail "grep $* failed (exit $rc): $(tr '\n' ' ' < "$errf")"
    elif [ "$rc" -eq 0 ]; then
        printf '%s\n' "$out"
    fi
    rm -f "$errf"
    return 0
}

# require_tool NAME: the scanner needs this program on PATH.
require_tool() {
    command -v "$1" > /dev/null 2>&1 || scanner_fail "required tool '$1' not found on PATH; install it (the lint needs grep, awk, find, xargs, sort, mktemp)"
}

# require_dir DIR: a scanned root must exist and be fully readable.
require_dir() {
    if [ ! -d "$1" ]; then
        scanner_fail "required input directory missing: $1 (run from a full checkout, or point ARCH_LINT_ROOT at one)"
        return 1
    fi
    if [ ! -r "$1" ] || [ ! -x "$1" ]; then
        scanner_fail "input directory not readable: $1"
        return 1
    fi
}

# list_rs OUTFILE DIR...: NUL-separated list of the .rs files under DIRs. An
# error walking a tree, or an empty tree, is a scanner error.
list_rs() {
    local out="$1" errf rc
    shift
    errf=$(new_errf) || return 0
    find "$@" -name '*.rs' -type f -print0 > "$out" 2>"$errf"
    rc=$?
    if [ "$rc" -ne 0 ]; then
        scanner_fail "find $* failed (exit $rc): $(tr '\n' ' ' < "$errf")"
    elif [ ! -s "$out" ]; then
        scanner_fail "no .rs files found under: $* (nothing to scan)"
    fi
    rm -f "$errf"
}

# drop_from_list LISTFILE REGEX: remove paths matching REGEX from a NUL list.
drop_from_list() {
    local list="$1" re="$2" errf rc
    errf=$(new_errf) || return 0
    grep -zEv "$re" < "$list" > "$list.new" 2>"$errf"
    rc=$?
    if [ "$rc" -gt 1 ]; then
        scanner_fail "grep -zEv $re failed (exit $rc): $(tr '\n' ' ' < "$errf")"
    else
        mv "$list.new" "$list" || scanner_fail "cannot update the file list $list"
    fi
    rm -f "$errf" "$list.new"
}

# awk_files LISTFILE PROGRAM [AWK-OPTIONS...]: run awk over every file in the
# NUL list; a failing awk (or xargs) is a scanner error, not "no matches".
awk_files() {
    local list="$1" prog="$2" errf out rc
    shift 2
    errf=$(new_errf) || return 0
    out=$(xargs -0 awk "$@" "$prog" < "$list" 2>"$errf")
    rc=$?
    if [ "$rc" -ne 0 ]; then
        scanner_fail "awk scan failed (exit $rc): $(tr '\n' ' ' < "$errf")"
    elif [ -n "$out" ]; then
        printf '%s\n' "$out"
    fi
    rm -f "$errf"
}

# reject_symlinks DIR...: the scanners do not follow links, but Rust does, so a
# linked input would be compiled without being checked. Fail rather than resolve:
# no source tree here needs one, and following them raises loops and dangling
# targets outside the checkout.
reject_symlinks() {
    local errf links rc
    errf=$(new_errf) || return 0
    links=$(find "$@" -type l 2>"$errf")
    rc=$?
    if [ "$rc" -ne 0 ]; then
        scanner_fail "find $* -type l failed (exit $rc): $(tr '\n' ' ' < "$errf")"
    elif [ -n "$links" ]; then
        scanner_fail "symlinked source input is not scanned; replace it with a regular file: $(printf '%s' "$links" | tr '\n' ' ')"
    fi
    rm -f "$errf"
}

# Capability probes: fail loudly when this host's grep or awk cannot express
# what the rules below rely on, instead of silently matching nothing.
probe() { # DESCRIPTION EXPECTED-OUTPUT ACTUAL-OUTPUT
    [ "$2" = "$3" ] || scanner_fail "unsupported scanner behavior: $1 (expected '$2', got '$3')"
}
for tool in grep awk find xargs mktemp tr paste; do require_tool "$tool"; done
probe "grep -E alternation with a trailing anchor" "crate::engine" \
    "$(printf 'use crate::engine;\n' | grep -oE 'crate::(engine|command)([^A-Za-z0-9_]|$)' | tr -d ';')"
probe "grep -z -E over NUL-separated input" "a.rs" \
    "$(printf 'a.rs\0b.txt\0' | grep -zE '\.rs$' | tr -d '\0')"
probe "grep -E byte ranges under LC_ALL=C (box-drawing)" "yes" \
    "$(printf '\342\224\200\n' | LC_ALL=C grep -qE $'\xe2[\x94\x95][\x80-\xbf]' && echo yes)"
probe "awk match() with a POSIX class" "5" \
    "$(echo 'use  crate::{' | awk '{ if (match($0, /use[[:space:]]+crate::\{/)) print RSTART + 4 }')"

for dir in "$SRC/data" "$SRC/engine" "$SRC/command" "$SRC/frontend" "$SHIMS"; do
    require_dir "$dir"
done
reject_symlinks "$SRC" "$SHIMS"

# If an input or tool is missing there is nothing trustworthy to scan.
if [ -s "$SCANNER_FILE" ]; then
    echo "architecture-lint: aborting; the scanners above cannot run on this host or checkout." >&2
    exit 2
fi

# Lists of the .rs files each rule family inspects.
list_rs "$SCRATCH/all.list" "$SRC"
list_rs "$SCRATCH/data.list" "$SRC/data"
list_rs "$SCRATCH/engine.list" "$SRC/engine"
list_rs "$SCRATCH/command.list" "$SRC/command"
list_rs "$SCRATCH/frontend.list" "$SRC/frontend"
list_rs "$SCRATCH/shims.list" "$SHIMS"
list_rs "$SCRATCH/upper.list" "$SRC/engine" "$SRC/command" "$SRC/frontend"
list_rs "$SCRATCH/lower.list" "$SRC/data" "$SRC/engine" "$SRC/command"
if [ -s "$SCANNER_FILE" ]; then
    echo "architecture-lint: aborting; the scanners above cannot run on this host or checkout." >&2
    exit 2
fi

# `use <root>::{ … };` blocks (possibly multi-line) collapsed onto one logical
# line per block: `<file>:<first line>:<collapsed text>`. A block ends at the
# statement's `;`, so nested braces do not end it early.
GROUP_IMPORT_AWK='
    FNR == 1 { buf = ""; start = 0 }
    {
        if (buf != "") {
            buf = buf " " $0
            if (index($0, ";") != 0) { print FILENAME ":" start ":" buf; buf = ""; start = 0 }
            next
        }
        if (match($0, "use[[:space:]]+" root "::\\{")) {
            if (index($0, ";") != 0) print FILENAME ":" FNR ":" $0
            else { buf = $0; start = FNR }
        }
    }
'

# awk_stdin PROGRAM [AWK-OPTIONS...]: awk over stdin with an exit-status check.
awk_stdin() {
    local prog="$1" errf out rc
    shift
    errf=$(new_errf) || return 0
    out=$(awk "$@" "$prog" 2>"$errf")
    rc=$?
    if [ "$rc" -ne 0 ]; then
        scanner_fail "awk filter failed (exit $rc): $(tr '\n' ' ' < "$errf")"
    elif [ -n "$out" ]; then
        printf '%s\n' "$out"
    fi
    rm -f "$errf"
}

# Keep collapsed `use <root>::{ … };` lines whose *top-level* group members
# start with one of `segs` (a|b|c). Members nested inside another group, such
# as `engine::{ frontend::{…} }`, are paths below an allowed module and are
# not top-level segments.
TOP_LEVEL_AWK='
    {
        if (!match($0, "use[[:space:]]+crate::\\{") && !match($0, "use[[:space:]]+awman::\\{")) next
        body = substr($0, RSTART + RLENGTH)
        depth = 1; expect = 1; hit = 0
        n = split(segs, want, "|")
        for (i = 1; i <= length(body) && depth > 0; i++) {
            c = substr(body, i, 1)
            if (c == "{") { depth++; expect = 0 }
            else if (c == "}") depth--
            else if (c == "," && depth == 1) expect = 1
            else if (expect && depth == 1 && c ~ /[A-Za-z_]/) {
                ident = ""
                while (i <= length(body) && substr(body, i, 1) ~ /[A-Za-z0-9_]/) { ident = ident substr(body, i, 1); i++ }
                i--; expect = 0
                for (k = 1; k <= n; k++) if (ident == want[k]) hit = 1
            }
        }
        if (hit) print
    }
'

check_layer() {
    local layer="$1"
    local pattern="$2"
    local dir="$3"
    local list="$4"
    # Forbidden top-level segments for this layer (one per line).
    local forbidden_segments="$5"

    # Two patterns:
    #   1. Direct: `crate::<forbidden>` anywhere on a non-comment line.
    #   2. Nested: a `use crate::{` block whose body lists a forbidden
    #      top-level segment, found on the collapsed logical lines.
    local matches direct nested
    direct=$(grep_scan -rnE --include='*.rs' -- "$pattern" "$dir")

    nested=$(awk_files "$list" "$GROUP_IMPORT_AWK" -v root=crate | awk_stdin "$TOP_LEVEL_AWK" -v "segs=$(echo "$forbidden_segments" | paste -sd '|' -)")

    matches="$direct"
    if [ -n "$nested" ]; then
        if [ -n "$matches" ]; then
            matches="$matches"$'\n'"$nested"
        else
            matches="$nested"
        fi
    fi

    if [ -z "$matches" ]; then
        return
    fi

    echo "$matches" | while IFS= read -r line; do
        # line looks like: /path/to/file.rs:42:    use crate::frontend::foo;
        local file_and_line="${line%%:*}"
        local rest="${line#*:}"
        local lineno="${rest%%:*}"
        local content="${rest#*:}"

        # Skip lines that are pure comments.
        local trimmed="${content#"${content%%[![:space:]]*}"}"
        case "$trimmed" in
            //*) continue ;;
            \#*) continue ;;
            \**) continue ;;
        esac

        local display="${file_and_line#"$REPO_ROOT/"}"
        echo "VIOLATION [Layer $layer]: $display:$lineno    $trimmed"
        record "$VIOLATION_FILE"
    done
}

# Match `crate::<segment>` where the segment is the whole word — the next
# character is anything other than `[A-Za-z0-9_]`. This catches both
# `use crate::engine::Foo` and the bare `use crate::engine;`, while not
# matching the (hypothetical) `crate::engineering` because the boundary
# requires a non-identifier character right after the segment.


# Layer 0: data/ must NOT import engine, command, or frontend
check_layer 0 'crate::(engine|command|frontend)([^A-Za-z0-9_]|$)' "$SRC/data" "$SCRATCH/data.list" "engine
command
frontend"

# Layer 1: engine/ must NOT import command or frontend
check_layer 1 'crate::(command|frontend)([^A-Za-z0-9_]|$)' "$SRC/engine" "$SCRATCH/engine.list" "command
frontend"

# Layer 2: command/ must NOT import frontend
check_layer 2 'crate::frontend([^A-Za-z0-9_]|$)' "$SRC/command" "$SCRATCH/command.list" "frontend"

# WI 0126 D1: host shims are Layer 0 glue. They may depend on awman's data
# contracts, but must not couple worker/XPC code to higher-layer internals.
# Every `awman::` path must continue with `data::` (or open a group, which the
# grouped check below inspects).
shim_layer_matches=$(awk_files "$SCRATCH/shims.list" '
    {
        s = $0
        while ((i = index(s, "awman::")) > 0) {
            rest = substr(s, i + 7)
            if (rest !~ /^(data::|\{)/) { print FILENAME ":" FNR ":" $0; break }
            s = rest
        }
    }')
if [ -n "$shim_layer_matches" ]; then
    echo ""
    echo "architecture-lint: awman-host-shims may import only awman::data:: paths:"
    echo "$shim_layer_matches" | while IFS= read -r line; do
        echo "VIOLATION [host-shims-layer]: $line"
        record "$VIOLATION_FILE"
    done
fi
# Also check grouped imports, where awman::{data::..., engine::...} would not
# contain a direct `awman::engine` path.
shim_group_matches=$(
    awk_files "$SCRATCH/shims.list" "$GROUP_IMPORT_AWK" -v root=awman |
        awk_stdin "$TOP_LEVEL_AWK" -v segs="engine|command|frontend"
)
if [ -n "$shim_group_matches" ]; then
    echo ""
    echo "architecture-lint: grouped host-shims imports may not name engine/command/frontend:"
    echo "$shim_group_matches" | while IFS= read -r line; do
        echo "VIOLATION [host-shims-layer]: $line"
        record "$VIOLATION_FILE"
    done
fi

# Layer 3: frontend/ can import everything — no check needed.

# Lint-suppression guard (WI 0113 F-12): a crate- or module-level
# `#![allow(dead_code)]` or `#![allow(unused_imports)]` is where cruft
# accumulates unseen (see aspec/review-notes/0113-architecture-audit.md,
# F-12). `src/lib.rs` and `src/data/mod.rs` carried exactly this and hid 29
# warnings, including an entire never-called sandbox backend surface. Fail
# if either inner-attribute form reappears anywhere under src/. Item-level
# `#[allow(dead_code)]` (single `#`, on one fn/field/struct with its own
# justification) is unaffected.
allow_matches=$(grep_scan -rnE --include='*.rs' '^[[:space:]]*#!\[allow\((dead_code|unused_imports)\)\]' "$SRC")
if [ -n "$allow_matches" ]; then
    echo ""
    echo "architecture-lint: crate/module-level #![allow(dead_code|unused_imports)] found:"
    echo "$allow_matches" | while IFS= read -r line; do
        file_and_line="${line%%:*}"
        rest="${line#*:}"
        lineno="${rest%%:*}"
        display="${file_and_line#"$REPO_ROOT/"}"
        echo "VIOLATION [lint-suppression]: $display:$lineno"
        record "$VIOLATION_FILE"
    done
fi

# WI 0116 §5 guard: a keychain payload value must never become a `Command`
# argument (world-readable via `/proc/<pid>/cmdline` / `ps`). The write path
# instead pipes an `add-generic-password ... -w <envelope>` line to `security
# -i` on stdin — `security_add_generic_password_script` in
# src/data/fs/daemon_env.rs is the ONLY place `add-generic-password` may appear
# in src/, apart from the round-trip test that asserts on the line it produces
# (src/engine/auth/keychain.rs). If it shows up anywhere else, either a second
# builder has been added (drifting from the one audited for
# shell-metacharacter safety) or — worse — someone put it directly into a
# `Command::arg`.
#
# The allowlist is by PATH, not by position within a file. An earlier version
# scanned only up to a file's `#[cfg(test)] mod tests` line; Rust accepts items
# *after* a test module, so anything placed there was invisible to the guard.
KEYCHAIN_ARGV_ALLOWED='^('"$SRC"'/data/fs/daemon_env\.rs|'"$SRC"'/engine/auth/keychain\.rs)$'
add_generic_password_matches=$(
    grep_scan -rl --include='*.rs' 'add-generic-password' "$SRC" | grep_scan -Ev "$KEYCHAIN_ARGV_ALLOWED"
)
if [ -n "$add_generic_password_matches" ]; then
    echo ""
    echo "architecture-lint: 'add-generic-password' found outside its one allowed builder (src/data/fs/daemon_env.rs) and its round-trip test (src/engine/auth/keychain.rs):"
    echo "$add_generic_password_matches" | while IFS= read -r f; do
        display="${f#"$REPO_ROOT/"}"
        echo "VIOLATION [keychain-argv]: $display"
        record "$VIOLATION_FILE"
    done
fi

# The second half of the same invariant: `-w ` must never be formatted with a
# runtime value outside that builder. `security find-generic-password ... -w`
# with NO argument is the legitimate *read* form (keychain.rs) and stays
# allowed; what is forbidden is a `-w` immediately followed by an interpolated
# value, in either the argv shape (`.arg("-w").arg(value)`, `args(["-w", v])`)
# or the string shape (`-w {…}` inside a `format!`).
#
# The argv shape is checked everywhere but the builder — that is the realistic
# regression, someone reaching for `-w <value>` in a `Command`. The string shape
# additionally excuses keychain.rs, whose round-trip test asserts on the exact
# stdin line the builder produces and must quote it to do so.
#
# `.arg("-w")` and its value are commonly written on separate lines, so the
# argv check reads each file with awk rather than grepping line by line: a bare
# `cmd.arg("-w");` whose next statement is not another `.arg(` is the read form
# and stays legal.
w_value_argv=$(
    awk_files "$SCRATCH/all.list" '
        function check(   i, j, l) {
            for (i = 1; i <= n; i++) {
                l = line[i]
                if (l ~ /"-w"[[:space:]]*,[[:space:]]*[a-z_&*]/) {
                    printf "%s:%d:%s\n", cur, i, l
                    continue
                }
                if (l !~ /\.arg\("-w"\)/) continue
                if (l ~ /\.arg\("-w"\)[[:space:]]*\.arg\(/) {
                    printf "%s:%d:%s\n", cur, i, l
                    continue
                }
                j = i + 1
                while (j <= n && line[j] ~ /^[[:space:]]*$/) j++
                if (j <= n && line[j] ~ /^[[:space:]]*\.arg\(/) {
                    printf "%s:%d:%s\n", cur, i, l
                }
            }
        }
        FNR == 1 { if (NR > 1) check(); n = 0; cur = FILENAME }
        { line[++n] = $0 }
        END { check() }
    ' | grep_scan -Ev '^('"$SRC"'/data/fs/daemon_env\.rs):'
)
w_value_string=$(
    grep_scan -rn --include='*.rs' -- '-w {' "$SRC" \
        | grep_scan -Ev '^('"$SRC"'/data/fs/daemon_env\.rs|'"$SRC"'/engine/auth/keychain\.rs):'
)
w_value_matches=$(printf '%s\n%s' "$w_value_argv" "$w_value_string" | grep_scan -v '^$')
if [ -n "$w_value_matches" ]; then
    echo ""
    echo "architecture-lint: a '-w' argument is formatted with a runtime value outside src/data/fs/daemon_env.rs; a keychain value must ride stdin, never argv:"
    echo "$w_value_matches" | while IFS= read -r line; do
        file_and_line="${line%%:*}"
        rest="${line#*:}"
        lineno="${rest%%:*}"
        display="${file_and_line#"$REPO_ROOT/"}"
        echo "VIOLATION [keychain-argv-w]: $display:$lineno"
        record "$VIOLATION_FILE"
    done
fi

# WI 0114 F-37 step 4: every environment variable awman reads is named and read
# in `src/data/config/env.rs`. Above Layer 0, a `std::env::var` is a second,
# undeclared config source — it bypasses `EnvSnapshot`'s known-key list, is
# invisible to `ForwardedEnv`'s daemon bootstrap allowlist, and cannot be
# tested without mutating the process environment. Read through `EnvSnapshot`,
# or through `host_var` for a value a squad client may have pushed.
#
# Test code is exempt: a test that mutates and restores `PATH` around a fake
# binary is not a config read. The exemption is by **brace span**, not by
# position — Rust accepts items after a `#[cfg(test)] mod tests`, so "anything
# past the first `#[cfg(test)]`" would hide a real hit (the same trap the
# keychain-argv guard above documents). The gate must be
# test-only, which means `#[cfg(test)]` or an `all(...)` with `test` as a direct
# member, such as `#[cfg(all(test, unix))]`. `not(test)` and `any(test, ...)`
# compile into production builds, so they and every other form are scanned.
#
# A whole *file* can also be test code: `#[cfg(test)] mod tests;` gates
# `foo/tests.rs` or `foo/tests/*.rs` from the parent, so the attribute is not
# in the file the scanner reads. Those paths are skipped wholesale — the
# convention is enforced by the fact that nothing else may live there.
TEST_FILE_RE='(/tests/|/tests\.rs$)'
#
# Layer 3 keeps the original variable-read guard. Layer 1 and Layer 2 use the
# broader snapshot rule and its exact symbol allow-list below.

# Production (non-test) files above Layer 0, as a NUL list.
cp "$SCRATCH/upper.list" "$SCRATCH/prod_upper.list" || scanner_fail "cannot copy the file list prod_upper.list"
drop_from_list "$SCRATCH/prod_upper.list" "$TEST_FILE_RE"

# True for an attribute that positively gates its item to test builds.
TEST_GATE_AWK='
    function test_only_gate(attr,    body, i, c, depth, member) {
        gsub(/[[:space:]]/, "", attr)
        if (!match(attr, /#\[cfg\(/)) return 0
        body = substr(attr, RSTART + RLENGTH)
        if (body ~ /^test\)\]/) return 1
        if (body !~ /^all\(/) return 0
        body = substr(body, 5)
        depth = 0
        member = ""
        for (i = 1; i <= length(body); i++) {
            c = substr(body, i, 1)
            if (depth == 0 && (c == "," || c == ")")) {
                if (member == "test") return 1
                if (c == ")") return 0
                member = ""
                continue
            }
            if (c == "(") depth++
            else if (c == ")") depth--
            member = member c
        }
        return member == "test"
    }
'

# Brace-span test exemption shared by the env-var and config-load guards; the
# caller supplies `re`, the production-code pattern to report.
PROD_CODE_AWK="$TEST_GATE_AWK"'
    FNR == 1 { depth = 0; pending = 0 }
    {
        code = $0
        sub(/\/\/.*$/, "", code)
        if (depth > 0) {
            depth += gsub(/\{/, "{", code) - gsub(/\}/, "}", code)
            next
        }
        if (test_only_gate(code)) {
            pending = 1
            sub(/^[^]]*]/, "", code)
        }
        if (pending) {
            if (code ~ /^[[:space:]]*$/) next
            opens = gsub(/\{/, "{", code)
            closes = gsub(/\}/, "}", code)
            if (opens > closes) depth = opens - closes
            pending = 0
            next
        }
        if (code ~ re) {
            printf "%s:%d:%s\n", FILENAME, FNR, $0
        }
    }
'

env_var_matches=$(
    awk_files "$SCRATCH/prod_upper.list" "$PROD_CODE_AWK" \
        -v 're=std::env::var|[^A-Za-z0-9_]env::var[(]|env::var_os[(]' |
        grep_scan -Ev "^$SRC/(engine|command)/"
)
if [ -n "$env_var_matches" ]; then
    echo ""
    echo "architecture-lint: std::env::var above Layer 0; declare the variable in src/data/config/env.rs and read it through EnvSnapshot (or host_var):"
    echo "$env_var_matches" | while IFS= read -r line; do
        file_and_line="${line%%:*}"
        rest="${line#*:}"
        lineno="${rest%%:*}"
        display="${file_and_line#"$REPO_ROOT/"}"
        echo "VIOLATION [env-var]: $display:$lineno"
        record "$VIOLATION_FILE"
    done
fi

# WI 0142: instance code cannot consult the daemon's process environment.
# Entries are path:symbol plus a required explanation, never regex exemptions.
# Keep the old env-var rule above for Layer 3 and aliases; this rule also covers
# cwd/temp/argv, home-directory fallbacks and ad-hoc snapshot captures in L1/L2.
ENV_READ_ALLOWLIST="$REPO_ROOT/tools/env-lint-allowlist.txt"
if [ ! -f "$ENV_READ_ALLOWLIST" ] || [ ! -r "$ENV_READ_ALLOWLIST" ]; then
    scanner_fail "environment allow-list missing or unreadable: $ENV_READ_ALLOWLIST"
else
    env_read_matches=$(
        awk_files "$SCRATCH/prod_upper.list" "$PROD_CODE_AWK" \
            -v 're=(std::|[^A-Za-z_0-9])env::(var|var_os|vars|vars_os|current_dir|temp_dir|current_exe|args|args_os)[[:space:]]*[(]|dirs::home_dir|Env::from_process' |
        awk_stdin '
            BEGIN {
                while ((rc = (getline entry < allowfile)) > 0) {
                    if (entry ~ /^[[:space:]]*(#|$)/) continue
                    if (entry !~ /^src\/(engine|command)\/[^ :]+:[A-Za-z_0-9:]+[[:space:]]+# .+/) exit 2
                    sub(/[[:space:]]+# .*/, "", entry)
                    allowed[entry] = 1
                }
                if (rc < 0) exit 2
                close(allowfile)
            }
            {
                file = $0; sub(/:[0-9]+:.*/, "", file)
                if (file !~ /\/src\/(engine|command)\//) next
                sub(root "/", "", file)
                code = $0; sub(/^[^:]+:[0-9]+:/, "", code)
                sub(/\/\/.*$/, "", code)
                while (match(code, /(std::|[^A-Za-z_0-9])env::(var_os|vars_os|vars|var|current_dir|temp_dir|current_exe|args_os|args)[[:space:]]*[(]|dirs::home_dir|Env::from_process/)) {
                    symbol = substr(code, RSTART, RLENGTH)
                    sub(/^[^A-Za-z_]/, "", symbol)
                    if (symbol ~ /^env::/) symbol = "std::" symbol
                    sub(/[[:space:]]*[(]$/, "", symbol)
                    if (!((file ":" symbol) in allowed)) print $0
                    code = substr(code, RSTART + RLENGTH)
                }
            }
        ' -v allowfile="$ENV_READ_ALLOWLIST" -v root="$REPO_ROOT"
    )
    if [ -n "$env_read_matches" ]; then
        echo ""
        echo "architecture-lint: process environment read on the instance path; use EnvSnapshot:"
        echo "$env_read_matches" | while IFS= read -r line; do
            echo "VIOLATION [env-snapshot]: ${line#"$REPO_ROOT/"}"
            record "$VIOLATION_FILE"
        done
    fi
fi

# WI 0114 F-31: configuration reaches a command through its `Session` (which
# merged global, repo, environment and flags) or, for a daemon that has none,
# through the `GlobalConfig` its `Engines` bundle was assembled from. A
# `GlobalConfig::load()` or `RepoConfig::load()` anywhere else is a second,
# ad-hoc config source: it re-reads the file the command was not built against,
# and every historical instance wrapped it in `unwrap_or_default()`, silently
# turning a malformed config into defaults.
#
# Same brace-span exemption for test code as the env-var guard above.
#
# Two production reads are allowlisted by path, each the single load its owner
# is entitled to:
#   * src/command/dispatch/{mod,engines}.rs — `Engines::for_daemon`, the bootstrap read
#     for a process with no `Session`. Propagates its error.
#   * src/engine/init/mod.rs — `InitEngine`'s `Preflight`, which loads the repo
#     config once and then *writes* it; it is the thing producing that file.
CONFIG_LOAD_ALLOWED="^($SRC/command/dispatch/(mod|engines)\.rs|$SRC/engine/init/mod\.rs):"
config_load_matches=$(
    awk_files "$SCRATCH/prod_upper.list" "$PROD_CODE_AWK" \
        -v 're=GlobalConfig::load[(][)]|RepoConfig::load[(]' |
        grep_scan -Ev "$CONFIG_LOAD_ALLOWED"
)
if [ -n "$config_load_matches" ]; then
    echo ""
    echo "architecture-lint: an ad-hoc config load; read through Session::effective_config() (or Engines::global_config in a daemon):"
    echo "$config_load_matches" | while IFS= read -r line; do
        file_and_line="${line%%:*}"
        rest="${line#*:}"
        lineno="${rest%%:*}"
        display="${file_and_line#"$REPO_ROOT/"}"
        echo "VIOLATION [config-load]: $display:$lineno"
        record "$VIOLATION_FILE"
    done
fi

# WI 0114 F-57 guard 2: a frontend may not hand-build a dispatch path.
#
# The grand architecture requires command-box input to be "routed directly to a
# method in the Dispatch package, no parsing or anything else done by the TUI
# itself". A `ParsedCommandBoxInput` literal whose `path` is a `vec![` of
# string literals bypasses the catalogue entirely: neither the subcommand nor
# the argument key is ever validated, so renaming either leaves the frontend
# dispatching a dead name and fails nothing until a user presses the key. That
# is exactly how F-15, F-21 and F-55 happened — three times, in three files.
#
# A frontend names the *intent* instead: a `FrontendAction` variant, turned
# into an invocation by `CommandCatalogue::action_input`, which reads the path,
# the flag names and the positional argument's name out of the catalogue.
#
# Detection is a four-line window from each `ParsedCommandBoxInput {` — the
# `-A3` the finding specifies — looking for a `path: vec![` opening on a string
# literal. Comment text is stripped first, so prose quoting the old shape (this
# very block, and `src/command/dispatch/frontend_action.rs`'s module doc) stays
# legal. There is no allowlist: as of this commit `src/frontend/` has no hits,
# test fixtures included, because the fixtures build their input the same way.

dispatch_bypass_matches=$(
    awk_files "$SCRATCH/frontend.list" '
        FNR == 1 { window = 0 }
        {
            code = $0
            sub(/\/\/.*$/, "", code)
            if (code ~ /ParsedCommandBoxInput[[:space:]]*\{/) window = 4
            if (window > 0) {
                if (code ~ /path:[[:space:]]*vec!\[[[:space:]]*"/) {
                    printf "%s:%d:%s\n", FILENAME, FNR, $0
                }
                window--
            }
        }
    '
)
if [ -n "$dispatch_bypass_matches" ]; then
    echo ""
    echo "architecture-lint: a frontend hand-builds a dispatch path; name a FrontendAction and let CommandCatalogue::action_input build the invocation:"
    echo "$dispatch_bypass_matches" | while IFS= read -r line; do
        file_and_line="${line%%:*}"
        rest="${line#*:}"
        lineno="${rest%%:*}"
        display="${file_and_line#"$REPO_ROOT/"}"
        echo "VIOLATION [dispatch-bypass]: $display:$lineno"
        record "$VIOLATION_FILE"
    done
fi

# WI 0114 F-57 guard 1: presentation lives in Layer 3.
#
# Box-drawing characters (Unicode block U+2500–U+257F: `╔ ═ ║ ╚ ─ │ ┌ …`) are
# terminal art. A layer below `src/frontend/` that composes them has decided
# how its output looks, and every frontend is then stuck with that decision:
# the TUI draws its own frames and ends up with a box inside a box, and the API
# serialises the `═` runs into JSON no HTTP client wanted. That is how the
# squad key banner (F-56) and the API-key banner (F-47 step 3) both went wrong.
# Engines and commands return facts; the frontend draws.
#
# Scope is `src/data`, `src/engine` and `src/command`. `#[cfg(test)]` code is
# not exempt — a test that pins the art pins the layering mistake with it; the
# api_server first-run test states the same property as a `'\u{2500}'..` range
# instead, which is also how to write one of these assertions.
#
# Comment text is stripped first (the same `//`-to-end-of-line filter the
# env-var and config-load guards use), so the `// ─── section ───` dividers
# throughout the tree stay legal, as does prose like this block. The match runs
# on raw UTF-8 bytes under `LC_ALL=C` — `E2 94 xx` / `E2 95 xx` is exactly the
# Box Drawing block — because BSD/macOS grep has no `-P`.
#
# There is no allowlist: as of this commit `src/{data,engine,command}` has no
# hits at all.
layer_render_matches=$(
    awk_files "$SCRATCH/lower.list" '
        {
            code = $0
            sub(/\/\/.*$/, "", code)
            printf "%s:%d:%s\n", FILENAME, FNR, code
        }
    ' | LC_ALL=C grep_scan -E $'\xe2[\x94\x95][\x80-\xbf]'
)
if [ -n "$layer_render_matches" ]; then
    echo ""
    echo "architecture-lint: box-drawing below src/frontend/; return the fact and let each frontend draw it:"
    echo "$layer_render_matches" | while IFS= read -r line; do
        file_and_line="${line%%:*}"
        rest="${line#*:}"
        lineno="${rest%%:*}"
        display="${file_and_line#"$REPO_ROOT/"}"
        echo "VIOLATION [layer-render]: $display:$lineno"
        record "$VIOLATION_FILE"
    done
fi

# WI 0129 D1: Layer 3 imports types and frontend traits from the lower layers
# but calls engine behavior only through Layer 2. A method call reached through
# an `Engines` engine field is the detectable form: `engines.git_engine.foo()`,
# `state.handles.engines().runtime.foo()`, or binding the field by reference to
# call it on the next line (`= &self.engines.container_runtime`). Only a
# terminal `.clone()` passes: handing a handle to a Layer 2 constructor is
# wiring, not a call. `global_config` and `config_notices` are not engines.
#
# rustfmt splits long method chains, so a line starting with `.` is first joined
# to the line before it. Test code is exempt as in PROD_CODE_AWK. A call behind
# a local alias or a function parameter is out of a line scanner's reach and
# remains a review matter.
ENGINE_FIELD_RE='engines([(][)])?[.](runtime|container_runtime|sandbox_runtime|git_engine|overlay_engine|auth_engine|agent_engine|workflow_state_store|credential_monitor)'
cp "$SCRATCH/frontend.list" "$SCRATCH/prod_frontend.list" || scanner_fail "cannot copy the file list prod_frontend.list"
drop_from_list "$SCRATCH/prod_frontend.list" "$TEST_FILE_RE"
frontend_engine_call_matches=$(
    awk_files "$SCRATCH/prod_frontend.list" "$TEST_GATE_AWK"'
        function report(text, lineno,    rest, before) {
            rest = text
            while (match(rest, field_re)) {
                before = substr(rest, 1, RSTART - 1)
                rest = substr(rest, RSTART + RLENGTH)
                if (rest ~ /^[a-z_0-9]/) continue
                if (rest ~ /^\.clone\(\)/ && rest !~ /^\.clone\(\)\./) continue
                if (rest ~ /^\./ || before ~ /=[[:space:]]*&[a-z_.()]*$/) {
                    printf "%s:%d:%s\n", held_file, lineno, text
                }
            }
        }
        function flush_held() {
            if (held != "") report(held, held_line)
            held = ""
        }
        FNR == 1 { flush_held(); depth = 0; pending = 0 }
        {
            code = $0
            sub(/\/\/.*$/, "", code)
            if (depth > 0) {
                depth += gsub(/\{/, "{", code) - gsub(/\}/, "}", code)
                next
            }
            if (test_only_gate(code)) {
                flush_held()
                pending = 1
                sub(/^[^]]*]/, "", code)
            }
            if (pending) {
                if (code ~ /^[[:space:]]*$/) next
                opens = gsub(/\{/, "{", code)
                closes = gsub(/\}/, "}", code)
                if (opens > closes) depth = opens - closes
                pending = 0
                next
            }
            if (code ~ /^[[:space:]]*\./) {
                sub(/^[[:space:]]*/, "", code)
                held = held code
                next
            }
            flush_held()
            held = code
            held_file = FILENAME
            held_line = FNR
        }
        END { flush_held() }
    ' -v field_re="$ENGINE_FIELD_RE"
)
if [ -n "$frontend_engine_call_matches" ]; then
    echo ""
    echo "architecture-lint: a frontend calls engine behavior; go through a Layer 2 type (a .clone() into a Layer 2 constructor is fine):"
    echo "$frontend_engine_call_matches" | while IFS= read -r line; do
        file_and_line="${line%%:*}"
        rest="${line#*:}"
        lineno="${rest%%:*}"
        display="${file_and_line#"$REPO_ROOT/"}"
        echo "VIOLATION [frontend-engine-call]: $display:$lineno"
        record "$VIOLATION_FILE"
    done
fi

# Report results. A scanner error outranks violations: a partial scan proves
# nothing about the rules it could not evaluate.
if [ -s "$SCANNER_FILE" ]; then
    count=$(wc -l < "$SCANNER_FILE" | tr -d ' ')
    echo ""
    echo "architecture-lint: $count scanner error(s); the result is NOT a pass (see SCANNER ERROR lines above)"
    exit 2
elif [ -s "$VIOLATION_FILE" ]; then
    count=$(wc -l < "$VIOLATION_FILE" | tr -d ' ')
    echo ""
    echo "architecture-lint: $count violation(s) found"
    exit 1
else
    echo "architecture-lint: OK — all imports respect the layering rules"
    exit 0
fi
