//! Publication obligations and the independently reviewed coverage floor.
use super::{validate::cells, Manifest};
use anyhow::{ensure, Context, Result};
use std::collections::BTreeSet;

pub const RELEASE_SUITES: &[&str] = &[
    "component-agent-profiles",
    "component-profile-commands",
    "component-core-internals",
    "component-command-models",
    "unit-awman",
    "unit-wsp",
    "component-wsp-terminals",
    "unit-test-support",
    "unit-xtask",
    "unit-host-shims",
    "component-data",
    "component-engine",
    "component-command",
    "component-command-edit-artifacts",
    "component-command-builtin",
    "component-command-daemon",
    "component-command-differential",
    "component-frontend",
    "component-daemon",
    "component-squad",
    "component-worker",
    "component-worker-terminal-exchange",
    "component-security",
    "component-tools",
    "component-parity-builtin",
    "component-parity-daemon",
    "component-parity-differential",
    "boundary-docker",
    "boundary-registry",
    "boundary-git",
    "boundary-conformance",
    "boundary-templates",
    "boundary-offline",
    "xtask-release-contract",
    "xtask-pipeline",
    "xtask-ci",
    "xtask-gates",
    "component-test-support",
];
pub const SBX_NATIVE: &str = "native.macos-arm64-sbx";
pub const SBX_SUITE: &str = "artifact-sbx-smoke";
pub const SBX_CASES: &[(&str, &str, &str, &str)] = &[
    (
        "claude",
        "claude_published_workload_ready_and_headless_task",
        "published-workload-with-mixin",
        "awman-claude-mixin",
    ),
    (
        "opencode",
        "opencode_owned_workload_ready_and_headless_task",
        "owned-workload",
        "awman-opencode",
    ),
];
pub fn native_tests(id: &str) -> Result<Vec<String>> {
    if id == SBX_NATIVE {
        return Ok(SBX_CASES
            .iter()
            .map(|(_, name, _, _)| format!("awman-cli::artifact sbx_smoke::{name}"))
            .collect());
    }
    let names: &[&str] = match id {
        "native.macos-arm64" => &[
            "signed_artifact_runs_an_offline_session",
            "signed_artifact_upgrades_previous_state_offline",
        ],
        "native.linux-x64" => &["linux_artifact_runs_an_offline_session"],
        _ => anyhow::bail!("unknown native obligation {id}"),
    };
    Ok(names
        .iter()
        .map(|n| format!("awman-cli::artifact signed_boot::{n}"))
        .collect())
}
pub fn validate(m: &Manifest) -> Result<()> {
    ensure!(
        m.release.assets
            == [
                "awman-linux-amd64",
                "awman-linux-arm64",
                "awman-macos-arm64",
                "awman-windows-amd64.exe"
            ],
        "all four shipping assets are required"
    );
    for (id, platform, features) in [
        ("linux-x64-default", "linux-x64", "default"),
        ("linux-x64-builtin", "linux-x64-release", "builtin"),
    ] {
        let b = m.builds.get(id).context("missing release CI build")?;
        ensure!(
            b.platform == platform
                && b.features == features
                && b.profile == "test"
                && b.kind == "test",
            "release CI build features/platform changed: {id}"
        );
    }
    for (id, triple, features) in [
        ("linux-x64-release", "x86_64-unknown-linux-gnu", "builtin"),
        (
            "linux-arm64-release",
            "aarch64-unknown-linux-gnu",
            "default",
        ),
        ("macos-arm64-release", "aarch64-apple-darwin", "builtin"),
        ("windows-x64-release", "x86_64-pc-windows-msvc", "default"),
    ] {
        let b = m.builds.get(id).context("missing shipping build")?;
        ensure!(
            m.platforms[&b.platform].triple == triple
                && b.features == features
                && b.profile == "release"
                && b.kind == "release",
            "shipping build features/platform changed: {id}"
        );
    }
    ensure!(
        m.release_critical.iter().all(|s| s.not_overridable),
        "release-critical results cannot be overridden"
    );
    let switch_tests: BTreeSet<_> = m.shipped_switch.iter().map(|s| s.proof.clone()).collect();
    ensure!(
        m.release_critical
            .iter()
            .any(|s| s.id == "artifact.access-reduction"
                && s.tests.iter().cloned().collect::<BTreeSet<_>>() == switch_tests),
        "exact-asset access reduction catalogue differs from shipped switches"
    );
    let release = &m.gates["release"];
    ensure!(
        release.includes.is_empty() && release.requires_gate.is_none(),
        "release must not inherit broad gates"
    );
    let expected: BTreeSet<_> = RELEASE_SUITES
        .iter()
        .map(|s| format!("{s}@linux-x64-default"))
        .chain(["unit-awman-builtin@linux-x64-builtin".into()])
        .collect();
    ensure!(
        cells(m, "release")?
            .iter()
            .map(|c| c.id.clone())
            .collect::<BTreeSet<_>>()
            == expected,
        "release cell set differs from reviewed policy"
    );
    ensure!(
        m.gates["native-diagnostic"].execution.as_deref() == Some("operator"),
        "native-diagnostic requires operator execution"
    );
    ensure!(
        m.release.gate == "release" && m.release.workflow == ".github/workflows/release.yml",
        "invalid release gate/workflow"
    );
    ensure!(
        m.release.smoke_suites
            == [
                "artifact-smoke",
                "artifact-entrypoints",
                "artifact-switches"
            ],
        "exact-asset smoke set changed"
    );
    ensure!(
        m.release.evidence_valid_days > 0
            && i64::from(m.release.retention_days) >= m.release.evidence_valid_days,
        "invalid evidence validity/retention"
    );
    ensure!(
        m.release.native.len() == 3,
        "two required native checks and the opt-in SBX diagnostic must remain registered"
    );
    for (id, asset, suite, fixture, arch) in [
        (
            "native.macos-arm64",
            "awman-macos-arm64",
            "artifact-signed-boot",
            "guest-oci-base",
            "arm64",
        ),
        (
            "native.linux-x64",
            "awman-linux-amd64",
            "artifact-linux-boot",
            "guest-oci-base-amd64",
            "amd64",
        ),
    ] {
        let n = m
            .release
            .native
            .get(id)
            .context("missing native obligation")?;
        ensure!(
            n.required
                && n.asset == asset
                && n.suites == [suite]
                && n.fixtures == [fixture]
                && n.budget_seconds > 0,
            "invalid required native obligation {id}"
        );
        ensure!(
            ["local", "ssh"].contains(&n.host.as_str()),
            "invalid native host"
        );
        ensure!(
            m.fixtures
                .get(fixture)
                .context("missing native fixture")?
                .arch
                .as_deref()
                == Some(arch),
            "native fixture architecture mismatch"
        );
        let expected_previous = if id == "native.macos-arm64" {
            Some("aarch64-apple-darwin")
        } else {
            None
        };
        ensure!(
            n.previous_release.as_deref() == expected_previous,
            "native predecessor selection changed"
        );
        if let Some(previous) = &n.previous_release {
            ensure!(
                m.previous_release.contains_key(previous),
                "missing native predecessor pin"
            );
        }
        let tests = native_tests(id)?;
        ensure!(
            m.release_critical.iter().any(|s| s.suite == suite
                && s.not_overridable
                && tests.iter().all(|t| s.tests.contains(t))),
            "missing native critical test IDs"
        );
    }
    let sbx = m
        .release
        .native
        .get(SBX_NATIVE)
        .context("missing SBX native obligation")?;
    ensure!(
        !sbx.required
            && sbx.asset == "awman-macos-arm64"
            && sbx.host == "local"
            && sbx.suites == [SBX_SUITE]
            && sbx.fixtures.is_empty()
            && sbx.previous_release.is_none()
            && sbx.budget_seconds == 1500,
        "invalid opt-in SBX diagnostic; experimental provider smoke must not gate release"
    );
    let suite = &m.suites[SBX_SUITE];
    ensure!(
        suite.tier == "artifact"
            && suite.package.as_deref() == Some("awman-cli")
            && suite.target.as_deref() == Some("test:artifact")
            && suite.budget_seconds == Some(600)
            && suite.requires == ["docker-sbx", "sbx-test-identity", "public-network"],
        "invalid SBX suite"
    );
    let filter = native_filter(SBX_NATIVE)?;
    ensure!(
        suite.filter.as_deref() == Some(&filter),
        "SBX suite must select exactly the two smoke cases"
    );
    for (id, other) in &m.suites {
        ensure!(
            other.tier != "artifact"
                || id == SBX_SUITE
                || !other
                    .requires
                    .iter()
                    .any(|c| c == "docker-sbx" || c == "sbx-test-identity"),
            "SBX artifact grant restricted to smoke suite"
        );
    }
    for gate in m.gates.keys() {
        ensure!(
            !cells(m, gate)?.iter().any(|c| c.suite == SBX_SUITE),
            "SBX smoke is native-only"
        );
    }
    ensure!(
        m.overrides.iter().all(|o| o.gate != "release"),
        "release overrides forbidden"
    );
    for asset in &m.release.assets {
        let builds: Vec<_> = m
            .builds
            .values()
            .filter(|b| b.asset.as_ref() == Some(asset))
            .collect();
        ensure!(
            builds.len() == 1
                && builds[0].builder == "hosted"
                && m.platforms[&builds[0].platform].trust == "hosted",
            "asset {asset} requires one hosted builder"
        );
    }
    Ok(())
}

