//! `cargo xtask release candidate`: candidate preflight, build-once, sign,
//! record and upload steps, the main-evidence reuse check and the candidate
//! gate (`.github/workflows/candidate.yml`).
//!
//! Owner: workflow step `candidate-promote-pipeline` (see the WI 0144
//! ownership map). The signature of `run` is fixed by `release/mod.rs`.
//!
//! Every step reads the store first and does only the missing work, so
//! `gh run rerun --failed` never repeats a build, a signature or an upload.

use super::state::args::{self, outputs, read_json, write_json, Flags, Stop};
use super::state::decide::{self, Assembly, CellRecord, MainGate, Resolution};
use super::state::hub::{self, Hub};
use super::state::inputs::{self, CandidateInputs, TargetInputs};
use super::state::ops;
use super::state::record::{self, ArtifactRecord, GateRecord, Identity, ReleaseRecord, Runs};
use super::state::{FailingGate, ReleaseState};
use crate::evidence::digest;
use crate::evidence::schema::{Evidence, GateEvidence, Outcome};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "\
usage: cargo xtask release candidate <step> [--repo owner/name] [--store DIR] ...

  preflight --version V --commit SHA [--dry-run] [--operator-record JSON]
            [--event E --ref R] --source DIR --out DIR
  build     --asset NAME --preflight FILE --source DIR --out DIR
  sign      --asset NAME --source DIR --out DIR          (macOS only)
  record    --asset NAME --preflight FILE --out DIR [--attestation URL]
  assemble  --preflight FILE --in DIR --out DIR
  fetch     --preflight FILE --asset NAME --out DIR
  reuse-main-evidence --preflight FILE --out DIR
  gate      --preflight FILE --evidence DIR [--main FILE] --main-checkout DIR --out DIR
  repro     --asset NAME --preflight FILE --source DIR --out DIR
  cleanup   --preflight FILE                              (rehearsal drafts only)
";

/// What preflight decided, handed to every later job as `preflight.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preflight {
    pub schema: u32,
    pub version: String,
    pub commit: String,
    pub namespace: String,
    pub dry_run: bool,
    /// `new`, `resume`, `published` or `journal`.
    pub decision: String,
    pub candidate: u32,
    pub release_id: Option<u64>,
    pub repository_id: String,
    pub request_id: String,
    pub input_digest: String,
    pub inputs: CandidateInputs,
    pub operator_record: serde_json::Value,
    pub run_id: u64,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl Preflight {
    pub fn identity(&self) -> Identity {
        Identity {
            repository_id: self.repository_id.clone(),
            version: self.version.clone(),
            namespace: self.namespace.clone(),
            commit: self.commit.clone(),
            input_digest: self.input_digest.clone(),
            manifest_version: self.inputs.manifest_version.clone(),
        }
    }
    pub fn target(&self, asset: &str) -> Result<&TargetInputs> {
        self.inputs
            .targets
            .iter()
            .find(|t| t.asset == asset)
            .with_context(|| format!("{asset} is not a release target of this candidate"))
    }
}

pub fn run(args: &[String]) -> Result<ExitCode> {
    let Some((step, rest)) = args.split_first() else {
        eprint!("{USAGE}");
        return Ok(ExitCode::from(2));
    };
    let f = Flags::parse(rest, &["dry-run"])?;
    let result = match step.as_str() {
        "preflight" => preflight(&f),
        "build" => build(&f),
        "sign" => sign(&f),
        "record" => record_step(&f),
        "assemble" => assemble(&f),
        "fetch" => fetch(&f),
        "reuse-main-evidence" => reuse_main_evidence(&f),
        "gate" => gate(&f),
        "repro" => repro(&f),
        "cleanup" => cleanup(&f),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            return Ok(ExitCode::SUCCESS);
        }
        other => {
            eprintln!("xtask release candidate: unknown step `{other}`\n\n{USAGE}");
            return Ok(ExitCode::from(2));
        }
    }?;
    Ok(match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(stop) => stop.report(),
    })
}

type Step = Result<Result<(), Stop>>;

