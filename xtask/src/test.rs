//! `cargo xtask test`: run manifest suites with the pinned runner.
//!
//! Every local entry point (`make test`, `make check`, `make test-boundary`, …)
//! and every CI test job goes through `test run`, so a developer and CI use the
//! same suite selection, the same nextest profile (`.config/nextest.toml`,
//! rendered from `ci/gates.toml`) and the same per-test timeouts.
//!
//! Subcommands:
//!   install-runner                 download the pinned cargo-nextest, verify its SHA-256
//!   run  (--suite <ids> | --tier <ids> | --gate local) [options]
//!   list [--build <id>] [--tier <ids>]   the expected test set (nextest list JSON)
//!   archive --build <id> --out <dir>      nextest archive + expected set + digests
//!   update-golden [--suite <id>]          rewrite golden files / snapshots
pub(crate) mod isolation;

use anyhow::{bail, ensure, Context, Result};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use crate::manifest::{schema::Suite, Manifest};

/// The nextest profile every caller uses. Per-suite timeouts, groups and
/// retries live in its rendered overrides.
const PROFILE: &str = "default";

pub fn run(args: &[String]) -> Result<ExitCode> {
    let root = crate::gates::root();
    match args.first().map(String::as_str) {
        Some("install-runner") => install_runner(&root),
        Some("run") => run_suites(&root, &Options::parse(&args[1..])?),
        Some("list") => list(&root, &Options::parse(&args[1..])?),
        Some("archive") => archive(&root, &Options::parse(&args[1..])?),
        Some("update-golden") => update_golden(&root, &Options::parse(&args[1..])?),
        _ => {
            eprintln!(
                "usage: cargo xtask test install-runner | run (--suite <ids> | --tier <ids> | --gate local) \
                 [--build <id>] [--archive <file>] [--shard i/N] [--evidence-dir <dir>] \
                 [--runner nextest|libtest] [--changed] [--shuffle-seed N] | list | archive | update-golden"
            );
            Ok(ExitCode::from(2))
        }
    }
}

