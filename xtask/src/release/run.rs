//! The phase driver (design 3.1, 3.5, 7.4, 7.5). One entry point starts or
//! resumes a release:
//!
//! 1. resolve the repository and this clone's owner ID;
//! 2. recover identity from the remote claim and the private journal
//!    (HEAD never overrides a pinned R);
//! 3. admit a new attempt, or check only the prerequisites of unfinished
//!    phases on resume;
//! 4. run the first incomplete phase onwards, re-probing completed phases
//!    whose external state could have changed.
//!
//! The same code runs against GitHub and the fault-injection world.

use super::error::{Kind, Step, Stop};
use super::evidence::{ClaimDoc, Owner};
use super::forge::{RefWrite, RepoInfo};
use super::journal::{
    operation_key, Journal, PhaseRecord, StateRoot, Status, Store, PHASES, SCHEMA,
};
use super::plan::Plan;
use super::preflight;
use super::util::{random_id, sha256_file, valid_ssh_destination, valid_version};
use super::world::World;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub version: Option<String>,
    pub linux_host: Option<String>,
    pub rehearse: bool,
    pub supersede: bool,
    pub takeover: Option<String>,
    /// The only repository a production release may run against; `None`
    /// accepts the forge's repository (fault world).
    pub canonical_repo: Option<String>,
    /// `release-doctor`: admission and prerequisites only, no state.
    pub preflight_only: bool,
    /// When release preparation began (`DECIDED_AT`), for the hour target.
    pub decided_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `REHEARSAL_REPO`: complete publication rehearsal in a disposable,
    /// allowlisted repository under the `rehearsal-*` tag namespace.
    pub rehearsal_repo: Option<String>,
}

pub struct Ctx<'a> {
    pub w: &'a World,
    pub store: Store,
    pub j: Journal,
    pub plan: Plan,
    pub repo: RepoInfo,
    pub linux_host: Option<String>,
    pub sbx_explicit_host: Option<String>,
    pub previous_owners: Vec<Owner>,
    /// Native errors held back so hosted work keeps going (design 3.4).
    pub deferred: Vec<Stop>,
    /// Items automatically retried during this invocation (once each).
    pub retried: BTreeSet<String>,
    pub controller_sha256: String,
    /// Native hosts created for this invocation (they own started children).
    pub hosts: BTreeMap<String, Box<dyn super::native::NativeHost + 'a>>,
    /// Monotonic start of this invocation (bootstrap included).
    pub invoked: std::time::Instant,
    pub(crate) sbx_workers:
        BTreeMap<String, std::thread::JoinHandle<Step<super::runtime_smoke::sbx::SbxSmokeRecord>>>,
}

impl Ctx<'_> {
    pub fn save(&self) -> Step {
        self.store.save(&self.j)
    }

    pub fn plan_path(&self) -> PathBuf {
        self.store.path("plan.json")
    }

    /// A read-only detached checkout of R inside the private state.
    pub fn source_dir(&self) -> Step<PathBuf> {
        self.w
            .source
            .checkout(&self.j.commit, &self.store.path("src"))
    }

    pub fn claim_key(&self) -> String {
        claim_key(&self.j.version, self.j.mode(), &self.j.owner)
    }

    /// Fast-forward the remote claim to the journal's durable records.
    pub fn update_claim(&mut self, seal: Option<&[u8]>) -> Step {
        let doc = ClaimDoc::from_journal(&self.j, self.previous_owners.clone());
        let mut files = BTreeMap::from([
            (
                "claim.json".to_string(),
                serde_json::to_vec_pretty(&doc).map_err(anyhow::Error::from)?,
            ),
            (
                "plan.json".to_string(),
                std::fs::read(self.plan_path()).map_err(anyhow::Error::from)?,
            ),
        ]);
        if let Some(seal) = seal {
            files.insert("seal.json".into(), seal.to_vec());
        } else if self.j.release.seal_sha256.is_some() {
            files.insert(
                "seal.json".into(),
                std::fs::read(self.store.path("evidence/seal.json"))
                    .map_err(anyhow::Error::from)?,
            );
        }
        let key = self.claim_key();
        let parent = self.j.claim_oid.clone();
        match self.w.forge.write_claim(
            &key,
            parent.as_deref(),
            &files,
            &format!("awman release {} claim", self.j.version),
        )? {
            RefWrite::Done(oid) => {
                self.j.claim_oid = Some(oid);
                self.save()
            }
            RefWrite::Rejected(current) => Err(Stop::new(
                "state-foreign",
                format!(
                    "the claim for {} moved to {} while this attempt held it",
                    self.j.version,
                    current.as_deref().unwrap_or("nothing")
                ),
            )),
        }
    }

    pub fn timing(&self, phase: &str, event: &str, detail: &str) {
        self.store.timing(self.w.clock.now(), phase, event, detail);
    }
}

pub fn claim_key(version: &str, mode: &str, owner: &str) -> String {
    match mode {
        "rehearsal" => format!("rehearsal-{version}-{}", &owner[..8]),
        "disposable" => format!("disposable-{version}"),
        _ => version.to_string(),
    }
}