fn open(f: &Flags) -> Result<Box<dyn Hub>> {
    hub::open(&hub::repo_from(f.get("repo"))?, f.get("store"))
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

/// `version = "…"` of the workspace package, as the release operator reads it.
pub fn cargo_version(src: &Path) -> Result<String> {
    let text = std::fs::read_to_string(src.join("Cargo.toml")).context("reading Cargo.toml")?;
    text.lines()
        .find(|l| l.starts_with("version"))
        .and_then(|l| l.split('"').nth(1))
        .map(str::to_owned)
        .context("Cargo.toml has no version")
}

/// The notes placeholder the release operator writes into a new notes file.
pub const NOTES_PLACEHOLDER: &str = "_Write release notes here._";

// ── preflight ────────────────────────────────────────────────────────────────

fn preflight(f: &Flags) -> Step {
    let version = f.req("version")?.to_owned();
    let commit = f.req("commit")?.to_owned();
    let dry_run = f.on("dry-run");
    let src = PathBuf::from(f.get("source").unwrap_or("."));
    let out = f.path("out")?;
    let me = args::run_env();
    let operator_record: serde_json::Value =
        match serde_json::from_str(f.get("operator-record").unwrap_or("{}")) {
            Ok(v @ serde_json::Value::Object(_)) => v,
            _ => {
                return Ok(Err(Stop::Failed(
                    "operator_record is not a JSON object".into(),
                )))
            }
        };
    if !record::valid_version(&version) {
        return Ok(Err(Stop::Failed(format!(
            "version must be vX.Y.Z, got `{version}`"
        ))));
    }
    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(Err(Stop::Failed(format!(
            "commit must be a full SHA, got `{commit}`"
        ))));
    }
    // A record-only operator journal is persisted by the workflow and stops
    // here, before any allocation, build, validation or publication.
    if operator_record.get("record_only").and_then(|v| v.as_bool()) == Some(true) {
        write_json(
            &out.join("journal.json"),
            &serde_json::json!({"version": version, "commit": commit, "run_id": me.run_id,
                                "recorded_at": now(), "operator_record": operator_record}),
        )?;
        outputs(&[("decision", "journal".into())])?;
        return Ok(Ok(()));
    }
    // Trust: a release run starts only from protected main, by dispatch (or
    // the nightly rehearsal's call from a schedule/dispatch on main).
    let event = f.get("event").unwrap_or("workflow_dispatch");
    let git_ref = f.get("ref").unwrap_or("refs/heads/main");
    let allowed: &[&str] = if dry_run {
        &["workflow_dispatch", "schedule"]
    } else {
        &["workflow_dispatch"]
    };
    if git_ref != "refs/heads/main" || !allowed.contains(&event) {
        return Ok(Err(Stop::Failed(format!(
            "release candidates run only from refs/heads/main by {allowed:?}; got {event} on \
             {git_ref}"
        ))));
    }
    let cargo = cargo_version(&src)?;
    if version.strip_prefix('v') != Some(cargo.as_str()) {
        return Ok(Err(Stop::Failed(format!(
            "Cargo.toml version is '{cargo}' but expected '{}'",
            &version[1..]
        ))));
    }
    let notes = src.join(format!("docs/releases/{version}.md"));
    match std::fs::read_to_string(&notes) {
        Err(_) => {
            return Ok(Err(Stop::Failed(format!(
                "docs/releases/{version}.md is missing; it is the GitHub Release body"
            ))))
        }
        Ok(t) if t.contains(NOTES_PLACEHOLDER) => {
            return Ok(Err(Stop::Failed(format!(
                "docs/releases/{version}.md still contains the placeholder"
            ))))
        }
        Ok(_) => {}
    }
    let m = match crate::manifest::load(&src) {
        Ok(m) => m,
        Err(e) => return Ok(Err(Stop::Failed(format!("ci/gates.toml: {e:#}")))),
    };
    let ins = match inputs::compute(&src, &m, &version, &commit) {
        Ok(i) => i,
        Err(e) => return Ok(Err(Stop::Failed(format!("{e:#}")))),
    };
    let mut pending = ins.pending_pins();
    pending.extend(crate::manifest::validate::pins(&m));
    if !pending.is_empty() {
        return Ok(Err(Stop::Blocked(format!(
            "pins are not published yet, so no candidate can be validated: {}",
            pending.join("; ")
        ))));
    }
    if let Err(e) = crate::manifest::validate::validate(&m, now().date_naive()) {
        return Ok(Err(Stop::Failed(format!("ci/gates.toml: {e:#}"))));
    }
    let hub = open(f)?;
    if !hub.is_ancestor_of_main(&commit)? {
        return Ok(Err(Stop::Failed(format!(
            "{commit} is neither the tip nor an ancestor of main"
        ))));
    }
    let namespace = if dry_run {
        format!("rehearsal/{}", me.run_id)
    } else {
        record::PRODUCTION.to_owned()
    };
    let repository_id = hub.repository_id()?;
    let input_digest = ins.digest()?;
    let identity = Identity {
        repository_id: repository_id.clone(),
        version: version.clone(),
        namespace: namespace.clone(),
        commit: commit.clone(),
        input_digest: input_digest.clone(),
        manifest_version: m.version().into(),
    };
    let resolution = if dry_run {
        Resolution::New { candidate: 1 }
    } else {
        let existing = super::state::existing(hub.as_ref(), &version)?;
        let max = super::state::recorded(hub.as_ref(), &version, &namespace)?
            .into_iter()
            .max()
            .unwrap_or(0);
        match decide::resolve(&identity, &existing, max, hub.tag(&version)?.as_deref()) {
            Ok(r) => r,
            Err(stop) => return Ok(Err(stop)),
        }
    };
    let (decision, candidate, release_id) = match &resolution {
        Resolution::New { candidate } => ("new", *candidate, None),
        Resolution::Resume {
            candidate,
            release_id,
            ..
        } => ("resume", *candidate, Some(*release_id)),
        Resolution::Published {
            release_id,
            candidate,
        } => ("published", candidate.unwrap_or(0), Some(*release_id)),
    };
    // Test jobs read bytes and records from the run that built them.
    let source_run = match release_id {
        Some(id) => super::state::existing(hub.as_ref(), &version)?
            .into_iter()
            .find(|e| e.release.id == id)
            .and_then(|e| e.record)
            .map(|r| r.runs.candidate)
            .unwrap_or(me.run_id),
        None => me.run_id,
    };
    let created = now();
    let pf = Preflight {
        schema: 1,
        version: version.clone(),
        commit: commit.clone(),
        namespace: namespace.clone(),
        dry_run,
        decision: decision.into(),
        candidate,
        release_id,
        repository_id,
        request_id: record::request_id(hub.repo(), &version, &commit),
        input_digest: input_digest.clone(),
        inputs: ins.clone(),
        operator_record,
        run_id: me.run_id,
        created_at: created,
        expires_at: created + chrono::Duration::days(m.release.evidence_valid_days),
    };
    write_json(&out.join("preflight.json"), &pf)?;
    let matrix: Vec<_> = ins
        .targets
        .iter()
        .map(|t| {
            serde_json::json!({"asset": t.asset, "target": t.triple, "build": t.build,
                "runs_on": t.runs_on, "sign": t.sign, "self_hosted": t.runs_on.iter().any(|l| l == "self-hosted")})
        })
        .collect();
    outputs(&[
        ("decision", decision.into()),
        ("candidate", candidate.to_string()),
        ("namespace", namespace),
        ("input_digest", input_digest),
        ("source_run", source_run.to_string()),
        (
            "record_artifact",
            record::record_artifact(&version, &pf.namespace, candidate),
        ),
        (
            "release_id",
            release_id.map(|i| i.to_string()).unwrap_or_default(),
        ),
        ("targets", serde_json::to_string(&matrix)?),
        (
            "release_labels",
            serde_json::to_string(&m.release.release_labels)?,
        ),
        ("environment", m.release.environment.clone()),
    ])?;
    println!(
        "preflight: {version} at {commit}: {decision} candidate {candidate} (input digest {})",
        pf.input_digest
    );
    Ok(Ok(()))
}