/// Command-line options shared by the subcommands.
#[derive(Debug, Default)]
struct Options {
    suites: Vec<String>,
    tiers: Vec<String>,
    gate: Option<String>,
    build: Option<String>,
    archive: Option<PathBuf>,
    shard: Option<String>,
    evidence_dir: Option<PathBuf>,
    out: Option<PathBuf>,
    runner: Runner,
    changed: bool,
    shuffle_seed: Option<u64>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Runner {
    #[default]
    Nextest,
    Libtest,
}

impl Options {
    fn parse(args: &[String]) -> Result<Options> {
        let mut o = Options::default();
        let mut i = 0;
        while i < args.len() {
            let flag = args[i].as_str();
            let value = || -> Result<String> {
                args.get(i + 1)
                    .cloned()
                    .with_context(|| format!("{flag} needs a value"))
            };
            match flag {
                "--suite" => o.suites.extend(split(&value()?)),
                "--tier" => o.tiers.extend(split(&value()?)),
                "--gate" => o.gate = Some(value()?),
                "--build" => o.build = Some(value()?),
                "--archive" => o.archive = Some(value()?.into()),
                "--shard" => o.shard = Some(value()?),
                "--evidence-dir" => o.evidence_dir = Some(value()?.into()),
                "--out" => o.out = Some(value()?.into()),
                "--shuffle-seed" => o.shuffle_seed = Some(value()?.parse()?),
                "--runner" => {
                    o.runner = match value()?.as_str() {
                        "nextest" => Runner::Nextest,
                        "libtest" => Runner::Libtest,
                        other => bail!("unknown runner {other}"),
                    }
                }
                "--changed" => {
                    o.changed = true;
                    i += 1;
                    continue;
                }
                other => bail!("unknown option {other}"),
            }
            i += 2;
        }
        Ok(o)
    }
}

fn split(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

// ── Suite selection ─────────────────────────────────────────────────────────

/// The suites a `run` invocation executes, in manifest order.
fn select<'m>(m: &'m Manifest, o: &Options, build: &str) -> Result<Vec<(&'m str, &'m Suite)>> {
    let mut chosen = Vec::new();
    for id in &o.suites {
        let (id, s) = m
            .suites
            .get_key_value(id)
            .with_context(|| format!("unknown suite {id}"))?;
        chosen.push((id.as_str(), s));
    }
    let mut tiers = o.tiers.clone();
    match o.gate.as_deref() {
        None => {}
        Some("local") => tiers.extend(["unit".to_string(), "component".to_string()]),
        Some(other) => {
            bail!("--gate {other}: only `local` is accepted (CI gates run planned suites with --suite)")
        }
    }
    for tier in &tiers {
        ensure!(
            m.tiers.contains_key(tier) || tier == "soak",
            "unknown tier {tier}"
        );
        let before = chosen.len();
        for (id, s) in &m.suites {
            // A tier selection runs test suites only. Lints and other command
            // suites run through `cargo xtask lint` or an explicit --suite.
            if &s.tier == tier
                && s.kind == "nextest"
                && suite_runs_on_build(m, id, build)
                && !chosen.iter().any(|(c, _)| c == id)
            {
                chosen.push((id.as_str(), s));
            }
        }
        ensure!(
            chosen.len() > before || !o.suites.is_empty(),
            "tier {tier} has no suite for build {build}"
        );
    }
    ensure!(
        !chosen.is_empty(),
        "nothing selected: pass --suite, --tier or --gate local"
    );
    Ok(chosen)
}

/// True when some gate places `suite` on `build`. A suite that no gate places
/// on any build (tier-only suites such as soak) is accepted on every build.
fn suite_runs_on_build(m: &Manifest, suite: &str, build: &str) -> bool {
    let mut placed = false;
    for gate in m.gates.values() {
        for cell in &gate.cells {
            if cell.suites.iter().any(|s| s == suite) {
                placed = true;
                if cell.builds.iter().any(|b| b == build) {
                    return true;
                }
            }
        }
    }
    !placed
}

/// The build cell a local run uses: the one whose platform triple is the
/// host's, else a default-feature test build for the same operating system.
fn host_build(m: &Manifest) -> Result<String> {
    let host = host_triple()?;
    let os = |triple: &str| -> &'static str {
        if triple.contains("apple-darwin") {
            "macos"
        } else if triple.contains("windows") {
            "windows"
        } else {
            "linux"
        }
    };
    let test_builds = || {
        m.builds
            .iter()
            .filter(|(_, b)| b.kind == "test" && b.features == "default")
    };
    let triple_of = |b: &crate::manifest::schema::Build| {
        m.platforms
            .get(&b.platform)
            .map(|p| p.triple.clone())
            .unwrap_or_default()
    };
    if let Some((id, _)) = test_builds().find(|(_, b)| triple_of(b) == host) {
        return Ok(id.clone());
    }
    if let Some((id, _)) = test_builds().find(|(_, b)| os(&triple_of(b)) == os(&host)) {
        return Ok(id.clone());
    }
    bail!("no default test build in ci/gates.toml for host {host}; pass --build")
}

fn host_triple() -> Result<String> {
    let out = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("run rustc -vV")?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    text.lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(String::from)
        .context("rustc -vV printed no host triple")
}

/// Cargo target-selection flags for a manifest target id (`lib`, `bin:x`, `test:x`).
fn target_flags(target: &str) -> Result<Vec<String>> {
    Ok(match target.split_once(':') {
        None if target == "lib" => vec!["--lib".into()],
        Some(("bin", name)) => vec!["--bin".into(), name.into()],
        Some(("test", name)) => vec!["--test".into(), name.into()],
        _ => bail!("unsupported suite target {target}"),
    })
}

/// The nextest filterset of one suite, scoped to its package and binary.
fn suite_filter(s: &Suite) -> Result<String> {
    let package = s
        .package
        .as_deref()
        .context("nextest suite without package")?;
    let target = s
        .target
        .as_deref()
        .context("nextest suite without target")?;
    let scope = match target.split_once(':') {
        None => format!("package(={package}) & kind(lib)"),
        Some((kind, name)) => format!("package(={package}) & kind({kind}) & binary(={name})"),
    };
    Ok(match &s.filter {
        Some(f) => format!("({scope}) & ({f})"),
        None => scope,
    })
}

