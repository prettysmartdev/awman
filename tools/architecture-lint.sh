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