// ── build / sign / record ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct BuildRecord {
    asset: String,
    target: String,
    build: String,
    unsigned_sha256: String,
    bytes: u64,
    toolchain: String,
    input_digest: String,
    command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SignRecord {
    unsigned_sha256: String,
    sha256: String,
    entitlements_sha256: String,
    command: Vec<String>,
}

fn rustc_version(src: &Path) -> Result<String> {
    let out = Command::new("rustc")
        .arg("--version")
        .current_dir(src)
        .output()?;
    anyhow::ensure!(out.status.success(), "rustc --version failed");
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn exe_name(triple: &str) -> &'static str {
    if triple.contains("windows") {
        "awman.exe"
    } else {
        "awman"
    }
}

fn build(f: &Flags) -> Step {
    let pf: Preflight = read_json(&f.path("preflight")?)?;
    let asset = f.req("asset")?;
    let t = pf.target(asset)?.clone();
    let src = PathBuf::from(f.get("source").unwrap_or("."));
    // Absolute: cargo runs in the candidate checkout, not in this directory.
    let out = std::path::absolute(f.path("out")?)?;
    std::fs::create_dir_all(&out)?;
    let input_digest = pf.inputs.target_digest(asset)?;
    let record_path = out.join("build.json");
    let bin = out.join(asset);
    if let Ok(prev) = read_json::<BuildRecord>(&record_path) {
        if prev.input_digest == input_digest
            && bin.exists()
            && digest::sha256_file(&bin)? == prev.unsigned_sha256
        {
            println!("{asset}: already built for input {input_digest}; not rebuilding");
            return Ok(Ok(()));
        }
        return Ok(Err(Stop::Failed(format!(
            "{asset}: a different build already exists in {}; one candidate never builds a \
             target twice. Start a new candidate",
            out.display()
        ))));
    }
    let toolchain = match rustc_version(&src) {
        Ok(v) => v,
        Err(e) => {
            return Ok(Err(Stop::Blocked(format!(
                "no Rust toolchain on this runner: {e:#}"
            ))))
        }
    };
    if !toolchain.contains(&pf.inputs.toolchain) {
        return Ok(Err(Stop::Blocked(format!(
            "runner toolchain `{toolchain}` is not the pinned {}",
            pf.inputs.toolchain
        ))));
    }
    // Release builds are cache-free: a fresh target directory per job.
    let target_dir = out.join("cargo-target");
    if target_dir.exists() {
        std::fs::remove_dir_all(&target_dir)?;
    }
    println!("BUILD-ONCE {asset}: {}", t.cargo_command.join(" "));
    let status = Command::new(&t.cargo_command[0])
        .args(&t.cargo_command[1..])
        .current_dir(&src)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("CARGO_INCREMENTAL", "0")
        .status()
        .context("running cargo")?;
    if !status.success() {
        return Ok(Err(Stop::Failed(format!(
            "optimized build of {asset} failed ({status})"
        ))));
    }
    let built = target_dir
        .join(&t.triple)
        .join("release")
        .join(exe_name(&t.triple));
    // A copy, never a hardlink to a mutable build output.
    std::fs::copy(&built, &bin).with_context(|| format!("copying {}", built.display()))?;
    ops::read_only(&bin)?;
    let rec = BuildRecord {
        asset: asset.into(),
        target: t.triple.clone(),
        build: t.build.clone(),
        unsigned_sha256: digest::sha256_file(&bin)?,
        bytes: std::fs::metadata(&bin)?.len(),
        toolchain,
        input_digest,
        command: t.cargo_command.clone(),
    };
    write_json(&record_path, &rec)?;
    std::fs::remove_dir_all(&target_dir).ok();
    println!(
        "{asset}: unsigned sha256 {} ({} bytes)",
        rec.unsigned_sha256, rec.bytes
    );
    outputs(&[("unsigned_sha256", rec.unsigned_sha256.clone())])?;
    Ok(Ok(()))
}