pub fn state_dir_name(version: &str, mode: &str) -> String {
    match mode {
        "rehearsal" => format!("rehearsal-{version}"),
        "disposable" => format!("disposable-{version}"),
        _ => version.to_string(),
    }
}

impl Options {
    pub fn mode(&self) -> &'static str {
        if self.rehearse {
            "rehearsal"
        } else if self.rehearsal_repo.is_some() {
            "disposable"
        } else {
            "production"
        }
    }
}

/// `REHEARSAL_REPO` boundary (design 7.13): only an explicitly allowlisted
/// disposable repository, never the canonical one, and the clone must point
/// at it. The allowlist is this clone's local git config
/// `awman.release.rehearsalRepos` (comma-separated), set by the maintainer
/// as the explicit authorization of remote writes there.
pub fn check_disposable(disposable: &str, actual: &str, allowlist: Option<&str>) -> Step {
    if disposable.eq_ignore_ascii_case(super::CANONICAL_REPO) {
        return Err(Stop::new(
            "rehearsal-refused",
            format!("{disposable} is the production repository; REHEARSAL_REPO must be a disposable repository"),
        ));
    }
    if !actual.eq_ignore_ascii_case(disposable) {
        return Err(Stop::new(
            "wrong-repo",
            format!("origin is {actual}, but REHEARSAL_REPO is {disposable}"),
        ));
    }
    let listed = allowlist
        .unwrap_or_default()
        .split(',')
        .any(|r| r.trim().eq_ignore_ascii_case(disposable));
    if !listed {
        return Err(Stop::new(
            "rehearsal-refused",
            format!("{disposable} is not in git config awman.release.rehearsalRepos"),
        )
        .with_remedy(format!(
            "authorize it explicitly: git config --local awman.release.rehearsalRepos {disposable}"
        )));
    }
    Ok(())
}

fn parse_claim(c: &super::forge::Claim) -> Step<ClaimDoc> {
    let bytes = c.files.get("claim.json").ok_or_else(|| {
        Stop::new(
            "state-foreign",
            format!("claim {} has no claim.json", c.oid),
        )
    })?;
    serde_json::from_slice(bytes).map_err(|e| {
        Stop::new(
            "state-foreign",
            format!("claim {} is not a release claim: {e}", c.oid),
        )
    })
}

fn check_linux_destination(frozen: Option<&str>, current: Option<&str>) -> Step {
    if frozen == current {
        Ok(())
    } else {
        Err(Stop::new(
            "linux-host-changed",
            "the KVM/Docker SSH destination differs from the frozen attempt",
        ))
    }
}

/// A journal rebuilt from the remote claim after the private state was lost:
/// identity, plan and run IDs survive; local bytes are re-downloaded by
/// pinned artifact ID, and a sealed attempt keeps its seal.
fn rebuild(
    w: &World,
    claim: &super::forge::Claim,
    doc: &ClaimDoc,
    store: &Store,
    repo: &RepoInfo,
    now: chrono::DateTime<chrono::Utc>,
) -> Step<Journal> {
    let plan = claim
        .files
        .get("plan.json")
        .ok_or_else(|| Stop::new("state-foreign", "claim has no plan.json"))?;
    super::util::write_atomic(&store.path("plan.json"), plan)?;
    let (p, _) = Plan::parse(plan)?;
    let mut j = Journal {
        schema: SCHEMA,
        version: doc.version.clone(),
        repo: repo.full_name.clone(),
        repository_id: doc.repository_id,
        rehearsal: doc.mode == "rehearsal",
        disposable_repo: (doc.mode == "disposable").then(|| repo.full_name.clone()),
        nonce: doc.nonce.clone(),
        commit: doc.commit.clone(),
        tree: doc.tree.clone(),
        base_main: doc.base_main.clone(),
        push_ref: doc.push_ref.clone(),
        generation: doc.generation,
        operation: doc.operation.clone(),
        owner: doc.owner.id.clone(),
        plan_sha256: doc.plan_sha256.clone(),
        policy_sha256: doc.policy_sha256.clone(),
        manifest_sha256: p.manifest_sha256.clone(),
        // The original admission time keeps the timing identity (design
        // 7.12); a claim from before it was recorded falls back to now.
        started: doc.started.unwrap_or(now),
        decided_at: doc.decided_at,
        invocations: Vec::new(),
        claim_oid: Some(claim.oid.clone()),
        phases: BTreeMap::new(),
        ci: Default::default(),
        builds: BTreeMap::new(),
        native: BTreeMap::new(),
        sbx_selection: None,
        linux_host: doc.linux_host.clone(),
        sbx_smoke: BTreeMap::new(),
        release: Default::default(),
    };
    j.sbx_selection = doc.sbx_selection.clone();
    let done = |j: &mut Journal, phase: &str| {
        j.phases.insert(
            phase.into(),
            PhaseRecord {
                status: Status::Done,
                ..Default::default()
            },
        );
    };
    done(&mut j, "pin");
    if let Some(run) = doc.ci_run {
        j.ci.dispatch = super::journal::Dispatch {
            key: j.key("ci"),
            acked: true,
            run_id: Some(run),
            ..Default::default()
        };
    }
    for (asset, run) in &doc.build_runs {
        let b = j.builds.entry(asset.clone()).or_default();
        b.dispatch = super::journal::Dispatch {
            key: format!("build-{}-{asset}", &doc.operation[..12]),
            acked: true,
            run_id: Some(*run),
            ..Default::default()
        };
    }
    if let (Some(seal_sha), Some(seal)) = (&doc.seal_sha256, claim.files.get("seal.json")) {
        if super::util::sha256(seal) != *seal_sha {
            return Err(Stop::new(
                "seal-mismatch",
                "the claim's seal.json does not match its recorded digest",
            ));
        }
        super::util::write_atomic(&store.path("evidence/seal.json"), seal)?;
        j.release.seal_sha256 = Some(seal_sha.clone());
        j.release.draft_id = doc.draft_id;
        restore_sealed_evidence(w, store, &p, seal, &mut j)?;
        for p in ["push", "dispatch", "collect", "native", "seal"] {
            done(&mut j, p);
        }
    }
    store.log(
        "preflight",
        &format!("journal rebuilt from claim {}", claim.oid),
    );
    Ok(j)
}

