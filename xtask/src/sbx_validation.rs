//! Opt-in smoke test of the developer's logged-in SBX session.
use crate::evidence::{digest, schema::Outcome};
use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    time::{Duration, Instant},
};

mod linux;

const TARGET: &str = "aarch64-apple-darwin";
const STEPS: &[&str] = &[
    "version", "session", "create", "inspect", "exec", "run", "stop", "restart", "remove",
    "cleanup",
];
const LIMITS: &str = "Experimental SBX smoke evidence: logged-in CLI, sandbox lifecycle, guest execution and workspace sharing. No provider tasks, credential-isolation claim or full policy qualification. The production SBX guard is unchanged. This is not a release gate.";
const ACCEPTANCE: &str = "A successful run is sufficient validation to enable experimental SBX on the tested platforms (Apple silicon macOS, plus Linux when LINUX_HOST is set). No further provider tests or manual qualification are required for enablement.";
const GUEST_SCRIPT: &str = "set -eu; cd \"$2\"; test \"$(uname -s)\" = Linux; test \"$(cat .host-probe)\" = \"$1\"; printf '%s\\n' \"$1\" > .guest-probe";

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Smoke-test the existing logged-in SBX session. No setup or API keys.
    Run {
        #[arg(long, default_value = "target/sbx-validation")]
        out: PathBuf,
        /// Also validate Linux over SSH (defaults to LINUX_HOST=user@host).
        #[arg(long)]
        linux_host: Option<String>,
    },
    /// Optional offline integrity check of a retained smoke report.
    Verify { dir: PathBuf },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    schema: u32,
    outcome: Outcome,
    target: String,
    sandbox: String,
    linux_host: Option<String>,
    cli_version: Option<String>,
    cli_sha256: Option<String>,
    steps: BTreeMap<String, i32>,
    files: BTreeMap<String, String>,
    error: Option<String>,
}
impl Proof {
    fn verify(&self, out: &Path) -> Result<()> {
        ensure!(
            self.schema == 2 && self.outcome == Outcome::Passed && self.error.is_none(),
            "SBX smoke did not pass"
        );
        ensure!(self.target == TARGET, "wrong SBX smoke platform");
        ensure!(
            self.cli_version.as_deref().is_some_and(live_cli_version)
                && self.cli_sha256.as_deref().is_some_and(digest::is_sha256),
            "missing SBX CLI identity"
        );
        ensure!(
            self.steps == STEPS.iter().map(|s| ((*s).into(), 0)).collect(),
            "missing or failed smoke step"
        );
        ensure!(self.files == files(out)?, "smoke evidence changed");
        for step in STEPS {
            ensure!(
                self.files.contains_key(&format!("{step}.stdout"))
                    && self.files.contains_key(&format!("{step}.stderr")),
                "missing {step} logs"
            );
        }
        let nonce = fs::read(out.join("workspace/.host-probe"))?;
        ensure!(
            !nonce.is_empty() && fs::read(out.join("workspace/.guest-probe"))? == nonce,
            "guest workspace round trip failed"
        );
        ensure!(
            !crate::release::sbx::contains_name(
                &serde_json::from_slice(&fs::read(out.join("cleanup.stdout"))?)?,
                &self.sandbox
            )?,
            "sandbox survived cleanup"
        );
        if let Some(host) = &self.linux_host {
            linux::verify(&out.join("linux"), host, &self.sandbox)?;
        }
        Ok(())
    }
}

fn live_cli_version(version: &str) -> bool {
    let parts: Option<Vec<u64>> = version.split('.').map(|s| s.parse().ok()).collect();
    parts.is_some_and(|p| p.len() == 3 && p.as_slice() >= [0, 54, 0].as_slice())
}

fn files(root: &Path) -> Result<BTreeMap<String, String>> {
    fn walk(root: &Path, dir: &Path, hashes: &mut BTreeMap<String, String>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(!kind.is_symlink(), "symlink in SBX smoke evidence");
            if kind.is_dir() {
                walk(root, &entry.path(), hashes)?;
            } else {
                ensure!(kind.is_file(), "non-file in SBX smoke evidence");
                let name = entry
                    .path()
                    .strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/");
                if name != "proof.json" {
                    hashes.insert(name, digest::sha256_file(&entry.path())?);
                }
            }
        }
        Ok(())
    }
    let mut hashes = BTreeMap::new();
    walk(root, root, &mut hashes)?;
    Ok(hashes)
}

