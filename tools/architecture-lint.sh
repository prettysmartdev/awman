#!/usr/bin/env bash
# architecture-lint.sh — enforce the four-layer import rule.
#
# Layers:
#   0  src/data/       → may only import crate::data::*
#   1  src/engine/     → may import crate::data::* and crate::engine::*
#   2  src/command/    → may import crate::data::*, crate::engine::*, crate::command::*
#   3  src/frontend/   → may import crate::data::*, crate::engine::*, crate::command::*, crate::frontend::*
#   4  src/main.rs, src/lib.rs → any
#
# Only inspects `crate::` paths. Ignores std::* and third-party crates.
# Exits non-zero on any violation.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$REPO_ROOT/src"
VIOLATION_FILE=$(mktemp)
trap 'rm -f "$VIOLATION_FILE"' EXIT

check_layer() {
    local layer="$1"
    local pattern="$2"
    local dir="$3"

    # Forbidden top-level segments for this layer (one per line).
    local forbidden_segments="$4"

    # Find all .rs files in the directory and grep for forbidden imports.
    # Two patterns:
    #   1. Direct: `crate::<forbidden>` anywhere on a non-comment line.
    #   2. Nested: a `use crate::{` block whose body lists a forbidden
    #      top-level segment. We collapse `use crate::{ … };` blocks (which
    #      can span multiple lines) onto one logical line via awk before
    #      grepping.
    local matches direct nested
    direct=$(grep -rnE "$pattern" "$dir" 2>/dev/null || true)

    # Build a single regex of forbidden segments for the nested check, e.g.
    # `\b(engine|command|frontend)\b`.
    local nested_re=""
    if [ -n "$forbidden_segments" ]; then
        nested_re="\\b($(echo "$forbidden_segments" | paste -sd '|' -))\\b"
    fi

    if [ -n "$nested_re" ]; then
        # awk: collapse `use crate::{ … };` blocks (possibly multi-line) into
        # a single logical line so a single regex can inspect the body.
        nested=$(
            find "$dir" -type f -name '*.rs' -print0 2>/dev/null |
            while IFS= read -r -d '' f; do
                awk -v file="$f" '
                    BEGIN { buf=""; start=0 }
                    {
                        if (buf != "") {
                            buf = buf " " $0
                            if (index($0, "}") != 0) {
                                print file ":" start ":" buf
                                buf=""; start=0
                            }
                            next
                        }
                        if (match($0, /use[[:space:]]+crate::\{/)) {
                            if (index($0, "}") != 0) {
                                print file ":" NR ":" $0
                            } else {
                                buf = $0
                                start = NR
                            }
                        }
                    }
                ' "$f"
            done | grep -E "$nested_re" || true
        )
    fi

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
        echo "1" >> "$VIOLATION_FILE"
    done
}

# Match `crate::<segment>` where the segment is the whole word — the next
# character is anything other than `[A-Za-z0-9_]`. This catches both
# `use crate::engine::Foo` and the bare `use crate::engine;`, while not
# matching the (hypothetical) `crate::engineering` because the boundary
# requires a non-identifier character right after the segment.

# Layer 0: data/ must NOT import engine, command, or frontend
check_layer 0 'crate::(engine|command|frontend)([^A-Za-z0-9_]|$)' "$SRC/data" "engine
command
frontend"

# Layer 1: engine/ must NOT import command or frontend
check_layer 1 'crate::(command|frontend)([^A-Za-z0-9_]|$)' "$SRC/engine" "command
frontend"

# Layer 2: command/ must NOT import frontend
check_layer 2 'crate::frontend([^A-Za-z0-9_]|$)' "$SRC/command" "frontend"

# Layer 3: frontend/ can import everything — no check needed.

# Lint-suppression guard (WI 0113 F-12): a crate- or module-level
# `#![allow(dead_code)]` or `#![allow(unused_imports)]` is where cruft
# accumulates unseen (see aspec/review-notes/0113-architecture-audit.md,
# F-12). `src/lib.rs` and `src/data/mod.rs` carried exactly this and hid 29
# warnings, including an entire never-called sandbox backend surface. Fail
# if either inner-attribute form reappears anywhere under src/. Item-level
# `#[allow(dead_code)]` (single `#`, on one fn/field/struct with its own
# justification) is unaffected.
allow_matches=$(grep -rnE '^\s*#!\[allow\((dead_code|unused_imports)\)\]' "$SRC" 2>/dev/null || true)
if [ -n "$allow_matches" ]; then
    echo ""
    echo "architecture-lint: crate/module-level #![allow(dead_code|unused_imports)] found:"
    echo "$allow_matches" | while IFS= read -r line; do
        file_and_line="${line%%:*}"
        rest="${line#*:}"
        lineno="${rest%%:*}"
        display="${file_and_line#"$REPO_ROOT/"}"
        echo "VIOLATION [lint-suppression]: $display:$lineno"
        echo "1" >> "$VIOLATION_FILE"
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
    grep -rl 'add-generic-password' "$SRC" 2>/dev/null | grep -Ev "$KEYCHAIN_ARGV_ALLOWED" || true
)
if [ -n "$add_generic_password_matches" ]; then
    echo ""
    echo "architecture-lint: 'add-generic-password' found outside its one allowed builder (src/data/fs/daemon_env.rs) and its round-trip test (src/engine/auth/keychain.rs):"
    echo "$add_generic_password_matches" | while IFS= read -r f; do
        display="${f#"$REPO_ROOT/"}"
        echo "VIOLATION [keychain-argv]: $display"
        echo "1" >> "$VIOLATION_FILE"
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
    while IFS= read -r -d '' f; do
        awk -v file="$f" '
            { line[NR] = $0 }
            END {
                for (i = 1; i <= NR; i++) {
                    l = line[i]
                    if (l ~ /"-w"[[:space:]]*,[[:space:]]*[a-z_&*]/) {
                        printf "%s:%d:%s\n", file, i, l
                        continue
                    }
                    if (l !~ /\.arg\("-w"\)/) continue
                    if (l ~ /\.arg\("-w"\)[[:space:]]*\.arg\(/) {
                        printf "%s:%d:%s\n", file, i, l
                        continue
                    }
                    j = i + 1
                    while (j <= NR && line[j] ~ /^[[:space:]]*$/) j++
                    if (j <= NR && line[j] ~ /^[[:space:]]*\.arg\(/) {
                        printf "%s:%d:%s\n", file, i, l
                    }
                }
            }
        ' "$f"
    done < <(find "$SRC" -name '*.rs' -print0) \
        | grep -Ev '^('"$SRC"'/data/fs/daemon_env\.rs):' || true
)
w_value_string=$(
    grep -rn -- '-w {' "$SRC" 2>/dev/null \
        | grep -Ev '^('"$SRC"'/data/fs/daemon_env\.rs|'"$SRC"'/engine/auth/keychain\.rs):' || true
)
w_value_matches=$(printf '%s\n%s' "$w_value_argv" "$w_value_string" | grep -v '^$' || true)
if [ -n "$w_value_matches" ]; then
    echo ""
    echo "architecture-lint: a '-w' argument is formatted with a runtime value outside src/data/fs/daemon_env.rs; a keychain value must ride stdin, never argv:"
    echo "$w_value_matches" | while IFS= read -r line; do
        file_and_line="${line%%:*}"
        rest="${line#*:}"
        lineno="${rest%%:*}"
        display="${file_and_line#"$REPO_ROOT/"}"
        echo "VIOLATION [keychain-argv-w]: $display:$lineno"
        echo "1" >> "$VIOLATION_FILE"
    done
fi

# Report results.
if [ -s "$VIOLATION_FILE" ]; then
    count=$(wc -l < "$VIOLATION_FILE" | tr -d ' ')
    echo ""
    echo "architecture-lint: $count violation(s) found"
    exit 1
else
    echo "architecture-lint: OK — all imports respect the layering rules"
    exit 0
fi