fn sign(f: &Flags) -> Step {
    let asset = f.req("asset")?;
    let src = PathBuf::from(f.get("source").unwrap_or("."));
    let out = std::path::absolute(f.path("out")?)?;
    let b: BuildRecord = read_json(&out.join("build.json"))?;
    let bin = out.join(asset);
    let sign_path = out.join("sign.json");
    let current = digest::sha256_file(&bin)?;
    if let Ok(prev) = read_json::<SignRecord>(&sign_path) {
        if prev.sha256 == current && prev.unsigned_sha256 == b.unsigned_sha256 {
            println!("{asset}: already signed ({current}); not signing again");
            return Ok(Ok(()));
        }
        return Ok(Err(Stop::Failed(format!(
            "{asset}: signed bytes changed after signing"
        ))));
    }
    if current != b.unsigned_sha256 {
        return Ok(Err(Stop::Failed(format!(
            "{asset}: bytes changed between build and signing"
        ))));
    }
    if !cfg!(target_os = "macos") {
        return Ok(Err(Stop::Blocked(
            "signing needs the macOS release runner".into(),
        )));
    }
    let entitlements = src.join(inputs::ENTITLEMENTS);
    let entitlements_sha256 = digest::sha256_file(&entitlements)?;
    let work = out.join("signing");
    std::fs::create_dir_all(&work)?;
    let staged = work.join(asset);
    std::fs::copy(&bin, &staged)?;
    let mut perms = std::fs::metadata(&staged)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&staged, perms)?;
    println!(
        "SIGN-ONCE {asset}: bash {} {}",
        inputs::SIGN_SCRIPT,
        staged.display()
    );
    let status = Command::new("bash")
        .arg(src.join(inputs::SIGN_SCRIPT))
        .arg(&staged)
        .status()?;
    if !status.success() {
        return Ok(Err(Stop::Failed(format!(
            "signing {asset} failed ({status})"
        ))));
    }
    let mut perms = std::fs::metadata(&bin)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&bin, perms)?;
    std::fs::rename(&staged, &bin)?;
    ops::read_only(&bin)?;
    let rec = SignRecord {
        unsigned_sha256: b.unsigned_sha256,
        sha256: digest::sha256_file(&bin)?,
        entitlements_sha256,
        command: vec!["bash".into(), inputs::SIGN_SCRIPT.into(), asset.into()],
    };
    write_json(&sign_path, &rec)?;
    println!("{asset}: signed sha256 {}", rec.sha256);
    Ok(Ok(()))
}

fn record_step(f: &Flags) -> Step {
    let pf: Preflight = read_json(&f.path("preflight")?)?;
    let asset = f.req("asset")?;
    let t = pf.target(asset)?.clone();
    let out = f.path("out")?;
    let b: BuildRecord = read_json(&out.join("build.json"))?;
    let s: Option<SignRecord> = if t.sign {
        match read_json(&out.join("sign.json")) {
            Ok(s) => Some(s),
            Err(_) => {
                return Ok(Err(Stop::Failed(format!(
                    "{asset} must be signed before it is recorded"
                ))))
            }
        }
    } else {
        None
    };
    let sha256 = digest::sha256_file(&out.join(asset))?;
    let expected = s
        .as_ref()
        .map(|s| s.sha256.clone())
        .unwrap_or(b.unsigned_sha256.clone());
    if sha256 != expected {
        return Ok(Err(Stop::Failed(format!(
            "{asset}: bytes differ from the build/sign record"
        ))));
    }
    if b.input_digest != pf.inputs.target_digest(asset)? {
        return Ok(Err(Stop::Failed(format!(
            "{asset}: built from other inputs than this candidate"
        ))));
    }
    let rec = ArtifactRecord {
        name: asset.into(),
        target: t.triple,
        build: t.build,
        sha256,
        unsigned_sha256: b.unsigned_sha256,
        bytes: std::fs::metadata(out.join(asset))?.len(),
        toolchain: b.toolchain,
        input_digest: b.input_digest,
        attestation: f
            .get("attestation")
            .filter(|a| !a.is_empty())
            .map(str::to_owned),
        entitlements_sha256: s.as_ref().map(|s| s.entitlements_sha256.clone()),
        signing_command: s.map(|s| s.command),
        producer: args::run_env(),
        asset_id: None,
    };
    write_json(&out.join(format!("record-{asset}.json")), &rec)?;
    println!("{asset}: recorded {}", rec.sha256);
    Ok(Ok(()))
}

// ── assemble ─────────────────────────────────────────────────────────────────

/// Find `name` (an asset or its record) anywhere below the download dir.
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(found) = find_file(&p, name) {
                return Some(found);
            }
        } else if p.file_name().is_some_and(|n| n == name) {
            return Some(p);
        }
    }
    None
}