/// Cleanup runs even after a failed/partially completed create. Existing names
/// are refused before mutation; only this attempt's random name is removed.
fn smoke(
    name: &str,
    workspace: &Path,
    nonce: &str,
    mut call: impl FnMut(&str, &[&str]) -> Result<Vec<u8>>,
) -> Result<String> {
    let raw = call("version", &["--version"])?;
    let version = crate::release::sbx::version(&String::from_utf8_lossy(&raw))
        .filter(|v| live_cli_version(v))
        .context("validate-sbx needs sbx >= 0.54.0")?;
    let listing = call("session", &["--no-input", "ls", "--json"])?;
    ensure!(
        !crate::release::sbx::contains_name(&serde_json::from_slice(&listing)?, name)?,
        "sandbox name already exists; refusing to touch it"
    );
    let workspace_str = workspace.to_str().context("workspace path is not UTF-8")?;
    fs::write(workspace.join(".host-probe"), format!("{nonce}\n"))?;
    let result: Result<()> = (|| {
        call(
            "create",
            &[
                "--no-input",
                "create",
                "--name",
                name,
                "shell",
                workspace_str,
            ],
        )?;
        let inspect = call("inspect", &["--no-input", "inspect", name, "--json"])?;
        let value: serde_json::Value = serde_json::from_slice(&inspect)?;
        ensure!(
            value.is_object() || value.is_array(),
            "invalid SBX inspection"
        );
        call(
            "exec",
            &[
                "--no-input",
                "exec",
                "-w",
                workspace_str,
                name,
                "sh",
                "-c",
                GUEST_SCRIPT,
                "awman-validation",
                nonce,
                workspace_str,
            ],
        )?;
        check_marker(workspace, nonce)?;
        // Distinct witnesses prevent exec output from standing in for run.
        fs::remove_file(workspace.join(".guest-probe"))?;
        call(
            "run",
            &[
                "--no-input",
                "run",
                "--name",
                name,
                "--",
                "-c",
                GUEST_SCRIPT,
                "awman-validation",
                nonce,
                workspace_str,
            ],
        )?;
        check_marker(workspace, nonce)?;
        call("stop", &["--no-input", "stop", name])?;
        call(
            "restart",
            &[
                "--no-input",
                "exec",
                "-w",
                workspace_str,
                name,
                "sh",
                "-c",
                "test \"$(cat .guest-probe)\" = \"$1\"",
                "awman-validation",
                nonce,
            ],
        )?;
        Ok(())
    })();
    let removed = call("remove", &["--no-input", "rm", "--force", name]);
    let cleanup = call("cleanup", &["--no-input", "ls", "--json"]);
    // Cleanup errors take precedence and can never produce successful proof.
    removed.context("sandbox cleanup failed")?;
    ensure!(
        !crate::release::sbx::contains_name(
            &serde_json::from_slice(&cleanup.context("cannot verify sandbox cleanup")?)?,
            name
        )?,
        "sandbox survived cleanup: {name}"
    );
    result?;
    Ok(version)
}

fn check_marker(workspace: &Path, nonce: &str) -> Result<()> {
    ensure!(
        fs::read(workspace.join(".guest-probe"))? == format!("{nonce}\n").as_bytes(),
        "guest workspace round trip failed"
    );
    Ok(())
}

/// Inherit the normal login/configuration. No test HOME, credential inputs,
/// secret commands, host agent or release preflight is involved.
fn command(cli: &Path, workspace: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(cli);
    command
        .current_dir(workspace)
        .args(args)
        .stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
}

fn invoke(
    cli: &Path,
    workspace: &Path,
    out: &Path,
    step: &str,
    args: &[&str],
    steps: &mut BTreeMap<String, i32>,
) -> Result<Vec<u8>> {
    eprintln!("validate-sbx [macOS]: {step} started");
    let started = Instant::now();
    let mut last_progress = started;
    let stdout = out.join(format!("{step}.stdout"));
    let mut c = command(cli, workspace, args);
    c.stdout(fs::File::create(&stdout)?)
        .stderr(fs::File::create(out.join(format!("{step}.stderr")))?);
    steps.insert(step.into(), -1);
    let mut child = c
        .spawn()
        .with_context(|| format!("cannot start sbx for {step}"))?;
    let deadline = Instant::now() + Duration::from_secs(if step == "create" { 300 } else { 60 });
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{step} timed out; see {}", out.display());
        }
        if last_progress.elapsed() >= Duration::from_secs(10) {
            eprintln!(
                "validate-sbx [macOS]: {step} still running ({}s)",
                started.elapsed().as_secs()
            );
            last_progress = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    steps.insert(step.into(), status.code().unwrap_or(-1));
    eprintln!(
        "validate-sbx [macOS]: {step} {} ({:.1}s)",
        if status.success() { "passed" } else { "failed" },
        started.elapsed().as_secs_f64()
    );
    ensure!(status.success(), "{step} failed; see {step}.stderr");
    Ok(fs::read(stdout)?)
}

