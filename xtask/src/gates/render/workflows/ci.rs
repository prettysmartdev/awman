//! Renders `.github/workflows/ci.yml`.
//!
//! The graph (design 11.3, with the job names of WI 4.2): `plan` -> `lint`,
//! `build-<build id>` (one nextest archive + debug binary per build cell),
//! `test-<tier>-<platform>` consuming those archives, `boundary-linux-docker`,
//! `static-musl`, `builtin-hermetic-linux-arm64`, `web`, guest tiers on the
//! self-hosted CI Mac, and `gate`, the only required check. Which of them run
//! is decided at run time by `plan` (path selection, gate); what exists, on
//! which runner and with which toolchain is decided here, from the manifest.

use super::graph::{self, TestJob};
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

/// Literal per-job timeout (the lint requires literals); all within the
/// manifest's workflow cap.
fn timeout(tier: &str) -> u64 {
    match tier {
        "unit" => 20,
        "component" => 25,
        "boundary" => 30,
        "guest" | "soak" => 45,
        _ => 30,
    }
}

const TRIGGERS: &str = r##"on:
  pull_request:
  merge_group:
  push:
    branches: [main]
  workflow_dispatch:
    inputs:
      gate:
        description: Gate to run (hardware jobs only ever run from main)
        type: choice
        options: [pr, main, nightly]
        default: pr
  workflow_call:
    inputs:
      gate:
        description: "pr, main or nightly"
        type: string
        default: pr
      ref:
        description: Commit to check out and gate (empty means the triggering commit)
        type: string
        default: ""
      mode:
        description: "gate, or a nightly mode run over the same build and test jobs"
        type: string
        default: gate
    outputs:
      verdict:
        description: passed, failed or blocked
        value: ${{ jobs.gate.outputs.verdict }}

# Read-only by default; no job in this file holds a write scope.
permissions:
  contents: read

# Only superseded pull-request runs are cancelled: main, merge-queue, nightly
# and called runs always finish so their evidence exists.
concurrency:
  group: ci-${{ github.workflow }}-${{ inputs.gate }}-${{ inputs.mode }}-${{ github.event.pull_request.number || github.run_id }}
  cancel-in-progress: ${{ github.event_name == 'pull_request' }}

env:
  CARGO_TERM_COLOR: always
  CARGO_INCREMENTAL: "0"
  RUST_BACKTRACE: "1"

defaults:
  run:
    shell: bash
"##;

const PLAN: &str = r##"  plan:
    name: Plan
    runs-on: @@RUNS_ON@@
    timeout-minutes: 15
    permissions:
      contents: read
      actions: read
    outputs:
      gate: ${{ steps.gate.outputs.gate }}
      builds: ${{ steps.outputs.outputs.builds }}
      jobs: ${{ steps.outputs.outputs.jobs }}
      shards: ${{ steps.outputs.outputs.shards }}
      hardware: ${{ steps.hardware.outputs.hardware }}
    steps:
@@CHECKOUT@@
@@RUST@@
      - name: Resolve the gate
        id: gate
        env:
          EVENT: ${{ github.event_name }}
          INPUT_GATE: ${{ inputs.gate }}
        run: |
          case "$EVENT" in
            pull_request|merge_group) gate='pr' ;;
            push) gate='main' ;;
            *) gate="${INPUT_GATE:-pr}" ;;
          esac
          case "$gate" in
            pr|main|nightly) ;;
            *) echo "unknown gate: $gate" >&2; exit 1 ;;
          esac
          echo "gate=$gate" >> "$GITHUB_OUTPUT"
      - name: Validate the manifest and every generated file
        run: cargo xtask gates check
      - name: Hardware admission (trusted refs only)
        id: hardware
        env:
          GATE: ${{ steps.gate.outputs.gate }}
          EVENT: ${{ github.event_name }}
          REF: ${{ github.ref }}
          REQUESTED_REF: ${{ inputs.ref }}
          SHA: ${{ github.sha }}
          RUNNER_ENABLED: ${{ vars.AWMAN_CI_GUEST_RUNNER }}
        run: |
          # Self-hosted CI hardware is reachable only from a protected-main
          # push, schedule or dispatch of the trusted gate on the commit that
          # triggered the run. Pull requests, merge groups (even from forks),
          # other refs and ref overrides never get it. The runner group's own
          # repository/workflow restriction is the server-side half (P1/P3).
          hardware=false
          case "$EVENT" in
            push|schedule|workflow_dispatch) ;;
            *) EVENT_OK=no ;;
          esac
          if [ "${EVENT_OK:-yes}" = yes ] \
            && [ "$GATE" != pr ] \
            && [ "$REF" = refs/heads/main ] \
            && { [ -z "$REQUESTED_REF" ] || [ "$REQUESTED_REF" = "$SHA" ]; } \
            && [ "$RUNNER_ENABLED" = enabled ]; then
            hardware=true
          fi
          echo "hardware=$hardware" >> "$GITHUB_OUTPUT"
      - name: Load recorded timings (diagnostic input to shard counts)
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          mkdir -p trends
          run_id="$(gh run list --repo "$GITHUB_REPOSITORY" --workflow nightly.yml --status success --limit 1 --json databaseId --jq '.[0].databaseId' 2>/dev/null || true)"
          if [ -n "$run_id" ] && gh run download "$run_id" --repo "$GITHUB_REPOSITORY" --name ci-trends --dir trends 2>/dev/null; then
            echo "using timings from nightly run $run_id"
          else
            echo "no recorded timings yet; every suite plans as one shard"
          fi
      - name: Plan the gate
        env:
          GATE: ${{ steps.gate.outputs.gate }}
          EVENT: ${{ github.event_name }}
          REPOSITORY_ID: ${{ github.repository_id }}
          PR_BASE: ${{ github.event.pull_request.base.sha }}
          MG_BASE: ${{ github.event.merge_group.base_sha }}
          PUSH_BASE: ${{ github.event.before }}
          HEAD_SHA: ${{ inputs.ref || github.sha }}
        run: |
          base="${PR_BASE:-${MG_BASE:-$PUSH_BASE}}"
          case "$base" in 0000000000000000000000000000000000000000) base= ;; esac
          args=(--gate "$GATE" --event "$EVENT" --repository-id "$REPOSITORY_ID" --head "$HEAD_SHA" --json plan.json)
          if [ -n "$base" ]; then args+=(--base "$base"); fi
          if [ -f trends/timings.json ]; then args+=(--timings trends/timings.json); fi
          cargo xtask gates plan "${args[@]}"
      - name: Publish plan outputs
        id: outputs
        env:
          HARDWARE: ${{ steps.hardware.outputs.hardware }}
        run: cargo xtask ci plan-outputs --plan plan.json --hardware "$HARDWARE" >> "$GITHUB_OUTPUT"
      - name: Upload the plan
        uses: @@UPLOAD@@
        with:
          name: plan-${{ steps.gate.outputs.gate }}-${{ inputs.mode }}-${{ github.sha }}
          path: plan.json
          if-no-files-found: error
          retention-days: 30
"##;

const LINT: &str = r##"  lint:
    name: Lint
    runs-on: @@RUNS_ON@@
    timeout-minutes: 30
    env:
      GATE_NAME: ${{ github.event_name == 'pull_request' && 'pr' || github.event_name == 'merge_group' && 'pr' || github.event_name == 'push' && 'main' || inputs.gate || 'pr' }}
    steps:
@@CHECKOUT@@
@@RUST@@
      - name: Compute the cache key
        id: cache-key
        run: echo "key=$(cargo xtask ci cache-key --build @@BUILD@@ --scope lint)" >> "$GITHUB_OUTPUT"
      - name: Restore the cargo and build cache (exact key, no fallback)
        id: cache
        uses: @@CACHE_RESTORE@@
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ steps.cache-key.outputs.key }}
      - name: Install pinned tools (shellcheck, actionlint, cargo-deny)
        run: |
          cargo xtask ci install-tool shellcheck
          cargo xtask ci install-tool actionlint
          cargo xtask ci install-tool cargo-deny
      - name: Workflows (actions pinned to SHAs, timeouts, actionlint)
        if: ${{ !cancelled() }}
        run: |
          cargo xtask ci actionlint-config --out "$RUNNER_TEMP/actionlint.yaml"
          actionlint -config-file "$RUNNER_TEMP/actionlint.yaml" -color
      - name: Acceptance inventory
        run: cargo xtask matrix check
      - name: Lint suites with evidence (@@SUITES@@)
        if: ${{ !cancelled() }}
        run: bash tools/ci-test-home.sh cargo xtask test run --suite "@@SUITE_LIST@@" --build @@BUILD@@ --evidence-dir evidence
      - name: Upload evidence
        if: ${{ always() }}
        uses: @@UPLOAD@@
        with:
          name: evidence-${{ env.GATE_NAME }}-${{ inputs.mode }}-lint
          path: evidence
          if-no-files-found: warn
          retention-days: 30
      - name: Save the cache (main pushes only)
        if: ${{ github.ref == 'refs/heads/main' && github.event_name == 'push' && steps.cache.outputs.cache-hit != 'true' }}
        uses: @@CACHE_SAVE@@
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ steps.cache-key.outputs.key }}
"##;