fn assemble(f: &Flags) -> Step {
    let pf: Preflight = read_json(&f.path("preflight")?)?;
    let input = f.path("in")?;
    let out = f.path("out")?;
    std::fs::create_dir_all(&out)?;
    if pf.decision != "new" {
        println!("assemble: decision {} needs no assembly", pf.decision);
        outputs(&[(
            "release_id",
            pf.release_id.map(|i| i.to_string()).unwrap_or_default(),
        )])?;
        return Ok(Ok(()));
    }
    let me = args::run_env();
    let mut artifacts = Vec::new();
    let mut files = Vec::new();
    for t in &pf.inputs.targets {
        let (Some(bin), Some(rec)) = (
            find_file(&input, &t.asset),
            find_file(&input, &format!("record-{}.json", t.asset)),
        ) else {
            return Ok(Err(Stop::Blocked(format!(
                "build output of {} is missing; rerun its failed build job",
                t.asset
            ))));
        };
        let a: ArtifactRecord = read_json(&rec)?;
        if a.name != t.asset
            || digest::sha256_file(&bin)? != a.sha256
            || std::fs::metadata(&bin)?.len() != a.bytes
            || a.input_digest != pf.inputs.target_digest(&t.asset)?
            || (t.sign && a.entitlements_sha256 != pf.inputs.entitlements_sha256)
        {
            return Ok(Err(Stop::Failed(format!(
                "{}: downloaded bytes or record do not match this candidate",
                t.asset
            ))));
        }
        artifacts.push(a);
        files.push(bin);
    }
    let hub = open(f)?;
    let identity = pf.identity();
    let tag = record::draft_tag(&pf.version, &pf.namespace);
    let existing = super::state::existing(hub.as_ref(), &tag)?;
    let plan = match decide::assembly(&identity, pf.candidate, me.run_id, &existing) {
        Ok(p) => p,
        Err(stop) => return Ok(Err(stop)),
    };
    let supersede = match &plan {
        Assembly::Create { supersede } | Assembly::Continue { supersede, .. } => supersede.clone(),
    };
    // Superseded drafts are archived into this run's artifacts before deletion.
    for id in supersede {
        if let Some(e) = existing.iter().find(|e| e.release.id == id) {
            let dir = out
                .join("superseded")
                .join(format!("c{}", e.candidate().unwrap_or(0)));
            for name in [
                record::STATE_ASSET,
                record::GATE_ASSET,
                record::MAIN_GATE_ASSET,
            ] {
                ops::download(hub.as_ref(), &e.release, name, &dir.join(name))?;
            }
            hub.delete_release(id)?;
            println!(
                "superseded candidate {} (draft {id})",
                e.candidate().unwrap_or(0)
            );
        }
    }
    let release = match plan {
        Assembly::Continue { release_id, .. } => existing
            .into_iter()
            .find(|e| e.release.id == release_id)
            .map(|e| e.release)
            .context("draft vanished")?,
        Assembly::Create { .. } => hub.create_draft(
            &tag,
            &record::draft_name(&pf.version, pf.candidate),
            &pf.commit,
            &decide::run_marker(me.run_id),
        )?,
    };
    let mut rec = ReleaseRecord {
        schema: 1,
        version: pf.version.clone(),
        candidate: pf.candidate,
        namespace: pf.namespace.clone(),
        repository_id: pf.repository_id.clone(),
        request_id: pf.request_id.clone(),
        commit: pf.commit.clone(),
        state: ReleaseState::Prepared,
        manifest_version: pf.inputs.manifest_version.clone(),
        input_digest: pf.input_digest.clone(),
        candidate_digest: None,
        dry_run: pf.dry_run,
        inputs: pf.inputs.clone(),
        artifacts,
        gates: Vec::new(),
        failing: Vec::new(),
        operator_record: pf.operator_record.clone(),
        runs: Runs {
            candidate: me.run_id,
            validation: None,
            promote: None,
        },
        release_id: Some(release.id),
        created_at: pf.created_at,
        expires_at: pf.expires_at,
    };
    rec.candidate_digest = Some(rec.compute_candidate_digest()?);
    // The journal is written first so an interrupted upload can be resumed.
    if let Err(stop) = ops::put_state(hub.as_ref(), release.id, &rec, &out)? {
        return Ok(Err(stop));
    }
    let release = refresh(hub.as_ref(), release.id)?;
    let missing = match decide::missing_uploads(&release, &rec) {
        Ok(m) => m,
        Err(stop) => return Ok(Err(stop)),
    };
    for (a, file) in rec.artifacts.iter().zip(&files) {
        if missing.contains(&a.name) {
            hub.upload_asset(release.id, &a.name, file)?;
            println!("uploaded {}", a.name);
        }
    }
    let sums = out.join(record::SUMS);
    std::fs::write(&sums, rec.sha256sums())?;
    let release = refresh(hub.as_ref(), release.id)?;
    match ops::download(
        hub.as_ref(),
        &release,
        record::SUMS,
        &out.join("SHA256SUMS.store"),
    )? {
        None => {
            hub.upload_asset(release.id, record::SUMS, &sums)?;
        }
        Some(_)
            if std::fs::read(out.join("SHA256SUMS.store"))? == rec.sha256sums().into_bytes() => {}
        Some(_) => {
            return Ok(Err(Stop::Failed(
                "SHA256SUMS on the draft differs from the records".into(),
            )))
        }
    }
    let release = refresh(hub.as_ref(), release.id)?;
    for a in rec.artifacts.iter_mut() {
        a.asset_id = release.asset(&a.name).map(|x| x.id);
    }
    rec.state = ReleaseState::CandidateBuilt;
    if let Err(stop) = ops::put_state(hub.as_ref(), release.id, &rec, &out)? {
        return Ok(Err(stop));
    }
    write_json(&out.join("candidate-record.json"), &rec)?;
    outputs(&[
        ("release_id", release.id.to_string()),
        (
            "record_artifact",
            record::record_artifact(&pf.version, &pf.namespace, pf.candidate),
        ),
    ])?;
    args::summary(&format!(
        "### Candidate {} of {}\n\nDraft {}, commit `{}`\n\n```\n{}```\n",
        pf.candidate,
        pf.version,
        release.url,
        pf.commit,
        rec.sha256sums()
    ));
    println!(
        "candidate {} of {} assembled: draft {}",
        pf.candidate, pf.version, release.id
    );
    Ok(Ok(()))
}

fn refresh(hub: &dyn Hub, id: u64) -> Result<hub::ReleaseFact> {
    hub.releases()?
        .into_iter()
        .find(|r| r.id == id)
        .context("the candidate's draft disappeared")
}