/// Suites that share a tier, package and feature set run in one runner
/// invocation, so each package is built once per tier.
struct Invocation {
    tier: String,
    package: String,
    suites: Vec<String>,
    targets: Vec<String>,
    filters: Vec<String>,
}

fn invocations(chosen: &[(&str, &Suite)]) -> Result<Vec<Invocation>> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (id, s) in chosen.iter().filter(|(_, s)| s.kind == "nextest") {
        if !seen.insert(*id) {
            continue;
        }
        out.push(Invocation {
            tier: s.tier.clone(),
            package: s.package.clone().context("nextest suite without package")?,
            suites: vec![id.to_string()],
            targets: vec![s.target.clone().context("nextest suite without target")?],
            filters: vec![suite_filter(s)?],
        });
    }
    // Preserve the selected (possibly seeded) suite order. Sorting by group
    // here would silently undo --shuffle-seed before any command executes.
    Ok(out)
}

// ── Runner discovery ─────────────────────────────────────────────────────────

fn tools_dir(root: &Path) -> PathBuf {
    root.join("target/xtask-tools/bin")
}

/// PATH with the xtask-installed tools first.
fn tool_path(root: &Path) -> OsString {
    let mut paths = vec![tools_dir(root)];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).unwrap_or_default()
}

/// Fails with the install command when the pinned cargo-nextest is missing.
fn require_nextest(root: &Path, m: &Manifest) -> Result<()> {
    let pinned = &m
        .tools
        .get("nextest")
        .context("ci/gates.toml has no [tools.nextest]")?
        .version;
    let found = Command::new("cargo")
        .args(["nextest", "--version"])
        .env("PATH", tool_path(root))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string());
    let help = format!(
        "cargo-nextest {pinned} is required (the same runner and profile as CI).\n\
         Install the pinned, SHA-256-verified release with:\n\n    cargo xtask test install-runner\n\n\
         (or `cargo install cargo-nextest --locked --version {pinned}`)."
    );
    match found {
        None => bail!("cargo-nextest was not found.\n{help}"),
        Some(text) if !text.contains(&format!("cargo-nextest {pinned}")) => bail!(
            "found {} but the manifest pins {pinned}.\n{help}",
            text.lines().next().unwrap_or("an unknown cargo-nextest")
        ),
        Some(_) => Ok(()),
    }
}

fn install_runner(root: &Path) -> Result<ExitCode> {
    let m = crate::manifest::load(root)?;
    let tool = m
        .tools
        .get("nextest")
        .context("ci/gates.toml has no [tools.nextest]")?;
    let host = host_triple()?;
    // nextest publishes one universal macOS archive.
    let asset_triple = if host.ends_with("apple-darwin") {
        "universal-apple-darwin".to_string()
    } else {
        host.clone()
    };
    let digest = tool
        .sha256
        .get(&asset_triple)
        .with_context(|| format!("no pinned cargo-nextest digest for {asset_triple}"))?;
    let extension = if host.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    let url = format!(
        "{}/cargo-nextest-{}-{asset_triple}.{extension}",
        tool.source, tool.version
    );
    let dir = tools_dir(root);
    std::fs::create_dir_all(&dir)?;
    let download = dir.join(format!("cargo-nextest.{extension}"));
    eprintln!("xtask: downloading {url}");
    let status = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--output",
        ])
        .arg(&download)
        .arg(&url)
        .status()
        .context("run curl")?;
    ensure!(status.success(), "download failed: {url}");
    let actual = crate::evidence::digest::sha256_file(&download)?;
    if &actual != digest {
        let _ = std::fs::remove_file(&download);
        bail!("cargo-nextest archive digest {actual} does not match the pinned {digest}");
    }
    let status = Command::new("tar")
        .arg("-xf")
        .arg(&download)
        .arg("-C")
        .arg(&dir)
        .status()
        .context("run tar")?;
    ensure!(status.success(), "could not unpack {}", download.display());
    std::fs::remove_file(&download)?;
    eprintln!(
        "xtask: cargo-nextest {} installed in {} (verified sha256 {digest})",
        tool.version,
        dir.display()
    );
    Ok(ExitCode::SUCCESS)
}