/// A sealed attempt whose journal was lost recovers its validation records
/// from the sealed `release-evidence.json` on the draft (authenticated by
/// the seal's digest), never from the reconstruction time: evidence ages,
/// CI/build/native records and the native map are what the seal froze. A
/// record without a completion time keeps no age, and `expiry` then blocks.
fn restore_sealed_evidence(
    w: &World,
    store: &Store,
    plan: &Plan,
    seal_bytes: &[u8],
    j: &mut Journal,
) -> Step {
    let seal: super::evidence::Seal = serde_json::from_slice(seal_bytes)
        .map_err(|e| Stop::new("seal-mismatch", format!("seal.json: {e}")))?;
    let sealed = seal
        .assets
        .iter()
        .find(|a| a.name == plan.evidence)
        .ok_or_else(|| Stop::new("seal-mismatch", format!("the seal lacks {}", plan.evidence)))?;
    let dest = store.path(&format!("evidence/{}", plan.evidence));
    w.forge.download_asset(sealed.asset_id, &dest)?;
    if sha256_file(&dest).ok().as_deref() != Some(sealed.sha256.as_str()) {
        return Err(Stop::new(
            "seal-mismatch",
            format!(
                "draft asset {} differs from the sealed {}",
                sealed.asset_id, plan.evidence
            ),
        ));
    }
    let ev: serde_json::Value = super::util::read_json(&dest)
        .map_err(|e| Stop::new("seal-mismatch", format!("{}: {e:#}", plan.evidence)))?;
    if ev["commit"] != j.commit.as_str() || ev["plan_sha256"] != j.plan_sha256.as_str() {
        return Err(Stop::new(
            "seal-mismatch",
            format!("sealed {} names another commit or plan", plan.evidence),
        ));
    }
    let time = |v: &serde_json::Value| serde_json::from_value(v.clone()).ok();
    j.ci.run_attempt = ev["ci"]["run_attempt"].as_u64().unwrap_or(0) as u32;
    j.ci.reused = ev["ci"]["reused"] == true;
    j.ci.verified_sha256 = ev["ci"]["verified_sha256"].as_str().map(str::to_string);
    j.ci.completed = time(&ev["ci"]["completed"]);
    for a in ev["assets"].as_array().cloned().unwrap_or_default() {
        let name = a["name"].as_str().unwrap_or_default().to_string();
        let b = j.builds.entry(name).or_default();
        b.sha256 = a["sha256"].as_str().map(str::to_string);
        b.size = a["size"].as_u64().unwrap_or(0);
        b.run_attempt = a["build"]["run_attempt"].as_u64().unwrap_or(0) as u32;
        b.artifact_id = a["build"]["artifact_id"].as_u64();
        b.build_json_sha256 = a["build"]["build_json_sha256"].as_str().map(str::to_string);
        b.attestation = serde_json::from_value(a["attestation"].clone()).ok();
        b.kit = serde_json::from_value(a["kit"].clone()).ok();
        b.additional_kits =
            serde_json::from_value(a["additional_kits"].clone()).unwrap_or_default();
        b.validated = time(&a["validated"]);
    }
    j.native = serde_json::from_value(ev["native"].clone()).unwrap_or_default();
    let selection =
        serde_json::from_value(ev["sbx_selection"].clone()).map_err(anyhow::Error::from)?;
    if j.sbx_selection != selection || j.linux_host.as_deref() != ev["linux_host"].as_str() {
        return Err(Stop::new(
            "seal-mismatch",
            "sealed SBX selection differs from claim",
        ));
    }
    j.sbx_smoke = serde_json::from_value(ev["sbx_smoke"].clone()).map_err(anyhow::Error::from)?;
    Ok(())
}

