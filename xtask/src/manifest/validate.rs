use super::*;
use anyhow::{bail, ensure, Context, Result};
use chrono::NaiveDate;
use std::collections::{BTreeMap, BTreeSet};
#[path = "policy.rs"]
mod policy;

pub fn cells(m: &Manifest, gate: &str) -> Result<Vec<Cell>> {
    fn expand(
        m: &Manifest,
        gate: &str,
        stack: &mut BTreeSet<String>,
        out: &mut BTreeMap<String, Cell>,
    ) -> Result<()> {
        ensure!(
            stack.insert(gate.into()),
            "gate inheritance cycle at {gate}"
        );
        let g = m
            .gates
            .get(gate)
            .with_context(|| format!("unknown gate {gate}"))?;
        for parent in g.includes.iter().chain(g.requires_gate.iter()) {
            expand(m, parent, stack, out)?;
        }
        for group in &g.cells {
            ensure!(
                !group.suites.is_empty() && !group.builds.is_empty(),
                "empty cell group in {gate}"
            );
            for suite in &group.suites {
                ensure!(
                    m.suites.contains_key(suite),
                    "removed required suite {suite} in gate {gate}"
                );
                for build in &group.builds {
                    let b = m
                        .builds
                        .get(build)
                        .with_context(|| format!("unknown build {build}"))?;
                    let platform = group.platform.as_ref().unwrap_or(&b.platform).clone();
                    let id = format!(
                        "{suite}@{build}{}",
                        if platform == b.platform {
                            String::new()
                        } else {
                            format!("+{platform}")
                        }
                    );
                    ensure!(
                        out.insert(
                            id.clone(),
                            Cell {
                                id: id.clone(),
                                suite: suite.clone(),
                                build: build.clone(),
                                platform
                            }
                        )
                        .is_none(),
                        "duplicate cell {id} in {gate}"
                    );
                }
            }
        }
        for build in &g.build_smokes {
            let b = m
                .builds
                .get(build)
                .with_context(|| format!("unknown build smoke {build}"))?;
            ensure!(b.kind == "build-smoke", "{build} is not a build-smoke");
            let id = format!("build-{build}@{build}");
            out.insert(
                id.clone(),
                Cell {
                    id,
                    suite: format!("build-{build}"),
                    build: build.clone(),
                    platform: b.platform.clone(),
                },
            );
        }
        stack.remove(gate);
        Ok(())
    }
    let mut out = BTreeMap::new();
    expand(m, gate, &mut BTreeSet::new(), &mut out)?;
    Ok(out.into_values().collect())
}

pub fn expiry(m: &Manifest, today: NaiveDate) -> Result<()> {
    for q in &m.quarantine {
        ensure!(
            !q.test.is_empty() && !q.owner.is_empty() && q.issue.starts_with("https://"),
            "quarantine {} needs test, owner and issue",
            q.test
        );
        let deadline = NaiveDate::parse_from_str(&q.expires, "%Y-%m-%d")
            .context("invalid quarantine expiry")?;
        ensure!(
            deadline > today,
            "expired quarantine for {} (expires {}, controller UTC {})",
            q.test,
            q.expires,
            today
        );
    }
    for o in &m.overrides {
        ensure!(
            !o.reason.is_empty() && !o.approved_by.is_empty(),
            "override {} needs reason and approver",
            o.cell
        );
        ensure!(
            NaiveDate::parse_from_str(&o.expires, "%Y-%m-%d")? > today,
            "expired override {}",
            o.cell
        );
        let cell = cells(m, &o.gate)?
            .into_iter()
            .find(|c| c.id == o.cell)
            .context("override names an unknown cell")?;
        let suite = m
            .suites
            .get(&cell.suite)
            .context("build/integrity overrides are forbidden")?;
        ensure!(
            !["guest", "artifact"].contains(&suite.tier.as_str()),
            "hardware, boot, upgrade and artifact integrity override forbidden: {}",
            o.cell
        );
        ensure!(
            !m.release_critical
                .iter()
                .any(|s| s.suite == cell.suite && s.not_overridable),
            "release-critical override forbidden: {}",
            o.cell
        );
    }
    Ok(())
}