fn lint(m: &Manifest) -> String {
    let suites = graph::lint_suites(m);
    let p = &m.platforms["linux-x64"];
    tpl(
        LINT,
        &[
            ("RUNS_ON", &runs_on(&p.runs_on)),
            ("CHECKOUT", &indent(&checkout(false), 6)),
            (
                "RUST",
                &indent(&rust(&p.toolchain, &[("components", "rustfmt, clippy")]), 6),
            ),
            ("BUILD", "linux-x64-default"),
            ("SUITES", &suites.len().to_string()),
            ("SUITE_LIST", &suites.join(",")),
            ("CACHE_RESTORE", CACHE_RESTORE),
            ("CACHE_SAVE", CACHE_SAVE),
            ("UPLOAD", UPLOAD),
        ],
    )
}

fn build_job(m: &Manifest, id: &str) -> String {
    let b = &m.builds[id];
    let p = &m.platforms[&b.platform];
    let job = graph::build_job_id(m, id);
    let hosted = p.trust == "hosted";
    let smoke = b.kind == "build-smoke";
    let mut cond = format!(
        "!cancelled() && needs.plan.result == 'success' && contains(fromJSON(needs.plan.outputs.builds), '{id}')"
    );
    if !hosted {
        cond.push_str(" && needs.plan.outputs.hardware == 'true' && github.ref == 'refs/heads/main' && github.event_name != 'pull_request' && github.event_name != 'merge_group'");
    }
    let mut s = format!(
        "  {job}:\n    name: Build {id}\n    needs: plan\n    if: ${{{{ {cond} }}}}\n    runs-on: {}\n    timeout-minutes: 30\n",
        runs_on(&p.runs_on)
    );
    if !smoke {
        s.push_str("    outputs:\n      archive_sha256: ${{ steps.digests.outputs.archive_sha256 }}\n      expected_sha256: ${{ steps.digests.outputs.expected_sha256 }}\n      binary_sha256: ${{ steps.digests.outputs.binary_sha256 }}\n");
    }
    s.push_str("    steps:\n");
    s.push_str(&indent(&checkout(false), 6));
    s.push('\n');
    let mut extra = Vec::new();
    let target;
    if let Some(t) = &b.target {
        target = t.clone();
        extra.push(("targets", target.as_str()));
    }
    s.push_str(&indent(&rust(&p.toolchain, &extra), 6));
    s.push('\n');
    if !hosted {
        s.push_str("      - name: Runner health (blocked early, never queued forever)\n        run: cargo xtask ci runner-preflight --platform ");
        s.push_str(&b.platform);
        s.push('\n');
    }
    s.push_str(&format!(
        "      - name: Compute the cache key\n        id: cache-key\n        run: echo \"key=$(cargo xtask ci cache-key --build {id})\" >> \"$GITHUB_OUTPUT\"\n      - name: Restore the cargo and build cache (exact key, no fallback)\n        id: cache\n        uses: {CACHE_RESTORE}\n        with:\n          path: |\n            ~/.cargo/registry\n            ~/.cargo/git\n            target\n          key: ${{{{ steps.cache-key.outputs.key }}}}\n"
    ));
    if smoke {
        let triple = b.target.clone().unwrap_or_else(|| p.triple.clone());
        let features = m.features[&b.features].cargo_args.join(" ");
        s.push_str(&format!(
            "      - name: Install the static C toolchain\n        run: sudo apt-get update && sudo apt-get install -y --no-install-recommends musl-tools\n      - name: Build the distribution executable ({triple})\n        run: cargo build -p awman-cli --bin awman --release --locked --target {triple} {features}\n      - name: Smoke test the static executable\n        run: |\n          artifact=target/{triple}/release/awman\n          \"$artifact\" --version\n          if readelf -l \"$artifact\" | grep -q INTERP; then\n            echo 'expected a static executable without a dynamic loader' >&2\n            exit 1\n          fi\n"
        ));
    } else {
        if b.features == "builtin" {
            s.push_str(&format!(
                "      - name: Prepare the verified builtin payload\n        run: bash tools/msb-payloads/fetch.sh {}\n",
                p.triple
            ));
        }
        s.push_str(&format!(
            "      - name: Install the pinned test runner\n        run: cargo xtask test install-runner\n      - name: Build the nextest archive and the debug binary\n        run: cargo xtask test archive --build {id} --out archive\n      - name: Record digests\n        id: digests\n        run: |\n          cp \"target/debug/awman$(if [ \"$RUNNER_OS\" = Windows ]; then echo .exe; fi)\" archive/awman-debug-{id}\n          cargo xtask ci digest record --out archive/digests.json \\\n            archive=archive/test-archive-{id}.tar.zst \\\n            expected=archive/expected-tests-{id}.json \\\n            binary=archive/awman-debug-{id} >> \"$GITHUB_OUTPUT\"\n      - name: Upload the archive\n        uses: {UPLOAD}\n        with:\n          name: test-archive-${{{{ needs.plan.outputs.gate }}}}-${{{{ inputs.mode }}}}-{id}\n          path: archive\n          if-no-files-found: error\n          retention-days: 7\n"
        ));
    }
    s.push_str(&format!(
        "      - name: Record cache statistics\n        if: ${{{{ always() }}}}\n        env:\n          CACHE_HIT: ${{{{ steps.cache.outputs.cache-hit }}}}\n          CACHE_KEY: ${{{{ steps.cache-key.outputs.key }}}}\n        run: cargo xtask ci cache-stats --build {id} --key \"$CACHE_KEY\" --hit \"${{CACHE_HIT:-false}}\" --out cache-stats\n      - name: Upload cache statistics\n        if: ${{{{ always() }}}}\n        uses: {UPLOAD}\n        with:\n          name: evidence-${{{{ needs.plan.outputs.gate }}}}-${{{{ inputs.mode }}}}-cache-{id}\n          path: cache-stats\n          if-no-files-found: warn\n          retention-days: 30\n      - name: Save the cache (main pushes only)\n        if: ${{{{ github.ref == 'refs/heads/main' && github.event_name == 'push' && steps.cache.outputs.cache-hit != 'true' }}}}\n        uses: {CACHE_SAVE}\n        with:\n          path: |\n            ~/.cargo/registry\n            ~/.cargo/git\n            target\n          key: ${{{{ steps.cache-key.outputs.key }}}}\n"
    ));
    if !hosted {
        s.push_str("      - name: Leak sweep\n        if: ${{ always() }}\n        run: cargo xtask ci sweep\n");
    }
    s
}

