//! Gate commands. CI/release callers can use the same pure planning and checking APIs.
#![allow(dead_code)]
pub mod check;
pub mod discovery;
pub mod doc_refs;
pub mod plan;
pub mod render;
pub mod selection;
#[cfg(test)]
pub(crate) mod tests;
use clap::{Parser, Subcommand};
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

#[derive(Parser)]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Check {
        #[arg(long)]
        report_only: bool,
        #[arg(long)]
        allow_placeholders: bool,
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        report: Option<PathBuf>,
    },
    Render {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    Plan {
        #[arg(long)]
        gate: String,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        head: Option<String>,
        #[arg(long)]
        json: Option<PathBuf>,
        #[arg(long)]
        paths_file: Option<PathBuf>,
        #[arg(long)]
        timings: Option<PathBuf>,
        #[arg(long, default_value = "local")]
        event: String,
        #[arg(long, default_value = "local")]
        repository_id: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
}
/// The repository root, resolved when xtask runs rather than when it was built.
///
/// A compile-time `env!("CARGO_MANIFEST_DIR")` goes stale when one `target/`
/// directory is shared by checkouts at different paths (a container and the
/// host worktree): cargo's fingerprints are workspace-relative, so it reuses
/// the binary and every command would point at the other checkout.
pub fn root() -> PathBuf {
    resolve_root(
        std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from),
        std::env::current_dir().ok(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

/// Resolution order: the `CARGO_MANIFEST_DIR` cargo sets at runtime for
/// `cargo run` and `cargo test` (xtask's own directory), then the nearest
/// ancestor of the working directory holding both `Cargo.toml` and
/// `ci/gates.toml`, then the build-time manifest directory as a last resort.
pub(crate) fn resolve_root(
    runtime_manifest_dir: Option<PathBuf>,
    cwd: Option<PathBuf>,
    compiled_manifest_dir: &Path,
) -> PathBuf {
    let is_root =
        |dir: &Path| dir.join("Cargo.toml").is_file() && dir.join("ci/gates.toml").is_file();
    if let Some(parent) = runtime_manifest_dir.as_deref().and_then(Path::parent) {
        if is_root(parent) {
            return parent.to_path_buf();
        }
    }
    if let Some(found) = cwd
        .as_deref()
        .and_then(|c| c.ancestors().find(|d| is_root(d)))
    {
        return found.to_path_buf();
    }
    compiled_manifest_dir
        .parent()
        .unwrap_or(compiled_manifest_dir)
        .to_path_buf()
}
pub fn run(args: &[String]) -> anyhow::Result<ExitCode> {
    let args =
        Arguments::try_parse_from(std::iter::once("gates".to_owned()).chain(args.iter().cloned()))?;
    match args.command {
        Command::Check {
            report_only,
            allow_placeholders,
            root: r,
            report,
        } => {
            anyhow::ensure!(
                !allow_placeholders || report_only,
                "--allow-placeholders is diagnostic only; combine with --report-only"
            );
            let r = r.unwrap_or_else(root);
            let report_data = check::check(&r, allow_placeholders)?;
            let text = report_data.markdown();
            print!("{text}");
            if let Some(p) = report {
                std::fs::write(p, text)?;
            }
            Ok(if report_only || report_data.errors.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Command::Render { check, root: r } => render::run(&r.unwrap_or_else(root), check),
        Command::Plan {
            gate,
            base,
            head,
            json,
            paths_file,
            timings,
            event,
            repository_id,
            root: r,
        } => {
            let r = r.unwrap_or_else(root);
            let m = crate::manifest::load(&r)?;
            let paths = if let Some(path) = paths_file {
                std::fs::read_to_string(path)?
                    .lines()
                    .map(str::to_owned)
                    .collect()
            } else if let (Some(base), Some(head)) = (&base, &head) {
                plan::changed_paths(&r, base, head)?
            } else {
                Vec::new()
            };
            let mut request = plan::Request {
                repository_id,
                event,
                head_sha: head.unwrap_or_else(|| "local".into()),
                base_sha: base,
                paths,
                diff_complete: true,
                admitted_platforms: Default::default(),
                suite_durations_ms: Default::default(),
                suite_test_counts: Default::default(),
                main_durations_ms: Default::default(),
            };
            if let Some(path) = timings {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Timings {
                    suite_durations_ms: std::collections::BTreeMap<String, u64>,
                    #[serde(default)]
                    suite_test_counts: std::collections::BTreeMap<String, u32>,
                    main_durations_ms: Vec<u64>,
                }
                let timings: Timings = serde_json::from_slice(&std::fs::read(path)?)?;
                request.suite_durations_ms = timings.suite_durations_ms;
                request.suite_test_counts = timings.suite_test_counts;
                request.main_durations_ms = timings.main_durations_ms;
            }
            let p = plan::create(&m, &gate, request, chrono::Utc::now().date_naive())?;
            for c in &p.cells {
                println!(
                    "{}: {} ({})",
                    c.cell.id,
                    if c.selected {
                        "selected"
                    } else {
                        "not selected"
                    },
                    c.reason
                );
            }
            if let Some(path) = json {
                std::fs::write(path, serde_json::to_vec_pretty(&p)?)?;
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}
