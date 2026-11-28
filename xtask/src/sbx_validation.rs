//! Opt-in smoke test of the developer's logged-in SBX session.
use crate::evidence::{digest, schema::Outcome};
use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

mod lifecycle;
mod linux;
// The release-owned SBX smoke (WI 0094). Its items are used through
// `release::runtime_smoke::sbx` once the release orchestration wires that in.
pub(crate) mod release;

use lifecycle::*;

const TARGET: &str = "aarch64-apple-darwin";
const LIMITS: &str = "Experimental SBX smoke evidence: logged-in CLI, sandbox lifecycle, guest execution and workspace sharing. No provider tasks, credential-isolation claim or full policy qualification. The production SBX guard is unchanged. This is not a release gate.";
const ACCEPTANCE: &str = "A successful run is sufficient validation to enable experimental SBX on the tested platforms (Apple silicon macOS, plus Linux when LINUX_HOST is set). No further provider tests or manual qualification are required for enablement.";

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

fn execute_mac(out: &Path, proof: &mut Proof) -> Result<String> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        anyhow::bail!("validate-sbx requires Apple silicon macOS");
    }
    let cli = locate_cli()?;
    proof.cli_sha256 = Some(digest::sha256_file(&cli)?);
    let workspace = out.join("workspace");
    fs::create_dir(&workspace)?;
    let (sandbox, nonce) = new_identity("awman-validate-")?;
    proof.sandbox = sandbox;
    proof.cli_version = Some(smoke(&proof.sandbox, &workspace, &nonce, |step, args| {
        invoke(
            &cli,
            &workspace,
            out,
            step,
            args,
            &mut proof.steps,
            "validate-sbx [macOS]",
            &Deadlines::DEFAULT,
        )
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