/// Start or resume. `Ok(())` means verified publication (or, for a
/// rehearsal, a complete prepublication rehearsal), or for `preflight_only`
/// a prepared environment.
pub fn run(w: &World, root: &StateRoot, opts: &Options) -> Step {
    let invoked = std::time::Instant::now();
    let repo = w.forge.repo()?;
    if let Some(canonical) = &opts.canonical_repo {
        if &repo.full_name != canonical {
            return Err(Stop::new(
                "wrong-repo",
                format!(
                    "origin is {}, releases come from {canonical}",
                    repo.full_name
                ),
            ));
        }
    }
    let origin = w.source.origin_repo()?;
    if origin != repo.full_name {
        return Err(Stop::new(
            "wrong-repo",
            format!("origin {origin} differs from {}", repo.full_name),
        ));
    }
    if let Some(disposable) = &opts.rehearsal_repo {
        if opts.rehearse {
            return Err(Stop::new(
                "usage",
                "REHEARSE=1 and REHEARSAL_REPO are separate modes; choose one",
            ));
        }
        check_disposable(
            disposable,
            &repo.full_name,
            w.source
                .config_get("awman.release.rehearsalRepos")
                .as_deref(),
        )?;
    }
    if !repo.can_push {
        return Err(Stop::new(
            "gh-auth",
            format!("this gh login cannot push to {}", repo.full_name),
        ));
    }
    let owner = root.owner_id()?;
    let head = w.source.head()?;
    let version = match &opts.version {
        Some(v) => v.clone(),
        None => preflight::version_at(w, &head.commit)?,
    };
    if !valid_version(&version) {
        return Err(Stop::new(
            "usage",
            format!("`{version}` is not a vX.Y.Z version"),
        ));
    }
    let linux_host = opts
        .linux_host
        .clone()
        .or_else(|| w.source.config_get("awman.release.linuxHost"));
    if let Some(h) = &linux_host {
        if !valid_ssh_destination(h) {
            return Err(Stop::new(
                "usage",
                format!("LINUX_HOST `{h}` is not a plain user@host destination"),
            ));
        }
    }
    if opts.preflight_only {
        return doctor(
            w,
            &repo,
            &version,
            opts,
            linux_host.as_deref(),
            root,
            &owner,
        );
    }

    let dir = root.version_dir(&state_dir_name(&version, opts.mode()));
    let store = Store::open(&dir)?;
    let mut journal = store.load()?;
    let key = claim_key(&version, opts.mode(), &owner);
    let mut claim = w.forge.read_claim(&key)?;
    let mut previous_owners = Vec::new();
    let mut took_over = false;

    // ── identity recovery ────────────────────────────────────────────────
    if let Some(c) = &claim {
        let mut doc = parse_claim(c)?;
        if doc.owner.id != owner {
            if opts.takeover.as_deref() == Some(doc.owner.id.as_str()) {
                doc.previous_owners.push(doc.owner.clone());
                took_over = true;
                store.log(
                    "preflight",
                    &format!("explicit takeover from {}", doc.owner.id),
                );
            } else {
                return Err(Stop::new(
                    "attempt-locked",
                    format!(
                        "{version} is owned by {} on {} (pid {})",
                        doc.owner.id, doc.owner.host, doc.owner.pid
                    ),
                )
                .with_remedy(format!(
                    "resume from that clone; or stop it and run make release TAKEOVER={}",
                    doc.owner.id
                )));
            }
        }
        previous_owners = doc.previous_owners.clone();
        match &journal {
            Some(j)
                if j.commit != doc.commit
                    || j.generation != doc.generation
                    || j.sbx_selection != doc.sbx_selection
                    || j.linux_host != doc.linux_host =>
            {
                return Err(Stop::new(
                    "state-foreign",
                    format!(
                        "the claim pins {} generation {}, local state pins {} generation {}",
                        &doc.commit[..12],
                        doc.generation,
                        &j.commit[..12],
                        j.generation
                    ),
                ));
            }
            Some(_) => {}
            None => {
                let mut j = rebuild(w, c, &doc, &store, &repo, w.clock.now())?;
                j.owner = owner.clone();
                store.save(&j)?;
                journal = Some(j);
            }
        }
        if let Some(j) = journal.as_mut() {
            j.owner = owner.clone();
            j.claim_oid = Some(c.oid.clone());
        }
    } else if let Some(j) = &journal {
        if j.claim_oid.is_some() {
            return Err(Stop::new(
                "state-foreign",
                format!("the remote claim for {version} disappeared; another operator may have abandoned it"),
            )
            .with_remedy(format!("inspect the release, then make release-abandon VERSION={version} to start over")));
        }
    }

    // ── explicit supersede of an unsealed attempt ───────────────────────
    if opts.supersede {
        if let Some(j) = journal.take() {
            supersede(w, &store, root, &j)?;
            // The new attempt keeps the claim chain (fast-forward).
            claim = w.forge.read_claim(&key)?;
            let generation = j.generation + 1;
            return start_new(
                w,
                opts,
                store,
                &repo,
                &owner,
                &version,
                linux_host,
                previous_owners,
                claim.map(|c| c.oid),
                generation,
                invoked,
            );
        }
    }

    match journal {
        None => start_new(
            w,
            opts,
            store,
            &repo,
            &owner,
            &version,
            linux_host,
            previous_owners,
            None,
            1,
            invoked,
        ),
        Some(j) => {
            let bytes = std::fs::read(store.path("plan.json"))
                .map_err(|e| Stop::new("state-corrupt", format!("plan.json: {e}")))?;
            let (plan, digest) = Plan::parse(&bytes)?;
            if digest != j.plan_sha256 {
                return Err(Stop::new(
                    "state-corrupt",
                    "plan.json differs from the pinned plan digest",
                ));
            }
            if linux_host.is_none()
                && plan
                    .native
                    .iter()
                    .any(|n| n.host == "ssh" && !j.native.contains_key(&n.id))
            {
                return Err(Stop::new(
                    "linux-host-missing",
                    "the Linux native check is unfinished and no LINUX_HOST is set",
                ));
            }
            let frozen_linux_host = j.linux_host.clone();
            let mut ctx = Ctx {
                w,
                store,
                j,
                plan,
                repo,
                linux_host: frozen_linux_host,
                sbx_explicit_host: opts.linux_host.clone(),
                previous_owners,
                deferred: Vec::new(),
                retried: BTreeSet::new(),
                controller_sha256: controller_sha256(),
                hosts: BTreeMap::new(),
                invoked,
                sbx_workers: BTreeMap::new(),
            };
            super::runtime_smoke::selection_check(&ctx, opts.linux_host.as_deref())?;
            check_linux_destination(ctx.j.linux_host.as_deref(), linux_host.as_deref())?;
            if took_over {
                // Record the new owner (and keep the old one) before any mutation.
                ctx.update_claim(None)?;
            }
            resume_checks(&mut ctx)?;
            drive(&mut ctx)
        }
    }
}