// ── run ──────────────────────────────────────────────────────────────────────

fn run_suites(root: &Path, o: &Options) -> Result<ExitCode> {
    let m = crate::manifest::load(root)?;
    let build_id = match &o.build {
        Some(b) => b.clone(),
        None => host_build(&m)?,
    };
    let build = m
        .builds
        .get(&build_id)
        .with_context(|| format!("unknown build {build_id}"))?;
    let features = m
        .features
        .get(&build.features)
        .with_context(|| format!("unknown feature set {}", build.features))?
        .cargo_args
        .clone();
    let mut chosen = select(&m, o, &build_id)?;
    if o.changed {
        chosen = changed_only(root, chosen);
    }
    if chosen.is_empty() {
        eprintln!("xtask: no suite is affected by the changed files");
        return Ok(ExitCode::SUCCESS);
    }
    let names: Vec<_> = chosen.iter().map(|(id, _)| *id).collect();
    eprintln!("xtask: build {build_id}; suites {}", names.join(", "));
    if let Some(seed) = o.shuffle_seed {
        eprintln!("xtask: shuffle seed {seed} (suite order permuted; recorded in run.json)");
        shuffle(&mut chosen, seed);
    }

    let nextest = chosen.iter().any(|(_, s)| s.kind == "nextest");
    if nextest && o.runner == Runner::Nextest {
        require_nextest(root, &m)?;
    }
    if let Some(dir) = &o.evidence_dir {
        std::fs::create_dir_all(dir)?;
    }

    let isolation = isolation::Run::new()?;
    let mut failed = Vec::new();
    let mut records = Vec::new();
    for inv in invocations(&chosen)? {
        let mut command = match o.runner {
            Runner::Nextest => nextest_command(root, &inv, &features, o)?,
            Runner::Libtest => libtest_command(&inv, &chosen, &features)?,
        };
        command.env("PATH", tool_path(root));
        isolation.apply(&mut command);
        command.env(&m.test_env["tier"], &inv.tier);
        if let Some(dir) = &o.evidence_dir {
            command.env(&m.test_env["evidence"], dir);
        }
        let expected = if let Some(dir) = o
            .evidence_dir
            .as_ref()
            .filter(|_| o.runner == Runner::Nextest)
        {
            let mut listing = Command::new(command.get_program());
            listing.env_clear();
            listing.current_dir(root);
            for (key, value) in command.get_envs() {
                if let Some(value) = value {
                    listing.env(key, value);
                } else {
                    listing.env_remove(key);
                }
            }
            let mut skip_value = false;
            for arg in command.get_args() {
                if skip_value {
                    skip_value = false;
                    continue;
                }
                if arg == "--no-tests" {
                    skip_value = true;
                    continue;
                }
                if arg == "--no-fail-fast" {
                    continue;
                }
                listing.arg(if arg == "run" {
                    std::ffi::OsStr::new("list")
                } else {
                    arg
                });
            }
            listing.args(["--message-format", "json"]);
            let result = listing
                .output()
                .context("enumerate selected tests before execution")?;
            ensure!(
                result.status.success(),
                "test inventory failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            let file = dir.join(format!("expected-{}.json", inv.suites.join("-")));
            std::fs::write(&file, &result.stdout)?;
            Some(file.file_name().unwrap().to_string_lossy().into_owned())
        } else {
            None
        };
        // A failed startup must not copy a previous invocation's JUnit as evidence.
        let junit = target_dir(root).join(format!("nextest/{PROFILE}/junit.xml"));
        if o.evidence_dir.is_some() && junit.exists() {
            std::fs::remove_file(&junit)?;
        }
        let code = execute(&mut command)?;
        records.push(serde_json::json!({
            "tier": inv.tier, "package": inv.package, "suites": inv.suites,
            "command": describe(&command), "exit": code,
            "expected": expected,
        }));
        if let (Some(dir), Runner::Nextest) = (&o.evidence_dir, o.runner) {
            let junit = target_dir(root).join(format!("nextest/{PROFILE}/junit.xml"));
            if junit.exists() {
                let name = format!(
                    "junit-{}-{}-{}.xml",
                    inv.tier,
                    inv.package,
                    inv.suites.join("-")
                );
                std::fs::copy(&junit, dir.join(name))?;
            }
        }
        if code != 0 {
            failed.extend(inv.suites.clone());
        }
    }
    for (id, s) in chosen.iter().filter(|(_, s)| s.kind != "nextest") {
        let mut command = match s.kind.as_str() {
            "command" => {
                let (program, rest) = s
                    .command
                    .split_first()
                    .with_context(|| format!("suite {id} has an empty command"))?;
                let mut c = Command::new(program);
                c.args(rest);
                c
            }
            "doctest" => {
                let mut c = Command::new("cargo");
                c.args(["test", "--locked", "--workspace", "--doc"]);
                c.args(&features);
                c
            }
            other => bail!("suite {id}: unsupported kind {other}"),
        };
        command.current_dir(root).env("PATH", tool_path(root));
        isolation.apply(&mut command);
        let code = execute(&mut command)?;
        records.push(serde_json::json!({
            "tier": s.tier, "suites": [id], "command": describe(&command), "exit": code,
        }));
        if code != 0 {
            failed.push(id.to_string());
        }
    }
    if let Some(dir) = &o.evidence_dir {
        // Raw run record. Schema-1 cell evidence is produced from these files and
        // the CI controller's run metadata by `cargo xtask evidence emit`.
        let run = serde_json::json!({
            "schema": 1, "build": build_id, "runner": format!("{:?}", o.runner).to_lowercase(),
            "shard": o.shard, "shuffle_seed": o.shuffle_seed, "manifest_version": m.version(),
            "invocations": records, "failed_suites": failed,
            "host_target": host_triple()?,
            "source": crate::evidence::actions::source(root)?,
            "producer": {
                "run_id": std::env::var("GITHUB_RUN_ID").ok(),
                "attempt": std::env::var("GITHUB_RUN_ATTEMPT").ok(),
                "job": std::env::var("GITHUB_JOB").ok(),
            },
        });
        std::fs::write(dir.join("run.json"), serde_json::to_vec_pretty(&run)?)?;
    }
    isolation.finish()?;
    if failed.is_empty() {
        eprintln!("xtask: all selected suites passed");
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("xtask: failed suites: {}", failed.join(", "));
        Ok(ExitCode::from(1))
    }
}

fn nextest_command(
    root: &Path,
    inv: &Invocation,
    features: &[String],
    o: &Options,
) -> Result<Command> {
    let mut c = Command::new("cargo");
    c.current_dir(root).args([
        "nextest",
        "run",
        "--profile",
        PROFILE,
        "--no-fail-fast",
        "--no-tests",
        "fail",
    ]);
    match &o.archive {
        Some(archive) => {
            c.arg("--archive-file")
                .arg(archive)
                .args(["--workspace-remap", "."]);
        }
        None => {
            c.args(["--locked", "-p", &inv.package]);
            for t in &inv.targets {
                c.args(target_flags(t)?);
            }
            c.args(features);
        }
    }
    if let Some(shard) = &o.shard {
        c.args(["--partition", &format!("count:{shard}")]);
    }
    let expr = inv
        .filters
        .iter()
        .map(|f| format!("({f})"))
        .collect::<Vec<_>>()
        .join(" | ");
    c.args(["-E", &expr]);
    Ok(c)
}

/// Diagnosis-only fallback (design 1.1): `cargo test` with libtest name
/// filters derived from the suites' `test(/^prefix::/)` filters.
fn libtest_command(
    inv: &Invocation,
    chosen: &[(&str, &Suite)],
    features: &[String],
) -> Result<Command> {
    let mut c = Command::new("cargo");
    c.args(["test", "--locked", "-p", &inv.package]);
    for t in &inv.targets {
        c.args(target_flags(t)?);
    }
    c.args(features).arg("--");
    for (id, s) in chosen
        .iter()
        .filter(|(id, _)| inv.suites.iter().any(|x| x == id))
    {
        if let Some(f) = &s.filter {
            let prefix = f
                .strip_prefix("test(/^")
                .and_then(|r| r.strip_suffix("/)"))
                .with_context(|| format!("suite {id}: filter {f} has no libtest equivalent"))?;
            c.arg(prefix);
        }
    }
    Ok(c)
}

fn execute(command: &mut Command) -> Result<i32> {
    eprintln!("xtask: $ {}", describe(command));
    let status = command
        .status()
        .with_context(|| format!("could not start {}", describe(command)))?;
    Ok(status.code().unwrap_or(1))
}

fn describe(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|a| a.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|p| if p.is_absolute() { p } else { root.join(p) })
        .unwrap_or_else(|| root.join("target"))
}