fn test_job(m: &Manifest, t: &TestJob) -> String {
    let p = &m.platforms[&t.platform];
    let build_job = graph::build_job_id(m, &t.build);
    let hosted = p.trust == "hosted";
    let mut cond = format!(
        "!cancelled() && needs.plan.result == 'success' && needs.{build_job}.result == 'success' && contains(fromJSON(needs.plan.outputs.jobs), '{}')",
        t.id
    );
    if !hosted {
        cond.push_str(" && needs.plan.outputs.hardware == 'true' && github.ref == 'refs/heads/main' && github.event_name != 'pull_request' && github.event_name != 'merge_group'");
    }
    let mut s = format!(
        "  {id}:\n    name: Test {tier} on {platform}\n    needs: [plan, {build_job}]\n    if: ${{{{ {cond} }}}}\n    runs-on: {runs_on}\n    timeout-minutes: {timeout}\n    strategy:\n      fail-fast: false\n      matrix:\n        shard: ${{{{ fromJSON(needs.plan.outputs.shards)['{id}'] || fromJSON('[{{\"i\":1,\"n\":1,\"suites\":\"\"}}]') }}}}\n",
        id = t.id,
        tier = t.tier,
        platform = t.platform,
        runs_on = runs_on(&p.runs_on),
        timeout = timeout(&t.tier),
    );
    if t.id == "boundary-linux-docker" || t.id.contains("private-registry") {
        s.push_str("    services:\n      registry:\n        image: registry:2.8.3\n        ports:\n          - 5000:5000\n");
    }
    s.push_str("    env:\n      GATE: ${{ needs.plan.outputs.gate }}\n      AWMAN_TEST_LEASE: ${{ strategy.job-index }}\n    steps:\n");
    s.push_str(&indent(&checkout(false), 6));
    s.push('\n');
    s.push_str(&indent(&rust(&p.toolchain, &[]), 6));
    s.push('\n');
    if t.id == "boundary-linux-docker" {
        s.push_str("      - name: Install the loopback Git transport\n        run: sudo apt-get update && sudo apt-get install -y --no-install-recommends openssh-server\n");
    }
    if !hosted {
        s.push_str(&format!(
            "      - name: Runner health (blocked early, never queued forever)\n        run: cargo xtask ci runner-preflight --platform {}\n",
            t.platform
        ));
    }
    let bid = &t.build;
    s.push_str(&format!(
        "      - name: Allocate this job attempt\n        run: cargo xtask ci sweep --init\n      - name: Download the test archive\n        uses: {DOWNLOAD}\n        with:\n          name: test-archive-${{{{ needs.plan.outputs.gate }}}}-${{{{ inputs.mode }}}}-{bid}\n          path: archive\n      - name: Verify the archive against the build job's recorded digests\n        env:\n          ARCHIVE_SHA256: ${{{{ needs.{build_job}.outputs.archive_sha256 }}}}\n          EXPECTED_SHA256: ${{{{ needs.{build_job}.outputs.expected_sha256 }}}}\n          BINARY_SHA256: ${{{{ needs.{build_job}.outputs.binary_sha256 }}}}\n        run: |\n          cargo xtask ci digest verify --file archive/digests.json \\\n            --expect \"archive=$ARCHIVE_SHA256\" \\\n            --expect \"expected=$EXPECTED_SHA256\" \\\n            --expect \"binary=$BINARY_SHA256\"\n      - name: Install the pinned test runner\n        run: cargo xtask test install-runner\n      - name: Fetch content-addressed fixtures\n        run: cargo xtask fixtures fetch --tier {tier}\n      - name: Run the suites from the archive\n        env:\n          SUITES: ${{{{ matrix.shard.suites }}}}\n          SHARD_I: ${{{{ matrix.shard.i }}}}\n          SHARD_N: ${{{{ matrix.shard.n }}}}\n          AWMAN_TEST_HOST_RESOURCES: ${{{{ matrix.shard.host_resources }}}}\n        run: |\n          bash tools/ci-test-home.sh cargo xtask test run --suite \"$SUITES\" --build {bid} \\\n            --archive archive/test-archive-{bid}.tar.zst \\\n            --shard \"$SHARD_I/$SHARD_N\" --evidence-dir evidence\n      - name: Leak sweep (processes, containers, VMs, temp roots of this run)\n        if: ${{{{ always() }}}}\n        run: cargo xtask ci sweep\n      - name: Upload evidence\n        if: ${{{{ always() }}}}\n        uses: {UPLOAD}\n        with:\n          name: evidence-${{{{ env.GATE }}}}-${{{{ inputs.mode }}}}-{id}-m${{{{ strategy.job-index }}}}\n          path: evidence\n          if-no-files-found: warn\n          retention-days: 30\n      - name: Upload failure records\n        if: ${{{{ failure() }}}}\n        uses: {UPLOAD}\n        with:\n          name: failure-${{{{ env.GATE }}}}-${{{{ inputs.mode }}}}-{id}-m${{{{ strategy.job-index }}}}\n          path: evidence/**/records\n          if-no-files-found: ignore\n          retention-days: 30\n",
        tier = t.tier,
        id = t.id,
    ));
    s
}