/// The record of this run's candidate, from the store.
fn load_candidate(
    hub: &dyn Hub,
    pf: &Preflight,
) -> Result<Result<(hub::ReleaseFact, ReleaseRecord), Stop>> {
    let e = match ops::find(hub, &pf.version, &pf.namespace, pf.candidate)? {
        Ok(e) => e,
        Err(stop) => return Ok(Err(stop)),
    };
    let Some(rec) = e.record else {
        return Ok(Err(Stop::Blocked(
            "the candidate's draft has no release-state.json yet; rerun assembly".into(),
        )));
    };
    if rec.identity() != pf.identity() {
        return Ok(Err(Stop::Failed(format!(
            "candidate {} on the store has another identity (commit/inputs/manifest); its \
             evidence cannot be reused",
            pf.candidate
        ))));
    }
    Ok(Ok((e.release, rec)))
}

// ── fetch (artifact/guest jobs) ──────────────────────────────────────────────

/// A record downloaded from the run's retained `candidate-record-…` artifact.
fn record_file(pf: &Preflight, path: &Path) -> Result<Result<ReleaseRecord, Stop>> {
    let rec: ReleaseRecord = read_json(path)?;
    if rec.identity() != pf.identity() || rec.candidate != pf.candidate {
        return Ok(Err(Stop::Failed(format!(
            "{} describes another candidate than this run's candidate {}",
            path.display(),
            pf.candidate
        ))));
    }
    Ok(Ok(rec))
}

/// Exact candidate bytes for a test job. With `--record FILE --in DIR` the
/// bytes come from the build job's workflow artifact and the record from the
/// retained record artifact (read-only token; test jobs never read drafts);
/// otherwise both come from the draft.
fn fetch(f: &Flags) -> Step {
    let pf: Preflight = read_json(&f.path("preflight")?)?;
    let asset = f.req("asset")?;
    let out = f.path("out")?;
    std::fs::create_dir_all(&out)?;
    let out = std::fs::canonicalize(out)?;
    let dest = out.join(asset);
    if dest.exists() {
        let mut perms = std::fs::metadata(&dest)?.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&dest, perms)?;
        std::fs::remove_file(&dest)?;
    }
    let rec = if let Some(file) = f.get("record") {
        let rec = match record_file(&pf, Path::new(file))? {
            Ok(r) => r,
            Err(stop) => return Ok(Err(stop)),
        };
        let Some(src) = find_file(&f.path("in")?, asset) else {
            return Ok(Err(Stop::Failed(format!(
                "{asset} is missing from the build artifacts; lost bytes are never rebuilt for \
                 the same candidate. Abandon it and start a new candidate"
            ))));
        };
        std::fs::copy(&src, &dest)?;
        rec
    } else {
        let hub = open(f)?;
        let (release, rec) = match load_candidate(hub.as_ref(), &pf)? {
            Ok(x) => x,
            Err(stop) => return Ok(Err(stop)),
        };
        if ops::download(hub.as_ref(), &release, asset, &dest)?.is_none() {
            return Ok(Err(Stop::Failed(format!(
                "{asset} is missing from the draft; lost bytes are never rebuilt for the same \
                 candidate. Abandon it and start a new candidate"
            ))));
        }
        rec
    };
    let Some(a) = rec.artifact(asset) else {
        return Ok(Err(Stop::Failed(format!(
            "{asset} is not recorded for this candidate"
        ))));
    };
    let sha = digest::sha256_file(&dest)?;
    if sha != a.sha256 {
        return Ok(Err(Stop::Failed(format!(
            "{asset} is {sha}, the evidence names {}; artifact integrity is never overridable",
            a.sha256
        ))));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o500))?;
    }
    ops::read_only(&dest)?;
    println!("{asset}: {sha} (exact candidate bytes)");
    outputs(&[("path", dest.display().to_string()), ("sha256", sha)])?;
    Ok(Ok(()))
}

// ── main gate evidence ───────────────────────────────────────────────────────

/// Main gate evidence for `commit` from an authenticated producer: a
/// successful protected-main `ci.yml` push run for the commit, or a
/// `ci.yml` subrun of this candidate run.
fn main_gate_from_store(
    hub: &dyn Hub,
    commit: &str,
    manifest_version: &str,
    this_run: u64,
    dir: &Path,
) -> Result<Option<(MainGate, u64)>> {
    // ci.yml names its artifact after the commit it ran for; a subrun of this
    // run is matched by run id and then by the commit inside the evidence.
    let name = format!("gate-evidence-main-{commit}");
    let ci_runs = hub.runs("ci.yml")?;
    let trusted = |run: u64| {
        run == this_run && run != 0
            || ci_runs.iter().any(|r| {
                r.id == run
                    && r.head_branch == "main"
                    && r.event == "push"
                    && r.conclusion.as_deref() == Some("success")
            })
    };
    let mut best = None;
    let candidates = hub
        .artifacts("gate-evidence-main-")?
        .into_iter()
        .filter(|a| a.name == name || (this_run != 0 && a.run_id == this_run));
    for a in candidates {
        if !trusted(a.run_id) || a.head_branch != "main" {
            continue;
        }
        let d = dir.join(a.id.to_string());
        hub.download_artifact(&a, &d)?;
        let Some(file) =
            find_file(&d, "gate-evidence.json").or_else(|| find_file(&d, record::MAIN_GATE_ASSET))
        else {
            continue;
        };
        let Ok(ev) = read_json::<GateEvidence>(&file) else {
            continue;
        };
        if ev.commit != commit || ev.manifest_version != manifest_version || ev.gate != "main" {
            continue;
        }
        let sha = digest::sha256_file(&file)?;
        std::fs::copy(&file, dir.join(record::MAIN_GATE_ASSET))?;
        best = Some((
            MainGate {
                evidence: ev,
                sha256: sha,
                authenticated: true,
            },
            a.run_id,
        ));
    }
    Ok(best)
}