/// Deterministic permutation of the suite order from a seed (flake hunt).
fn shuffle(chosen: &mut [(&str, &Suite)], seed: u64) {
    let mut state = seed | 1;
    for i in (1..chosen.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chosen.swap(i, (state % (i as u64 + 1)) as usize);
    }
}

// ── --changed (make check) ──────────────────────────────────────────────────

/// Keeps the suites of packages touched by uncommitted or branch changes.
/// Without a usable git repository every suite is kept.
fn changed_only<'m>(root: &Path, chosen: Vec<(&'m str, &'m Suite)>) -> Vec<(&'m str, &'m Suite)> {
    let Some(paths) = changed_paths(root) else {
        eprintln!("xtask: --changed: git is unavailable here; running every selected suite");
        return chosen;
    };
    if paths.is_empty() {
        eprintln!("xtask: --changed: empty diff; running every selected suite");
        return chosen;
    }
    let mut packages = std::collections::BTreeSet::new();
    for p in &paths {
        match package_of(p) {
            Some(pkg) => {
                packages.insert(pkg);
            }
            None => {
                eprintln!(
                    "xtask: --changed: {p} is shared infrastructure; running every selected suite"
                );
                return chosen;
            }
        }
    }
    chosen
        .into_iter()
        .filter(|(_, s)| s.package.as_deref().is_some_and(|p| packages.contains(p)))
        .collect()
}