pub fn validate(m: &Manifest, today: NaiveDate) -> Result<()> {
    expiry(m, today)?;
    for id in policy::REQUIRED_SUITES {
        ensure!(m.suites.contains_key(*id), "removed required suite {id}");
    }
    for id in policy::REQUIRED_SCENARIOS {
        ensure!(
            m.release_critical.iter().any(|s| s.id == *id),
            "dropped release-critical scenario ID {id}"
        );
    }
    for tier in ["unit", "component", "boundary", "guest", "artifact", "soak"] {
        ensure!(m.tiers.contains_key(tier), "missing tier {tier}");
    }
    for gate in ["pr", "main", "nightly", "candidate", "promote"] {
        ensure!(m.gates.contains_key(gate), "missing gate {gate}");
    }
    for (id, t) in &m.tiers {
        for c in t.guarantees.iter().chain(&t.allowed_capabilities) {
            ensure!(
                m.capabilities.contains_key(c),
                "tier {id} names unknown capability {c}"
            );
        }
        if id != "soak" {
            ensure!(
                t.budget_seconds.is_some_and(|n| n > 0),
                "tier {id} needs a positive timeout"
            );
        }
    }
    // These are structural tier promises, not editable selection hints.
    for (tier, required) in [
        ("unit", &["tempdir"][..]),
        (
            "component",
            &[
                "tempdir",
                "loopback",
                "subprocess",
                "awman-binary",
                "fake-cli",
                "local-git",
            ][..],
        ),
        (
            "boundary",
            &[
                "tempdir",
                "loopback",
                "subprocess",
                "awman-binary",
                "fake-cli",
                "local-git",
            ][..],
        ),
        (
            "guest",
            &[
                "tempdir",
                "loopback",
                "subprocess",
                "awman-binary",
                "hypervisor",
                "builtin-payload",
                "guest-fixtures",
                "previous-release",
            ][..],
        ),
        (
            "artifact",
            &["tempdir", "loopback", "subprocess", "release-artifact"][..],
        ),
        (
            "soak",
            &["tempdir", "loopback", "subprocess", "awman-binary"][..],
        ),
    ] {
        for cap in required {
            ensure!(
                m.tiers[tier].guarantees.iter().any(|c| c == *cap),
                "capability {cap} removed from tier {tier}"
            );
        }
    }
    for (id, p) in &m.platforms {
        ensure!(
            !p.runs_on.is_empty() && !p.image.is_empty() && p.toolchain == "1.94.0",
            "platform {id} needs pinned image, labels and toolchain 1.94.0"
        );
        ensure!(
            ["hosted", "ci-guest", "release"].contains(&p.trust.as_str()),
            "unknown trust for {id}"
        );
        for cap in &p.provides {
            ensure!(
                m.capabilities.contains_key(cap),
                "platform {id}: unknown capability {cap}"
            );
        }
    }
    for (id, b) in &m.builds {
        ensure!(
            m.platforms.contains_key(&b.platform) && m.features.contains_key(&b.features),
            "build {id} has unknown platform or features"
        );
        ensure!(
            ["test", "release"].contains(&b.profile.as_str())
                && ["test", "release", "build-smoke"].contains(&b.kind.as_str()),
            "invalid build kind/profile {id}"
        );
    }
    for (id, s) in &m.suites {
        let t = m
            .tiers
            .get(&s.tier)
            .with_context(|| format!("suite {id}: unknown tier {}", s.tier))?;
        ensure!(
            !s.contracts.is_empty(),
            "suite {id} is missing explicit contracts"
        );
        match s.kind.as_str() {
            "nextest" => ensure!(
                s.package.is_some() && s.target.is_some(),
                "suite {id} needs package and target"
            ),
            "command" => ensure!(!s.command.is_empty(), "suite {id} needs a command"),
            "doctest" => ensure!(!s.packages.is_empty(), "suite {id} needs doctest packages"),
            _ => bail!("suite {id}: unknown kind {}", s.kind),
        }
        if s.tier == "soak" {
            ensure!(
                s.budget_seconds.is_some_and(|n| n > 0),
                "soak suite {id} needs its own timeout"
            );
        }
        for c in &s.requires {
            ensure!(
                t.guarantees.contains(c) || t.allowed_capabilities.contains(c),
                "suite {id}: capability {c} outside tier {}",
                s.tier
            );
        }
        for a in &s.applicability {
            ensure!(
                !a.test.is_empty()
                    && !a.reason.is_empty()
                    && m.platforms.contains_key(&a.platform)
                    && m.features.contains_key(&a.features),
                "invalid applicability in {id}"
            );
        }
    }
    for id in m.gates.keys() {
        for c in cells(m, id)? {
            let p = m
                .platforms
                .get(&c.platform)
                .context("cell has unknown platform")?;
            if id == "pr" {
                ensure!(
                    p.trust == "hosted",
                    "PR/merge-group cannot schedule self-hosted cell {}",
                    c.id
                );
            }
            if let Some(s) = m.suites.get(&c.suite) {
                for cap in m.tiers[&s.tier].guarantees.iter().chain(&s.requires) {
                    ensure!(p.provides.contains(cap), "cell {}: required capability {cap} removed from runner {}; coverage cannot disappear", c.id, c.platform);
                }
            }
        }
    }
    let pr = cells(m, "pr")?;
    for id in policy::REQUIRED_SUITES {
        let s = &m.suites[*id];
        if s.tier == "unit"
            || id.starts_with("component-")
            || id.starts_with("xtask-")
            || [
                "boundary-docker",
                "boundary-registry",
                "boundary-git",
                "boundary-conformance",
                "boundary-templates",
                "web",
            ]
            .contains(id)
        {
            ensure!(
                pr.iter().any(|c| c.suite == *id),
                "required suite {id} removed from PR gate"
            );
        }
        if (s.tier == "component"
            && id.starts_with("component-")
            && *id != "component-test-support")
            || (id.starts_with("unit-") && !id.ends_with("-builtin"))
        {
            for build in policy::HOSTED_BUILDS {
                ensure!(
                    pr.iter().any(|c| c.suite == *id && c.build == *build),
                    "required suite {id} missing hosted build {build}"
                );
            }
        }
    }
    let mut scenario_ids = BTreeSet::new();
    for s in &m.release_critical {
        ensure!(
            scenario_ids.insert(&s.id) && !s.tests.is_empty(),
            "duplicate or empty release-critical scenario {}",
            s.id
        );
        ensure!(
            m.suites.contains_key(&s.suite),
            "scenario {}: missing suite {}",
            s.id,
            s.suite
        );
        for gate in &s.gates {
            ensure!(
                cells(m, gate)?.iter().any(|c| c.suite == s.suite),
                "scenario {} is missing from gate {gate}",
                s.id
            );
        }
    }
    for s in &m.shipped_switch {
        ensure!(
            s.reduces_only && !s.proof.is_empty(),
            "shipped switch {} must reduce access and have a proof",
            s.name
        );
    }
    ensure!(
        m.gates["pr"].required_check.as_deref() == Some("gate"),
        "required check must be gate"
    );
    let workflow = m
        .workflows
        .get(".github/workflows/ci.yml")
        .context("missing required CI workflow contract")?;
    for role in ["plan", "lint", "build", "test", "gate"] {
        ensure!(
            workflow.jobs.iter().any(|j| j == role),
            "removed required workflow job expectation {role}"
        );
    }
    Ok(())
}