fn controller_sha256() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| sha256_file(&p).ok())
        .unwrap_or_default()
}

fn remaining(j: &Journal, plan: &Plan) -> (BTreeSet<&'static str>, Vec<String>) {
    let phases = PHASES.iter().copied().filter(|p| !j.done(p)).collect();
    let natives = plan
        .native
        .iter()
        .filter(|n| j.native.get(&n.id).is_none_or(|r| r.outcome != "passed"))
        .map(|n| n.id.clone())
        .collect();
    (phases, natives)
}

fn resume_checks(ctx: &mut Ctx) -> Step {
    ctx.timing("preflight", "start", "resume");
    super::runtime_smoke::resume(ctx)?;
    // Reuse a passed SBX check only for the same currently qualified CLI and identity.
    // Sealed evidence is immutable and publication retries need no native host.
    let sbx_id = crate::manifest::release_policy::SBX_NATIVE;
    if ctx.j.release.seal_sha256.is_none()
        && ctx
            .j
            .native
            .get(sbx_id)
            .is_some_and(|r| r.outcome == "passed")
    {
        let current = ctx.w.hosts.sbx_qualification()?;
        let old = ctx.j.native[sbx_id].sbx.clone().unwrap_or_default();
        if serde_json::to_value(&current).map_err(anyhow::Error::from)? != {
            let mut old = old;
            if let Some(o) = old.as_object_mut() {
                o.remove("cases");
                o.remove("ownership_sha256");
            }
            old
        } {
            // Verify old raw proof before invalidating it; tampering is a failure.
            super::phases::hosted::verify_sbx_retained(ctx)?;
            ctx.j.native.remove(sbx_id);
            let out = ctx.store.path(&format!("native/{sbx_id}"));
            let retained = ctx
                .store
                .path(&format!("native/{sbx_id}-previous-{}", random_id()?));
            std::fs::rename(out, retained).map_err(anyhow::Error::from)?;
            for phase in ["native", "seal", "confirm"] {
                ctx.j.phases.remove(phase);
            }
            ctx.save()?;
        }
    }
    let (phases, natives) = remaining(&ctx.j, &ctx.plan);
    let natives = if phases.contains("native") {
        natives
    } else {
        vec![]
    };
    let src = if phases.contains("dispatch") {
        ctx.source_dir()?
    } else {
        ctx.store.path("src")
    };
    let r = preflight::check(
        ctx.w,
        &ctx.repo,
        &ctx.plan,
        &src,
        &ctx.j.tag_name(),
        ctx.j.rehearsal,
        &preflight::Remaining {
            phases,
            natives: natives.iter().map(String::as_str).collect(),
        },
        ctx.linux_host.as_deref(),
        &ctx.store.path("fixtures"),
    );
    ctx.timing(
        "preflight",
        if r.is_ok() { "finish" } else { "stop" },
        "resume",
    );
    r
}