fn changed_paths(root: &Path) -> Option<Vec<String>> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).to_string())
    };
    let base = git(&["merge-base", "HEAD", "origin/main"]).unwrap_or_else(|| "HEAD".into());
    let mut paths: Vec<String> = git(&["diff", "--name-only", base.trim()])?
        .lines()
        .chain(git(&["ls-files", "--others", "--exclude-standard"])?.lines())
        .map(String::from)
        .collect();
    paths.sort();
    paths.dedup();
    Some(paths)
}

/// The package a changed file belongs to; `None` for shared inputs.
fn package_of(path: &str) -> Option<&'static str> {
    let prefixes: [(&str, &str); 6] = [
        ("src/", "awman"),
        ("crates/awman-cli/", "awman-cli"),
        ("crates/awman-host-shims/", "awman-host-shims"),
        ("crates/awman-wsp/", "awman-wsp"),
        ("crates/awman-test-support/", "awman-test-support"),
        ("xtask/", "xtask"),
    ];
    if path.ends_with(".md") || path.starts_with("docs/") || path.starts_with("aspec/") {
        return Some("docs");
    }
    prefixes
        .iter()
        .find(|(prefix, _)| path.starts_with(prefix))
        .map(|(_, pkg)| *pkg)
}

// ── list / archive / update-golden ──────────────────────────────────────────

fn list(root: &Path, o: &Options) -> Result<ExitCode> {
    let m = crate::manifest::load(root)?;
    require_nextest(root, &m)?;
    let build = o.build.clone().map_or_else(|| host_build(&m), Ok)?;
    let features = m.features[&m.builds[&build].features].cargo_args.clone();
    let mut c = Command::new("cargo");
    c.current_dir(root)
        .env("PATH", tool_path(root))
        .args([
            "nextest",
            "list",
            "--locked",
            "--workspace",
            "--message-format",
            "json",
        ])
        .args(&features);
    let code = execute(&mut c)?;
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}

