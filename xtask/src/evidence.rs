//! Evidence APIs are shared with the CI controller and release pipeline.
#![allow(dead_code)]
pub mod actions;
pub mod aggregate;
pub mod digest;
pub mod emit;
pub mod history;
pub mod junit;
pub mod schema;
pub mod summary;
#[cfg(test)]
mod tests;
use clap::{Parser, Subcommand};
use std::{path::PathBuf, process::ExitCode};
#[derive(Parser)]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Emit {
        #[arg(long)]
        cell: String,
        #[arg(long)]
        gate: String,
        #[arg(long)]
        junit: PathBuf,
        #[arg(long)]
        expected: PathBuf,
        #[arg(long)]
        phase: Option<String>,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        metadata: Option<PathBuf>,
        #[arg(long)]
        results: Option<PathBuf>,
    },
    Aggregate {
        #[arg(long)]
        gate: String,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long = "in")]
        input: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        summary: Option<PathBuf>,
    },
    History {
        #[command(subcommand)]
        command: History,
    },
}
#[derive(Subcommand)]
enum History {
    AppendRun {
        #[arg(long)]
        store: PathBuf,
        #[arg(long)]
        run: PathBuf,
        #[arg(long)]
        gate: String,
    },
    AppendGate {
        #[arg(long)]
        store: PathBuf,
        #[arg(long)]
        evidence: PathBuf,
    },
    Append {
        #[arg(long)]
        store: PathBuf,
        #[arg(long)]
        evidence: PathBuf,
    },
    Report {
        #[arg(long)]
        store: PathBuf,
    },
}
pub fn run(args: &[String]) -> anyhow::Result<ExitCode> {
    let args = Arguments::try_parse_from(
        std::iter::once("evidence".to_owned()).chain(args.iter().cloned()),
    )?;
    match args.command {
        Command::Emit {
            cell,
            gate,
            junit,
            expected,
            phase,
            out,
            metadata,
            results,
        } => {
            let metadata = metadata.unwrap_or_else(|| out.join("run-metadata.json"));
            let mut e: schema::Evidence =
                serde_json::from_slice(&std::fs::read(&metadata).map_err(|e| {
                    anyhow::anyhow!(
                        "run metadata {} required to bind inputs/producer/phases: {e}",
                        metadata.display()
                    )
                })?)?;
            anyhow::ensure!(
                e.cell == cell && e.gate == gate,
                "emit cell/gate metadata mismatch"
            );
            let expected: schema::ExpectedSet = serde_json::from_slice(&std::fs::read(expected)?)?;
            let mut parsed = junit::parse(&std::fs::read_to_string(junit)?)?;
            if let Some(path) = results {
                emit::merge_records(
                    &mut parsed,
                    &serde_json::from_slice::<Vec<schema::ResultRecord>>(&std::fs::read(path)?)?,
                )?;
            }
            if let Some(phase) = phase {
                e.phases.insert(phase, schema::Outcome::Blocked);
            }
            let e = emit::finish(e, &expected, parsed)?;
            emit::write(&out, &e)?;
            Ok(if e.outcome == schema::Outcome::Passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Command::Aggregate {
            gate,
            plan,
            input: _,
            out,
            summary,
        } => {
            let m = crate::manifest::load(&crate::gates::root())?;
            let p: crate::gates::plan::Plan = serde_json::from_slice(&std::fs::read(plan)?)?;
            anyhow::ensure!(p.gate == gate, "aggregate gate/plan mismatch");
            manifest_check(&m)?;
            std::fs::create_dir_all(&out)?;
            // Re-download and digest-check artifacts through the service. The
            // caller's --in directory is diagnostic material, never provenance.
            let report = match actions::aggregate(&crate::gates::root(), &m, &p) {
                Ok(report) => report,
                Err(error) => {
                    let report = serde_json::json!({"schema":1,"kind":"gate","gate":gate,"verdict":"blocked","errors":[format!("{error:#}")]});
                    std::fs::write(
                        out.join("gate-evidence.json"),
                        serde_json::to_vec_pretty(&report)?,
                    )?;
                    if let Some(path) = summary {
                        std::fs::write(path, format!("{error:#}"))?;
                    }
                    eprintln!("{error:#}");
                    return Ok(ExitCode::from(1));
                }
            };
            std::fs::write(
                out.join("gate-evidence.json"),
                serde_json::to_vec_pretty(&report)?,
            )?;
            if let Some(path) = summary {
                std::fs::write(
                    path,
                    format!(
                        "Verdict: {:?}\n{}\nMissing: {}",
                        report.verdict,
                        report.errors.join("\n"),
                        report.missing.join(", ")
                    ),
                )?;
            }
            for e in &report.errors {
                eprintln!("{e}");
            }
            Ok(if report.verdict == schema::Outcome::Passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Command::History { command } => match command {
            History::AppendRun { store, run, gate } => {
                history::raw::append_run(&store, &run, &gate)?;
                Ok(ExitCode::SUCCESS)
            }
            History::AppendGate { store, evidence } => {
                history::raw::append_gate(&store, &evidence)?;
                Ok(ExitCode::SUCCESS)
            }
            History::Append { store, evidence } => {
                let e: schema::Evidence = serde_json::from_slice(&std::fs::read(evidence)?)?;
                history::append(&store, &e)?;
                Ok(ExitCode::SUCCESS)
            }
            History::Report { store } => {
                print!("{}", history::report(&history::read(&store)?));
                Ok(ExitCode::SUCCESS)
            }
        },
    }
}
fn manifest_check(m: &crate::manifest::Manifest) -> anyhow::Result<()> {
    crate::manifest::validate::validate(m, chrono::Utc::now().date_naive())
}