/// Publication-critical policy, independently comparable when main advances.
/// Advisory nightly scheduling does not invalidate passing release work.
pub fn digest(m: &Manifest) -> Result<String> {
    use std::collections::BTreeMap;
    let cells = cells(m, "release")?;
    let builds: BTreeSet<_> = cells
        .iter()
        .map(|c| c.build.as_str())
        .chain(
            m.builds
                .iter()
                .filter(|(_, b)| b.kind == "release")
                .map(|(id, _)| id.as_str()),
        )
        .collect();
    let suites: BTreeSet<_> = cells
        .iter()
        .map(|c| c.suite.as_str())
        .chain(m.release.smoke_suites.iter().map(String::as_str))
        .chain(
            m.release
                .native
                .values()
                .flat_map(|n| n.suites.iter().map(String::as_str)),
        )
        .collect();
    let build_policy: BTreeMap<_, _> = builds.iter().map(|id| (*id, &m.builds[*id])).collect();
    let suite_policy: BTreeMap<_, _> = suites.iter().map(|id| (*id, &m.suites[*id])).collect();
    let platforms: BTreeMap<_, _> = build_policy
        .values()
        .map(|b| (&b.platform, &m.platforms[&b.platform]))
        .collect();
    let fixtures: BTreeMap<_, _> = m
        .release
        .native
        .values()
        .flat_map(|n| &n.fixtures)
        .map(|id| (id, &m.fixtures[id]))
        .collect();
    let previous: BTreeMap<_, _> = m
        .release
        .native
        .values()
        .filter_map(|n| n.previous_release.as_ref())
        .map(|id| (id, &m.previous_release[id]))
        .collect();
    let mut source_inputs = BTreeMap::new();
    if !m.root.as_os_str().is_empty() {
        for path in [
            "aspec/architecture/security.md",
            "rust-toolchain.toml",
            ".config/nextest.toml",
            ".github/workflows/ci.yml",
            ".github/workflows/release.yml",
        ] {
            let path_on_disk = m.root.join(path);
            source_inputs.insert(
                path,
                if path_on_disk.exists() {
                    Some(crate::evidence::digest::sha256_file(&path_on_disk)?)
                } else {
                    None
                },
            );
        }
    }
    crate::evidence::digest::input_digest(&serde_json::json!({
        "schema":1,"release":m.release,"cells":cells,"builds":build_policy,"platforms":platforms,
        "features":m.features,"suites":suite_policy,"tiers":m.tiers,"tools":m.tools["nextest"],
        "fixtures":fixtures,"previous_release":previous,"critical":m.release_critical,
        "switches":m.shipped_switch,"quarantine":m.quarantine,"overrides":m.overrides,
        "test_env":m.test_env,"source_inputs":source_inputs
    }))
}

/// Stable exact test selection shared by kit production and qualification evidence.
pub fn native_filter(id: &str) -> Result<String> {
    let mut tests = native_tests(id)?;
    tests.sort();
    Ok(tests
        .iter()
        .map(|t| format!("test(={})", t.split_once(' ').unwrap().1))
        .collect::<Vec<_>>()
        .join(" | "))
}