fn reuse_main_evidence(f: &Flags) -> Step {
    let pf: Preflight = read_json(&f.path("preflight")?)?;
    let out = f.path("out")?;
    std::fs::create_dir_all(&out)?;
    let hub = open(f)?;
    let found = main_gate_from_store(
        hub.as_ref(),
        &pf.commit,
        &pf.inputs.manifest_version,
        0,
        &out,
    )?;
    let reusable = found.filter(|(g, _)| {
        g.evidence.verdict == Outcome::Passed && g.evidence.expires_at.is_some_and(|x| x > now())
    });
    match reusable {
        Some((_, run)) => {
            println!(
                "reusing main gate evidence of ci.yml run {run} for {}",
                pf.commit
            );
            outputs(&[("reused", "true".into()), ("run_id", run.to_string())])?;
        }
        None => {
            println!(
                "no reusable main gate evidence for {}; running the main gate",
                pf.commit
            );
            outputs(&[("reused", "false".into())])?;
        }
    }
    Ok(Ok(()))
}

// ── gate ─────────────────────────────────────────────────────────────────────

fn collect_cells(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_cells(&p, out);
            } else if p.file_name().is_some_and(|n| n == "evidence.json") {
                out.push(p);
            }
        }
    }
}

fn gate(f: &Flags) -> Step {
    let pf: Preflight = read_json(&f.path("preflight")?)?;
    let out = f.path("out")?;
    std::fs::create_dir_all(&out)?;
    let hub = open(f)?;
    let me = args::run_env();
    let (release, mut rec) = match load_candidate(hub.as_ref(), &pf)? {
        Ok(x) => x,
        Err(stop) => return Ok(Err(stop)),
    };
    if !matches!(
        rec.state,
        ReleaseState::CandidateBuilt | ReleaseState::Validated
    ) {
        return Ok(Err(Stop::Failed(format!(
            "candidate is {:?}; nothing to validate",
            rec.state
        ))));
    }
    let main_checkout = PathBuf::from(f.get("main-checkout").unwrap_or("."));
    let m = crate::manifest::load(&main_checkout)?;
    let repo_id = hub.repository_id()?;
    // Cell records: downloaded from this run's artifacts by the gate job.
    let mut files = Vec::new();
    if let Some(dir) = f.get("evidence") {
        collect_cells(Path::new(dir), &mut files);
    }
    let mut cells = Vec::new();
    for file in files {
        let Ok(e) = read_json::<Evidence>(&file) else {
            return Ok(Err(Stop::Failed(format!(
                "{} is not cell evidence",
                file.display()
            ))));
        };
        let authenticated = me.run_id != 0
            && e.run.provider == "github-actions"
            && e.run.run_id == me.run_id
            && e.run.repository_id == repo_id
            && hub.authenticate_cell(&e, &file)?;
        cells.push(CellRecord {
            sha256: digest::sha256_file(&file)?,
            evidence: e,
            authenticated,
        });
    }
    let main = match f.get("main") {
        Some(p) if Path::new(p).exists() => {
            // Re-authenticate through the store; a file alone is not proof.
            main_gate_from_store(
                hub.as_ref(),
                &pf.commit,
                &rec.manifest_version,
                me.run_id,
                &out,
            )?
            .map(|(g, _)| g)
        }
        _ => main_gate_from_store(
            hub.as_ref(),
            &pf.commit,
            &rec.manifest_version,
            me.run_id,
            &out,
        )?
        .map(|(g, _)| g),
    };
    let t = now();
    let g = decide::candidate_gate(&m, &rec, main.as_ref(), &cells, t)?;
    let gate_path = out.join(record::GATE_ASSET);
    write_json(&gate_path, &g)?;
    let gate_sha = digest::sha256_file(&gate_path)?;
    let cur = decide::Current {
        now: t,
        main_contains_commit: hub.is_ancestor_of_main(&rec.commit)?,
        policy: inputs::Policy::current(&main_checkout, &rec.version)?,
    };
    let mut verdict = g.verdict;
    let mut findings = decide::evidence_findings(&rec, &m, &cur, false);
    if !findings.is_empty() {
        verdict = Outcome::Failed;
    }
    let expires = g.expires_at.unwrap_or(rec.expires_at).min(rec.expires_at);
    rec.gates.retain(|x| x.id != "candidate" && x.id != "main");
    if let Some(mg) = &main {
        rec.gates.push(GateRecord {
            id: "main".into(),
            verdict: verdict_name(mg.evidence.verdict).into(),
            gate_evidence_sha256: mg.sha256.clone(),
            run_id: me.run_id,
            commit: mg.evidence.commit.clone(),
            manifest_version: mg.evidence.manifest_version.clone(),
            expires_at: mg.evidence.expires_at.unwrap_or(t),
            overrides_applied: mg.evidence.overrides_applied.clone(),
        });
        ops::replace_asset(
            hub.as_ref(),
            &refresh(hub.as_ref(), release.id)?,
            record::MAIN_GATE_ASSET,
            &out.join(record::MAIN_GATE_ASSET),
        )?;
    }
    rec.gates.push(GateRecord {
        id: "candidate".into(),
        verdict: verdict_name(verdict).into(),
        gate_evidence_sha256: gate_sha,
        run_id: me.run_id,
        commit: rec.commit.clone(),
        manifest_version: rec.manifest_version.clone(),
        expires_at: expires,
        overrides_applied: g.overrides_applied.clone(),
    });
    rec.failing = decide::failing_gates(&g);
    for s in &findings {
        rec.failing.push(FailingGate {
            gate_id: "candidate".into(),
            outcome: match s {
                Stop::Blocked(_) => "blocked",
                Stop::Failed(_) => "failed",
            }
            .into(),
            reason: s.to_string(),
        });
    }
    rec.runs.validation = Some(me.run_id);
    rec.expires_at = expires;
    rec.state = if verdict == Outcome::Passed {
        ReleaseState::Validated
    } else {
        ReleaseState::CandidateBuilt
    };
    ops::replace_asset(
        hub.as_ref(),
        &refresh(hub.as_ref(), release.id)?,
        record::GATE_ASSET,
        &gate_path,
    )?;
    if let Err(stop) = ops::put_state(hub.as_ref(), release.id, &rec, &out)? {
        return Ok(Err(stop));
    }
    if verdict == Outcome::Passed {
        let path = out.join(record::VALIDATION_ASSET);
        write_json(
            &path,
            &record::Validation {
                record: rec.clone(),
                gate_sha256: digest::sha256_file(&gate_path)?,
            },
        )?;
        ops::replace_asset(
            hub.as_ref(),
            &refresh(hub.as_ref(), release.id)?,
            record::VALIDATION_ASSET,
            &path,
        )?;
    }
    write_json(&out.join("candidate-record.json"), &rec)?;
    let mut md = format!(
        "### Candidate gate: {}\n\n| cell | outcome |\n|---|---|\n",
        verdict_name(verdict)
    );
    for c in &g.cells {
        md.push_str(&format!("| {} | {} |\n", c.cell, verdict_name(c.outcome)));
    }
    for e in &g.errors {
        md.push_str(&format!("\n- {e}"));
    }
    args::summary(&md);
    outputs(&[("verdict", verdict_name(verdict).into())])?;
    if verdict == Outcome::Passed {
        println!("candidate {} of {} validated", rec.candidate, rec.version);
        return Ok(Ok(()));
    }
    for e in &g.errors {
        eprintln!("  {e}");
    }
    if let Some(stop) = findings.drain(..).next() {
        return Ok(Err(stop));
    }
    let reason = format!(
        "candidate gate {} ({} errors, missing {:?}); rerun failed jobs against the same \
         artifacts once the cause is fixed",
        verdict_name(verdict),
        g.errors.len(),
        g.missing
    );
    Ok(Err(if verdict == Outcome::Blocked {
        Stop::Blocked(reason)
    } else {
        Stop::Failed(reason)
    }))
}

