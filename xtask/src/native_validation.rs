//! Broad local hardware diagnostic (`make validate-native`). Not a release
//! step: releases run only the narrow exact-asset native checks of the
//! release plan. Evidence is operator-produced, never a CI attestation.
mod packet;
mod prepare;

use crate::{evidence::digest::sha256_file, manifest::Manifest};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    time::Instant,
};

#[derive(Clone, Copy)]
struct Platform {
    target: &'static str,
    build: &'static str,
    os: &'static str,
    arch: &'static str,
    image_platform: &'static str,
}
impl Platform {
    fn for_target(target: &str) -> Result<Self> {
        match target {
            "aarch64-apple-darwin" => Ok(Self {
                target: "aarch64-apple-darwin",
                build: "macos-arm64-ci-guest-builtin",
                os: "Darwin",
                arch: "arm64",
                image_platform: "linux/arm64",
            }),
            "x86_64-unknown-linux-gnu" => Ok(Self {
                target: "x86_64-unknown-linux-gnu",
                build: "linux-x64-native-builtin",
                os: "Linux",
                arch: "x86_64",
                image_platform: "linux/amd64",
            }),
            "aarch64-unknown-linux-gnu" => Ok(Self {
                target: "aarch64-unknown-linux-gnu",
                build: "linux-arm64-native-builtin",
                os: "Linux",
                arch: "aarch64",
                image_platform: "linux/arm64",
            }),
            _ => anyhow::bail!(
                "native validation requires Apple Silicon macOS or Linux x86_64/aarch64 KVM"
            ),
        }
    }
    fn host() -> Result<Self> {
        let output = Command::new("rustc").arg("-vV").output()?;
        ensure!(output.status.success(), "cannot determine Rust host target");
        let version = String::from_utf8(output.stdout)?;
        let target = version
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .context("Rust host target missing")?;
        Self::for_target(target)
    }
    fn apple(self) -> bool {
        self.os == "Darwin"
    }
}

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
    docker_fixture_sha256: String,
    files: BTreeMap<String, String>,
    /// Registered suites that could not run, with the reason. Any entry makes
    /// the verdict non-success; a blocked suite is never a pass or a skip.
    #[serde(default)]
    blocked: BTreeMap<String, String>,
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
    platform: Platform,
    root: PathBuf,
    out: PathBuf,
    work: PathBuf,
    build: PathBuf,
    env: BTreeMap<String, String>,
    packet: Packet,
    registry: Option<String>,
    _test_tmproot: Option<tempfile::TempDir>,
}
impl Drop for Run {
    fn drop(&mut self) {
        if let Some(id) = &self.registry {
            // Last-resort cleanup for preparation errors; successful runs record removal.
            let _ = self
                .native_command("docker")
                .args(["rm", "--force", id])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}
impl Run {
    fn native_command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command.current_dir(&self.root).envs(&self.env);
        remove_cargo_package_metadata(
            &mut command,
            std::env::vars_os()
                .map(|(name, _)| name)
                .chain(self.env.keys().map(OsString::from)),
        );
        command
    }

    fn command(&mut self, name: &str, program: &str, args: &[&str]) -> Result<String> {
        eprintln!("native validation: {name} (logs: {})", self.out.display());
        let stdout = self.out.join(format!("{name}.stdout"));
        let stderr = self.out.join(format!("{name}.stderr"));
        let start = Instant::now();
        let status = self
            .native_command(program)
            .args(args)
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
    fn suite(&mut self, id: &str) -> Result<String> {
        let dir = self.out.join(id);
        self.xtask(
            &format!("run-{id}"),
            &[
                "test",
                "run",
                "--suite",
                id,
                "--build",
                self.platform.build,
                "--evidence-dir",
                dir.to_str().unwrap(),
            ],
        )
    }
}

/// `cargo run -p xtask` injects metadata about xtask into this process. Passing
/// it to another Cargo invocation changes dependency build-script fingerprints
/// (ring tracks these names), forcing rebuilds when switching to `make install`.
/// Keep actual Cargo/toolchain settings and PATH; the child Cargo supplies its
/// own package metadata to each build script and executable.
fn remove_cargo_package_metadata(command: &mut Command, names: impl IntoIterator<Item = OsString>) {
    for name in names {
        let metadata = name.to_str().is_some_and(|name| {
            name.starts_with("CARGO_PKG_")
                || matches!(
                    name,
                    "CARGO_MANIFEST_DIR" | "CARGO_MANIFEST_PATH" | "CARGO_MANIFEST_LINKS"
                )
        });
        if metadata {
            command.env_remove(name);
        }
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").args(args).current_dir(root).output()?;
    ensure!(out.status.success(), "git {} failed", args.join(" "));
    Ok(String::from_utf8(out.stdout)?.trim().into())
}
fn suites(m: &Manifest, platform: Platform) -> Vec<String> {
    // The private-memory command has its own recorded execution below. Disabled
    // platform overrides (sbx) are reported by the separate nightly diagnostic.
    m.gates["native-diagnostic"]
        .cells
        .iter()
        .filter(|c| c.builds.iter().any(|b| b == platform.build) && c.platform.is_none())
        .flat_map(|c| c.suites.iter())
        .filter(|id| m.suites[*id].kind == "nextest")
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(crate) fn guest_host_build() -> Result<String> {
    Ok(Platform::host()?.build.into())
}

pub(crate) fn suites_for_target(m: &Manifest, target: &str) -> Result<Vec<String>> {
    Ok(suites(m, Platform::for_target(target)?))
}
fn source_hashes(root: &Path) -> Result<BTreeMap<String, String>> {
    [
        "ci/gates.toml",
        "ci/fixture-base-image.txt",
        "Cargo.lock",
        "rust-toolchain.toml",
        ".config/nextest.toml",
        "packaging/macos/awman.entitlements",
        "tools/msb-payloads/check-private-memory.sh",
    ]
    .into_iter()
    .map(|p| Ok((p.into(), sha256_file(&root.join(p))?)))
    .collect()
}
/// A reviewed, digest-pinned published predecessor exists for this platform.
fn predecessor_pinned(m: &Manifest, platform: Platform) -> bool {
    m.previous_release
        .get(platform.target)
        .is_some_and(|p| p.pending.is_none() && p.sha256.is_some())
}
/// Registered suites that cannot run on this platform, with the reason. Only
/// suites needing the published predecessor are affected; every independent
/// suite still runs, and any entry here keeps the verdict non-success.
fn blocked_suites(m: &Manifest, platform: Platform) -> BTreeMap<String, String> {
    if predecessor_pinned(m, platform) {
        return BTreeMap::new();
    }
    suites(m, platform)
        .into_iter()
        .filter(|id| {
            m.suites[id]
                .requires
                .iter()
                .any(|c| c == "previous-release")
        })
        .map(|id| {
            let reason = format!(
                "BLOCKED[previous-release]: no reviewed predecessor pin for {}",
                platform.target
            );
            (id, reason)
        })
        .collect()
}
/// The registered suites this run executes: all of them except the blocked ones.
fn runnable_suites(m: &Manifest, platform: Platform) -> Vec<String> {
    let blocked = blocked_suites(m, platform);
    suites(m, platform)
        .into_iter()
        .filter(|id| !blocked.contains_key(id))
        .collect()
}
fn execute(r: &mut Run, m: &Manifest) -> Result<()> {
    ensure!(
        git(&r.root, &["status", "--porcelain"])?.is_empty(),
        "commit all source changes before native validation"
    );
    let platform = r.platform;
    ensure!(
        r.command("host-os", "uname", &["-s"])?.trim() == platform.os,
        "wrong native OS"
    );
    ensure!(
        r.command("host-arch", "uname", &["-m"])?.trim() == platform.arch,
        "native architecture required (no emulation)"
    );
    if platform.apple() {
        ensure!(
            r.command("hypervisor", "sysctl", &["-n", "kern.hv_support"])?
                .trim()
                == "1",
            "native hypervisor unavailable"
        );
        r.command("os-version", "sw_vers", &[])?;
    } else {
        r.command(
            "hypervisor",
            "sh",
            &[
                "-c",
                "test -c /dev/kvm && test -r /dev/kvm && test -w /dev/kvm && echo 1",
            ],
        )?;
        r.command("os-version", "uname", &["-a"])?;
        let umask = r.command("host-umask", "sh", &["-c", "umask"])?;
        ensure!(
            u32::from_str_radix(umask.trim(), 8) == Ok(0o002),
            "the Linux native diagnostic requires umask 0002; run `umask 0002` in this shell first"
        );
    }
    r.command("rust-version", "rustc", &["-vV"])?;
    prepare::run(r, m)?;
    if !platform.apple() {
        let work = r.work.join("upstream-memory");
        r.command(
            "run-boundary-private-memory",
            "bash",
            &[
                "tools/msb-payloads/check-private-memory.sh",
                work.to_str().context("private-memory work path")?,
            ],
        )?;
    }
    let features = &m.features[&m.builds[platform.build].features].cargo_args;
    // Compile and independently enumerate each selected suite before executing any tests.
    // Enumeration is separate from JUnit: a missing/ignored result cannot become a pass.
    for id in &runnable_suites(m, platform) {
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
        let features =
            crate::test::package_features(features, s.package.as_deref().context("suite package")?);
        args.extend(features.iter().map(String::as_str));
        if let Some(filter) = &s.filter {
            args.extend(["-E", filter]);
        }
        r.command(&format!("list-{id}"), "cargo", &args)?;
    }
    if platform.apple() {
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
    }
    if !platform.apple() {
        r.suite("boundary-offline")
        .context("Linux offline acceptance requires kernel seccomp filters: the registered probe must deny IP sockets, preserve Unix IPC and UID, and enforce the restriction in descendants; see boundary-offline evidence")?;
    }
    // Resolve native boot/source failures before spending time on the optimized
    // executable. Artifact suites still run against the exact immutable candidate.
    let mut failures = Vec::new();
    for id in &runnable_suites(m, platform) {
        if m.suites[id].tier == "artifact" || id == "boundary-offline" {
            continue;
        }
        if let Err(e) = r.suite(id) {
            failures.push(format!("{id}: {e:#}"));
        }
    }
    ensure!(failures.is_empty(), "{}", failures.join("\n"));
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
    if platform.apple() {
        r.command(
            "sign-artifact",
            "bash",
            &["tools/macos-sign.sh", artifact.to_str().unwrap()],
        )?;
    }
    r.packet.artifact_sha256 = sha256_file(&artifact)?;
    r.env
        .insert("AWMAN_TEST_ARTIFACT".into(), artifact.display().to_string());
    r.env.insert(
        "AWMAN_TEST_ARTIFACT_SHA256".into(),
        r.packet.artifact_sha256.clone(),
    );
    for id in &runnable_suites(m, platform) {
        if m.suites[id].tier != "artifact" {
            continue;
        }
        if let Err(e) = r.suite(id) {
            failures.push(format!("{id}: {e:#}"));
        }
    }
    if platform.apple() {
        r.command(
            "verify-artifact",
            "bash",
            &[
                "tools/macos-sign.sh",
                "--verify",
                artifact.to_str().unwrap(),
            ],
        )?;
    }
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
    blocked_outcome(&r.packet.blocked)
}
/// Everything that could run has passed; coverage that could not run keeps
/// the diagnostic BLOCKED rather than letting it read as complete.
fn blocked_outcome(blocked: &BTreeMap<String, String>) -> Result<()> {
    ensure!(
        blocked.is_empty(),
        "{} registered suite(s) did not run; every other suite passed:\n{}",
        blocked.len(),
        blocked
            .iter()
            .map(|(id, reason)| format!("{id}: {reason}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    Ok(())
}
/// Packet verdict: a `BLOCKED[...]` reason anywhere in the error is missing
/// infrastructure; any other error is a failure.
fn verdict(result: &Result<()>) -> &'static str {
    match result {
        Ok(()) => "passed",
        Err(e) if format!("{e:#}").contains("BLOCKED[") => "blocked",
        Err(_) => "failed",
    }
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
        Some("run") if args.len() == 1 || (args.len() == 3 && args[1] == "--platform") => {
            let platform = Platform::host()?;
            if args.len() == 3 {
                ensure!(args[2] == "linux" && !platform.apple(), "--platform linux requires native Linux KVM");
            }
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
            let test_tmproot = if platform.apple() {
                Some(crate::test::temp_root::short_apple_root()?)
            } else {
                None
            };
            if let Some(temp) = &test_tmproot {
                fs::write(
                    out.join("test-temp-root.txt"),
                    format!("{}\n", temp.path().canonicalize()?.display()),
                )?;
            }
            let mut env = BTreeMap::from([
                ("CARGO_TARGET_DIR".into(), build.display().to_string()),
                (
                    "PATH".into(),
                    format!(
                        "{}:{}",
                        root.join("target/xtask-tools/bin").display(),
                        std::env::var("PATH")?
                    ),
                ),
            ]);
            if let Some(temp) = &test_tmproot {
                env.insert(
                    "AWMAN_TEST_TMPROOT".into(),
                    temp.path().display().to_string(),
                );
            }
            let mut r = Run {
                platform,
                registry: None,
                _test_tmproot: test_tmproot,
                root: root.clone(),
                out: out.clone(),
                work: work.clone(),
                build: build.clone(),
                env,
                packet: Packet {
                    schema: 1,
                    commit: git(&root, &["rev-parse", "HEAD"])?,
                    tree: git(&root, &["rev-parse", "HEAD^{tree}"])?,
                    target: platform.target.into(),
                    started: chrono::Utc::now().to_rfc3339(),
                    source: source_hashes(&root)?,
                    suites: suites(&m, platform),
                    blocked: blocked_suites(&m, platform),
                    ..Default::default()
                },
            };
            let result = execute(&mut r, &m);
            let cleanup = if let Some(id) = r.registry.clone() {
                r.command("remove-registry", "docker", &["rm", "--force", &id]).map(|_| {
                    r.registry = None;
                })
            } else { Ok(()) };
            let result = result.and(cleanup);
            r.packet.finished = chrono::Utc::now().to_rfc3339();
            r.packet.verdict = verdict(&result).into();
            r.packet.error = result.as_ref().err().map(|e| format!("{e:#}"));
            r.packet.files = packet::files(&out)?;
            fs::write(
                out.join("packet.json"),
                serde_json::to_vec_pretty(&r.packet)?,
            )?;
            // Diagnostic evidence stays a directory: no tar hand-off, but
            // `native-validation verify DIR` still checks its completeness.
            eprintln!(
                "Diagnostic evidence: {}\npacket.json SHA-256: {}",
                out.display(),
                sha256_file(&out.join("packet.json"))?
            );
            if let Err(e) = result {
                // Blocked (missing infrastructure) is distinct from failed, and neither passes.
                if r.packet.verdict == "blocked" {
                    eprintln!("Native validation BLOCKED: {e:#}");
                    return Ok(ExitCode::from(2));
                }
                eprintln!("Native validation FAILED: {e:#}");
                return Ok(ExitCode::from(1));
            }
            println!(
                "Native diagnostic passed; check completeness with `cargo xtask native-validation verify {}`.",
                out.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        _ => anyhow::bail!(
            "usage: cargo xtask native-validation run [--platform linux] | verify <extracted-evidence-directory>"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    /// hygiene-allow(H2): Inspects child environment directives without starting a process.
    fn native_children_drop_parent_package_metadata_but_keep_build_settings() {
        let metadata = [
            "CARGO_MANIFEST_DIR",
            "CARGO_MANIFEST_PATH",
            "CARGO_MANIFEST_LINKS",
            "CARGO_PKG_NAME",
            "CARGO_PKG_VERSION",
            "CARGO_PKG_VERSION_MAJOR",
            "CARGO_PKG_VERSION_MINOR",
            "CARGO_PKG_VERSION_PATCH",
            "CARGO_PKG_VERSION_PRE",
            "CARGO_PKG_AUTHORS",
            "CARGO_PKG_DESCRIPTION",
        ];
        let settings = [
            ("PATH", "/pinned-tools:/usr/bin"),
            ("CARGO_TARGET_DIR", "/private/build"),
            ("CARGO_HOME", "/tools/cargo"),
            ("RUSTUP_HOME", "/tools/rustup"),
            ("RUSTUP_TOOLCHAIN", "1.94.0"),
            ("CARGO_BUILD_TARGET", "x86_64-unknown-linux-gnu"),
            ("CARGO_BUILD_JOBS", "2"),
            ("CARGO_MAKEFLAGS", "--jobserver-auth=8,9"),
            ("CARGO_PROFILE_RELEASE_LTO", "thin"),
            ("RUSTFLAGS", "-C debuginfo=0"),
            ("CARGO_ENCODED_RUSTFLAGS", "-C\u{1f}debuginfo=0"),
        ];
        let mut command = Command::new("cargo");
        command.envs(metadata.iter().map(|name| (name, "parent-xtask-value")));
        command.envs(settings);
        remove_cargo_package_metadata(
            &mut command,
            metadata
                .iter()
                .copied()
                .chain(settings.iter().map(|(name, _)| *name))
                .map(OsString::from),
        );
        let environment: BTreeMap<_, _> = command.get_envs().collect();
        for name in metadata {
            // None is an explicit removal, preventing inherited parent values.
            assert_eq!(environment[OsStr::new(name)], None, "{name}");
        }
        for (name, value) in settings {
            assert_eq!(
                environment[OsStr::new(name)],
                Some(OsStr::new(value)),
                "{name}"
            );
        }
    }
}

#[cfg(test)]
mod predecessor_tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn missing_predecessor_blocks_only_dependent_suites_and_the_overall_verdict() {
        let mut m = crate::manifest::load(&crate::gates::root()).unwrap();
        for target in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"] {
            let linux = Platform::for_target(target).unwrap();
            m.previous_release.remove(target);
            let blocked = blocked_suites(&m, linux);
            assert_eq!(
                blocked.keys().map(String::as_str).collect::<BTreeSet<_>>(),
                BTreeSet::from([
                    "artifact-linux-published-upgrade",
                    "guest-linux-published-upgrade"
                ]),
                "{target}"
            );
            assert!(blocked
                .values()
                .all(|reason| reason.starts_with("BLOCKED[previous-release]")));
            // Every independent registered suite still runs.
            let runnable = runnable_suites(&m, linux);
            for id in [
                "guest-boot",
                "guest-exec",
                "boundary-docker",
                "artifact-linux-boot",
            ] {
                assert!(runnable.iter().any(|r| r == id), "{target}: {id}");
            }
            assert!(runnable.iter().all(|id| !blocked.contains_key(id)));
            assert_eq!(runnable.len() + blocked.len(), suites(&m, linux).len());

            let outcome = blocked_outcome(&blocked);
            assert_eq!(verdict(&outcome), "blocked", "{target}");
        }

        // A pending pin is as unavailable as no pin; a reviewed pin unblocks.
        let linux = Platform::for_target("x86_64-unknown-linux-gnu").unwrap();
        let mut pin = m.previous_release["aarch64-apple-darwin"].clone();
        pin.pending = Some("awaiting the first Linux builtin release".into());
        m.previous_release.insert(linux.target.into(), pin.clone());
        assert_eq!(blocked_suites(&m, linux).len(), 2);
        pin.pending = None;
        m.previous_release.insert(linux.target.into(), pin);
        assert!(blocked_suites(&m, linux).is_empty());
        assert_eq!(runnable_suites(&m, linux), suites(&m, linux));
        let mac = Platform::for_target("aarch64-apple-darwin").unwrap();
        assert!(blocked_suites(&m, mac).is_empty());

        assert_eq!(verdict(&blocked_outcome(&BTreeMap::new())), "passed");
        assert_eq!(
            verdict(&Err(anyhow::anyhow!("guest-boot failed"))),
            "failed"
        );
    }
}
