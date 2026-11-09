#!/usr/bin/env bash
# Exercise tools/architecture-lint.sh on the fixture trees in
# tools/test-fixtures/architecture-lint/: allowed imports pass, direct and
# grouped forbidden imports fail (exit 1), and an unusable scanner or input
# fails with a scanner diagnostic (exit 2) instead of reading as a pass.
set -uo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
lint="$repo/tools/architecture-lint.sh"
fixtures="$repo/tools/test-fixtures/architecture-lint"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/awman-arch-lint-test.XXXXXX")
trap 'chmod -R u+rwx "$scratch" 2>/dev/null; rm -rf -- "$scratch"' EXIT
real_grep=$(command -v grep)
failures=0

# A minimal valid tree, with the named fixture overlaid on it.
tree() { # name [fixture]
  local root="$scratch/$1" d
  rm -rf "$root"
  for d in src/data src/engine src/command src/frontend crates/awman-host-shims/src; do
    mkdir -p "$root/$d"
    echo '// placeholder' > "$root/$d/placeholder.rs"
  done
  mkdir -p "$root/tools"
  cp "$repo/tools/env-lint-allowlist.txt" "$root/tools/"
  if [ -n "${2:-}" ]; then cp -R "$fixtures/$2/." "$root/"; fi
  echo "$root"
}

# run_lint ROOT [PATH-prefix]: sets status and output.
run_lint() {
  output=$(ARCH_LINT_ROOT="$1" PATH="${2:+$2:}$PATH" bash "$lint" 2>&1)
  status=$?
}

expect() { # description wanted-status wanted-text
  if [ "$status" -ne "$2" ] || ! printf '%s' "$output" | "$real_grep" -qF -- "$3"; then
    echo "FAIL: $1 (status $status, wanted $2 and '$3')"
    printf '%s\n' "$output" | sed 's/^/    /'
    failures=$((failures + 1))
  else
    echo "ok:   $1"
  fi
}

run_lint "$(tree empty)"
expect "minimal tree passes" 0 "OK"
for symbol in var var_os vars vars_os current_dir temp_dir current_exe args args_os; do
  root=$(tree "env-$symbol")
  printf 'fn run() { std::env::%s(); }\n' "$symbol" > "$root/src/engine/env.rs"
  run_lint "$root"
  expect "instance env read $symbol fails" 1 "VIOLATION [env-snapshot]"
done
root=$(tree env-allowed)
printf 'fn run() { std::env::current_dir(); }\n' > "$root/src/command/deferred.rs"
printf 'src/command/deferred.rs:std::env::current_dir # to be routed by 0142 instance host\n' > "$root/tools/env-lint-allowlist.txt"
run_lint "$root"
expect "explicit deferred environment read passes" 0 "OK"
printf 'fn run() { std::env::temp_dir(); }\n' >> "$root/src/command/deferred.rs"
run_lint "$root"
expect "allow-list grants only its named symbol" 1 "VIOLATION [env-snapshot]"
printf 'fn run() { std::env::var("DEFERRED"); }\n' > "$root/src/command/deferred.rs"
printf 'src/command/deferred.rs:std::env::var # to be routed by 0142 instance host\n' > "$root/tools/env-lint-allowlist.txt"
run_lint "$root"
expect "allow-listed direct variable read passes" 0 "OK"
printf 'use std::env;\nfn run() { env::current_dir(); }\n' > "$root/src/command/deferred.rs"
run_lint "$root"
expect "aliased environment read is also guarded" 1 "VIOLATION [env-snapshot]"
root=$(tree env-missing-allowlist)
rm "$root/tools/env-lint-allowlist.txt"
run_lint "$root"
expect "missing environment allow-list is a scanner error" 2 "allow-list missing or unreadable"
root=$(tree env-bad-allowlist)
printf 'src/engine/.*:.*\n' > "$root/tools/env-lint-allowlist.txt"
run_lint "$root"
expect "malformed environment allow-list is a scanner error" 2 "SCANNER ERROR"
run_lint "$(tree env-fixture forbidden-env-snapshot)"
expect "environment read fixture fails" 1 "VIOLATION [env-snapshot]"
run_lint "$(tree env-allowed-fixture allowed-env-snapshot)"
expect "explicit environment allow-list fixture passes" 0 "OK"
run_lint "$(tree allowed allowed)"
expect "allowed imports pass" 0 "OK"
run_lint "$(tree direct forbidden-direct)"
expect "direct forbidden import fails" 1 "VIOLATION [Layer 0]"
run_lint "$(tree grouped forbidden-grouped)"
expect "multi-line grouped forbidden import fails" 1 "VIOLATION [Layer 1]"
run_lint "$(tree grouped-last forbidden-grouped-last)"
expect "grouped forbidden import after allowed members fails" 1 "VIOLATION [Layer 2]"
run_lint "$(tree shim-direct forbidden-shim-direct)"
expect "host-shim engine import fails" 1 "VIOLATION [host-shims-layer]"
run_lint "$(tree shim-grouped forbidden-shim-grouped)"
expect "host-shim grouped engine import fails" 1 "VIOLATION [host-shims-layer]"

