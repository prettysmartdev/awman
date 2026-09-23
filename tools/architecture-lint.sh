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
# keychain-argv guard above documents). Any `#[cfg(...)]` naming `test`
# qualifies, including `#[cfg(all(test, unix))]`.
#
# A whole *file* can also be test code: `#[cfg(test)] mod tests;` gates
# `foo/tests.rs` or `foo/tests/*.rs` from the parent, so the attribute is not
# in the file the scanner reads. Those paths are skipped wholesale — the
# convention is enforced by the fact that nothing else may live there.
TEST_FILE_RE='(/tests/|/tests\.rs$)'
#
# One production read is deliberate and allowlisted by path:
# `src/engine/sandbox/dsbx/session_config.rs` writes the non-sensitive env map
# into a workspace-readable `session.json`, and must read *this process's* own
# environment rather than the daemon overlay — the file itself explains why.
ENV_VAR_ALLOWED='^'"$SRC"'/engine/sandbox/dsbx/session_config\.rs$'
env_var_matches=$(
    find "$SRC/engine" "$SRC/command" "$SRC/frontend" -name '*.rs' -print0 2>/dev/null |
    grep -zEv "$TEST_FILE_RE" |
    xargs -0 awk '
        FNR == 1 { depth = 0; pending = 0 }
        {
            code = $0
            sub(/\/\/.*$/, "", code)
            if (depth > 0) {
                depth += gsub(/\{/, "{", code) - gsub(/\}/, "}", code)
                next
            }
            if (code ~ /#\[cfg\(/ && code ~ /[(,[:space:]]test[),[:space:]]/) {
                pending = 1
                next
            }
            if (pending) {
                if (code ~ /^[[:space:]]*$/) next
                opens = gsub(/\{/, "{", code)
                closes = gsub(/\}/, "}", code)
                if (opens > closes) depth = opens - closes
                pending = 0
                next
            }
            if (code ~ /std::env::var|[^A-Za-z0-9_]env::var\(|env::var_os\(/) {
                printf "%s:%d:%s\n", FILENAME, FNR, $0
            }
        }
    ' | grep -Ev "^($(echo "$ENV_VAR_ALLOWED" | sed 's/^\^//;s/\$$//')):" || true
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
        echo "1" >> "$VIOLATION_FILE"
    done
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
#   * src/command/dispatch/mod.rs — `Engines::for_daemon`, the bootstrap read
#     for a process with no `Session`. Propagates its error.
#   * src/engine/init/mod.rs — `InitEngine`'s `Preflight`, which loads the repo
#     config once and then *writes* it; it is the thing producing that file.
CONFIG_LOAD_ALLOWED="^($SRC/command/dispatch/mod\.rs|$SRC/engine/init/mod\.rs):"
config_load_matches=$(
    find "$SRC/engine" "$SRC/command" "$SRC/frontend" -name '*.rs' -print0 2>/dev/null |
    grep -zEv "$TEST_FILE_RE" |
    xargs -0 awk '
        FNR == 1 { depth = 0; pending = 0 }
        {
            code = $0
            sub(/\/\/.*$/, "", code)
            if (depth > 0) {
                depth += gsub(/\{/, "{", code) - gsub(/\}/, "}", code)
                next
            }
            if (code ~ /#\[cfg\(/ && code ~ /[(,[:space:]]test[),[:space:]]/) {
                pending = 1
                next
            }
            if (pending) {
                if (code ~ /^[[:space:]]*$/) next
                opens = gsub(/\{/, "{", code)
                closes = gsub(/\}/, "}", code)
                if (opens > closes) depth = opens - closes
                pending = 0
                next
            }
            if (code ~ /GlobalConfig::load\(\)|RepoConfig::load\(/) {
                printf "%s:%d:%s\n", FILENAME, FNR, $0
            }
        }
    ' | grep -Ev "$CONFIG_LOAD_ALLOWED" || true
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
        echo "1" >> "$VIOLATION_FILE"
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
    find "$SRC/frontend" -name '*.rs' -print0 2>/dev/null |
    xargs -0 awk '
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
    ' || true
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
        echo "1" >> "$VIOLATION_FILE"
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
    find "$SRC/data" "$SRC/engine" "$SRC/command" -name '*.rs' -print0 2>/dev/null |
    xargs -0 awk '
        {
            code = $0
            sub(/\/\/.*$/, "", code)
            printf "%s:%d:%s\n", FILENAME, FNR, code
        }
    ' | LC_ALL=C grep -E $'\xe2[\x94\x95][\x80-\xbf]' || true
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
