#!/usr/bin/env bash
# Exercise the pinned standalone runner with a small compiled fixture. This
# proves relocation/argv/inventory collection, never native guest acceptance.
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/awr-kit.XXXXXXXX")"
trap 'rm -rf "$work"' EXIT
runner="$repo/target/xtask-tools/bin/cargo-nextest"
if [ ! -x "$runner" ]; then
  echo 'BLOCKED[kit-corrupt]: run cargo xtask test install-runner' >&2
  exit 2
fi
mkdir -p "$work/source/tests" "$work/kit/workspace" "$work/kit/payload" "$work/kit/bin" "$work/home" "$work/path"
cat > "$work/source/Cargo.toml" <<'TOML'
[package]
name = "awman-cli"
version = "0.0.0"
edition = "2021"
[workspace]
[[test]]
name = "artifact"
path = "tests/artifact.rs"
TOML
cat > "$work/source/tests/artifact.rs" <<'RS'
mod signed_boot {
    #[test]
    fn relocation_contract() {
        assert!(std::process::Command::new("cargo").output().is_err());
        assert!(std::process::Command::new("rustc").output().is_err());
        let exe = std::env::current_exe().unwrap();
        assert!(exe.parent().unwrap().parent().unwrap().join("awman-fake-cli").is_file());
        assert!(!std::path::Path::new(env!("CARGO_MANIFEST_DIR")).exists());
        assert!(std::env::var("HOME").unwrap().contains("home"));
    }
}
RS
cargo generate-lockfile --offline --manifest-path "$work/source/Cargo.toml"
"$runner" nextest archive --manifest-path "$work/source/Cargo.toml" --locked -p awman-cli --test artifact --archive-file "$work/kit/tests.tar.zst"
printf '[workspace]\n' > "$work/kit/workspace/Cargo.toml"
cat > "$work/kit/nextest.toml" <<'TOML'
nextest-version = "0.9.150"
[profile.default]
retries = 0
slow-timeout = { period = "1s", terminate-after = 20 }
[profile.default.junit]
path = "junit.xml"
TOML
cp "$runner" "$work/kit/bin/cargo-nextest"
"$runner" nextest list --archive-file "$work/kit/tests.tar.zst" --extract-to "$work/kit/payload" --workspace-remap "$work/kit/workspace" --config-file "$work/kit/nextest.toml" --user-config-file none --message-format json > "$work/kit/compiled.json"
# The real kit copies its registered fake executable here. This fixture needs
# only the companion layout assertion and never executes a host agent.
printf 'relocation companion fixture\n' > "$work/kit/payload/target/debug/awman-fake-cli"
mv "$work/source" "$work/source-unavailable"
mv "$work/kit" "$work/relocated"
kit="$work/relocated"
args=(--cargo-metadata "$kit/payload/target/nextest/cargo-metadata.json" --binaries-metadata "$kit/payload/target/nextest/binaries-metadata.json" --workspace-remap "$kit/workspace" --target-dir-remap "$kit/payload/target" --config-file "$kit/nextest.toml" --user-config-file none -E 'test(=signed_boot::relocation_contract)')
# Git is a host prerequisite; Cargo, Rust and the original checkout are absent
# from this process environment. No environment from a developer HOME is used.
ln -s "$(command -v git)" "$work/path/git"
(cd "$kit/workspace"; env -i HOME="$work/home" PATH="$work/path" "$kit/bin/cargo-nextest" nextest list "${args[@]}" --message-format json) > "$work/list.json"
(cd "$kit/workspace"; env -i HOME="$work/home" PATH="$work/path" "$kit/bin/cargo-nextest" nextest run "${args[@]}" --no-tests fail --no-fail-fast)
python3 - "$work/list.json" "$kit/workspace/target/nextest/default/junit.xml" <<'PY'
import json, sys, xml.etree.ElementTree as E
v=json.load(open(sys.argv[1]))
expected={s['binary-id']+' '+n for s in v['rust-suites'].values() for n,t in s['testcases'].items() if t['filter-match']['status']=='matches'}
cases=list(E.parse(sys.argv[2]).iter('testcase'))
actual={t.attrib['classname']+' '+t.attrib['name'] for t in cases}
assert expected == actual and len(cases)==len(actual)==1
assert not any(list(t) for t in cases), 'missing, failed or skipped relocation execution'
print('standalone kit relocation: complete passing inventory; Cargo/source unavailable')
PY