for case in call:"direct engine call" call-chain:"engine call in a split method chain" binding:"engine field bound by reference" after-clone:"engine call after a clone"; do
  name=${case%%:*}
  run_lint "$(tree "engine-$name" "forbidden-engine-$name")"
  expect "frontend ${case#*:} fails" 1 "VIOLATION [frontend-engine-call]"
done

# Missing input directory.
root=$(tree missing)
rm -rf "$root/crates"
run_lint "$root"
expect "missing input directory is a scanner error" 2 "required input directory missing"

# Unreadable input directory (root ignores permission bits).
root=$(tree unreadable)
chmod 000 "$root/src/engine"
if [ "$(id -u)" -ne 0 ]; then
  run_lint "$root"
  expect "unreadable input directory is a scanner error" 2 "not readable"
else
  echo "skip: unreadable directory case (running as root)"
fi
chmod 755 "$root/src/engine"

# An empty source tree has nothing to scan.
root=$(tree emptydir)
rm -f "$root/src/data/placeholder.rs"
run_lint "$root"
expect "tree without sources is a scanner error" 2 "no .rs files found"

# grep that fails mid-scan with a pattern the probes do not use.
shim="$scratch/bin-failing"
mkdir -p "$shim"
cat > "$shim/grep" <<SH
#!/usr/bin/env bash
for a in "\$@"; do
  case "\$a" in -rnE) echo "grep: simulated scanner failure" >&2; exit 2 ;; esac
done
exec "$real_grep" "\$@"
SH
chmod +x "$shim/grep"
run_lint "$(tree failing-grep allowed)" "$shim"
expect "grep execution error is not a pass" 2 "SCANNER ERROR: grep"

# grep without a capability the rules need (-z).
shim="$scratch/bin-nogz"
mkdir -p "$shim"
cat > "$shim/grep" <<SH
#!/usr/bin/env bash
case "\$*" in *-zE*) echo "grep: invalid option -- z" >&2; exit 2 ;; esac
exec "$real_grep" "\$@"
SH
chmod +x "$shim/grep"
run_lint "$(tree nogz allowed)" "$shim"
expect "unsupported grep capability is reported" 2 "unsupported scanner behavior"

# Keychain argv forms are checked in every Rust file under src/, including the
# top-level ones outside the four layers.
for file in main.rs command/placeholder.rs; do
  for form in '.arg("-w").arg(secret)' $'.arg("-w")\n        .arg(secret)'; do
    root=$(tree keychain-argv)
    printf 'fn f() { Command::new("security")%s; }\n' "$form" > "$root/src/$file"
    run_lint "$root"
    expect "keychain value in argv fails in src/$file" 1 "VIOLATION [keychain-argv-w]"
    printf 'fn f() { Command::new("security").arg("-w"); }\n' > "$root/src/$file"
    run_lint "$root"
    expect "keychain read form passes in src/$file" 0 "OK"
  done
done

# Only a gate that limits an item to test builds exempts it. Each rule sharing
# the exemption is checked with its own production pattern.
for gate in 'not(test)' 'any(test, unix)' 'all(not(test), unix)' 'feature = "test"'; do
  root=$(tree cfg-engine)
  printf '#[cfg(%s)]\nfn run(engines: &Engines) { engines.git_engine.diff_summary(root); }\n' "$gate" > "$root/src/frontend/gated.rs"
  run_lint "$root"
  expect "engine call behind cfg($gate) fails" 1 "VIOLATION [frontend-engine-call]"
  root=$(tree cfg-env)
  printf '#[cfg(%s)]\nfn run() { std::env::var("X"); }\n' "$gate" > "$root/src/command/gated.rs"
  run_lint "$root"
  expect "env read behind cfg($gate) fails" 1 "VIOLATION [env-snapshot]"
done
for gate in 'test' 'all(test, unix)' 'all(unix, test)' 'all(target_os = "linux", test)'; do
  root=$(tree cfg-test)
  printf '#[cfg(%s)]\nfn run(engines: &Engines) { engines.git_engine.diff_summary(root); std::env::var("X"); }\n' "$gate" > "$root/src/frontend/gated.rs"
  printf '#[cfg(%s)]\nfn run() { std::env::var("X"); }\n' "$gate" > "$root/src/command/gated.rs"
  run_lint "$root"
  expect "test code behind cfg($gate) is exempt" 0 "OK"
done