pub fn verdict_name(o: Outcome) -> &'static str {
    match o {
        Outcome::Passed => "passed",
        Outcome::Blocked => "blocked",
        Outcome::Skipped => "skipped",
        Outcome::Inconclusive => "inconclusive",
        Outcome::NotSelected => "not-selected",
        Outcome::Failed => "failed",
    }
}

// ── repro / cleanup ──────────────────────────────────────────────────────────

/// Clean-room unsigned rebuild of one target, compared with the recorded
/// unsigned digest. Reported, never blocking, and never the shipped bytes.
fn repro(f: &Flags) -> Step {
    let pf: Preflight = read_json(&f.path("preflight")?)?;
    let asset = f.req("asset")?;
    let t = pf.target(asset)?.clone();
    let src = PathBuf::from(f.get("source").unwrap_or("."));
    let out = std::path::absolute(f.path("out")?)?;
    let found = match f.get("record") {
        Some(file) => record_file(&pf, Path::new(file))?,
        None => load_candidate(open(f)?.as_ref(), &pf)?.map(|(_, r)| r),
    };
    let rec = match found {
        Ok(rec) => rec,
        Err(stop) => return Ok(Err(stop)),
    };
    let want = rec
        .artifact(asset)
        .map(|a| a.unsigned_sha256.clone())
        .unwrap_or_default();
    let target_dir = out.join("repro-target");
    let status = Command::new(&t.cargo_command[0])
        .args(&t.cargo_command[1..])
        .current_dir(&src)
        .env("CARGO_TARGET_DIR", &target_dir)
        .status()?;
    if !status.success() {
        println!("REPRO {asset}: audit build failed ({status}); reported only");
        return Ok(Ok(()));
    }
    let got = digest::sha256_file(
        &target_dir
            .join(&t.triple)
            .join("release")
            .join(exe_name(&t.triple)),
    )?;
    let same = got == want;
    let line = format!(
        "REPRO {asset}: {} (audit {got}, candidate unsigned {want})",
        if same { "reproduced" } else { "DIFFERS" }
    );
    println!("{line}");
    args::summary(&format!("\n- {line}\n"));
    write_json(
        &out.join(format!("repro-{asset}.json")),
        &serde_json::json!({"asset": asset, "audit": got, "candidate_unsigned": want, "reproduced": same}),
    )?;
    Ok(Ok(()))
}

/// Delete this rehearsal's own draft, and nothing else.
fn cleanup(f: &Flags) -> Step {
    let pf: Preflight = read_json(&f.path("preflight")?)?;
    let me = args::run_env();
    if !pf.dry_run || pf.namespace != format!("rehearsal/{}", me.run_id) || me.run_id == 0 {
        bail!("cleanup deletes only this run's own rehearsal draft");
    }
    let hub = open(f)?;
    let tag = record::draft_tag(&pf.version, &pf.namespace);
    for e in super::state::existing(hub.as_ref(), &tag)? {
        let ours = e.release.draft
            && e.release.tag == tag
            && e.record
                .as_ref()
                .is_none_or(|r| r.dry_run && r.runs.candidate == me.run_id);
        if ours {
            hub.delete_release(e.release.id)?;
            println!("rehearsal draft {} deleted", e.release.id);
        }
    }
    Ok(Ok(()))
}
