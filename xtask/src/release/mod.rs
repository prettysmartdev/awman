//! `cargo xtask release <subcommand>`: one command starts or resumes a
//! release (design 3.1); everything else is read-only, cleanup, rehearsal,
//! or a step of `.github/workflows/release.yml`. This file only parses
//! arguments and assembles the production or fault-injection world.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

mod abandon;
mod abi;
pub mod error;
pub mod evidence;
pub mod forge;
pub mod forge_dir;
mod gh;
pub mod journal;
pub mod native;
mod phases;
pub mod plan;
mod preflight;
mod produce;
mod rehearse;
pub mod run;
pub(crate) mod runtime_smoke;
pub(crate) mod sbx;
mod status;
pub mod util;
mod workflows;
pub mod world;

/// The only repository production releases are published from.
pub const CANONICAL_REPO: &str = "prettysmartdev/awman";

const USAGE: &str = "\
usage: cargo xtask release <subcommand> [args]

  run [--version vX.Y.Z] [--linux-host user@host] [--yes] [--supersede]
      [--takeover OWNER] [--rehearse]
      [--decided-at RFC3339] [--rehearsal-repo OWNER/REPO]
                                           start or resume a release (make release);
                                           the hour target counts from --decided-at;
                                           --rehearsal-repo: complete rehearsal in an
                                           allowlisted disposable repository
  doctor [--linux-host user@host]          prerequisites only; changes nothing (make release-doctor)
  status [--version vX.Y.Z]                phase table, first error, recovery (make release-status)
  abandon --version vX.Y.Z                 delete this attempt's draft, claim and state
  rehearse [--repeat N] [--scenario NAME] [--out DIR] [--list]
                                           fault-injected rehearsal, no network (make release-rehearse)
  check-workflows [--root DIR]             permission closure and release.yml contract
  produce | kit-pack | smoke | build-record  steps of release.yml (hosted runners)

Test seam: --world DIR runs run/doctor/status/abandon against the
directory-backed fault world (forge_dir.rs) instead of GitHub and real hosts;
`world-seed --world DIR` creates one. --state DIR overrides the private state
root (also AWMAN_RELEASE_STATE).
";

struct Args {
    rest: Vec<String>,
}

impl Args {
    fn value(&mut self, name: &str) -> anyhow::Result<Option<String>> {
        match self.rest.iter().position(|a| a == name) {
            Some(i) => {
                anyhow::ensure!(i + 1 < self.rest.len(), "{name} needs a value");
                let v = self.rest.remove(i + 1);
                self.rest.remove(i);
                Ok(Some(v))
            }
            None => Ok(None),
        }
    }
    fn flag(&mut self, name: &str) -> bool {
        match self.rest.iter().position(|a| a == name) {
            Some(i) => {
                self.rest.remove(i);
                true
            }
            None => false,
        }
    }
    fn done(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.rest.is_empty(),
            "unexpected arguments: {}",
            self.rest.join(" ")
        );
        Ok(())
    }
}

fn toplevel() -> anyhow::Result<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    anyhow::ensure!(out.status.success(), "not inside a Git checkout");
    Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

/// `$AWMAN_RELEASE_STATE` or `$(git rev-parse --git-common-dir)/awman-release`.
fn default_state_root() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("AWMAN_RELEASE_STATE") {
        return Ok(PathBuf::from(p));
    }
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()?;
    anyhow::ensure!(out.status.success(), "not inside a Git checkout");
    Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()).join("awman-release"))
}

fn production_world(yes: bool, interrupted: Arc<AtomicBool>) -> anyhow::Result<world::World> {
    let root = toplevel()?;
    let source = world::GitSource { root: root.clone() };
    let repo = world::Source::origin_repo(&source).map_err(|s| anyhow::anyhow!("{s}"))?;
    Ok(world::World {
        forge: Box::new(gh::GhForge {
            repo,
            checkout: root,
        }),
        source: Box::new(source),
        tools: Box::new(world::ExeTools),
        clock: Box::new(world::SystemClock),
        prompt: Box::new(world::TtyPrompt { yes }),
        hosts: Box::new(native::RealHosts::new()?),
        interrupted,
        poll_seconds: 30,
        production: true,
    })
}

fn interrupt_flag() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        let _ = signal_hook::flag::register(sig, flag.clone());
    }
    flag
}

fn report(result: error::Step) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(stop) => stop.report(),
    }
}