#[allow(clippy::too_many_arguments)]
fn start_new(
    w: &World,
    opts: &Options,
    store: Store,
    repo: &RepoInfo,
    owner: &str,
    version: &str,
    linux_host: Option<String>,
    previous_owners: Vec<Owner>,
    claim_parent: Option<String>,
    generation: u32,
    invoked: std::time::Instant,
) -> Step {
    let now = w.clock.now();
    store.timing(now, "preflight", "start", "new attempt");
    // The nonce exists before admission so a disposable rehearsal's
    // `rehearsal-*` tag name is what admission and rule checks examine.
    let nonce = random_id()?[..8].to_string();
    let tag_name = if opts.rehearsal_repo.is_some() {
        format!("rehearsal-{version}-{nonce}")
    } else {
        version.to_string()
    };
    let (head, main) = preflight::admit(w, repo, version, &tag_name, opts.rehearse)?;
    let src = w.source.checkout(&head.commit, &store.path("src"))?;
    let bytes = w.tools.release_plan(&src)?;
    let (plan, plan_sha256) = Plan::parse(&bytes)?;
    let all: BTreeSet<&'static str> = PHASES.iter().copied().collect();
    let natives: Vec<&str> = plan.native.iter().map(|n| n.id.as_str()).collect();
    if linux_host.is_none() && plan.native.iter().any(|n| n.host == "ssh") {
        return Err(Stop::new(
            "linux-host-missing",
            "the Linux native check needs a KVM host and no LINUX_HOST is set",
        ));
    }
    preflight::check(
        w,
        repo,
        &plan,
        &src,
        &tag_name,
        opts.rehearse,
        &preflight::Remaining {
            phases: all,
            natives,
        },
        linux_host.as_deref(),
        &store.path("fixtures"),
    )?;
    super::runtime_smoke::preflight(
        w,
        &plan,
        &super::runtime_smoke::selection(opts.linux_host.as_deref())?,
        &BTreeMap::new(),
    )?;
    store.timing(w.clock.now(), "preflight", "finish", "new attempt");
    if let Some(h) = &linux_host {
        w.source.config_set("awman.release.linuxHost", h);
    }
    super::util::write_atomic(&store.path("plan.json"), &bytes)?;
    // ── pin ─────────────────────────────────────────────────────────────
    let tree = w.source.tree(&head.commit)?;
    let operation = operation_key(
        repo.id,
        opts.rehearse,
        version,
        &head.commit,
        &plan_sha256,
        generation,
    );
    let push_ref = if opts.rehearse {
        format!("heads/rehearsal/{nonce}")
    } else {
        format!("heads/release/{version}")
    };
    let mut j = Journal {
        schema: SCHEMA,
        version: version.into(),
        repo: repo.full_name.clone(),
        repository_id: repo.id,
        rehearsal: opts.rehearse,
        disposable_repo: opts.rehearsal_repo.clone(),
        nonce,
        commit: head.commit.clone(),
        tree,
        base_main: main,
        push_ref,
        generation,
        operation,
        owner: owner.into(),
        plan_sha256,
        policy_sha256: plan.policy_sha256.clone(),
        manifest_sha256: plan.manifest_sha256.clone(),
        started: now,
        decided_at: opts.decided_at,
        invocations: Vec::new(),
        claim_oid: claim_parent,
        phases: BTreeMap::new(),
        ci: Default::default(),
        builds: BTreeMap::new(),
        native: BTreeMap::new(),
        sbx_selection: None,
        linux_host: linux_host.clone(),
        sbx_smoke: BTreeMap::new(),
        release: Default::default(),
    };
    j.sbx_selection = Some(super::runtime_smoke::selection(opts.linux_host.as_deref())?);
    j.phases.insert(
        "preflight".into(),
        PhaseRecord {
            status: Status::Done,
            started: Some(now),
            finished: Some(w.clock.now()),
            error: None,
        },
    );
    j.phases.insert(
        "pin".into(),
        PhaseRecord {
            status: Status::Running,
            started: Some(w.clock.now()),
            ..Default::default()
        },
    );
    store.save(&j)?;
    let mut ctx = Ctx {
        w,
        store,
        j,
        plan,
        repo: repo.clone(),
        linux_host,
        sbx_explicit_host: opts.linux_host.clone(),
        previous_owners,
        deferred: Vec::new(),
        retried: BTreeSet::new(),
        controller_sha256: controller_sha256(),
        hosts: BTreeMap::new(),
        invoked,
        sbx_workers: BTreeMap::new(),
    };
    drive(&mut ctx)
}

fn finish_phase(ctx: &mut Ctx, phase: &str) -> Step {
    let now = ctx.w.clock.now();
    let rec = ctx.j.phases.entry(phase.into()).or_default();
    rec.status = Status::Done;
    rec.finished = Some(now);
    rec.error = None;
    ctx.timing(phase, "finish", "");
    ctx.save()
}

