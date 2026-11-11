//! `cargo xtask ci`: helpers the GitHub workflows call (cache keys, digests,
//! tool installs, plan outputs, runner health, leak sweep, summaries, the
//! shell/NOTICE lints and the nightly flake and seeded-defect tooling).
//!
//! Owner: workflow step `ci-workflows` (see the WI 0144 ownership map). The
//! signature of `run` is fixed by `main.rs`; everything else is the owner's.
//! Core modules never depend on this one: it only reads the manifest, the plan
//! and the evidence schema through their public APIs.

mod args;
mod cache;
mod digest;
mod flake;
mod lint_notice;
mod lint_shell;
mod plan_outputs;
mod preflight;
mod seeded;
mod summary;
mod sweep;
mod timings;
mod tools;

use anyhow::Result;
use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask ci <helper> [args]

  cache-key --build ID [--scope S]           exact cache key over every declared input
  cache-stats --build ID --key K --hit B --out DIR
  digest record --out FILE name=path...      record SHA-256 of build outputs
  digest verify --file FILE [--expect name=sha256]...
  install-tool NAME                          install a pinned tool from its verified source
  plan-outputs --plan FILE --hardware BOOL   job-graph outputs of plan.json (GITHUB_OUTPUT lines)
  runner-preflight --platform ID             health of a self-hosted runner (blocked, never queued)
  sweep                                      fail if this run leaked processes, containers or temp roots
  summary ...                                gate summary markdown for the step summary
  lint-shell [--allow-missing-shellcheck]    bash -n + shellcheck over every tracked script
  lint-notice                                NOTICE.third-party.md against Cargo.lock
  actionlint-config --out FILE               custom runner labels from the manifest
  flake-report --in DIR --out FILE           intermittent failures across repeated runs
  seeded-defects pipeline|source             seeded-defect runs (nightly)
  timings --store FILE --out FILE            plan timing input from the timing store
";

pub fn run(args: &[String]) -> anyhow::Result<ExitCode> {
    let Some((cmd, rest)) = args.split_first() else {
        eprint!("{USAGE}");
        return Ok(ExitCode::from(2));
    };
    let root = crate::gates::root();
    match cmd.as_str() {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        "cache-key" => cache::key_command(&root, rest),
        "cache-stats" => cache::stats_command(rest),
        "digest" => digest::run(rest),
        "install-tool" => tools::install_command(&root, rest),
        "plan-outputs" => plan_outputs::run(&root, rest),
        "runner-preflight" => preflight::run(&root, rest),
        "sweep" => sweep::run(rest),
        "summary" => summary::run(&root, rest),
        "lint-shell" => lint_shell::run(&root, rest),
        "lint-notice" => lint_notice::run(&root),
        "actionlint-config" => tools::actionlint_config(&root, rest),
        "flake-report" => flake::run(rest),
        "seeded-defects" => seeded::run(&root, rest),
        "timings" => timings::run(rest),
        other => {
            eprint!("xtask ci: unknown helper `{other}`\n\n{USAGE}");
            Ok(ExitCode::from(2))
        }
    }
}

/// Exit-code convention shared with every xtask command: 0 ok, 1 check failed.
pub(crate) fn verdict(ok: bool) -> Result<ExitCode> {
    if ok {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}
