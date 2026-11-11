//! Local hardware acceptance. These packets are operator evidence, never CI attestations.
mod packet;
mod prepare;

use crate::{evidence::digest::sha256_file, manifest::Manifest};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    time::Instant,
};

const BUILD: &str = "macos-arm64-ci-guest-builtin";
const TARGET: &str = "aarch64-apple-darwin";

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Packet {
    schema: u32,
    commit: String,
    tree: String,
    target: String,
    started: String,
    finished: String,
    verdict: String,
    error: Option<String>,
    source: BTreeMap<String, String>,
    steps: Vec<Step>,
    suites: Vec<String>,
    artifact_sha256: String,
    fixture_sha256: String,
    files: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    name: String,
    argv: Vec<String>,
    exit: i32,
    milliseconds: u128,
}
struct Run {
    root: PathBuf,
    out: PathBuf,
    work: PathBuf,
    build: PathBuf,
    env: BTreeMap<String, String>,
    packet: Packet,
}
impl Run {
    fn command(&mut self, name: &str, program: &str, args: &[&str]) -> Result<String> {
        eprintln!("native validation: {name} (logs: {})", self.out.display());
        let stdout = self.out.join(format!("{name}.stdout"));
        let stderr = self.out.join(format!("{name}.stderr"));
        let start = Instant::now();
        let status = Command::new(program)
            .args(args)
            .current_dir(&self.root)
            .envs(&self.env)
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout)?)
            .stderr(fs::File::create(stderr)?)
            .status()
            .with_context(|| format!("start {program}"))?;
        self.packet.steps.push(Step {
            name: name.into(),
            argv: std::iter::once(program.to_string())
                .chain(args.iter().map(|s| s.to_string()))
                .collect(),
            exit: status.code().unwrap_or(-1),
            milliseconds: start.elapsed().as_millis(),
        });
        ensure!(
            status.success(),
            "{name} failed ({status}); see the packet logs"
        );
        Ok(fs::read_to_string(stdout)?)
    }
    fn xtask(&mut self, name: &str, args: &[&str]) -> Result<String> {
        self.command(
            name,
            std::env::current_exe()?.to_str().context("xtask path")?,
            args,
        )
    }
}
fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").args(args).current_dir(root).output()?;
    ensure!(out.status.success(), "git {} failed", args.join(" "));
    Ok(String::from_utf8(out.stdout)?.trim().into())
}
fn suites(m: &Manifest) -> Vec<String> {
    m.suites
        .iter()
        .filter(|(id, s)| {
            s.kind == "nextest"
                && (s.tier == "guest"
                    || s.tier == "artifact"
                    || ["boundary-apple", "boundary-sbx"].contains(&id.as_str()))
        })
        .map(|(id, _)| id.clone())
        .collect()
}
fn source_hashes(root: &Path) -> Result<BTreeMap<String, String>> {
    [
        "ci/gates.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        ".config/nextest.toml",
        "packaging/macos/awman.entitlements",
    ]
    .into_iter()
    .map(|p| Ok((p.into(), sha256_file(&root.join(p))?)))
    .collect()
}
fn execute(r: &mut Run, m: &Manifest) -> Result<()> {
    ensure!(
        git(&r.root, &["status", "--porcelain"])?.is_empty(),
        "commit all source changes before native validation"
    );
    ensure!(
        r.command("host-os", "uname", &["-s"])?.trim() == "Darwin",
        "Apple Silicon macOS required"
    );
    ensure!(
        r.command("host-arch", "uname", &["-m"])?.trim() == "arm64",
        "native arm64 required (no Rosetta)"
    );
    ensure!(
        r.command("hypervisor", "sysctl", &["-n", "kern.hv_support"])?
            .trim()
            == "1",
        "native hypervisor unavailable"
    );
    r.command("os-version", "sw_vers", &[])?;
    r.command("rust-version", "rustc", &["-vV"])?;
    prepare::run(r, m)?;
    let features = &m.features[&m.builds[BUILD].features].cargo_args;
    // Compile and independently enumerate each selected suite before executing any tests.
    // Enumeration is separate from JUnit: a missing/ignored result cannot become a pass.
    for id in &suites(m) {
        let s = &m.suites[id];
        let mut args = vec![
            "nextest",
            "list",
            "--locked",
            "-p",
            s.package.as_deref().context("suite package")?,
            "--message-format",
            "json",
        ];
        let target = s.target.as_deref().context("suite target")?;
        if let Some(t) = target.strip_prefix("test:") {
            args.extend(["--test", t]);
        } else {
            ensure!(target == "lib", "unsupported native target {target}");
            args.push("--lib");
        }
        args.extend(features.iter().map(String::as_str));
        if let Some(filter) = &s.filter {
            args.extend(["-E", filter]);
        }
        r.command(&format!("list-{id}"), "cargo", &args)?;
    }
    for name in ["awman", "awman-guest-driver", "awman-guest-net-driver"] {
        // Cargo can copy a hashed executable back over debug/<name> on the
        // next invocation. Sign both copies before any guest can execute.
        let prefix = format!("{}-", name.replace('-', "_"));
        let mut copies = fs::read_dir(r.build.join("debug/deps"))?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension().is_none()
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
            })
            .collect::<Vec<_>>();
        copies.sort();
        ensure!(!copies.is_empty(), "missing Cargo executable for {name}");
        for (index, bin) in copies.iter().enumerate() {
            r.command(
                &format!("sign-cargo-{name}-{index}"),
                "bash",
                &["tools/macos-sign.sh", bin.to_str().unwrap()],
            )?;
        }
        let bin = r.build.join("debug").join(name);
        r.command(
            &format!("sign-{name}"),
            "bash",
            &["tools/macos-sign.sh", bin.to_str().unwrap()],
        )?;
    }
    r.command(
        "build-artifact",
        "cargo",
        &[
            "build",
            "--locked",
            "--release",
            "-p",
            "awman-cli",
            "--bin",
            "awman",
            "--features",
            "builtin-runtime",
        ],
    )?;
    fs::create_dir_all(r.out.join("candidate"))?;
    let artifact = r.out.join("candidate/awman");
    fs::copy(r.build.join("release/awman"), &artifact)?;
    r.command(
        "sign-artifact",
        "bash",
        &["tools/macos-sign.sh", artifact.to_str().unwrap()],
    )?;
    r.packet.artifact_sha256 = sha256_file(&artifact)?;
    r.env
        .insert("AWMAN_TEST_ARTIFACT".into(), artifact.display().to_string());
    r.env.insert(
        "AWMAN_TEST_ARTIFACT_SHA256".into(),
        r.packet.artifact_sha256.clone(),
    );
    let mut failures = Vec::new();
    for id in &suites(m) {
        let dir = r.out.join(id);
        if let Err(e) = r.xtask(
            &format!("run-{id}"),
            &[
                "test",
                "run",
                "--suite",
                id,
                "--build",
                BUILD,
                "--evidence-dir",
                dir.to_str().unwrap(),
            ],
        ) {
            failures.push(format!("{id}: {e:#}"));
        }
    }
    r.command(
        "verify-artifact",
        "bash",
        &[
            "tools/macos-sign.sh",
            "--verify",
            artifact.to_str().unwrap(),
        ],
    )?;
    ensure!(
        sha256_file(&artifact)? == r.packet.artifact_sha256,
        "candidate changed during validation"
    );
    ensure!(
        git(&r.root, &["rev-parse", "HEAD"])? == r.packet.commit
            && git(&r.root, &["status", "--porcelain"])?.is_empty(),
        "source changed during validation"
    );
    ensure!(
        source_hashes(&r.root)? == r.packet.source,
        "validation inputs changed"
    );
    ensure!(failures.is_empty(), "{}", failures.join("\n"));
    packet::check_results(&r.out, &r.packet, m)?;
    Ok(())
}
pub fn run(args: &[String]) -> Result<ExitCode> {
    let root = crate::gates::root();
    let m = crate::manifest::load(&root)?;
    match args.first().map(String::as_str) {
        Some("verify") if args.len() == 2 => {
            packet::verify(&root, Path::new(&args[1]), &m)?;
            println!("Packet checks passed. Review native provenance and coverage before closing the work item; this is not a CI attestation.");
            Ok(ExitCode::SUCCESS)
        }
        Some("run") if args.len() == 1 => {
            let stamp = format!(
                "{}-{}",
                chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
                std::process::id()
            );
            let base = root.join("target/native-validation").join(stamp);
            let out = base.join("evidence");
            let work = base.join("work");
            // Retain compilation caches between runs; test resources and evidence
            // are still private to this attempt. Cargo validates current inputs.
            let build = root.join("target/native-validation-build");
            fs::create_dir_all(&out)?;
            fs::create_dir_all(&work)?;
            let mut r = Run {
                root: root.clone(),
                out: out.clone(),
                work: work.clone(),
                build: build.clone(),
                env: BTreeMap::from([
                    ("CARGO_TARGET_DIR".into(), build.display().to_string()),
                    (
                        "PATH".into(),
                        format!(
                            "{}:{}",
                            root.join("target/xtask-tools/bin").display(),
                            std::env::var("PATH")?
                        ),
                    ),
                ]),
                packet: Packet {
                    schema: 1,
                    commit: git(&root, &["rev-parse", "HEAD"])?,
                    tree: git(&root, &["rev-parse", "HEAD^{tree}"])?,
                    target: TARGET.into(),
                    started: chrono::Utc::now().to_rfc3339(),
                    source: source_hashes(&root)?,
                    suites: suites(&m),
                    ..Default::default()
                },
            };
            let result = execute(&mut r, &m);
            r.packet.finished = chrono::Utc::now().to_rfc3339();
            r.packet.verdict = if result.is_ok() { "passed" } else { "failed" }.into();
            r.packet.error = result.as_ref().err().map(|e| format!("{e:#}"));
            r.packet.files = packet::files(&out)?;
            fs::write(
                out.join("packet.json"),
                serde_json::to_vec_pretty(&r.packet)?,
            )?;
            let archive = base.with_extension("tar.gz");
            let status = Command::new("tar")
                .args(["-czf"])
                .arg(&archive)
                .arg("-C")
                .arg(&base)
                .arg("evidence")
                .status()?;
            ensure!(
                status.success(),
                "could not archive evidence; directory preserved at {}",
                out.display()
            );
            eprintln!(
                "Evidence packet: {}\nSHA-256: {}",
                archive.display(),
                sha256_file(&archive)?
            );
            if let Err(e) = result {
                eprintln!("Native validation FAILED: {e:#}");
                return Ok(ExitCode::from(1));
            }
            println!(
                "Native execution passed; send the evidence packet for independent final review."
            );
            Ok(ExitCode::SUCCESS)
        }
        _ => anyhow::bail!(
            "usage: cargo xtask native-validation run | verify <extracted-evidence-directory>"
        ),
    }
}