/// Completed phases whose external state could have changed are re-probed.
fn still_done(ctx: &mut Ctx, phase: &str) -> Step<bool> {
    let j = &ctx.j;
    Ok(match phase {
        "push" => {
            ctx.w.forge.get_ref(&j.push_ref)?.as_deref() == Some(j.commit.as_str())
                || j.done("land")
        }
        "collect" => {
            j.release.seal_sha256.is_some()
                || j.builds.iter().all(|(name, b)| {
                    b.sha256.as_ref().is_some_and(|sha| {
                        sha256_file(&ctx.store.path(&format!("assets/{name}")))
                            .ok()
                            .as_deref()
                            == Some(sha)
                    })
                })
        }
        "land" => {
            let main = ctx
                .w
                .forge
                .get_ref(&format!("heads/{}", ctx.repo.default_branch))?
                .unwrap_or_default();
            matches!(
                ctx.w.forge.compare(&main, &j.commit)?,
                super::forge::Ancestry::Identical | super::forge::Ancestry::Behind
            )
        }
        // The tag must still be annotated, at R, and carry this seal.
        "tag" => {
            let seal = super::phases::seal::load_seal(ctx)?.digest();
            ctx.w.forge.tag(&j.tag_name())?.is_some_and(|t| {
                t.annotated
                    && t.commit == j.commit
                    && super::evidence::tag_seal(&t.message).as_deref() == Some(seal.as_str())
            })
        }
        "publish" => {
            let id = j.release.draft_id;
            ctx.w
                .forge
                .releases()?
                .iter()
                .any(|r| Some(r.id) == id && !r.draft)
        }
        // Verified means a bound receipt *and* the public release still
        // holds exactly the sealed files; otherwise verification reruns
        // (read-only) and either rewrites the receipt or names the defect.
        "verify" => {
            let seal = super::phases::seal::load_seal(ctx)?;
            let receipt = super::evidence::check_receipt(
                &ctx.store.path("evidence/receipt.json"),
                &ctx.j,
                &seal,
            );
            if let Err(why) = &receipt {
                ctx.store.log("verify", &format!("re-verifying: {why}"));
            }
            let mut want: Vec<_> = seal
                .assets
                .iter()
                .map(|a| (a.name.clone(), a.asset_id))
                .collect();
            want.sort();
            let public = ctx
                .w
                .forge
                .releases()?
                .into_iter()
                .find(|r| Some(r.id) == ctx.j.release.draft_id && !r.draft)
                .map(|r| {
                    let mut have: Vec<_> =
                        r.assets.iter().map(|a| (a.name.clone(), a.id)).collect();
                    have.sort();
                    have
                });
            receipt.is_ok() && public.as_ref() == Some(&want)
        }
        _ => true,
    })
}

/// Run every phase from the first incomplete one, then record this
/// invocation in the timing ledger (design 7.12). A killed driver records
/// nothing, exactly like SIGKILL.
pub fn drive(ctx: &mut Ctx) -> Step {
    let result = drive_phases(ctx);
    super::runtime_smoke::drain(ctx);
    if matches!(&result, Err(s) if s.kind == Kind::Killed) {
        return result;
    }
    let now = ctx.w.clock.now();
    let outcome = match &result {
        Ok(()) => "done".to_string(),
        Err(s) => s.code.clone(),
    };
    let seconds = ctx.invoked.elapsed().as_secs_f64();
    ctx.j.invocations.push(super::journal::Invocation {
        started: now - chrono::Duration::milliseconds((seconds * 1000.0) as i64),
        monotonic_seconds: seconds,
        outcome: outcome.clone(),
    });
    ctx.timing(
        "invocation",
        "finish",
        &format!("{outcome} after {seconds:.1} s (monotonic)"),
    );
    let (minutes, missed) = ctx.j.target(now);
    if missed {
        // Never a reason to discard work: completed phases stay recorded and
        // the stop (if any) keeps its own recovery instruction.
        ctx.timing(
            "target",
            "missed",
            &format!("{minutes} min > {} min", super::journal::TARGET_MINUTES),
        );
    }
    ctx.save()?;
    if result.is_ok() {
        print_timings(ctx);
    }
    result
}

fn drive_phases(ctx: &mut Ctx) -> Step {
    // A repaired controller may resume an attempt (design 3.8); record which.
    ctx.store.log(
        "preflight",
        &format!("controller sha256 {}", ctx.controller_sha256),
    );
    // `pin` ends when the remote claim exists; a driver killed between the
    // journal write and the claim creates it on resume.
    if !ctx.j.done("pin") {
        ctx.w.forge.checkpoint("pin")?;
        if let Err(e) = ctx.update_claim(None) {
            return Err(if e.code == "state-foreign" {
                Stop::new(
                    "attempt-locked",
                    format!("another controller claimed {} first", ctx.j.version),
                )
            } else {
                e
            });
        }
        finish_phase(ctx, "pin")?;
    }
    let rehearsal_skips = ["confirm", "land", "tag", "publish", "verify"];
    for &phase in PHASES {
        if phase == "preflight" || phase == "pin" {
            continue;
        }
        if ctx.j.rehearsal && rehearsal_skips.contains(&phase) {
            continue;
        }
        if ctx.j.done(phase) && still_done(ctx, phase)? {
            continue;
        }
        ctx.w.interrupted()?;
        let now = ctx.w.clock.now();
        let rec = ctx.j.phases.entry(phase.into()).or_default();
        rec.status = Status::Running;
        rec.started.get_or_insert(now);
        ctx.timing(phase, "start", "");
        ctx.save()?;
        let result = ctx
            .w
            .forge
            .checkpoint(phase)
            .and_then(|_| super::phases::run(ctx, phase));
        match result {
            Ok(()) => finish_phase(ctx, phase)?,
            Err(stop) if stop.kind == Kind::Killed => return Err(stop),
            Err(stop) => {
                let rec = ctx.j.phases.entry(phase.into()).or_default();
                rec.status = if stop.kind == Kind::Failed {
                    Status::Failed
                } else {
                    Status::Blocked
                };
                rec.error = Some(super::journal::ErrorRecord {
                    code: stop.code.clone(),
                    message: stop.message.clone(),
                    remedy: stop.remedy.clone(),
                });
                ctx.timing(phase, "stop", &stop.code);
                ctx.save()?;
                return Err(stop);
            }
        }
    }
    Ok(())
}