const WEB: &str = r##"  web:
    name: Web app, edge Worker and browser acceptance
    needs: plan
    if: ${{ !cancelled() && needs.plan.result == 'success' && contains(fromJSON(needs.plan.outputs.jobs), 'web') }}
    runs-on: @@RUNS_ON@@
    timeout-minutes: 40
    env:
      GATE: ${{ needs.plan.outputs.gate }}
    steps:
@@CHECKOUT@@
@@RUST@@
      - name: Install Node.js
        uses: @@NODE@@
        with:
          node-version: "22"
      - name: Install npm 11
        run: npm install --global npm@11.17.0
      - name: Install pinned web dependencies
        run: |
          npm --prefix web ci --ignore-scripts
          npm --prefix web/edge ci --ignore-scripts
          npm --prefix web/e2e ci --ignore-scripts
      - name: Install Playwright browsers
        env:
          PLAYWRIGHT_BROWSERS_PATH: ${{ runner.temp }}/ms-playwright
        run: npm --prefix web/e2e exec -- playwright install --with-deps chromium firefox
      - name: Web gates with evidence
        env:
          PLAYWRIGHT_BROWSERS_PATH: ${{ runner.temp }}/ms-playwright
        run: bash tools/ci-test-home.sh cargo xtask test run --suite web --build linux-x64-default --evidence-dir evidence
      - name: Upload evidence
        if: ${{ always() }}
        uses: @@UPLOAD@@
        with:
          name: evidence-${{ env.GATE }}-${{ inputs.mode }}-web
          path: evidence
          if-no-files-found: warn
          retention-days: 30
