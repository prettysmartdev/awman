//! Renders `.github/workflows/nightly.yml`: everything too slow or random for
//! the PR path. The full gate is `ci.yml`
//! called with `gate: nightly`, so nightly runs the same build and test jobs.

use super::pieces::*;
use crate::manifest::Manifest;
use anyhow::Result;

fn tpl(text: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = text.to_owned();
    for (k, v) in pairs {
        out = out.replace(&format!("@@{k}@@"), v);
    }
    assert!(!out.contains("@@"), "unreplaced placeholder in template");
    out
}

const TOP: &str = r####"on:
  schedule:
    - cron: "23 3 * * *"
  workflow_dispatch:

# Read-only by default. Write scopes are granted per job, and only to jobs that
# cannot be reached from a pull request.
permissions:
  contents: read

# Nightly never cancels a run in progress: a half-finished hunt is no evidence.
concurrency:
  group: nightly-${{ github.workflow }}-${{ github.run_id }}
  cancel-in-progress: false

env:
  CARGO_TERM_COLOR: always
  CARGO_INCREMENTAL: "0"
  RUST_BACKTRACE: "1"

defaults:
  run:
    shell: bash

jobs:
"####;

const HEAD: &str = r####"  full:
    name: Full hosted gate
    uses: ./.github/workflows/ci.yml
    with:
      gate: nightly
    permissions:
      contents: read
      actions: read
      pull-requests: read

  flake-hunt:
    name: Flake hunt (shuffled, repeated, under CPU contention)
    runs-on: @@RUNS_ON@@
    timeout-minutes: 60
    permissions:
      contents: read
      issues: write
    env:
      RUNS: "5"
    steps:
@@CHECKOUT@@
@@RUST@@
      - name: Install the pinned test runner and a CPU stressor
        run: |
          cargo xtask test install-runner
          sudo apt-get update
          sudo apt-get install -y --no-install-recommends stress-ng
      - name: Build the archive once
        run: cargo xtask test archive --build linux-x64-default --out archive
      - name: Repeat the tiers with a recorded shuffle seed under contention
        run: |
          stress-ng --cpu "$(nproc)" --cpu-load 90 --timeout 55m --quiet &
          stress_pid=$!
          status=0
          mkdir -p flake
          for run in $(seq 1 "$RUNS"); do
            seed="$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
            echo "run $run seed $seed" | tee -a flake/seeds.txt
            cargo xtask test run --tier unit --tier component --tier boundary \
              --build linux-x64-default --archive archive/test-archive-linux-x64-default.tar.zst \
              --shuffle-seed "$seed" --evidence-dir "flake/run$run" || status=1
          done
          kill "$stress_pid" 2>/dev/null || true
          exit "$status"
      - name: Report intermittent failures
        id: report
        if: ${{ always() }}
        run: cargo xtask ci flake-report --in flake --out flake/report.md >> "$GITHUB_STEP_SUMMARY"
      - name: Open an issue with the failure record
        if: ${{ failure() && steps.report.outcome == 'failure' }}
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          gh issue create --repo "$GITHUB_REPOSITORY" \
            --title "Nightly flake hunt: intermittent test failure ($(date -u +%F))" \
            --label flaky-test \
            --body-file flake/report.md
      - name: Upload the flake evidence and failure records
        if: ${{ always() }}
        uses: @@UPLOAD@@
        with:
          name: flake-hunt-${{ github.sha }}
          path: flake
          if-no-files-found: warn
          retention-days: 30

  seeded-defects:
    name: Seeded defects (each must turn its cell red)
    runs-on: @@RUNS_ON@@
    timeout-minutes: 45
    steps:
@@CHECKOUT@@
@@RUST@@
      - name: Install the pinned test runner
        run: cargo xtask test install-runner
      - name: Pipeline faults (missing target, renamed job, skipped hardware)
        run: cargo xtask ci seeded-defects pipeline
      - name: Source defects (ci/seeded-defects/<layer>/*.patch)
        run: cargo xtask ci seeded-defects source --dir ci/seeded-defects --report seeded-defects.md
      - name: Report
        if: ${{ always() }}
        run: cat seeded-defects.md >> "$GITHUB_STEP_SUMMARY" || true
      - name: Retain baseline, per-patch JUnit and compiler diagnostics
        if: ${{ always() }}
        uses: @@UPLOAD@@
        with:
          name: seeded-defects-${{ github.run_id }}-${{ github.run_attempt }}
          path: |
            seeded-defects.md
            seeded-defects.evidence/
          if-no-files-found: error
          retention-days: 30

  mutation:
    name: Mutation testing (Layer 0 and Layer 2, sampled)
    runs-on: @@RUNS_ON@@
    timeout-minutes: 60
    steps:
@@CHECKOUT@@
@@RUST@@
      - name: Install the pinned test runner and cargo-mutants
        run: |
          cargo xtask test install-runner
          cargo xtask ci install-tool cargo-mutants
          echo "$PWD/target/xtask-tools" >> "$GITHUB_PATH"
      - name: Mutate a sample (shard chosen by the date, so the whole tree cycles)
        run: |
          shards=40
          shard=$(( $(date -u +%j | sed 's/^0*//') % shards ))
          echo "mutation shard $shard of $shards" | tee -a "$GITHUB_STEP_SUMMARY"
          status=0
          cargo mutants --package awman --test-tool nextest \
            --file 'src/data/**/*.rs' --file 'src/command/**/*.rs' \
            --shard "$shard/$shards" --jobs 2 --minimum-test-timeout 60 --output mutants || status=$?
          # Exit 2 means some mutants survived: that is the finding, not a failure to run.
          if [ "$status" -ne 0 ] && [ "$status" -ne 2 ]; then exit "$status"; fi
      - name: Surviving mutants
        if: ${{ always() }}
        run: |
          if [ -f mutants/mutants.out/missed.txt ]; then
            { echo '### Surviving mutants'; echo; echo '```'; head -n 200 mutants/mutants.out/missed.txt; echo '```'; } >> "$GITHUB_STEP_SUMMARY"
          fi
      - name: Upload mutation results
        if: ${{ always() }}
        uses: @@UPLOAD@@
        with:
          name: mutation-${{ github.sha }}
          path: mutants
          if-no-files-found: warn
          retention-days: 30

  coverage:
    name: Coverage per layer (diagnostic)
    runs-on: @@RUNS_ON@@
    timeout-minutes: 45
    steps:
@@CHECKOUT@@
@@RUST@@
      - name: Install tools
        run: |
          rustup component add llvm-tools-preview
          cargo xtask test install-runner
          cargo xtask ci install-tool cargo-llvm-cov
          echo "$PWD/target/xtask-tools" >> "$GITHUB_PATH"
      - name: Measure
        run: cargo llvm-cov nextest --workspace --locked --no-report
      - name: Report per layer
        run: |
          layers="data engine command frontend"
          for layer in $layers; do
            others=""
            for other in $layers; do
              if [ "$other" != "$layer" ]; then others="$others|src/$other/"; fi
            done
            echo "### Layer: $layer" >> "$GITHUB_STEP_SUMMARY"
            cargo llvm-cov report --summary-only --ignore-filename-regex "(${others#|}|tests/|xtask/|crates/)" | tail -n 3 >> "$GITHUB_STEP_SUMMARY"
          done
          cargo llvm-cov report --lcov --output-path lcov.info
      - name: Upload coverage
        uses: @@UPLOAD@@
        with:
          name: coverage-${{ github.sha }}
          path: lcov.info
          retention-days: 30

  repro-build:
    name: Clean-room reproducibility build (non-blocking)
    runs-on: @@RUNS_ON@@
    timeout-minutes: 45
    steps:
@@CHECKOUT@@
@@RUST@@
      - name: Two clean-room unsigned builds with separate target directories
        run: |
          SOURCE_DATE_EPOCH="$(git log -1 --format=%ct 2>/dev/null || echo 0)"
          export SOURCE_DATE_EPOCH
          for n in 1 2; do
            CARGO_TARGET_DIR="$RUNNER_TEMP/repro-$n" \
              RUSTFLAGS="--remap-path-prefix=$PWD=/build --remap-path-prefix=$RUNNER_TEMP/repro-$n=/target" \
              cargo build -p awman-cli --bin awman --release --locked
          done
          mkdir -p repro
          sha256sum "$RUNNER_TEMP/repro-1/release/awman" "$RUNNER_TEMP/repro-2/release/awman" | tee repro/digests.txt
          a="$(sha256sum "$RUNNER_TEMP/repro-1/release/awman" | cut -d' ' -f1)"
          b="$(sha256sum "$RUNNER_TEMP/repro-2/release/awman" | cut -d' ' -f1)"
          if [ "$a" != "$b" ]; then
            echo "::warning title=Not reproducible::two clean-room builds of this commit differ"
            exit 1
          fi
      - name: Upload digests
        if: ${{ always() }}
        uses: @@UPLOAD@@
        with:
          name: repro-${{ github.sha }}
          path: repro
          if-no-files-found: warn
          retention-days: 30

  audit:
    name: Dependency audit (advisories)
    runs-on: @@RUNS_ON@@
    timeout-minutes: 20
    steps:
@@CHECKOUT@@
@@RUST@@
      - name: Install cargo-deny
        run: |
          cargo xtask ci install-tool cargo-deny
          echo "$PWD/target/xtask-tools/bin" >> "$GITHUB_PATH"
      - name: Check advisories against the current database
        run: cargo deny check advisories

  ci-trends:
    name: Timing trends (p50/p95, queue time, flake rate, cache hit rate)
    needs: full
    if: ${{ !cancelled() }}
    runs-on: @@RUNS_ON@@
    timeout-minutes: 20
    permissions:
      contents: read
      actions: read
    env:
      GH_TOKEN: ${{ github.token }}
    steps:
@@CHECKOUT@@
@@RUST@@
      - name: Collect the last 30 days of timing artifacts
        run: |
          mkdir -p trends/raw
          since="$(date -u -d '30 days ago' +%FT%TZ)"
          gh api --paginate "repos/$GITHUB_REPOSITORY/actions/artifacts?per_page=100" \
            --jq ".artifacts[] | select((.name | startswith(\"timing-\")) and .expired == false and .created_at >= \"$since\") | .id" |
          while IFS= read -r id; do
            gh api "repos/$GITHUB_REPOSITORY/actions/artifacts/$id/zip" > "trends/raw/$id.zip"
            unzip -q -o "trends/raw/$id.zip" -d "trends/raw/$id" || true
          done
          find trends/raw -name history.jsonl -exec cat {} + > trends/history.jsonl || true
          test -s trends/history.jsonl || echo "no timing history yet" >&2
      - name: Report and publish ci-trends
        run: |
          if [ -s trends/history.jsonl ]; then
            cargo xtask evidence history report --store trends/history.jsonl | tee trends/report.txt >> "$GITHUB_STEP_SUMMARY"
            cargo xtask ci timings --store trends/history.jsonl --out trends/timings.json
          else
            echo '{"suite_durations_ms":{},"main_durations_ms":[]}' > trends/timings.json
          fi
      - name: Upload ci-trends
        uses: @@UPLOAD@@
        with:
          name: ci-trends
          path: trends
          if-no-files-found: error
          retention-days: 90

  nightly:
    name: Nightly verdict
    needs: [full, flake-hunt, seeded-defects, mutation, coverage, repro-build, audit, ci-trends]
    if: ${{ always() }}
    runs-on: @@RUNS_ON@@
    timeout-minutes: 10
    steps:
      - name: Verdict (fails closed on required jobs)
        env:
          NEEDS: ${{ toJSON(needs) }}
        run: |
          blocking="full flake-hunt seeded-defects audit"
          diagnostic="mutation coverage repro-build ci-trends"
          status=0
          for job in $blocking; do
            result="$(printf '%s' "$NEEDS" | jq -r --arg j "$job" '.[$j].result')"
            if [ "$result" != success ]; then
              echo "FAIL: $job: $result" | tee -a "$GITHUB_STEP_SUMMARY" >&2
              status=1
            fi
          done
          for job in $diagnostic; do
            result="$(printf '%s' "$NEEDS" | jq -r --arg j "$job" '.[$j].result')"
            if [ "$result" != success ]; then
              echo "warning: $job: $result (diagnostic, does not fail the verdict)" | tee -a "$GITHUB_STEP_SUMMARY"
            fi
          done
          exit "$status"
"####;

pub fn render(m: &Manifest) -> Result<String> {
    let linux = &m.platforms["linux-x64"];
    let mut out = header("Nightly");
    out.push('\n');
    out.push_str(TOP);
    out.push_str(&tpl(
        HEAD,
        &[
            ("RUNS_ON", &runs_on(&linux.runs_on)),
            (
                "CHECKOUT",
                &indent(&checkout_ref(false, "${{ github.sha }}"), 6),
            ),
            ("RUST", &indent(&rust(&linux.toolchain, &[]), 6)),
            ("UPLOAD", UPLOAD),
        ],
    ));
    let artifact_jobs = artifacts(m)?;
    out = out.replace(
        "needs: [full, flake-hunt",
        &format!("needs: [{}, full, flake-hunt", artifact_jobs.0.join(", ")),
    );
    out = out.replace(
        "blocking=\"full flake-hunt seeded-defects audit\"",
        &format!(
            "blocking=\"{} full flake-hunt seeded-defects audit\"",
            artifact_jobs.0.join(" ")
        ),
    );
    out.push_str(&artifact_jobs.1);
    out = out.replace("needs: [artifact-", "needs: [kit-relocation, artifact-");
    out = out.replace(
        "blocking=\"artifact-",
        "blocking=\"kit-relocation artifact-",
    );
    out.push_str(&format!("\n  kit-relocation:\n    name: Standalone kit relocation\n    runs-on: {}\n    timeout-minutes: 10\n    steps:\n{}\n{}\n      - name: Install pinned runner\n        run: cargo xtask test install-runner\n      - name: Run registered relocation contract\n        run: cargo xtask test run --suite kit-relocation --build linux-x64-default --evidence-dir evidence\n      - name: Retain execution evidence\n        if: ${{{{ always() }}}}\n        uses: {UPLOAD}\n        with:\n          name: kit-relocation\n          path: evidence\n          if-no-files-found: error\n          retention-days: 30\n", runs_on(&linux.runs_on), indent(&checkout_ref(false, "${{ github.sha }}"),6), indent(&rust(&linux.toolchain,&[]),6)));

    out = out.replace(
        "needs: [kit-relocation, artifact-",
        "needs: [sandbox-coverage, kit-relocation, artifact-",
    );
    out = out.replace(
        "diagnostic=\"mutation",
        "diagnostic=\"sandbox-coverage mutation",
    );
    out.push_str(&sandbox_coverage(m)?);
    Ok(out)
}

/// Scheduled checks use shipping features, exact signed bytes and all declared
/// artifact suites. Native boot is deliberately a separate operator obligation.
fn artifacts(m: &Manifest) -> Result<(Vec<String>, String)> {
    let mut jobs = Vec::new();
    let mut text = String::new();
    let plan = crate::gates::release_plan::create(m)?;
    let suites = &m.gates["nightly-artifacts"].cells[0].suites.join(",");
    for a in plan["assets"].as_array().expect("asset array") {
        let build = a["build"].as_str().expect("build");
        let b = &m.builds[build];
        let p = &m.platforms[&b.platform];
        let job = format!("artifact-{build}");
        jobs.push(job.clone());
        text.push_str(&format!("\n  {job}:\n    name: Scheduled artifact {build}\n    runs-on: {}\n    timeout-minutes: 45\n    steps:\n{}\n{}\n", runs_on(&p.runs_on), indent(&checkout_ref(false, "${{ github.sha }}"), 6), indent(&rust(&p.toolchain, &[]), 6)));
        if b.features == "builtin" {
            text.push_str(&format!("      - name: Verified payload\n        run: bash tools/msb-payloads/fetch.sh {}\n", p.triple));
        }
        let cargo = a["cargo"]
            .as_array()
            .expect("cargo")
            .iter()
            .map(|s| s.as_str().expect("arg"))
            .collect::<Vec<_>>()
            .join(" ");
        text.push_str(&format!(
            "      - name: Build shipping executable\n        run: cargo {cargo}\n"
        ));
        let binary = a["binary"].as_str().expect("binary");
        if a["sign"] == true {
            text.push_str(&format!("      - name: Sign once before validation\n        run: bash tools/macos-sign.sh {binary}\n"));
        }
        if p.triple.contains("windows") {
            text.push_str(&format!("      - name: Exact executable contracts with verified firewall\n        shell: pwsh\n        run: |\n          cargo xtask test install-runner\n          $digest = (Get-FileHash -Algorithm SHA256 {binary}).Hash.ToLowerInvariant()\n          ./ci/artifact-offline.ps1 -Artifact {binary} -Sha256 $digest -Build {build} -Suites '{suites}' -Evidence evidence\n"));
        } else {
            text.push_str(&format!("      - name: Exact executable contracts\n        run: |\n          cargo xtask test install-runner\n          digest=\"$(shasum -a 256 {binary} | cut -d' ' -f1)\"\n          AWMAN_TEST_HOST_RESOURCES=offline cargo xtask test run --suite {suites} --build {build} --artifact \"$PWD/{binary}\" --artifact-sha256 \"$digest\" --evidence-dir evidence\n"));
        }
        text.push_str(&format!("      - name: Retain complete inventory, command exits and JUnit\n        if: ${{{{ always() }}}}\n        uses: {UPLOAD}\n        with:\n          name: nightly-{job}\n          path: evidence\n          if-no-files-found: error\n          retention-days: 30\n"));
    }
    Ok((jobs, text))
}

/// Execute the registered cell when provisioned; absent labels are infrastructure.
fn sandbox_coverage(m: &Manifest) -> Result<String> {
    use anyhow::Context;
    let cells = crate::manifest::validate::cells(m, "native-diagnostic")?;
    let cell = cells
        .iter()
        .find(|c| c.suite == "boundary-sbx")
        .context("missing registered boundary-sbx cell")?;
    let platform = &m.platforms[&cell.platform];
    if platform.runs_on.is_empty() {
        return Ok(format!("\n  sandbox-coverage:\n    name: Sandbox coverage\n    runs-on: {}\n    timeout-minutes: 5\n    steps:\n      - name: Report unprovisioned platform\n        run: |\n          echo 'BLOCKED [infrastructure]: {} has no runs_on labels; registered cell {} cannot execute' | tee -a \"$GITHUB_STEP_SUMMARY\"\n          exit 2\n", runs_on(&m.platforms["linux-x64"].runs_on), cell.platform, cell.id));
    }
    let build = &m.builds[&cell.build];
    let payload = if build.features == "builtin" {
        format!("      - name: Verified builtin payload\n        run: |\n          bash tools/msb-payloads/fetch.sh {}\n          bash tools/msb-payloads/verify.sh {}\n", platform.triple, platform.triple)
    } else {
        String::new()
    };
    let grants = m.suites[&cell.suite].requires.join(",");
    Ok(format!("\n  sandbox-coverage:\n    name: Sandbox coverage\n    runs-on: {}\n    timeout-minutes: 30\n    env:\n      AWMAN_TEST_HOST_RESOURCES: {grants}\n      AWMAN_TEST_SBX_HOME: ${{{{ secrets.AWMAN_TEST_SBX_HOME }}}}\n      AWMAN_TEST_SBX_IDENTITY: ${{{{ secrets.AWMAN_TEST_SBX_IDENTITY }}}}\n      AWMAN_TEST_SBX_ANTHROPIC_KEY_FILE: ${{{{ secrets.AWMAN_TEST_SBX_ANTHROPIC_KEY_FILE }}}}\n      AWMAN_TEST_SBX_PROVIDER_KEYS_DIR: ${{{{ secrets.AWMAN_TEST_SBX_PROVIDER_KEYS_DIR }}}}\n    steps:\n{}\n{}\n{}      - name: Install pinned runner\n        run: cargo xtask test install-runner\n      - name: Execute registered cell {}\n        run: cargo xtask test run --suite boundary-sbx --build {} --evidence-dir evidence\n      - name: Retain boundary evidence\n        if: ${{{{ always() }}}}\n        uses: {UPLOAD}\n        with:\n          name: sandbox-coverage\n          path: evidence\n          if-no-files-found: error\n          retention-days: 30\n", runs_on(&platform.runs_on), indent(&checkout_ref(false, "${{ github.sha }}"), 6), indent(&rust(&platform.toolchain, &[]), 6), payload, cell.id, cell.build))
}

#[cfg(test)]
mod sbx_tests {
    #[test]
    fn provisioned_sbx_cell_has_all_grants_private_inputs_and_verified_payload() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let mut m = crate::manifest::load(root).unwrap();
        m.platforms.get_mut("macos-arm64-sbx").unwrap().runs_on = vec!["macos-15".into()];
        let workflow = super::sandbox_coverage(&m).unwrap();
        for value in [
            "docker-sbx,sbx-test-identity,public-network",
            "secrets.AWMAN_TEST_SBX_HOME",
            "secrets.AWMAN_TEST_SBX_IDENTITY",
            "secrets.AWMAN_TEST_SBX_ANTHROPIC_KEY_FILE",
            "tools/msb-payloads/verify.sh",
        ] {
            assert!(workflow.contains(value), "{value}: {workflow}");
        }
    }
}