pub fn run(args: &[String]) -> anyhow::Result<ExitCode> {
    let Some((subcommand, rest)) = args.split_first() else {
        eprint!("{USAGE}");
        return Ok(ExitCode::from(2));
    };
    let mut a = Args {
        rest: rest.to_vec(),
    };
    match subcommand.as_str() {
        "run" | "doctor" => {
            let opts = run::Options {
                version: a.value("--version")?,
                linux_host: a.value("--linux-host")?,
                rehearse: a.flag("--rehearse"),
                supersede: a.flag("--supersede"),
                takeover: a.value("--takeover")?,
                canonical_repo: None,
                preflight_only: subcommand == "doctor",
                rehearsal_repo: a.value("--rehearsal-repo")?,
                decided_at: a
                    .value("--decided-at")?
                    .map(|t| {
                        chrono::DateTime::parse_from_rfc3339(&t)
                            .map(|t| t.with_timezone(&chrono::Utc))
                            .map_err(|e| anyhow::anyhow!("--decided-at {t}: {e}"))
                    })
                    .transpose()?,
            };
            let yes = a.flag("--yes");
            let world_dir = a.value("--world")?;
            let state = a.value("--state")?;
            a.done()?;
            let interrupted = interrupt_flag();
            let (w, opts, root) = match &world_dir {
                Some(dir) => {
                    let root = state
                        .map(PathBuf::from)
                        .unwrap_or_else(|| Path::new(dir).join("state"));
                    (forge_dir::world(Path::new(dir), interrupted), opts, root)
                }
                None => {
                    let root = state
                        .map(PathBuf::from)
                        .map(Ok)
                        .unwrap_or_else(default_state_root)?;
                    // A disposable rehearsal is pinned to its own allowlisted
                    // repository instead, and refused for the canonical one.
                    let opts = run::Options {
                        canonical_repo: opts
                            .rehearsal_repo
                            .is_none()
                            .then(|| CANONICAL_REPO.into()),
                        ..opts
                    };
                    (production_world(yes, interrupted)?, opts, root)
                }
            };
            Ok(report(
                journal::StateRoot::new(root).and_then(|r| run::run(&w, &r, &opts)),
            ))
        }
        "status" | "abandon" => {
            let version = a
                .value("--version")?
                .or_else(|| a.rest.first().cloned().filter(|v| !v.starts_with('-')));
            if let Some(v) = &version {
                a.rest.retain(|x| x != v);
            }
            let world_dir = a.value("--world")?;
            let state = a.value("--state")?;
            let rehearsal = a.flag("--rehearse");
            let disposable = a.value("--rehearsal-repo")?.is_some();
            a.done()?;
            let root = match (&state, &world_dir) {
                (Some(s), _) => PathBuf::from(s),
                (None, Some(w)) => Path::new(w).join("state"),
                (None, None) => default_state_root()?,
            };
            let Some(version) = version.filter(|v| !v.is_empty()) else {
                anyhow::bail!("{subcommand} needs --version vX.Y.Z (or VERSION=… for make)");
            };
            let mode = match (rehearsal, disposable) {
                (true, _) => "rehearsal",
                (false, true) => "disposable",
                _ => "production",
            };
            let dir = root.join(run::state_dir_name(&version, mode));
            let w = match &world_dir {
                Some(d) => Some(forge_dir::world(Path::new(d), interrupt_flag())),
                None => production_world(false, interrupt_flag()).ok(),
            };
            if subcommand == "status" {
                return status::show(&dir, w.as_ref());
            }
            let w = w.ok_or_else(|| anyhow::anyhow!("abandon needs GitHub access"))?;
            Ok(report(abandon::abandon(&w, &dir)))
        }
        "rehearse" => rehearse::run(rest),
        "world-seed" => {
            let dir = a
                .value("--world")?
                .ok_or_else(|| anyhow::anyhow!("world-seed needs --world DIR"))?;
            let version = a
                .value("--version")?
                .unwrap_or_else(|| rehearse::VERSION.into());
            a.done()?;
            let plan: serde_json::Value = serde_json::from_str(include_str!(
                "../../tests/release_contract/fixtures/release-plan.json"
            ))?;
            std::fs::create_dir_all(&dir)?;
            forge_dir::seed(Path::new(&dir), &version, plan);
            Ok(ExitCode::SUCCESS)
        }
        "check-workflows" => workflows::run(rest),
        "produce" | "produce-native" | "kit-pack" | "smoke" | "build-record" => {
            produce::run_cmd(subcommand, rest)
        }
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        other => {
            eprintln!("xtask release: unknown subcommand `{other}`\n\n{USAGE}");
            Ok(ExitCode::from(2))
        }
    }
}
