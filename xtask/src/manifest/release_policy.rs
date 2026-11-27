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
pub const APPLE_NATIVE: &str = "native.macos-arm64-apple";
pub const DOCKER_NATIVE: &str = "native.linux-x64-docker";
pub const APPLE_SUITE: &str = "artifact-apple-smoke";
pub const DOCKER_SUITE: &str = "artifact-docker-smoke";
pub const SBX_SMOKE_MAC: &str = "sbx.macos-arm64";
pub const SBX_SMOKE_LINUX: &str = "sbx.linux-x64";
pub const SBX_SMOKE_STEPS: &[&str] = &[
    "version", "session", "create", "inspect", "exec", "run", "stop", "restart", "remove",
    "cleanup",
];
pub const SBX_SMOKE_AGENT: &str = "claude";

/// Independently checked grants; kit documents cannot widen host access.
pub fn native_grants(id: &str) -> Result<&'static [&'static str]> {
    Ok(match id {
        "native.macos-arm64" | "native.linux-x64" => &["hypervisor", "offline"],
        APPLE_NATIVE => &["apple-containers"],
        DOCKER_NATIVE => &["docker"],
        SBX_NATIVE => &["docker-sbx", "sbx-test-identity", "public-network"],
        _ => anyhow::bail!("unknown native obligation {id}"),
    })
}
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
    for (native, module) in [
        (APPLE_NATIVE, "apple_smoke"),
        (DOCKER_NATIVE, "docker_smoke"),
    ] {
        if id == native {
            return Ok(vec![format!("awman-cli::artifact {module}::exact_asset_runs_a_synthetic_session_in_an_owned_container")]);
        }
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
        m.release.native.len() == 5,
        "four required native checks and the opt-in SBX diagnostic must remain registered"
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
    for (id, asset, host, suite, fixture, arch, cap, group, critical) in [
        (
            APPLE_NATIVE,
            "awman-macos-arm64",
            "local",
            APPLE_SUITE,
            "guest-oci-base",
            "arm64",
            "apple-containers",
            "apple",
            "artifact.apple-container-session",
        ),
        (
            DOCKER_NATIVE,
            "awman-linux-amd64",
            "ssh",
            DOCKER_SUITE,
            "guest-oci-base-amd64",
            "amd64",
            "docker",
            "docker",
            "artifact.docker-container-session",
        ),
    ] {
        let n = m
            .release
            .native
            .get(id)
            .context("missing runtime native obligation")?;
        ensure!(
            n.required
                && n.asset == asset
                && n.host == host
                && n.suites == [suite]
                && n.fixtures == [fixture]
                && n.previous_release.is_none()
                && n.budget_seconds == 480,
            "invalid runtime native obligation {id}"
        );
        ensure!(
            m.fixtures
                .get(fixture)
                .context("missing runtime fixture")?
                .arch
                .as_deref()
                == Some(arch),
            "runtime fixture architecture mismatch"
        );
        let s = m.suites.get(suite).context("missing runtime suite")?;
        ensure!(
            s.tier == "artifact"
                && s.kind == "nextest"
                && s.package.as_deref() == Some("awman-cli")
                && s.target.as_deref() == Some("test:artifact")
                && s.contracts == [suite]
                && s.requires == [cap, "guest-fixtures"]
                && s.budget_seconds == Some(420)
                && s.serial
                && s.group.as_deref() == Some(group)
                && s.filter.as_deref() == Some(native_filter(id)?.as_str()),
            "invalid runtime suite {suite}"
        );
        let tests = native_tests(id)?;
        ensure!(
            m.release_critical
                .iter()
                .filter(|c| c.suite == suite)
                .count()
                == 1,
            "runtime critical inventory must have one declaration"
        );
        ensure!(
            m.release_critical.iter().any(|c| c.id == critical
                && c.suite == suite
                && c.not_overridable
                && c.tests == tests
                && c.gates.is_empty()),
            "runtime critical test inventory differs: {id}"
        );
    }
    ensure!(
        m.release.sbx_smoke.len() == 2,
        "exactly two SBX readiness declarations required"
    );
    for (id, asset, triple, host, selection) in [
        (
            SBX_SMOKE_MAC,
            "awman-macos-arm64",
            "aarch64-apple-darwin",
            "local",
            "always",
        ),
        (
            SBX_SMOKE_LINUX,
            "awman-linux-amd64",
            "x86_64-unknown-linux-gnu",
            "ssh",
            "explicit-linux-host",
        ),
    ] {
        let s = m
            .release
            .sbx_smoke
            .get(id)
            .context("missing SBX readiness declaration")?;
        ensure!(
            s.asset == asset
                && s.triple == triple
                && s.host == host
                && s.selection == selection
                && s.steps == SBX_SMOKE_STEPS
                && s.ready_agent == SBX_SMOKE_AGENT
                && s.min_cli_version == "0.54.0"
                && s.lifecycle_budget_seconds == 600
                && s.ready_budget_seconds == 420
                && s.budget_seconds == 1080,
            "invalid SBX readiness declaration {id}"
        );
    }
    ensure!(
        m.release
            .native
            .values()
            .all(|n| !n.required || !n.suites.iter().any(|s| s == SBX_SUITE)),
        "invalid opt-in SBX diagnostic; experimental provider smoke must not gate release"
    );
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
        for (cap, suite) in [("docker", DOCKER_SUITE), ("apple-containers", APPLE_SUITE)] {
            ensure!(
                other.tier != "artifact" || id == suite || !other.requires.iter().any(|c| c == cap),
                "runtime artifact grant restricted to {suite}"
            );
        }
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
            !cells(m, gate)?
                .iter()
                .any(|c| [SBX_SUITE, APPLE_SUITE, DOCKER_SUITE].contains(&c.suite.as_str())),
            "runtime smoke is native-only"
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

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> Manifest {
        crate::manifest::parse(include_bytes!("../../../ci/gates.toml")).unwrap()
    }
    #[test]
    fn native_inventory_and_grants_are_narrow_and_provider_sbx_stays_optional() {
        let m = manifest();
        validate(&m).unwrap();
        assert_eq!(m.release.native.values().filter(|n| n.required).count(), 4);
        assert!(!m.release.native[SBX_NATIVE].required);
        for (id, suite, cap, module) in [
            (APPLE_NATIVE, APPLE_SUITE, "apple-containers", "apple_smoke"),
            (DOCKER_NATIVE, DOCKER_SUITE, "docker", "docker_smoke"),
        ] {
            assert_eq!(native_grants(id).unwrap(), [cap]);
            assert_eq!(m.release.native[id].suites, [suite]);
            assert_eq!(native_tests(id).unwrap(), [format!("awman-cli::artifact {module}::exact_asset_runs_a_synthetic_session_in_an_owned_container")]);
            assert_eq!(
                native_filter(id).unwrap(),
                m.suites[suite].filter.clone().unwrap()
            );
        }
        assert!(native_grants("native.windows").is_err());
        assert!(native_tests("native.linux-arm64").is_err());
    }
    #[test]
    fn readiness_rejects_changed_host_asset_selection_steps_agent_and_budgets() {
        for id in [SBX_SMOKE_MAC, SBX_SMOKE_LINUX] {
            for field in [
                "asset",
                "triple",
                "host",
                "selection",
                "ready_agent",
                "min_cli_version",
            ] {
                let mut m = manifest();
                let s = m.release.sbx_smoke.get_mut(id).unwrap();
                let value = match field {
                    "asset" => &mut s.asset,
                    "triple" => &mut s.triple,
                    "host" => &mut s.host,
                    "selection" => &mut s.selection,
                    "ready_agent" => &mut s.ready_agent,
                    _ => &mut s.min_cli_version,
                };
                *value = "disallowed".into();
                assert!(validate(&m).is_err(), "{id}: {field}");
            }
            for field in 0..4 {
                let mut m = manifest();
                let s = m.release.sbx_smoke.get_mut(id).unwrap();
                match field {
                    0 => s.steps.swap(0, 1),
                    1 => s.lifecycle_budget_seconds += 1,
                    2 => s.ready_budget_seconds += 1,
                    _ => s.budget_seconds += 1,
                }
                assert!(validate(&m).is_err());
            }
        }
        let mut m = manifest();
        m.release.sbx_smoke.remove(SBX_SMOKE_LINUX);
        assert!(validate(&m).is_err());
        let mut m = manifest();
        m.release.sbx_smoke.insert(
            "sbx.linux-arm64".into(),
            m.release.sbx_smoke[SBX_SMOKE_LINUX].clone(),
        );
        assert!(validate(&m).is_err());
        let text = std::str::from_utf8(include_bytes!("../../../ci/gates.toml")).unwrap();
        assert!(crate::manifest::parse(
            text.replace(
                "selection = \"always\"",
                "selection = \"always\"\nsuites = [\"artifact-sbx-smoke\"]"
            )
            .as_bytes()
        )
        .is_err());
    }
    #[test]
    fn runtime_inventory_and_native_hosts_cannot_be_widened_or_waived() {
        for (id, suite, other) in [
            (APPLE_NATIVE, APPLE_SUITE, "docker"),
            (DOCKER_NATIVE, DOCKER_SUITE, "apple-containers"),
        ] {
            for field in 0..8 {
                let mut m = manifest();
                let n = m.release.native.get_mut(id).unwrap();
                match field {
                    0 => n.required = false,
                    1 => n.host = "other".into(),
                    2 => n.asset = "awman-linux-arm64".into(),
                    3 => n.budget_seconds += 1,
                    4 => n.suites.push(SBX_SUITE.into()),
                    5 => n.previous_release = Some("aarch64-apple-darwin".into()),
                    6 => m.suites.get_mut(suite).unwrap().requires.push(other.into()),
                    _ => m.suites.get_mut(suite).unwrap().filter = Some("all()".into()),
                }
                assert!(validate(&m).is_err(), "{id}: {field}");
            }
            let mut m = manifest();
            m.release_critical
                .iter_mut()
                .find(|c| c.suite == suite)
                .unwrap()
                .tests
                .push("awman-cli::artifact unexpected".into());
            assert!(validate(&m).is_err());
            let mut m = manifest();
            m.gates.get_mut("release").unwrap().cells[0]
                .suites
                .push(suite.into());
            assert!(validate(&m).is_err());
        }
        let mut m = manifest();
        m.release.native.get_mut(SBX_NATIVE).unwrap().required = true;
        assert!(validate(&m).is_err());
        for cap in ["apple-containers", "docker"] {
            let mut m = manifest();
            m.suites
                .get_mut("artifact-smoke")
                .unwrap()
                .requires
                .push(cap.into());
            assert!(validate(&m).is_err());
        }
    }
    #[test]
    fn readiness_and_runtime_changes_are_bound_into_policy_digest() {
        let m = manifest();
        let before = digest(&m).unwrap();
        let mut other = m.clone();
        other
            .release
            .sbx_smoke
            .get_mut(SBX_SMOKE_MAC)
            .unwrap()
            .ready_agent = "other".into();
        assert_ne!(before, digest(&other).unwrap());
        let mut other = m.clone();
        other.suites.get_mut(APPLE_SUITE).unwrap().filter = Some("all()".into());
        assert_ne!(before, digest(&other).unwrap());
    }
}