"##;

const GATE: &str = r##"  gate:
    name: @@REQUIRED@@
    needs: [@@NEEDS@@]
    if: ${{ always() }}
    runs-on: @@RUNS_ON@@
    timeout-minutes: 20
    permissions:
      contents: read
      actions: read
    outputs:
      verdict: ${{ steps.verdict.outputs.verdict }}
    env:
      GATE: ${{ needs.plan.outputs.gate }}
      GH_TOKEN: ${{ github.token }}
    steps:
@@CHECKOUT@@
@@RUST@@
      - name: Download the plan
        uses: @@DOWNLOAD@@
        with:
          name: plan-${{ needs.plan.outputs.gate }}-${{ inputs.mode }}-${{ github.sha }}
          path: plan
      - name: Download every cell's evidence
        uses: @@DOWNLOAD@@
        with:
          pattern: evidence-${{ env.GATE }}-${{ inputs.mode }}-*
          path: evidence-in
      - name: Aggregate (fails closed on missing, renamed, skipped or stale evidence)
        id: aggregate
        continue-on-error: true
        run: |
          mkdir -p gate-out
          cargo xtask evidence aggregate --gate "$GATE" --plan plan/plan.json --in evidence-in --out gate-out --summary gate-out/details.md
      - name: Job timings from the Actions API
        if: ${{ always() }}
        run: gh api --paginate "repos/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID/attempts/$GITHUB_RUN_ATTEMPT/jobs" --jq '.jobs' | jq -s 'add' > jobs.json
      - name: Artifact links (failure records)
        if: ${{ always() }}
        run: gh api --paginate "repos/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID/artifacts" --jq '.artifacts' | jq -s 'add' > artifacts.json
      - name: Gate summary (tier x platform, queue vs execution, slowest tests, cache, failures)
        if: ${{ always() }}
        run: |
          mkdir -p gate-out
          cargo xtask ci summary --plan plan/plan.json --gate-evidence gate-out/gate-evidence.json \
            --jobs jobs.json --artifacts artifacts.json --details gate-out/details.md \
            --evidence evidence-in --run-url "$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID" >> "$GITHUB_STEP_SUMMARY"
      - name: Append to the timing store
        if: ${{ always() }}
        run: |
          mkdir -p timing
          find evidence-in -name evidence.json -print0 | while IFS= read -r -d '' file; do
            cargo xtask evidence history append --store timing/history.jsonl --evidence "$file"
          done
      - name: Upload the gate evidence
        if: ${{ always() }}
        uses: @@UPLOAD@@
        with:
          name: gate-evidence-${{ env.GATE }}-${{ github.sha }}
          path: gate-out
          if-no-files-found: warn
          retention-days: 90
      - name: Upload the timing store entry (durable, 90 days)
        if: ${{ always() }}
        uses: @@UPLOAD@@
        with:
          name: timing-${{ env.GATE }}-${{ github.sha }}
          path: timing
          if-no-files-found: warn
          retention-days: 90
      - name: Verdict (fails closed)
        id: verdict
        if: ${{ always() }}
        env:
          NEEDS: ${{ toJSON(needs) }}
          AGGREGATE: ${{ steps.aggregate.outcome }}
        run: |
          verdict=failed
          if [ -f gate-out/gate-evidence.json ]; then
            verdict="$(jq -r '.verdict // "failed"' gate-out/gate-evidence.json)"
          fi
          bad="$(printf '%s' "$NEEDS" | jq -r 'to_entries[] | select(.value.result == "failure" or .value.result == "cancelled") | .key' | tr '\n' ' ')"
          if [ "$(printf '%s' "$NEEDS" | jq -r '.plan.result')" != success ]; then
            bad="$bad plan"
          fi
          if [ "$verdict" = passed ] && { [ "$AGGREGATE" != success ] || [ -n "$bad" ]; }; then
            verdict=failed
          fi
          echo "verdict=$verdict" >> "$GITHUB_OUTPUT"
          if [ -n "$bad" ]; then
            echo "jobs not successful: $bad" >&2
          fi
          if [ "$verdict" != passed ]; then
            echo "gate $GATE: $verdict (aggregate: $AGGREGATE)" >&2
            exit 1
          fi