fn print_timings(ctx: &Ctx) {
    let (minutes, missed) = ctx.j.target(ctx.w.clock.now());
    println!(
        "release {}: {} in {} min{}",
        ctx.j.version,
        if ctx.j.rehearsal {
            "rehearsal complete (nothing published)"
        } else if ctx.j.disposable_repo.is_some() {
            "disposable-repository rehearsal published and verified (rehearsal-* tag only)"
        } else {
            "published and verified"
        },
        minutes,
        if ctx.j.decided_at.is_some() {
            " since the recorded decision"
        } else {
            " since admission"
        }
    );
    if missed {
        println!(
            "  performance target MISSED: {minutes} min > {} min (the release itself is complete; see the timing ledger)",
            super::journal::TARGET_MINUTES
        );
    }
    let active: f64 = ctx.j.invocations.iter().map(|i| i.monotonic_seconds).sum();
    println!(
        "  {} invocation(s), {active:.0} s of controller time (monotonic)",
        ctx.j.invocations.len()
    );
    for p in PHASES {
        if let Some(r) = ctx.j.phases.get(*p) {
            if let (Some(s), Some(f)) = (r.started, r.finished) {
                println!("  {p:<10} {:>6} s", (f - s).num_seconds());
            }
        }
    }
    println!("  timings: {}", ctx.store.path("timings.jsonl").display());
}

/// Keep the old attempt's record; delete only its owned, unsealed draft.
fn supersede(w: &World, store: &Store, root: &StateRoot, j: &Journal) -> Step {
    if j.release.seal_sha256.is_some() || j.done("seal") || w.forge.tag(&j.tag_name())?.is_some() {
        return Err(Stop::new(
            "supersede-refused",
            format!("{} is sealed or tagged; its bytes cannot change", j.version),
        ));
    }
    super::runtime_smoke::cleanup_attempt(w, store, j)?;
    if let Some(id) = j.release.draft_id {
        let marker = super::evidence::draft_marker(&j.operation);
        if w.forge
            .releases()?
            .iter()
            .any(|r| r.id == id && r.draft && r.body.contains(&marker))
        {
            w.forge.delete_release(id)?;
        }
    }
    let archive = root.root.join(format!(
        "superseded-{}-{}-g{}",
        j.version,
        &j.commit[..8],
        j.generation
    ));
    std::fs::create_dir_all(&archive).map_err(anyhow::Error::from)?;
    for f in ["state.json", "plan.json", "timings.jsonl"] {
        let _ = std::fs::rename(store.path(f), archive.join(f));
    }
    for d in [
        "assets",
        "kits",
        "native",
        "sbx-smoke",
        "evidence",
        "ci",
        "logs",
    ] {
        let _ = std::fs::rename(store.path(d), archive.join(d));
        super::util::private_dir(&store.path(d))?;
    }
    if store.path("src").exists() {
        w.source.remove_checkout(&store.path("src"));
    }
    store.log(
        "preflight",
        &format!(
            "superseded generation {} (archived at {})",
            j.generation,
            archive.display()
        ),
    );
    Ok(())
}

/// `release-doctor`: the prepared-environment check for HEAD; no state, no
/// remote writes.
fn doctor(
    w: &World,
    repo: &RepoInfo,
    version: &str,
    opts: &Options,
    linux_host: Option<&str>,
    root: &StateRoot,
    owner: &str,
) -> Step {
    let scratch = root.root.join("doctor");
    super::util::private_dir(&scratch)?;
    let head = w.source.head()?;
    let src = w.source.checkout(&head.commit, &scratch.join("src"))?;
    let bytes = w.tools.release_plan(&src)?;
    let (plan, _) = Plan::parse(&bytes)?;
    if linux_host.is_none() && plan.native.iter().any(|n| n.host == "ssh") {
        return Err(Stop::new(
            "linux-host-missing",
            "the Linux native check needs a KVM host and no LINUX_HOST is set",
        ));
    }
    let all: BTreeSet<&'static str> = PHASES.iter().copied().collect();
    let natives: Vec<&str> = plan.native.iter().map(|n| n.id.as_str()).collect();
    let r = preflight::check(
        w,
        repo,
        &plan,
        &src,
        version,
        opts.rehearse,
        &preflight::Remaining {
            phases: all,
            natives,
        },
        linux_host,
        &scratch.join("fixtures"),
    );
    w.source.remove_checkout(&scratch.join("src"));
    r?;
    let _ = owner;
    super::runtime_smoke::preflight(
        w,
        &plan,
        &super::runtime_smoke::selection(opts.linux_host.as_deref())?,
        &BTreeMap::new(),
    )?;
    println!(
        "release-doctor: environment prepared for {version} ({} assets, {} native checks)",
        plan.assets.len(),
        plan.native.len()
    );
    Ok(())
}

#[cfg(test)]
mod destination_tests {
    use super::*;

    #[test]
    fn a_saved_kvm_docker_destination_cannot_change_on_resume() {
        assert!(check_linux_destination(Some("build@A"), Some("build@A")).is_ok());
        let changed = check_linux_destination(Some("build@A"), Some("build@B")).unwrap_err();
        assert_eq!(changed.code, "linux-host-changed");
        assert_eq!(
            check_linux_destination(Some("build@A"), None)
                .unwrap_err()
                .code,
            "linux-host-changed"
        );
    }
}
