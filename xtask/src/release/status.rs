//! `make release-status`: read-only phase table, first error and its
//! recovery, and advisory signals. Takes no lock and changes nothing.

use super::evidence::{check_receipt, Seal};
use super::journal::{Journal, Status, Store, PHASES};
use super::world::World;
use std::path::Path;

/// Advisory lines (they never block, design 3.7): the latest CI run at the
/// tip of the default branch and the latest nightly run with its age.
/// Unknown or unavailable state is shown as such, never omitted.
pub fn advisories(w: &World, ci_workflow: &str) -> Vec<String> {
    let now = w.clock.now();
    let describe = |r: Option<super::forge::RunInfo>| match r {
        Some(r) => format!(
            "{} {} ({} h ago, run {})",
            r.conclusion.clone().unwrap_or(r.status.clone()),
            r.updated.date_naive(),
            (now - r.updated).num_hours(),
            r.id
        ),
        None => "none recorded".into(),
    };
    let branch = w
        .forge
        .repo()
        .map(|r| r.default_branch)
        .unwrap_or_else(|_| "main".into());
    let mut out = Vec::new();
    match w.forge.get_ref(&format!("heads/{branch}")) {
        Ok(Some(main)) => {
            let latest = w
                .forge
                .runs(ci_workflow, &main)
                .map(|r| r.into_iter().max_by_key(|r| r.id));
            out.push(format!(
                "advisory: latest CI at {branch} {}: {} (does not block)",
                &main[..12.min(main.len())],
                latest
                    .map(describe)
                    .unwrap_or_else(|e| format!("unavailable ({})", e.code))
            ));
        }
        Ok(None) => out.push(format!("advisory: {branch} is unknown (does not block)")),
        Err(e) => out.push(format!(
            "advisory: CI at {branch} unavailable ({}) (does not block)",
            e.code
        )),
    }
    out.push(format!(
        "advisory: latest nightly on {branch}: {} (does not block)",
        w.forge
            .latest_run("nightly.yml", &branch)
            .map(describe)
            .unwrap_or_else(|e| format!("unavailable ({})", e.code))
    ));
    out
}

/// `Ok(())` when the receipt is bound to the recorded seal; otherwise why not.
pub fn receipt_state(store: &Store, j: &Journal) -> Result<(), String> {
    let seal: Seal = super::util::read_json(&store.path("evidence/seal.json"))
        .map_err(|_| "no seal recorded".to_string())?;
    if Some(seal.digest()) != j.release.seal_sha256 {
        return Err("evidence/seal.json differs from the recorded seal".into());
    }
    check_receipt(&store.path("evidence/receipt.json"), j, &seal).map(|_| ())
}

pub fn render(j: &Journal, w: Option<&World>, receipt: Result<(), String>) -> String {
    let mut s = format!(
        "release {}{}  commit {}  generation {}\n",
        j.version,
        if j.rehearsal { " (rehearsal)" } else { "" },
        &j.commit[..12],
        j.generation
    );
    for p in PHASES {
        let rec = j.phases.get(*p).cloned().unwrap_or_default();
        let status = match rec.status {
            Status::Pending => "pending",
            Status::Running => "running",
            Status::Done => "done",
            Status::Failed => "FAILED",
            Status::Blocked => "BLOCKED",
        };
        let took = match (rec.started, rec.finished) {
            (Some(a), Some(b)) => format!("{} s", (b - a).num_seconds()),
            _ => String::new(),
        };
        s.push_str(&format!("  {p:<10} {status:<8} {took}\n"));
    }
    if let Some((p, e)) = PHASES
        .iter()
        .filter_map(|p| {
            j.phases
                .get(*p)
                .and_then(|r| r.error.clone())
                .map(|e| (p, e))
        })
        .next()
    {
        s.push_str(&format!(
            "first error in {p}: [{}] {}\n  recovery: {}\n",
            e.code, e.message, e.remedy
        ));
    }
    match receipt {
        Ok(()) => s.push_str("verified: evidence/receipt.json is bound to the seal\n"),
        Err(why) if j.release.published => s.push_str(&format!(
            "published but NOT yet verified ({why}): run make release to finish public verification\n"
        )),
        Err(_) => {}
    }
    for (id, n) in &j.native {
        s.push_str(&format!("  {id:<20} {}\n", n.outcome));
    }
    let active: f64 = j.invocations.iter().map(|i| i.monotonic_seconds).sum();
    s.push_str(&format!(
        "timing: {} invocation(s), {active:.0} s of controller time (monotonic)\n",
        j.invocations.len()
    ));
    if let Some(w) = w {
        let (minutes, missed) = j.target(w.clock.now());
        s.push_str(&format!(
            "elapsed: {minutes} min since {}{}\n",
            if j.decided_at.is_some() {
                "the recorded decision"
            } else {
                "admission"
            },
            if missed { " - hour target MISSED" } else { "" }
        ));
        for line in advisories(w, "ci.yml") {
            s.push_str(&line);
            s.push('\n');
        }
    }
    s
}

pub fn show(dir: &Path, w: Option<&World>) -> anyhow::Result<std::process::ExitCode> {
    let store = Store::view(dir);
    match store.load() {
        Ok(Some(j)) => {
            print!("{}", render(&j, w, receipt_state(&store, &j)));
            Ok(std::process::ExitCode::SUCCESS)
        }
        Ok(None) => {
            println!("no release attempt recorded in {}", dir.display());
            Ok(std::process::ExitCode::SUCCESS)
        }
        Err(stop) => Ok(stop.report()),
    }
}