fn archive(root: &Path, o: &Options) -> Result<ExitCode> {
    let m = crate::manifest::load(root)?;
    require_nextest(root, &m)?;
    let build = o.build.clone().context("archive needs --build <id>")?;
    let out = o.out.clone().context("archive needs --out <dir>")?;
    let b = m
        .builds
        .get(&build)
        .with_context(|| format!("unknown build {build}"))?;
    let features = m.features[&b.features].cargo_args.clone();
    std::fs::create_dir_all(&out)?;
    let file = out.join(format!("test-archive-{build}.tar.zst"));
    let mut c = Command::new("cargo");
    c.current_dir(root)
        .env("PATH", tool_path(root))
        .args(["nextest", "archive", "--locked"]);
    for p in &b.packages {
        c.args(["-p", p]);
    }
    if let Some(t) = &b.target {
        c.args(["--target", t]);
    }
    c.args(&features).arg("--archive-file").arg(&file);
    if execute(&mut c)? != 0 {
        eprintln!("xtask: nextest archive failed. On a workspace mount that reports ENXIO for SEEK_HOLE, set CARGO_TARGET_DIR to a native filesystem.");
        return Ok(ExitCode::from(1));
    }
    let listing = Command::new("cargo")
        .current_dir(root)
        .env("PATH", tool_path(root))
        .args([
            "nextest",
            "list",
            "--message-format",
            "json",
            "--archive-file",
        ])
        .arg(&file)
        .args(["--workspace-remap", "."])
        .output()?;
    ensure!(
        listing.status.success(),
        "nextest list of the archive failed"
    );
    let expected = out.join(format!("expected-tests-{build}.json"));
    std::fs::write(&expected, &listing.stdout)?;
    let digests = serde_json::json!({
        "archive": crate::evidence::digest::sha256_file(&file)?,
        "expected_tests": crate::evidence::digest::sha256_file(&expected)?,
    });
    std::fs::write(
        out.join("digests.json"),
        serde_json::to_vec_pretty(&digests)?,
    )?;
    Ok(ExitCode::SUCCESS)
}