"##;

const HARDWARE_BLOCKED: &str = r##"  hardware-blocked:
    name: Hardware cells blocked (no CI guest runner admitted)
    needs: plan
    if: ${{ !cancelled() && needs.plan.result == 'success' && needs.plan.outputs.hardware != 'true' && @@COND@@ }}
    runs-on: @@RUNS_ON@@
    timeout-minutes: 5
    steps:
      - name: Report blocked, visibly non-green
        env:
          GATE: ${{ needs.plan.outputs.gate }}
        run: |
          echo "BLOCKED: gate $GATE selects hardware cells but no CI guest runner is admitted (repository variable AWMAN_CI_GUEST_RUNNER is not 'enabled', or this ref/event may not use hardware). pending-cutover P1/P3." >&2
          echo "::error title=Hardware cells blocked::no admitted CI guest runner (pending-cutover P3)"
          exit 1
"##;

pub fn render(m: &Manifest) -> Result<String> {
    let linux = &m.platforms["linux-x64"];
    let hosted_runs_on = runs_on(&linux.runs_on);
    let mut jobs: Vec<(String, String)> = Vec::new();

    jobs.push((
        "plan".into(),
        tpl(
            PLAN,
            &[
                ("RUNS_ON", &hosted_runs_on),
                ("CHECKOUT", &indent(&checkout(true), 6)),
                ("RUST", &indent(&rust(&linux.toolchain, &[]), 6)),
                ("UPLOAD", UPLOAD),
            ],
        ),
    ));
    jobs.push(("lint".into(), lint(m)));
    for id in graph::ci_builds(m)? {
        jobs.push((graph::build_job_id(m, &id), build_job(m, &id)));
    }
    let tests = graph::ci_test_jobs(m)?;
    for t in &tests {
        jobs.push((t.id.clone(), test_job(m, t)));
    }
    jobs.push((
        "web".into(),
        tpl(
            WEB,
            &[
                ("RUNS_ON", &hosted_runs_on),
                ("CHECKOUT", &indent(&checkout(false), 6)),
                ("RUST", &indent(&rust(&linux.toolchain, &[]), 6)),
                ("NODE", NODE),
                ("UPLOAD", UPLOAD),
            ],
        ),
    ));
    let hw: Vec<String> = tests
        .iter()
        .filter(|t| graph::self_hosted(m, &t.platform))
        .map(|t| format!("contains(fromJSON(needs.plan.outputs.jobs), '{}')", t.id))
        .collect();
    if !hw.is_empty() {
        let cond = format!("({})", hw.join(" || "));
        jobs.push((
            "hardware-blocked".into(),
            tpl(
                HARDWARE_BLOCKED,
                &[("COND", &cond), ("RUNS_ON", &hosted_runs_on)],
            ),
        ));
    }
    let needs: Vec<String> = jobs.iter().map(|(id, _)| id.clone()).collect();
    let required = m.gates["pr"]
        .required_check
        .clone()
        .unwrap_or_else(|| "gate".into());
    jobs.push((
        "gate".into(),
        tpl(
            GATE,
            &[
                ("REQUIRED", &required),
                ("NEEDS", &needs.join(", ")),
                ("RUNS_ON", &hosted_runs_on),
                ("CHECKOUT", &indent(&checkout(false), 6)),
                ("RUST", &indent(&rust(&linux.toolchain, &[]), 6)),
                ("DOWNLOAD", DOWNLOAD),
                ("UPLOAD", UPLOAD),
            ],
        ),
    ));

    let mut out = header("CI");
    out.push('\n');
    out.push_str(TRIGGERS);
    out.push_str("\njobs:\n");
    for (i, (_, text)) in jobs.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(text.trim_end_matches('\n'));
        out.push('\n');
    }
    Ok(out)
}