/// Every unverified pin in the manifest (`cargo xtask lint pins`).
pub fn pins(m: &Manifest) -> Vec<String> {
    let mut errors = tool_pins(m);
    errors.extend(fixture_pins(m));
    for (id, p) in &m.platforms {
        if p.image.starts_with("pending") {
            errors.push(format!("platform {id}: image digest pending provisioning"));
        }
    }
    errors
}

/// The unverified pins a gate depends on: every tool, the fixtures and the
/// previous release when the gate runs guest or artifact suites, and the image
/// of each platform the gate's cells run on. A pending pin outside the gate's
/// closure cannot make that gate red, and one inside it always does.
pub fn pins_for_gate(m: &Manifest, gate: &str) -> Vec<String> {
    let mut errors = tool_pins(m);
    let mut suites = std::collections::BTreeSet::new();
    let mut platforms = std::collections::BTreeSet::new();
    let mut pending = vec![gate.to_string()];
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some(g) = m.gates.get(&id) else {
            errors.push(format!("gate {id}: not declared"));
            continue;
        };
        pending.extend(g.includes.iter().cloned());
        pending.extend(g.requires_gate.iter().cloned());
        for cell in &g.cells {
            suites.extend(cell.suites.iter().cloned());
            for b in &cell.builds {
                if let Some(build) = m.builds.get(b) {
                    platforms.insert(
                        cell.platform
                            .clone()
                            .unwrap_or_else(|| build.platform.clone()),
                    );
                }
            }
        }
    }
    let needs_fixtures = suites.iter().any(|s| {
        m.suites.get(s).is_some_and(|s| {
            ["guest", "artifact"].contains(&s.tier.as_str())
                || s.requires
                    .iter()
                    .any(|r| r == "guest-fixtures" || r == "previous-release")
        })
    });
    if needs_fixtures {
        errors.extend(fixture_pins(m));
    }
    for id in platforms {
        if m.platforms
            .get(&id)
            .is_some_and(|p| p.image.starts_with("pending"))
        {
            errors.push(format!("platform {id}: image digest pending provisioning"));
        }
    }
    errors
}

/// Release-archive tools need a SHA-256 per platform. A crates.io tool is
/// installed with `cargo install --locked --version <pin>` and verified by
/// the registry index checksum, so it carries no per-platform digest.
fn tool_pins(m: &Manifest) -> Vec<String> {
    let mut errors = Vec::new();
    for (id, t) in &m.tools {
        let crates_io = t.source.starts_with("https://crates.io/crates/") && t.sha256.is_empty();
        let digests_ok = !t.sha256.is_empty()
            && t.sha256
                .values()
                .all(|d| crate::evidence::digest::is_sha256(d));
        if t.pending.is_some() || !(crates_io || digests_ok) {
            errors.push(format!("tool {id}: missing verified installer digests"));
        }
    }
    errors
}

fn fixture_pins(m: &Manifest) -> Vec<String> {
    let mut errors = Vec::new();
    for (id, f) in &m.fixtures {
        if f.verified_record.is_none()
            && !f
                .sha256
                .as_ref()
                .is_some_and(|d| crate::evidence::digest::is_sha256(d))
        {
            errors.push(format!(
                "fixture {id}: missing published digest ({})",
                f.pending.as_deref().unwrap_or("not pinned")
            ));
        }
    }
    for (id, p) in &m.previous_release {
        if !p
            .sha256
            .as_ref()
            .is_some_and(|d| crate::evidence::digest::is_sha256(d))
        {
            errors.push(format!(
                "previous release {id}: missing authenticated asset digest"
            ));
        }
    }
    errors
}
