//! `cargo xtask release abandon`: delete an unpromoted candidate's draft
//! (`make release-abandon VERSION=…`), after its evidence is retained.
//!
//! Owner: workflow step `candidate-promote-pipeline` (see the WI 0144
//! ownership map). The signature of `run` is fixed by `release/mod.rs`.
//!
//! A promoted or published version is refused: published releases are never
//! removed or moved. The evidence stays in the candidate run's retained
//! `candidate-record-…` artifact, and is also copied to
//! `target/release-abandoned/<version>-c<N>/` before the draft is deleted.

use super::state::args::{Flags, Stop};
use super::state::decide::{self, AbandonAction};
use super::state::{self, hub, ops, record};
use anyhow::Result;
use std::path::PathBuf;
use std::process::ExitCode;

pub fn run(args: &[String]) -> Result<ExitCode> {
    let Some((version, rest)) = args.split_first() else {
        eprintln!("usage: cargo xtask release abandon <vX.Y.Z> [--repo owner/name] [--keep DIR]");
        return Ok(ExitCode::from(2));
    };
    if !record::valid_version(version) {
        eprintln!("version must be vX.Y.Z, got {version}");
        return Ok(ExitCode::from(2));
    }
    let f = Flags::parse(rest, &[])?;
    let repo = super::status::repo(f.get("repo"))?;
    let hub = hub::open(&repo, f.get("store"))?;
    let existing = state::existing(hub.as_ref(), version)?;
    let action = match decide::abandon_action(version, &existing, hub.tag(version)?.as_deref()) {
        Ok(a) => a,
        Err(stop) => return Ok(stop.report()),
    };
    let AbandonAction::Delete {
        release_id,
        candidate,
    } = action
    else {
        println!("{version}: no unpromoted candidate to abandon");
        return Ok(ExitCode::SUCCESS);
    };
    let e = existing
        .iter()
        .find(|e| e.release.id == release_id)
        .expect("chosen above");
    let keep = f.get("keep").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(format!("target/release-abandoned/{version}-c{candidate}"))
    });
    std::fs::create_dir_all(&keep)?;
    let mut kept = 0;
    for a in &e.release.assets {
        if [
            record::STATE_ASSET,
            record::GATE_ASSET,
            record::MAIN_GATE_ASSET,
            record::SUMS,
        ]
        .contains(&a.name.as_str())
        {
            ops::download(hub.as_ref(), &e.release, &a.name, &keep.join(&a.name))?;
            kept += 1;
        }
    }
    let retained = state::recorded(hub.as_ref(), version, record::PRODUCTION)?.contains(&candidate);
    if kept == 0 && !retained {
        return Ok(Stop::Blocked(format!(
            "candidate {candidate} has neither a retained record artifact nor a state asset; \
             refusing to delete evidence. Inspect draft {release_id} by hand"
        ))
        .report());
    }
    hub.delete_release(release_id)?;
    println!("Abandoned candidate {candidate} of {version} (draft {release_id} deleted)");
    println!(
        "  evidence kept in {}{}",
        keep.display(),
        if retained {
            " and the run's record artifact"
        } else {
            ""
        }
    );
    Ok(ExitCode::SUCCESS)
}
