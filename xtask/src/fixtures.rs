//! `cargo xtask fixtures`: content-addressed test fixtures.
//!
//!   fetch [--tier <id>] [--store <dir>]   place every pinned fixture the tier needs
//!   verify [--store <dir>]                check every placed fixture against its pin
//!   build <recipe> …                      explicit preparation recipes (fixtures/recipes)
//!   publish-plan                          JSON list of fixtures to build and publish (Fixtures workflow)
//!
//! The store layout is the one `awman-test-support` reads: `<store>/<file or id>`
//! and `<store>/previous-release/<triple>/<asset>`, where `<store>` is
//! `AWMAN_TEST_FIXTURES` or `<workspace>/target/awman-fixtures`. A fixture
//! without a published SHA-256 is BLOCKED: fetch and verify fail, they never
//! place unverified bytes (design 16.9, row P7).
pub mod recipes;
use anyhow::{bail, Context, Result};
use std::{
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use crate::manifest::Manifest;

pub fn run(args: &[String]) -> Result<ExitCode> {
    let Some((cmd, rest)) = args.split_first() else {
        eprintln!(
            "usage: cargo xtask fixtures fetch [--tier T] | verify | build <recipe> | publish-plan"
        );
        return Ok(ExitCode::from(2));
    };
    let root = crate::gates::root();
    match cmd.as_str() {
        "build" => {
            let Some((recipe, args)) = rest.split_first() else {
                bail!("fixtures build requires a recipe");
            };
            recipes::run(recipe, args)
        }
        "fetch" => fetch(&root, flag(rest, "--tier").as_deref(), &store(&root, rest)),
        "verify" => verify(&root, &store(&root, rest)),
        "publish-plan" => publish_plan(&root),
        other => bail!("unknown fixtures command {other}"),
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn store(root: &Path, args: &[String]) -> PathBuf {
    if let Some(dir) = flag(args, "--store") {
        return PathBuf::from(dir);
    }
    // The same variable the support crate reads (manifest [test_env].fixtures).
    std::env::var_os("AWMAN_TEST_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target/awman-fixtures"))
}

/// One fixture file the tier needs: where it goes and what it must hash to.
struct Wanted {
    id: String,
    path: PathBuf,
    sha256: Option<String>,
    source: Option<String>,
    pending: Option<String>,
}

fn wanted(m: &Manifest, tier: Option<&str>, store: &Path) -> Vec<Wanted> {
    let mut out = Vec::new();
    for (id, f) in &m.fixtures {
        if f.verified_record.is_some() {
            // Verified in-repository records (payload pins) are checked by their own tools.
            continue;
        }
        if tier.is_some_and(|t| !f.tiers.is_empty() && !f.tiers.iter().any(|x| x == t)) {
            continue;
        }
        out.push(Wanted {
            id: id.clone(),
            path: store.join(f.file.clone().unwrap_or_else(|| id.clone())),
            sha256: f.sha256.clone(),
            source: f.source.clone(),
            pending: f.pending.clone(),
        });
    }
    let previous_needed = tier.is_none_or(|t| {
        m.tiers
            .get(t)
            .is_some_and(|tier| tier.guarantees.iter().any(|g| g == "previous-release"))
            || t == "artifact"
    });
    if previous_needed {
        for (triple, p) in &m.previous_release {
            out.push(Wanted {
                id: format!("previous-release {triple}"),
                path: store.join("previous-release").join(triple).join(&p.asset),
                sha256: p.sha256.clone(),
                source: None,
                pending: p.pending.clone(),
            });
        }
    }
    out
}

fn fetch(root: &Path, tier: Option<&str>, store: &Path) -> Result<ExitCode> {
    let m = crate::manifest::load(root)?;
    let mut blocked = Vec::new();
    for w in wanted(&m, tier, store) {
        let Some(digest) = w
            .sha256
            .as_deref()
            .filter(|d| crate::evidence::digest::is_sha256(d))
        else {
            blocked.push(format!(
                "BLOCKED[fixture:{}]: no published SHA-256 ({})",
                w.id,
                w.pending.as_deref().unwrap_or("not pinned")
            ));
            continue;
        };
        if w.path.is_file() && crate::evidence::digest::sha256_file(&w.path)? == digest {
            eprintln!("fixtures: {} present and verified", w.id);
            continue;
        }
        let Some(url) = &w.source else {
            blocked.push(format!(
                "BLOCKED[fixture:{}]: pinned but has no download source; place it with `cargo xtask fixtures build`",
                w.id
            ));
            continue;
        };
        download_verified(url, digest, &w.path)?;
        eprintln!("fixtures: {} fetched and verified", w.id);
    }
    report(blocked)
}

/// Downloads to a temporary name, verifies, then renames into place, so an
/// incomplete or wrong file never sits at the fixture's path.
fn download_verified(url: &str, digest: &str, path: &Path) -> Result<()> {
    let dir = path.parent().context("fixture path without a parent")?;
    std::fs::create_dir_all(dir)?;
    let partial = path.with_extension("partial");
    let status = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--output",
        ])
        .arg(&partial)
        .arg(url)
        .status()
        .context("run curl")?;
    if !status.success() {
        bail!("download failed: {url}");
    }
    let actual = crate::evidence::digest::sha256_file(&partial)?;
    if actual != digest {
        std::fs::remove_file(&partial)?;
        bail!("{url}: SHA-256 {actual} does not match the pinned {digest}");
    }
    std::fs::rename(&partial, path)?;
    Ok(())
}

fn verify(root: &Path, store: &Path) -> Result<ExitCode> {
    let m = crate::manifest::load(root)?;
    let mut problems = Vec::new();
    for w in wanted(&m, None, store) {
        match w
            .sha256
            .as_deref()
            .filter(|d| crate::evidence::digest::is_sha256(d))
        {
            None => problems.push(format!("BLOCKED[fixture:{}]: no published SHA-256", w.id)),
            Some(digest) if !w.path.is_file() => problems.push(format!(
                "fixture {}: missing at {} (expected {digest})",
                w.id,
                w.path.display()
            )),
            Some(digest) => {
                let actual = crate::evidence::digest::sha256_file(&w.path)?;
                if actual != digest {
                    problems.push(format!(
                        "fixture {}: {} hashes to {actual}, pinned {digest}",
                        w.id,
                        w.path.display()
                    ));
                }
            }
        }
    }
    report(problems)
}

fn report(problems: Vec<String>) -> Result<ExitCode> {
    for p in &problems {
        eprintln!("{p}");
    }
    Ok(if problems.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// What the trusted Fixtures workflow must build and publish: every fixture
/// with a recipe (`build`) and every previous release still lacking a pin.
fn publish_plan(root: &Path) -> Result<ExitCode> {
    let m = crate::manifest::load(root)?;
    let mut items = Vec::new();
    for (id, f) in &m.fixtures {
        if let Some(recipe) = &f.build {
            items.push(serde_json::json!({
                "kind": "fixture", "id": id, "recipe": recipe,
                "file": f.file.clone().unwrap_or_else(|| id.clone()),
                "pinned": f.sha256.is_some(), "pending": f.pending,
            }));
        }
    }
    for (triple, p) in &m.previous_release {
        items.push(serde_json::json!({
            "kind": "previous-release", "id": triple, "recipe": "previous-published",
            "version": p.version, "asset": p.asset,
            "pinned": p.sha256.is_some(), "pending": p.pending,
        }));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({"schema": 1, "items": items}))?
    );
    Ok(ExitCode::SUCCESS)
}