fn execute_mac(out: &Path, proof: &mut Proof) -> Result<String> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        anyhow::bail!("validate-sbx requires Apple silicon macOS");
    }
    let cli = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join("sbx"))
        .find(|p| p.is_file())
        .and_then(|p| p.canonicalize().ok())
        .context("sbx is not on PATH; install it and sign in with sbx login")?;
    proof.cli_sha256 = Some(digest::sha256_file(&cli)?);
    let workspace = out.join("workspace");
    fs::create_dir(&workspace)?;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|e| anyhow::anyhow!("cannot allocate a random sandbox name: {e}"))?;
    let nonce = digest::sha256(&random);
    proof.sandbox = format!("awman-validate-{}", &nonce[..24]);
    proof.cli_version = Some(smoke(&proof.sandbox, &workspace, &nonce, |step, args| {
        invoke(&cli, &workspace, out, step, args, &mut proof.steps)
    })?);
    ensure!(
        proof.cli_sha256.as_deref() == Some(&digest::sha256_file(&cli)?),
        "sbx executable changed during validation"
    );
    Ok(nonce)
}

#[derive(Debug, PartialEq, Eq)]
enum ValidationHost<'a> {
    Mac,
    Linux(&'a str),
}

fn run_selected_hosts<'a>(
    linux_host: Option<&'a str>,
    mut run: impl FnMut(ValidationHost<'a>) -> Result<()>,
) -> Result<()> {
    run(ValidationHost::Mac)?;
    if let Some(host) = linux_host {
        run(ValidationHost::Linux(host))?;
    }
    Ok(())
}

fn execute(out: &Path, proof: &mut Proof) -> Result<()> {
    let linux_host = proof.linux_host.clone();
    if let Some(host) = &linux_host {
        ensure!(
            crate::release::util::valid_ssh_destination(host),
            "invalid LINUX_HOST; expected user@host or an SSH host alias"
        );
    }
    let mut nonce = String::new();
    run_selected_hosts(linux_host.as_deref(), |host| {
        match host {
            ValidationHost::Mac => nonce = execute_mac(out, proof)?,
            ValidationHost::Linux(host) => {
                linux::run(&out.join("linux"), host, &proof.sandbox, &nonce)?
            }
        }
        Ok(())
    })?;
    proof.outcome = Outcome::Passed;
    proof.files = files(out)?;
    proof.verify(out)
}

pub fn run(args: &[String]) -> Result<ExitCode> {
    let args = Args::try_parse_from(
        std::iter::once("sbx-validation".to_owned()).chain(args.iter().cloned()),
    )?;
    match args.command {
        Action::Run { out, linux_host } => {
            let linux_host = linux_host
                .or_else(|| std::env::var("LINUX_HOST").ok())
                .filter(|host| !host.is_empty());
            let parent = if out.is_absolute() {
                out
            } else {
                crate::gates::root().join(out)
            };
            fs::create_dir_all(&parent)?;
            let out = tempfile::Builder::new()
                .prefix("run-")
                .tempdir_in(parent)?
                .keep()
                .canonicalize()?;
            match &linux_host {
                Some(host) => eprintln!("validate-sbx: selected macOS + Linux ({host})"),
                None => eprintln!("validate-sbx: selected macOS only (LINUX_HOST is unset)"),
            }
            eprintln!("validate-sbx: report and logs in {}", out.display());
            let mut proof = Proof {
                schema: 2,
                outcome: Outcome::Blocked,
                target: if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                    TARGET.into()
                } else {
                    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
                },
                sandbox: String::new(),
                linux_host,
                cli_version: None,
                cli_sha256: None,
                steps: BTreeMap::new(),
                files: BTreeMap::new(),
                error: None,
            };
            if let Err(error) = execute(&out, &mut proof) {
                let stop = error.downcast_ref::<crate::release::error::Stop>();
                proof.outcome =
                    if stop.is_some_and(|s| s.kind == crate::release::error::Kind::Blocked) {
                        Outcome::Blocked
                    } else if proof.steps.contains_key("create") {
                        Outcome::Failed
                    } else {
                        Outcome::Blocked
                    };
                proof.error = Some(match stop {
                    Some(stop) => format!("[{}] {}", stop.code, stop.message),
                    None => format!("{error:#}"),
                });
            }
            let report = format!(
                "SBX validation: {:?}\nReport: {}\n{}\n{}\n{}\n",
                proof.outcome,
                out.display(),
                LIMITS,
                ACCEPTANCE,
                proof.error.as_deref().unwrap_or("All smoke checks passed.")
            );
            fs::write(out.join("summary.txt"), &report)?;
            proof.files = files(&out)?;
            fs::write(out.join("proof.json"), serde_json::to_vec_pretty(&proof)?)?;
            eprintln!("{report}");
            Ok(ExitCode::from(match proof.outcome {
                Outcome::Passed => 0,
                Outcome::Blocked => 2,
                _ => 1,
            }))
        }
        Action::Verify { dir } => {
            let proof: Proof = serde_json::from_slice(&fs::read(dir.join("proof.json"))?)?;
            proof.verify(&dir)?;
            println!("SBX smoke report verified. {LIMITS} {ACCEPTANCE}");
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
mod tests;