# Inline test items must not exempt the next production item.
for item in 'mod tests {}' 'mod tests;' 'fn test_only() {}'; do
  root=$(tree inline-test)
  printf '#[cfg(test)] %s\nfn production(engines: &Engines) { engines.git_engine.diff_summary(root); }\n' "$item" > "$root/src/frontend/inline.rs"
  run_lint "$root"
  expect "engine call after inline $item fails" 1 "VIOLATION [frontend-engine-call]"
  printf '#[cfg(test)] %s\nfn production() { std::env::var("X"); GlobalConfig::load(); }\n' "$item" > "$root/src/command/inline.rs"
  run_lint "$root"
  expect "env read after inline $item fails" 1 "VIOLATION [env-snapshot]"
  expect "config reload after inline $item fails" 1 "VIOLATION [config-load]"
done

# Brackets in a same-line test item are not the end of its cfg attribute.
root=$(tree inline-array)
cat > "$root/src/frontend/inline.rs" <<'RS'
#[cfg(test)] mod inline_tests { const VALUES: [u8; 1] = [0];
fn test_only(engines: &Engines) { engines.git_engine.diff_summary(root); }
}
RS
run_lint "$root"
expect "inline test module with an array remains exempt" 0 "OK"

# A linked input is compiled by Rust but not followed by the scanners.
root=$(tree symlink-forbidden)
echo 'use crate::engine::git::GitEngine;' > "$root/linked-target.rs"
ln -s ../../linked-target.rs "$root/src/data/symlinked.rs"
run_lint "$root"
expect "symlinked forbidden source is a scanner error" 2 "symlinked source input"
root=$(tree symlink-dangling)
ln -s missing.rs "$root/src/engine/dangling.rs"
run_lint "$root"
expect "dangling source symlink is a scanner error" 2 "symlinked source input"
root=$(tree symlink-dir)
mkdir "$root/elsewhere"
ln -s ../../elsewhere "$root/src/command/linked"
run_lint "$root"
expect "symlinked source directory is a scanner error" 2 "symlinked source input"

# mktemp that fails on the Nth diagnostic-file allocation. The tree holds a
# violation, so a skipped scan that read as success would be exit 1 or 0.
real_mktemp=$(command -v mktemp)
shim="$scratch/bin-mktemp"
mkdir -p "$shim"
cat > "$shim/mktemp" <<SH
#!/usr/bin/env bash
case "\$*" in
  *err.XXXXXX*)
    # Pipeline stages allocate concurrently; noclobber's O_EXCL open makes each
    # number unique. uutils mkdir can report success to the loser of a race.
    n=1
    until (set -C; : > "\$MKTEMP_STATE/\$n") 2>/dev/null; do n=\$((n + 1)); done
    if [ "\$n" = "\$MKTEMP_FAIL_AT" ]; then
      case "\${MKTEMP_LOCK:-}" in
        ?*) chmod 000 "\$(dirname "\${!#}")/\$MKTEMP_LOCK" ;;
        *) echo "mktemp: injected failure" >&2; exit 1 ;;
      esac
    fi
    ;;
esac
exec "$real_mktemp" "\$@"
SH
chmod +x "$shim/mktemp"
mktemp_run() { # fail-at [locked-result-file]
  state=$("$real_mktemp" -d "$scratch/mktemp-state.XXXXXX")
  export MKTEMP_STATE="$state" MKTEMP_FAIL_AT="$1" MKTEMP_LOCK="${2:-}"
  run_lint "$root" "$shim"
  unset MKTEMP_STATE MKTEMP_FAIL_AT MKTEMP_LOCK
  allocations=$(find "$state" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')
}
root=$(tree mktemp-sweep forbidden-direct)
mktemp_run 0
expect "the injected tree fails on its violation" 1 "VIOLATION [Layer 0]"
total=$allocations
n=1
while [ "$n" -le "$total" ]; do
  mktemp_run "$n"
  expect "diagnostic-file allocation $n of $total is a scanner error" 2 "cannot create a diagnostic file"
  n=$((n + 1))
done

# A result that cannot be written must not read as a clean scan. Allocation 2 is
# before the scans, so the later violation or scanner error hits the locked file.
if [ "$(id -u)" -ne 0 ]; then
  mktemp_run 2 violations
  expect "an unwritable violation record is a scanner error" 2 "cannot record a result"
  rm -rf "$root/src/frontend"
  mkdir -p "$root/src/frontend"
  echo '// placeholder' > "$root/src/frontend/placeholder.rs"
  ln -s missing.rs "$root/src/frontend/dangling.rs"
  mktemp_run 1 scanner-errors
  expect "an unwritable scanner-error record still fails" 2 "cannot record a result"
else
  echo "skip: unwritable result file cases (running as root)"
fi

if [ "$failures" -ne 0 ]; then
  echo "test-architecture-lint: $failures failure(s)"
  exit 1
fi
echo "test-architecture-lint: all cases passed"