/// The one golden/snapshot update command: reruns the selected suites with
/// the support crate's update switch and insta's overwrite mode.
fn update_golden(root: &Path, o: &Options) -> Result<ExitCode> {
    let mut o2 = Options {
        suites: o.suites.clone(),
        ..Options::default()
    };
    if o2.suites.is_empty() {
        o2.tiers = vec!["component".into()];
    }
    // SAFETY of intent: these variables are read only by the child test
    // processes through awman_test_support::runtime_env and insta.
    let m = crate::manifest::load(root)?;
    let build = host_build(&m)?;
    let features = m.features[&m.builds[&build].features].cargo_args.clone();
    let chosen = select(&m, &o2, &build)?;
    require_nextest(root, &m)?;
    let mut code = 0;
    for inv in invocations(&chosen)? {
        let mut c = nextest_command(root, &inv, &features, &o2)?;
        c.env("PATH", tool_path(root))
            .env("INSTA_UPDATE", "always")
            .env("AWMAN_UPDATE_GOLDEN", "1")
            .env(&m.test_env["tier"], &inv.tier);
        code = code.max(execute(&mut c)?);
    }
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_flags_follow_manifest_target_ids() {
        assert_eq!(target_flags("lib").unwrap(), ["--lib"]);
        assert_eq!(target_flags("bin:awman").unwrap(), ["--bin", "awman"]);
        assert_eq!(
            target_flags("test:component").unwrap(),
            ["--test", "component"]
        );
        assert!(target_flags("example:x").is_err());
    }

    #[test]
    fn options_parse_lists_and_flags() {
        let args: Vec<String> = ["--tier", "unit,component", "--changed", "--shard", "1/3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let o = Options::parse(&args).unwrap();
        assert_eq!(o.tiers, ["unit", "component"]);
        assert!(o.changed);
        assert_eq!(o.shard.as_deref(), Some("1/3"));
        assert!(Options::parse(&["--bogus".to_string()]).is_err());
    }

    #[test]
    fn changed_files_map_to_packages_or_force_a_full_run() {
        assert_eq!(package_of("src/engine/mod.rs"), Some("awman"));
        assert_eq!(package_of("crates/awman-wsp/src/lib.rs"), Some("awman-wsp"));
        assert_eq!(package_of("Cargo.lock"), None);
        assert_eq!(package_of("tools/install.sh"), None);
    }

    #[test]
    fn shuffle_is_a_deterministic_permutation() {
        let s: Suite =
            toml::from_str("tier='unit'\nkind='nextest'\ncontracts=[]\npackage='a'\ntarget='lib'")
                .unwrap();
        let ids = ["a", "b", "c", "d", "e"];
        let mut one: Vec<(&str, &Suite)> = ids.iter().map(|i| (*i, &s)).collect();
        let mut two = one.clone();
        shuffle(&mut one, 7);
        shuffle(&mut two, 7);
        let a: Vec<_> = one.iter().map(|(i, _)| *i).collect();
        let b: Vec<_> = two.iter().map(|(i, _)| *i).collect();
        assert_eq!(a, b);
        let mut sorted = a.clone();
        sorted.sort();
        assert_eq!(sorted, ids);
    }

    #[test]
    fn seeded_suite_order_reaches_execution_invocations() {
        let suite: Suite = toml::from_str(
            "tier='component'\nkind='nextest'\ncontracts=[]\npackage='a'\ntarget='lib'",
        )
        .unwrap();
        let mut chosen: Vec<_> = ["a", "b", "c", "d", "e"]
            .into_iter()
            .map(|id| (id, &suite))
            .collect();
        shuffle(&mut chosen, 7);
        let ordered: Vec<_> = invocations(&chosen)
            .unwrap()
            .into_iter()
            .flat_map(|i| i.suites)
            .collect();
        let selected: Vec<_> = chosen.iter().map(|(id, _)| id.to_string()).collect();
        assert_eq!(ordered, selected);
        assert_ne!(ordered, ["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn local_gate_selects_unit_and_component_test_suites_only() {
        let m = crate::manifest::load(&crate::gates::root()).unwrap();
        let o = Options {
            gate: Some("local".into()),
            ..Options::default()
        };
        let chosen = select(&m, &o, "linux-x64-default").unwrap();
        assert!(chosen.iter().all(|(_, s)| s.kind == "nextest"));
        assert!(chosen
            .iter()
            .all(|(_, s)| s.tier == "unit" || s.tier == "component"));
        assert!(chosen
            .iter()
            .any(|(id, _)| *id == "component-parity-daemon"));
        assert!(!chosen.iter().any(|(id, _)| *id == "unit-awman-builtin"));
    }
    #[test]
    fn a_clean_changed_checkout_runs_the_selected_suites() {
        use awman_test_support::{fixtures::RepoBuilder, TestCase, TestSandbox};
        let sandbox = TestSandbox::for_test(TestCase::component("test::clean_changed_checkout"));
        let repo = RepoBuilder::new(&sandbox, "repo")
            .file("README", "base")
            .init();
        repo.commit("base");
        let suite: Suite = toml::from_str(
            "tier='unit'\nkind='nextest'\ncontracts=[]\npackage='awman'\ntarget='lib'",
        )
        .unwrap();
        let selected = changed_only(repo.path(), vec![("unit-awman", &suite)]);
        assert_eq!(selected.len(), 1);
        sandbox.finish();
    }
    #[test]
    fn suites_are_not_unioned_into_a_single_empty_hiding_invocation() {
        let suite: Suite = toml::from_str(
            "tier='component'\nkind='nextest'\ncontracts=[]\npackage='awman'\ntarget='lib'",
        )
        .unwrap();
        let invocations = invocations(&[("first", &suite), ("second", &suite)]).unwrap();
        assert_eq!(invocations.len(), 2);
        for invocation in invocations {
            assert_eq!(invocation.suites.len(), 1);
            let command =
                nextest_command(&crate::gates::root(), &invocation, &[], &Options::default())
                    .unwrap();
            let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy()).collect();
            assert!(args.windows(2).any(|a| a == ["--no-tests", "fail"]));
        }
    }
}
