//! The fault-injection world (design 6.1 layer 1): `DirForge`, `FakeHost`,
//! `FakeSource`, `FakeTools`, a virtual clock and a scripted prompt, all
//! backed by one directory so a second process can resume after the first
//! is killed. The production driver (`run.rs` and every phase) runs
//! unchanged against it: `cargo xtask release run --world DIR`.
//!
//! `DIR/world.json` holds refs, commits, runs, artifacts, attestations,
//! releases, tags, claims and rules with the semantics of the GitHub calls
//! the release uses; file bodies live in `DIR/blobs/<sha256>`. Hosted runs
//! follow scripted timelines on the virtual clock, so waits cost nothing.
//!
//! Faults: `world.json` `faults` is a list of
//! `{"op": "upload_asset", "nth": 2, "effect": "error-after-commit"}`.
//! Effects: `error` (transient failure before the operation), `error-after-
//! commit` (the operation happens, the response is lost), `crash` / `crash-
//! after-commit` (the driver stops as if SIGKILLed: exit 137, no further
//! writes), `stub` (an incomplete upload stub is left), `mismatch` (a
//! download returns different bytes). `op` is a `Forge` method name, a
//! driver checkpoint (`checkpoint:<name>`), or `native_run:<id>`.

use super::error::{Step, Stop};
use super::forge::*;
use super::journal::Attestation;
use super::native::{NativeHost, NativeJob};
use super::plan::NativePlan;
use super::util::{read_json, sha256, sha256_file, write_json};
use super::world::{Clock, Head, Hosts, Prompt, Source, Tools, World};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Fault {
    pub op: String,
    #[serde(default = "one")]
    pub nth: u32,
    pub effect: String,
}
fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Commit {
    pub tree: String,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceState {
    pub origin: String,
    pub head: String,
    pub branch: String,
    pub clean: bool,
    pub commits: BTreeMap<String, Commit>,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    /// Commits that exist on origin but not yet in the local object
    /// database: `checkout` fails until a `fetch` of their branch.
    #[serde(default)]
    pub remote_only: std::collections::BTreeSet<String>,
}

/// How a scripted run behaves: per attempt failure kinds (`infra`,
/// `product`, `startup`), timings in seconds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunScript {
    #[serde(default)]
    pub queue_s: u64,
    #[serde(default)]
    pub run_s: u64,
    /// attempt number (1-based) → failure kind.
    #[serde(default)]
    pub fail: BTreeMap<u32, String>,
    /// Omit build.json / a smoke test from the artifact.
    #[serde(default)]
    pub missing: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NativeScript {
    /// `passed`, `failed` or `blocked`.
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub reason_code: Option<String>,
    /// Drop one expected test from the result.
    #[serde(default)]
    pub omit_test: bool,
    #[serde(default)]
    pub run_s: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Script {
    #[serde(default)]
    pub sbx_smoke: BTreeMap<String, NativeScript>,
    #[serde(default)]
    pub sbx_smoke_unavailable: BTreeMap<String, String>,
    #[serde(default)]
    pub sbx_smoke_version: Option<String>,
    #[serde(default)]
    pub sbx_version: Option<String>,
    #[serde(default)]
    pub sbx_change_before_seal: bool,
    #[serde(default)]
    pub sbx_label: Option<String>,
    #[serde(default)]
    pub ci: RunScript,
    #[serde(default)]
    pub builds: BTreeMap<String, RunScript>,
    #[serde(default)]
    pub native: BTreeMap<String, NativeScript>,
    /// SSH destinations that are unreachable.
    #[serde(default)]
    pub unreachable: Vec<String>,
    #[serde(default)]
    pub mac_unavailable: Option<String>,
    #[serde(default)]
    pub sbx_unavailable: Option<String>,
    #[serde(default = "yes")]
    pub confirm: bool,
    #[serde(default)]
    pub merge_conflict: bool,
    /// A publication-policy change that lands on main concurrently with the
    /// release's server-side merge: the merge commit carries this policy.
    #[serde(default)]
    pub merge_policy: Option<String>,
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Run {
    pub info: RunInfo,
    pub inputs: BTreeMap<String, String>,
    pub attempt_started: DateTime<Utc>,
    pub queue_s: u64,
    pub run_s: u64,
    /// Failure kind of the current attempt, if any.
    pub fail: Option<String>,
    #[serde(default)]
    pub materialized: bool,
    #[serde(default)]
    pub artifacts: Vec<u64>,
}

impl Default for RunInfo {
    fn default() -> Self {
        RunInfo {
            id: 0,
            workflow: String::new(),
            title: String::new(),
            head_sha: String::new(),
            event: String::new(),
            status: String::new(),
            conclusion: None,
            attempt: 1,
            created: DateTime::default(),
            updated: DateTime::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub run_id: u64,
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub expired: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    pub repo: String,
    pub repo_id: u64,
    pub default_branch: String,
    pub clock: DateTime<Utc>,
    pub next_id: u64,
    pub source: SourceState,
    /// Release plan JSON per commit (missing → `default`).
    pub plans: BTreeMap<String, Value>,
    pub refs: BTreeMap<String, String>,
    pub workflows: BTreeMap<String, bool>,
    pub runs: Vec<Run>,
    pub artifacts: BTreeMap<u64, Artifact>,
    pub attestations: BTreeMap<String, Attestation>,
    pub releases: Vec<ReleaseInfo>,
    /// asset id → blob sha
    pub asset_blobs: BTreeMap<u64, String>,
    pub tags: BTreeMap<String, TagInfo>,
    pub claims: BTreeMap<String, (String, BTreeMap<String, String>)>,
    pub rules: Vec<String>,
    /// Public URLs outside releases (fixtures, previous release).
    pub public: BTreeMap<String, String>,
    pub script: Script,
    pub faults: Vec<Fault>,
    pub counters: BTreeMap<String, u32>,
    pub calls: Vec<String>,
}

#[derive(Clone)]
pub struct Dir {
    pub path: PathBuf,
}

impl Dir {
    pub fn load(&self) -> Store {
        read_json(&self.path.join("world.json")).expect("world.json")
    }
    pub fn save(&self, s: &Store) {
        // Atomic (rename) but not fsynced: the fault world models GitHub, it
        // is not durable state; the private journal keeps its fsync.
        let tmp = self.path.join(".world.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(s).expect("world serializes")).expect("world.json");
        std::fs::rename(&tmp, self.path.join("world.json")).expect("world.json");
    }
    pub fn with<T>(&self, f: impl FnOnce(&mut Store) -> T) -> T {
        let mut s = self.load();
        let out = f(&mut s);
        self.save(&s);
        out
    }
    pub fn put_blob(&self, bytes: &[u8]) -> String {
        let sha = sha256(bytes);
        let p = self.path.join("blobs").join(&sha);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
        sha
    }
    pub fn blob(&self, sha: &str) -> Vec<u8> {
        std::fs::read(self.path.join("blobs").join(sha)).unwrap_or_default()
    }

    /// Count this operation; return the effect of a matching fault, if any.
    pub fn fault(&self, op: &str) -> Option<String> {
        // Counting only matters for operations with a scripted fault; others
        // stay read-only (status must not change the world).
        if !self.load().faults.iter().any(|f| f.op == op) {
            return None;
        }
        self.with(|s| {
            let n = s.counters.entry(op.to_string()).or_insert(0);
            *n += 1;
            let n = *n;
            s.faults.iter().find(|f| f.op == op && f.nth == n).map(|f| {
                s.calls.push(format!("fault {op} #{n} {}", f.effect));
                f.effect.clone()
            })
        })
    }

    pub fn call(&self, text: String) {
        self.with(|s| s.calls.push(text));
    }
}

fn transient(op: &str) -> Stop {
    Stop::new("network", format!("{op}: injected transient failure"))
}
fn killed(op: &str) -> Stop {
    Stop::new("killed", format!("driver killed at {op}"))
}

/// Apply a fault around an operation.
fn around<T>(dir: &Dir, op: &str, body: impl FnOnce() -> Step<T>) -> Step<T> {
    match dir.fault(op).as_deref() {
        Some("error") => Err(transient(op)),
        Some("crash") => Err(killed(op)),
        Some("error-after-commit") => {
            body()?;
            Err(transient(op))
        }
        Some("crash-after-commit") => {
            body()?;
            Err(killed(op))
        }
        _ => body(),
    }
}

fn is_ancestor(s: &Store, a: &str, b: &str) -> bool {
    let mut stack = vec![b.to_string()];
    let mut seen = std::collections::BTreeSet::new();
    while let Some(c) = stack.pop() {
        if c == a {
            return true;
        }
        if seen.insert(c.clone()) {
            if let Some(commit) = s.source.commits.get(&c) {
                stack.extend(commit.parents.iter().cloned());
            }
        }
    }
    false
}

pub struct DirForge {
    pub dir: Dir,
}

impl DirForge {
    /// Bring a run's state up to the virtual clock and materialize its
    /// outputs once it completes.
    fn refresh(&self, s: &mut Store, id: u64) {
        let now = s.clock;
        let Some(i) = s.runs.iter().position(|r| r.info.id == id) else {
            return;
        };
        let r = s.runs[i].clone();
        let started = r.attempt_started + chrono::Duration::seconds(r.queue_s as i64);
        let done = started + chrono::Duration::seconds(r.run_s as i64);
        let mut run = r.clone();
        if r.fail.as_deref() == Some("startup") {
            run.info.status = "completed".into();
            run.info.conclusion = Some("startup_failure".into());
        } else if now < started {
            run.info.status = "queued".into();
            run.info.conclusion = None;
        } else if now < done {
            run.info.status = "in_progress".into();
            run.info.conclusion = None;
        } else {
            run.info.status = "completed".into();
            run.info.conclusion = Some(
                if r.fail.is_some() {
                    "failure"
                } else {
                    "success"
                }
                .into(),
            );
            run.info.updated = done;
            if r.fail.is_none() && !r.materialized {
                self.materialize(s, &mut run);
                run.materialized = true;
            }
        }
        s.runs[i] = run;
    }

    fn materialize(&self, s: &mut Store, run: &mut Run) {
        let commit = run.info.head_sha.clone();
        let tree = s
            .source
            .commits
            .get(&commit)
            .map(|c| c.tree.clone())
            .unwrap_or_default();
        let mut add = |s: &mut Store, name: String, files: BTreeMap<String, String>| {
            s.next_id += 1;
            let id = s.next_id;
            s.artifacts.insert(
                id,
                Artifact {
                    name,
                    run_id: run.info.id,
                    files,
                    expired: false,
                },
            );
            run.artifacts.push(id);
        };
        if run.info.workflow == "ci.yml" {
            let body = self.dir.put_blob(
                json!({"schema": 1, "gate": "release", "commit": commit})
                    .to_string()
                    .as_bytes(),
            );
            add(
                s,
                format!("gate-evidence-release-{commit}"),
                BTreeMap::from([("gate-evidence.json".into(), body)]),
            );
            return;
        }
        let asset = run.inputs.get("asset").cloned().unwrap_or_default();
        let missing = s.script.builds.get(&asset).and_then(|b| b.missing.clone());
        let bytes = format!(
            "binary {asset} {commit} {}",
            run.inputs.get("key").cloned().unwrap_or_default()
        );
        let asset_sha = self.dir.put_blob(bytes.as_bytes());
        let tests = [
            "artifact smoke::starts",
            "artifact entrypoints::help",
            "artifact switches::only_reduce",
        ];
        let mut smoke: Vec<Value> = tests
            .iter()
            .map(|t| json!({"test": t, "outcome": "passed"}))
            .collect();
        if missing.as_deref() == Some("smoke-test") {
            smoke.pop();
        }
        let build = json!({"schema": 1, "asset": asset, "sha256": asset_sha, "size": bytes.len(),
            "commit": commit, "tree": tree, "triple": "fake", "toolchain": "1.94.0", "runner_image": "fake-1",
            "smoke": {"expected": tests, "results": smoke}, "abi": {"ok": true}});
        let mut files = BTreeMap::from([(asset.clone(), asset_sha.clone())]);
        if missing.as_deref() != Some("build.json") {
            files.insert(
                "build.json".into(),
                self.dir
                    .put_blob(serde_json::to_vec(&build).unwrap().as_slice()),
            );
        }
        let signer = format!("{}/.github/workflows/release.yml", s.repo);
        s.attestations.insert(
            asset_sha.clone(),
            Attestation {
                subject_sha256: asset_sha,
                signer_workflow: signer.clone(),
                source_digest: commit.clone(),
                hosted: true,
            },
        );
        add(s, format!("release-asset-{asset}"), files);
        let plan = plan_for(s, &commit);
        let native = plan["native"].as_array().cloned().unwrap_or_default();
        for n in native.iter().filter(|n| n["asset"] == asset.as_str()) {
            let id = n["id"].as_str().unwrap_or("").to_string();
            let kit = fake_kit(n, &plan, &commit, &tree);
            let kit_sha = self.dir.put_blob(&kit);
            s.attestations.insert(
                kit_sha.clone(),
                Attestation {
                    subject_sha256: kit_sha.clone(),
                    signer_workflow: signer.clone(),
                    source_digest: commit.clone(),
                    hosted: true,
                },
            );
            add(
                s,
                format!("native-kit-{id}"),
                BTreeMap::from([(format!("native-kit-{id}.tar"), kit_sha)]),
            );
        }
    }
}

pub fn plan_for(s: &Store, commit: &str) -> Value {
    s.plans
        .get(commit)
        .or_else(|| s.plans.get("default"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// A kit with the same metadata/raw-inventory contract as the real producer.
pub fn fake_kit(native: &Value, plan: &Value, commit: &str, tree: &str) -> Vec<u8> {
    let exe = b"#!/bin/sh\nexit 2\n".to_vec();
    let expected =
        serde_json::to_vec(&fake_inventory(native["tests"].as_array().unwrap())).unwrap();
    let capabilities = serde_json::to_vec(&json!({"schema":1,"grants":crate::manifest::release_policy::native_grants(native["id"].as_str().unwrap()).unwrap()})).unwrap();
    let mut files = BTreeMap::from([
        ("bin/xtask", exe),
        ("expected.json", expected),
        ("capabilities.json", capabilities),
    ]);
    let mut digests: BTreeMap<_, _> = files.iter().map(|(p, b)| (*p, sha256(b))).collect();
    let fixtures = native["fixtures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            json!({
                "id":f["id"],"file":f["file"],"arch":f["arch"],"sha256":f["sha256"]
            })
        })
        .collect::<Vec<_>>();
    let previous = if native["previous_release"].is_null() {
        Value::Null
    } else {
        json!({
            "triple":native["triple"],"asset":native["previous_release"]["asset"],"sha256":native["previous_release"]["sha256"]
        })
    };
    let kit = serde_json::to_vec(
        &json!({"schema":1,"native_id":native["id"],"commit":commit,"tree":tree,
        "tests":native["tests"],"triple":native["triple"],"files":digests,
        "fixtures":fixtures,"previous_release":previous,"manifest_sha256":plan["manifest_sha256"],
        "plan_sha256":sha256(&plan_bytes(plan)),"policy_sha256":plan["policy_sha256"]}),
    )
    .unwrap();
    digests.insert("kit.json", sha256(&kit));
    files.insert("kit.json", kit);
    files.insert("digests.json", serde_json::to_vec(&digests).unwrap());
    let mut b = tar::Builder::new(Vec::new());
    for (name, body) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(if name.starts_with("bin/") {
            0o755
        } else {
            0o644
        });
        h.set_mtime(0);
        h.set_cksum();
        b.append_data(&mut h, name, body.as_slice()).unwrap();
    }
    b.into_inner().unwrap()
}

fn plan_bytes(plan: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(plan).unwrap();
    bytes.push(b'\n');
    bytes
}

fn fake_inventory(tests: &[Value]) -> Value {
    let mut suites = serde_json::Map::new();
    for test in tests {
        let (binary, name) = test.as_str().unwrap().split_once(' ').unwrap();
        let suite = suites
            .entry(binary)
            .or_insert_with(|| json!({"binary-id":binary,"testcases":{}}));
        suite["testcases"][name] = json!({"ignored":false,"filter-match":{"status":"matches"}});
    }
    json!({"rust-suites":suites})
}

impl Forge for DirForge {
    fn repo(&self) -> Step<RepoInfo> {
        around(&self.dir, "repo", || {
            let s = self.dir.load();
            Ok(RepoInfo {
                id: s.repo_id,
                full_name: s.repo,
                default_branch: s.default_branch,
                can_push: true,
            })
        })
    }

    fn get_ref(&self, name: &str) -> Step<Option<String>> {
        around(&self.dir, "get_ref", || {
            Ok(self.dir.load().refs.get(name).cloned())
        })
    }

    fn compare(&self, base: &str, head: &str) -> Step<Ancestry> {
        around(&self.dir, "compare", || {
            let s = self.dir.load();
            Ok(if base == head {
                Ancestry::Identical
            } else if is_ancestor(&s, base, head) {
                Ancestry::Ahead
            } else if is_ancestor(&s, head, base) {
                Ancestry::Behind
            } else {
                Ancestry::Diverged
            })
        })
    }

    fn push(&self, commit: &str, branch_ref: &str) -> Step<RefWrite> {
        around(&self.dir, "push", || {
            self.dir.with(|s| {
                s.calls.push(format!("push {branch_ref} {commit}"));
                match s.refs.get(branch_ref).cloned() {
                    Some(cur) if cur != commit && !is_ancestor(s, &cur, commit) => {
                        Ok(RefWrite::Rejected(Some(cur)))
                    }
                    _ => {
                        s.refs.insert(branch_ref.into(), commit.into());
                        Ok(RefWrite::Done(commit.into()))
                    }
                }
            })
        })
    }

    fn fast_forward(&self, branch_ref: &str, commit: &str) -> Step<RefWrite> {
        around(&self.dir, "fast_forward", || {
            self.dir.with(|s| {
                let cur = s.refs.get(branch_ref).cloned();
                match cur {
                    Some(c) if c == commit || is_ancestor(s, &c, commit) => {
                        s.calls.push(format!("fast-forward {branch_ref} {commit}"));
                        s.refs.insert(branch_ref.into(), commit.into());
                        Ok(RefWrite::Done(commit.into()))
                    }
                    other => Ok(RefWrite::Rejected(other)),
                }
            })
        })
    }

    fn delete_branch(&self, branch_ref: &str) -> Step {
        around(&self.dir, "delete_branch", || {
            self.dir.with(|s| {
                s.calls.push(format!("delete-branch {branch_ref}"));
                s.refs.remove(branch_ref);
            });
            Ok(())
        })
    }

    fn merge(&self, branch: &str, head: &str, message: &str) -> Step<Merge> {
        around(&self.dir, "merge", || {
            self.dir.with(|s| {
                let key = format!("heads/{branch}");
                let tip = s.refs.get(&key).cloned().unwrap_or_default();
                if tip == head || is_ancestor(s, head, &tip) {
                    return Ok(Merge::UpToDate);
                }
                if s.script.merge_conflict {
                    return Ok(Merge::Conflict);
                }
                let oid =
                    sha256(format!("merge {tip} {head} {message}").as_bytes())[..40].to_string();
                s.source.commits.insert(
                    oid.clone(),
                    Commit {
                        tree: format!("merged-{oid}"),
                        parents: vec![tip, head.into()],
                        files: BTreeMap::new(),
                    },
                );
                if let Some(policy) = s.script.merge_policy.clone() {
                    let mut p = plan_for(s, "default");
                    p["policy_sha256"] = policy.into();
                    s.plans.insert(oid.clone(), p);
                }
                s.refs.insert(key, oid.clone());
                s.calls.push(format!("merge {branch} {head}"));
                Ok(Merge::Merged(oid))
            })
        })
    }

    fn read_claim(&self, version: &str) -> Step<Option<Claim>> {
        around(&self.dir, "read_claim", || {
            let s = self.dir.load();
            Ok(s.claims.get(version).map(|(oid, files)| Claim {
                oid: oid.clone(),
                files: files
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone().into_bytes()))
                    .collect(),
            }))
        })
    }

    fn write_claim(
        &self,
        version: &str,
        parent: Option<&str>,
        files: &BTreeMap<String, Vec<u8>>,
        _m: &str,
    ) -> Step<RefWrite> {
        around(&self.dir, "write_claim", || {
            self.dir.with(|s| {
                let cur = s.claims.get(version).map(|(o, _)| o.clone());
                if cur.as_deref() != parent {
                    return Ok(RefWrite::Rejected(cur));
                }
                let text: BTreeMap<String, String> = files
                    .iter()
                    .map(|(k, v)| (k.clone(), String::from_utf8_lossy(v).into_owned()))
                    .collect();
                let oid = sha256(format!("{parent:?}{text:?}").as_bytes())[..40].to_string();
                s.claims.insert(version.into(), (oid.clone(), text));
                s.calls.push(format!(
                    "claim {version} {}",
                    if parent.is_some() { "update" } else { "create" }
                ));
                Ok(RefWrite::Done(oid))
            })
        })
    }

    fn delete_claim(&self, version: &str) -> Step {
        around(&self.dir, "delete_claim", || {
            self.dir.with(|s| s.claims.remove(version));
            Ok(())
        })
    }

    fn workflow_revision(&self, workflow: &str, _git_ref: &str) -> Step<Option<String>> {
        around(&self.dir, "workflow_revision", || {
            Ok(self
                .dir
                .load()
                .workflows
                .contains_key(workflow)
                .then(|| sha256(workflow.as_bytes())))
        })
    }

    fn workflow_active(&self, workflow: &str) -> Step<bool> {
        around(&self.dir, "workflow_active", || {
            Ok(self
                .dir
                .load()
                .workflows
                .get(workflow)
                .copied()
                .unwrap_or(false))
        })
    }

    fn dispatch(&self, workflow: &str, git_ref: &str, inputs: &BTreeMap<String, String>) -> Step {
        around(&self.dir, &format!("dispatch:{workflow}"), || {
            self.dir.with(|s| {
                let head = s
                    .refs
                    .get(&format!("heads/{git_ref}"))
                    .cloned()
                    .unwrap_or_default();
                s.next_id += 1;
                let id = s.next_id;
                let (script, title) = if workflow == "ci.yml" {
                    (
                        s.script.ci.clone(),
                        format!(
                            "CI {} {}",
                            inputs.get("gate").cloned().unwrap_or_default(),
                            inputs.get("key").cloned().unwrap_or_default()
                        ),
                    )
                } else {
                    let asset = inputs.get("asset").cloned().unwrap_or_default();
                    (
                        s.script.builds.get(&asset).cloned().unwrap_or_default(),
                        format!(
                            "release {} {asset} {}",
                            inputs.get("version").cloned().unwrap_or_default(),
                            inputs.get("key").cloned().unwrap_or_default()
                        ),
                    )
                };
                s.calls.push(format!("dispatch {workflow} {title}"));
                let now = s.clock;
                s.runs.push(Run {
                    info: RunInfo {
                        id,
                        workflow: workflow.into(),
                        title,
                        head_sha: head,
                        event: "workflow_dispatch".into(),
                        status: "queued".into(),
                        conclusion: None,
                        attempt: 1,
                        created: now,
                        updated: now,
                    },
                    inputs: inputs.clone(),
                    attempt_started: now,
                    queue_s: script.queue_s,
                    run_s: script.run_s,
                    fail: script.fail.get(&1).cloned(),
                    materialized: false,
                    artifacts: vec![],
                });
                Ok(())
            })
        })
    }

    fn runs(&self, workflow: &str, head_sha: &str) -> Step<Vec<RunInfo>> {
        around(&self.dir, "runs", || {
            self.dir.with(|s| {
                let ids: Vec<u64> = s
                    .runs
                    .iter()
                    .filter(|r| r.info.workflow == workflow && r.info.head_sha == head_sha)
                    .map(|r| r.info.id)
                    .collect();
                ids.iter().for_each(|id| self.refresh(s, *id));
                Ok(s.runs
                    .iter()
                    .filter(|r| ids.contains(&r.info.id))
                    .map(|r| r.info.clone())
                    .collect())
            })
        })
    }

    fn run(&self, id: u64) -> Step<RunInfo> {
        around(&self.dir, "run", || {
            self.dir.with(|s| {
                self.refresh(s, id);
                s.runs
                    .iter()
                    .find(|r| r.info.id == id)
                    .map(|r| r.info.clone())
                    .ok_or_else(|| Stop::new("ci-unavailable", format!("run {id} not found")))
            })
        })
    }

    fn jobs(&self, run_id: u64) -> Step<Vec<JobInfo>> {
        around(&self.dir, "jobs", || {
            self.dir.with(|s| {
                self.refresh(s, run_id);
                let r = s
                    .runs
                    .iter()
                    .find(|r| r.info.id == run_id)
                    .cloned()
                    .unwrap_or_default();
                let names: &[&str] = if r.info.workflow == "ci.yml" {
                    &["test (linux-x64-default)", "gate"]
                } else {
                    &["produce", "validate"]
                };
                Ok(names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| {
                        let failed_here = r.info.status == "completed"
                            && r.fail.is_some()
                            && i == names.len() - 1;
                        JobInfo {
                            id: run_id * 10 + i as u64,
                            name: n.to_string(),
                            status: r.info.status.clone(),
                            conclusion: if r.info.status != "completed" {
                                None
                            } else if failed_here {
                                Some("failure".into())
                            } else {
                                Some("success".into())
                            },
                            attempt: r.info.attempt,
                            failed_steps: match (failed_here, r.fail.as_deref()) {
                                (true, Some("product")) => vec!["Run tests".into()],
                                // An artifact upload that failed after the
                                // job's own work succeeded.
                                (true, Some("transport")) => {
                                    vec!["Validated release asset".into()]
                                }
                                _ => vec![],
                            },
                        }
                    })
                    .collect())
            })
        })
    }

    fn rerun_failed(&self, run_id: u64) -> Step {
        around(&self.dir, "rerun_failed", || {
            self.dir.with(|s| {
                let now = s.clock;
                let script = s.script.clone();
                if let Some(r) = s.runs.iter_mut().find(|r| r.info.id == run_id) {
                    r.info.attempt += 1;
                    r.attempt_started = now;
                    let sc = if r.info.workflow == "ci.yml" {
                        script.ci.clone()
                    } else {
                        script
                            .builds
                            .get(r.inputs.get("asset").map(String::as_str).unwrap_or(""))
                            .cloned()
                            .unwrap_or_default()
                    };
                    r.fail = sc.fail.get(&r.info.attempt).cloned();
                    r.queue_s = sc.queue_s;
                    r.run_s = sc.run_s;
                    s.calls
                        .push(format!("rerun {run_id} attempt {}", r.info.attempt));
                }
                Ok(())
            })
        })
    }

    fn cancel(&self, run_id: u64) -> Step {
        around(&self.dir, "cancel", || {
            self.dir.call(format!("cancel {run_id}"));
            Ok(())
        })
    }

    fn artifacts(&self, run_id: u64) -> Step<Vec<ArtifactInfo>> {
        around(&self.dir, "artifacts", || {
            self.dir.with(|s| {
                self.refresh(s, run_id);
                Ok(s.artifacts
                    .iter()
                    .filter(|(_, a)| a.run_id == run_id)
                    .map(|(id, a)| ArtifactInfo {
                        id: *id,
                        name: a.name.clone(),
                        digest: Some(sha256(format!("{:?}", a.files).as_bytes())),
                        expired: a.expired,
                        size: 1,
                    })
                    .collect())
            })
        })
    }

    fn download_artifact(&self, _run_id: u64, artifact: &ArtifactInfo, dest: &Path) -> Step {
        around(&self.dir, "download_artifact", || {
            let s = self.dir.load();
            let a = s.artifacts.get(&artifact.id).ok_or_else(|| {
                Stop::new(
                    "retained-bytes-missing",
                    format!("artifact {} is gone", artifact.name),
                )
            })?;
            if a.expired {
                return Err(Stop::new(
                    "retained-bytes-missing",
                    format!("artifact {} expired", artifact.name),
                ));
            }
            std::fs::create_dir_all(dest).map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
            for (name, blob) in &a.files {
                std::fs::write(dest.join(name), self.dir.blob(blob))
                    .map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
            }
            Ok(())
        })
    }

    fn verify_attestation(
        &self,
        file: &Path,
        signer_workflow: &str,
        source_digest: &str,
    ) -> Step<Attestation> {
        around(&self.dir, "verify_attestation", || {
            let s = self.dir.load();
            let sha = sha256_file(file)?;
            match s.attestations.get(&sha) {
                Some(a)
                    if a.source_digest == source_digest
                        && a.signer_workflow.ends_with(&format!("/{signer_workflow}"))
                        && a.hosted =>
                {
                    Ok(a.clone())
                }
                _ => Err(Stop::new(
                    "attestation-invalid",
                    format!("{}: no matching hosted attestation", file.display()),
                )),
            }
        })
    }

    fn releases(&self) -> Step<Vec<ReleaseInfo>> {
        around(&self.dir, "releases", || Ok(self.dir.load().releases))
    }

    fn create_draft(&self, tag: &str, name: &str, target: &str, body: &str) -> Step<ReleaseInfo> {
        around(&self.dir, "create_draft", || {
            self.dir.with(|s| {
                s.next_id += 1;
                let r = ReleaseInfo {
                    id: s.next_id,
                    tag: tag.into(),
                    name: name.into(),
                    draft: true,
                    target: target.into(),
                    body: body.into(),
                    assets: vec![],
                };
                s.releases.push(r.clone());
                s.calls.push(format!("create-draft {name}"));
                Ok(r)
            })
        })
    }

    fn upload_asset(&self, release_id: u64, name: &str, file: &Path) -> Step<AssetInfo> {
        let effect = self.dir.fault("upload_asset");
        if effect.as_deref() == Some("error") {
            return Err(transient("upload_asset"));
        }
        let bytes = std::fs::read(file).map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
        let blob = self.dir.put_blob(&bytes);
        let stub = effect.as_deref() == Some("stub");
        let info = self.dir.with(|s| {
            s.next_id += 1;
            let tag = s
                .releases
                .iter()
                .find(|r| r.id == release_id)
                .map(|r| r.tag.clone())
                .unwrap_or_default();
            let a = AssetInfo {
                id: s.next_id,
                name: name.into(),
                size: bytes.len() as u64,
                state: if stub {
                    "starter".into()
                } else {
                    "uploaded".into()
                },
                digest: (!stub).then(|| sha256(&bytes)),
                url: format!(
                    "https://github.com/{}/releases/download/{tag}/{name}",
                    s.repo
                ),
            };
            s.asset_blobs.insert(a.id, blob);
            if let Some(r) = s.releases.iter_mut().find(|r| r.id == release_id) {
                r.assets.push(a.clone());
            }
            s.calls.push(format!("upload {name}"));
            a
        });
        match effect.as_deref() {
            Some("error-after-commit") | Some("stub") => Err(transient("upload_asset")),
            Some("crash-after-commit") => Err(killed("upload_asset")),
            _ => Ok(info),
        }
    }

    fn delete_asset(&self, asset_id: u64) -> Step {
        around(&self.dir, "delete_asset", || {
            self.dir.with(|s| {
                for r in &mut s.releases {
                    r.assets.retain(|a| a.id != asset_id);
                }
                s.calls.push(format!("delete-asset {asset_id}"));
            });
            Ok(())
        })
    }

    fn download_asset(&self, asset_id: u64, dest: &Path) -> Step {
        let effect = self.dir.fault("download_asset");
        if effect.as_deref() == Some("error") {
            return Err(transient("download_asset"));
        }
        let s = self.dir.load();
        let mut bytes = s
            .asset_blobs
            .get(&asset_id)
            .map(|b| self.dir.blob(b))
            .unwrap_or_default();
        if effect.as_deref() == Some("mismatch") {
            bytes.extend_from_slice(b"tampered");
        }
        std::fs::write(dest, bytes).map_err(|e| Stop::new("state-corrupt", e.to_string()))
    }

    fn publish(&self, release_id: u64) -> Step {
        around(&self.dir, "publish", || {
            self.dir.with(|s| {
                if let Some(r) = s.releases.iter_mut().find(|r| r.id == release_id) {
                    r.draft = false;
                }
                s.calls.push(format!("publish {release_id}"));
            });
            Ok(())
        })
        .map_err(|e| {
            if e.code == "network" {
                Stop::new("publish-failed", e.message)
            } else {
                e
            }
        })
    }

    fn delete_release(&self, release_id: u64) -> Step {
        around(&self.dir, "delete_release", || {
            self.dir.with(|s| {
                s.releases.retain(|r| r.id != release_id);
                s.calls.push(format!("delete-release {release_id}"));
            });
            Ok(())
        })
    }

    fn tag(&self, name: &str) -> Step<Option<TagInfo>> {
        around(&self.dir, "tag", || {
            Ok(self.dir.load().tags.get(name).cloned())
        })
    }

    fn create_tag(&self, name: &str, commit: &str, message: &str) -> Step<RefWrite> {
        around(&self.dir, "create_tag", || {
            self.dir.with(|s| {
                if s.tags.contains_key(name) {
                    return Ok(RefWrite::Rejected(
                        s.tags.get(name).map(|t| t.commit.clone()),
                    ));
                }
                s.tags.insert(
                    name.into(),
                    TagInfo {
                        commit: commit.into(),
                        annotated: true,
                        message: message.into(),
                    },
                );
                s.calls.push(format!("tag {name} {commit}"));
                Ok(RefWrite::Done(commit.into()))
            })
        })
    }

    fn download_public(&self, url: &str, dest: &Path) -> Step {
        let effect = self.dir.fault("download_public");
        if effect.as_deref() == Some("error") {
            return Err(Stop::new(
                "verify-unavailable",
                format!("{url}: injected transient failure"),
            ));
        }
        let s = self.dir.load();
        let blob = s.public.get(url).cloned().or_else(|| {
            s.releases
                .iter()
                .filter(|r| !r.draft)
                .flat_map(|r| r.assets.iter())
                .find(|a| a.url == url)
                .and_then(|a| s.asset_blobs.get(&a.id).cloned())
        });
        let Some(blob) = blob else {
            return Err(Stop::new(
                "verify-unavailable",
                format!("{url} is not publicly available"),
            ));
        };
        let mut bytes = self.dir.blob(&blob);
        if effect.as_deref() == Some("mismatch") {
            bytes.extend_from_slice(b"tampered");
        }
        self.dir.call(format!("download-public {url}"));
        std::fs::write(dest, bytes).map_err(|e| Stop::new("state-corrupt", e.to_string()))
    }

    fn blocking_rules(&self, _branch: &str, _tag: &str) -> Step<Vec<String>> {
        around(&self.dir, "blocking_rules", || Ok(self.dir.load().rules))
    }

    fn latest_run(&self, workflow: &str, _branch: &str) -> Step<Option<RunInfo>> {
        around(&self.dir, "latest_run", || {
            Ok(self
                .dir
                .load()
                .runs
                .iter()
                .filter(|r| r.info.workflow == workflow)
                .max_by_key(|r| r.info.id)
                .map(|r| r.info.clone()))
        })
    }

    fn checkpoint(&self, op: &str) -> Step {
        around(&self.dir, &format!("checkpoint:{op}"), || Ok(()))
    }
}

// ── source, tools, clock, prompt ─────────────────────────────────────────────

pub struct FakeSource {
    pub dir: Dir,
}

impl Source for FakeSource {
    fn origin_repo(&self) -> Step<String> {
        Ok(self.dir.load().source.origin)
    }
    fn head(&self) -> Step<Head> {
        let s = self.dir.load().source;
        Ok(Head {
            commit: s.head,
            branch: s.branch,
            clean: s.clean,
        })
    }
    fn tree(&self, commit: &str) -> Step<String> {
        self.dir
            .load()
            .source
            .commits
            .get(commit)
            .map(|c| c.tree.clone())
            .ok_or_else(|| Stop::new("state-corrupt", format!("unknown commit {commit}")))
    }
    fn file_at(&self, commit: &str, path: &str) -> Step<Option<Vec<u8>>> {
        Ok(self
            .dir
            .load()
            .source
            .commits
            .get(commit)
            .and_then(|c| c.files.get(path))
            .map(|t| t.clone().into_bytes()))
    }
    fn fetch(&self, branch: &str) -> Step<String> {
        around(&self.dir, "fetch", || {
            self.dir.with(|s| {
                let tip = s
                    .refs
                    .get(&format!("heads/{branch}"))
                    .cloned()
                    .ok_or_else(|| Stop::new("network", format!("origin has no {branch}")))?;
                s.source.remote_only.remove(&tip);
                Ok(tip)
            })
        })
    }
    fn is_ancestor(&self, ancestor: &str, commit: &str) -> Step<bool> {
        Ok(is_ancestor(&self.dir.load(), ancestor, commit))
    }
    fn checkout(&self, commit: &str, dir: &Path) -> Step<PathBuf> {
        if self.dir.load().source.remote_only.contains(commit) {
            return Err(Stop::new(
                "state-corrupt",
                format!("git worktree add {commit}: not a local object (not fetched)"),
            ));
        }
        std::fs::create_dir_all(dir).map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
        std::fs::write(dir.join(".commit"), commit)
            .map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
        Ok(dir.to_path_buf())
    }
    fn remove_checkout(&self, dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }
    fn config_get(&self, key: &str) -> Option<String> {
        self.dir.load().source.config.get(key).cloned()
    }
    fn config_set(&self, key: &str, value: &str) {
        self.dir
            .with(|s| s.source.config.insert(key.into(), value.into()));
    }
}

pub struct FakeTools {
    pub dir: Dir,
}

impl Tools for FakeTools {
    fn release_plan(&self, root: &Path) -> Step<Vec<u8>> {
        let commit = std::fs::read_to_string(root.join(".commit")).unwrap_or_default();
        let plan = plan_for(&self.dir.load(), &commit);
        if plan.is_null() {
            return Err(Stop::new("manifest-stale", "no release plan in the world"));
        }
        Ok(plan_bytes(&plan))
    }

    fn verify_ci(
        &self,
        _root: &Path,
        plan: &Path,
        _repo: &str,
        run_id: u64,
        out: &Path,
    ) -> Step<(i32, Vec<u8>)> {
        let s = self.dir.load();
        let r = s
            .runs
            .iter()
            .find(|r| r.info.id == run_id)
            .cloned()
            .unwrap_or_default();
        let plan_bytes = std::fs::read(plan).unwrap_or_default();
        let p: Value = serde_json::from_slice(&plan_bytes).unwrap_or(Value::Null);
        let tree = s
            .source
            .commits
            .get(&r.info.head_sha)
            .map(|c| c.tree.clone())
            .unwrap_or_default();
        let ok = r.info.conclusion.as_deref() == Some("success");
        let v = json!({"schema": 1, "outcome": if ok { "passed" } else { "failed" },
            "reason_code": if ok { Value::Null } else { "ci-failed".into() },
            "repository_id": s.repo_id, "run_id": run_id, "run_attempt": r.info.attempt,
            "commit": r.info.head_sha, "tree": tree, "manifest_sha256": p["manifest_sha256"],
            "plan_sha256": sha256(&plan_bytes), "policy_sha256": p["policy_sha256"],
            "workflow": {"path": ".github/workflows/ci.yml", "revision": sha256(b"ci.yml")},
            "cells": [{"suite": "unit-awman", "build": "linux-x64-default", "tests": ["t"], "job_id": run_id * 10, "attempt": r.info.attempt}],
            "artifacts": [{"id": r.artifacts.first().copied().unwrap_or(0)}]});
        let bytes = serde_json::to_vec_pretty(&v).unwrap();
        std::fs::write(out, &bytes).map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
        Ok((if ok { 0 } else { 1 }, bytes))
    }
}

pub struct VirtualClock {
    pub dir: Dir,
}

impl Clock for VirtualClock {
    fn now(&self) -> DateTime<Utc> {
        self.dir.load().clock
    }
    fn sleep(&self, seconds: u64) {
        std::thread::yield_now();
        self.dir
            .with(|s| s.clock += chrono::Duration::seconds(seconds as i64));
    }
}

pub struct ScriptPrompt {
    pub dir: Dir,
}

impl Prompt for ScriptPrompt {
    fn confirm(&self, summary: &str) -> bool {
        std::fs::write(self.dir.path.join("last-summary.txt"), summary).ok();
        self.dir.with(|s| {
            s.calls.push("confirm".into());
            s.script.confirm
        })
    }
}

// ── native hosts ─────────────────────────────────────────────────────────────

pub struct FakeHosts {
    pub dir: Dir,
    /// Identifies this controller invocation; a lease with another token
    /// is an orphan of a killed driver.
    pub token: String,
}

#[allow(private_interfaces)]
impl Hosts for FakeHosts {
    fn sbx_smoke_reconcile(&self, job: &super::runtime_smoke::sbx::SbxSmokeJob) -> Step {
        self.dir
            .call(format!("sbx-smoke-reconcile {}", job.binding.obligation));
        Ok(())
    }
    fn sbx_smoke_probe(
        &self,
        target: &super::runtime_smoke::sbx::Target,
        _min: &str,
    ) -> Step<super::runtime_smoke::sbx::CliIdentity> {
        let id = match target {
            super::runtime_smoke::sbx::Target::Local => "sbx.macos-arm64",
            super::runtime_smoke::sbx::Target::Ssh { .. } => "sbx.linux-x64",
        };
        let s = self.dir.load();
        if let Some(reason) = s.script.sbx_smoke_unavailable.get(id) {
            return Err(Stop::new("sbx-smoke-blocked", reason));
        }
        if let super::runtime_smoke::sbx::Target::Ssh { destination } = target {
            if s.script.unreachable.contains(destination) {
                return Err(Stop::new("sbx-smoke-blocked", "linux-host-unreachable"));
            }
        }
        self.sbx_smoke_identity(target)
    }
    fn sbx_smoke_identity(
        &self,
        _target: &super::runtime_smoke::sbx::Target,
    ) -> Step<super::runtime_smoke::sbx::CliIdentity> {
        let s = self.dir.load();
        Ok(super::runtime_smoke::sbx::CliIdentity {
            path: "/fake/sbx".into(),
            version: s.script.sbx_smoke_version.unwrap_or("0.54.1".into()),
            sha256: "e".repeat(64),
        })
    }
    fn sbx_smoke_start(
        &self,
        job: super::runtime_smoke::sbx::SbxSmokeJob,
    ) -> Step<std::thread::JoinHandle<Step<super::runtime_smoke::sbx::SbxSmokeRecord>>> {
        // Materialize synchronously: the virtual clock must not race directory mutations.
        let result = super::runtime_smoke::fake::run(self, &job);
        Ok(std::thread::spawn(move || result))
    }
    fn sbx_smoke_verify(
        &self,
        job: &super::runtime_smoke::sbx::SbxSmokeJob,
    ) -> Step<super::runtime_smoke::sbx::SbxSmokeRecord> {
        super::runtime_smoke::fake::verify(job)
    }
    fn sbx_qualification(&self) -> Step<super::sbx::Qualification> {
        let s = self.dir.load();
        if let Some(code) = s.script.sbx_unavailable {
            return Err(Stop::new(&code, "isolated SBX prerequisite missing"));
        }
        let changed =
            s.script.sbx_change_before_seal && s.calls.iter().any(|c| c == "sbx-qualification");
        self.dir.call("sbx-qualification".into());
        let cli_version = if changed {
            "0.54.2".into()
        } else {
            s.script.sbx_version.unwrap_or_else(|| "0.54.1".into())
        };
        let operator_label = s
            .script
            .sbx_label
            .unwrap_or_else(|| "release-test-account".into());
        Ok(super::sbx::Qualification {
            inputs_sha256: super::sbx::inputs_digest(&cli_version, &operator_label)?,
            cli_version,
            operator_label,
            cli_sha256: "a".repeat(64),
        })
    }
    fn host(&self, plan: &NativePlan, linux_host: Option<&str>) -> Step<Box<dyn NativeHost>> {
        let destination = if plan.host == "ssh" {
            linux_host.map(str::to_string).ok_or_else(|| {
                Stop::new(
                    "linux-host-missing",
                    format!("{} needs LINUX_HOST", plan.id),
                )
            })?
        } else {
            "local".into()
        };
        Ok(Box::new(FakeHost {
            dir: self.dir.clone(),
            destination,
            token: self.token.clone(),
        }))
    }
}

pub struct FakeHost {
    pub dir: Dir,
    pub destination: String,
    pub token: String,
}

impl NativeHost for FakeHost {
    fn cancel(&self, job: &NativeJob, _world: &World) -> Step {
        self.dir.call(format!("native-cancel {}", job.plan.id));
        for name in ["lease.json", "intent.json"] {
            let _ = std::fs::remove_file(job.out_dir.join(name));
        }
        Ok(())
    }
    fn describe(&self) -> String {
        self.destination.clone()
    }

    fn probe(&self, plan: &NativePlan) -> Step {
        let s = self.dir.load();
        if plan.id == crate::manifest::release_policy::SBX_NATIVE {
            if let Some(code) = s.script.sbx_unavailable {
                return Err(Stop::new(&code, "isolated SBX prerequisite missing"));
            }
        }
        if self.destination == "local" {
            if let Some(why) = s.script.mac_unavailable {
                return Err(Stop::new("mac-host", why));
            }
            return Ok(());
        }
        if s.script.unreachable.contains(&self.destination) {
            return Err(Stop::new(
                "linux-host-unreachable",
                format!("ssh {} timed out after 8 s", self.destination),
            ));
        }
        self.dir
            .call(format!("probe {} {}", self.destination, plan.id));
        Ok(())
    }

    fn start(&self, job: &NativeJob, world: &World) -> Step {
        let id = &job.plan.id;
        if super::native::durable(job).is_some() {
            return Ok(());
        }
        let lease = job.out_dir.join("lease.json");
        if let Ok(l) = read_json::<Value>(&lease) {
            if l["token"] == self.token.as_str() {
                return Ok(());
            }
        }
        if lease.exists() {
            // A previous driver died mid-run: its orphan is owned (recorded
            // identity), so it is stopped and only this check reruns.
            self.dir.call(format!("native-orphan-stopped {id}"));
            let _ = std::fs::remove_file(&lease);
        }
        let s = self.dir.load();
        if self.destination != "local" && s.script.unreachable.contains(&self.destination) {
            return Err(Stop::new(
                "linux-host-unreachable",
                format!("ssh {} timed out after 8 s", self.destination),
            ));
        }
        let run_s = s.script.native.get(id).map(|n| n.run_s).unwrap_or(0);
        let done = world.clock.now() + chrono::Duration::seconds(run_s as i64);
        std::fs::write(
            &lease,
            json!({"allocation": "fake", "token": self.token, "done": done.to_rfc3339()})
                .to_string(),
        )
        .map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
        self.dir
            .call(format!("native-run {id} on {}", self.destination));
        Ok(())
    }

    fn poll(&self, job: &NativeJob, world: &World) -> Step<Option<(PathBuf, i32)>> {
        let id = &job.plan.id;
        if let Some(done) = super::native::durable(job) {
            return Ok(Some(done));
        }
        let lease = job.out_dir.join("lease.json");
        let l: Value = read_json(&lease)
            .map_err(|_| Stop::new("native-blocked", format!("{id} has no running check")))?;
        let done = l["done"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_default();
        if world.clock.now() < done {
            return Ok(None);
        }
        match self.dir.fault(&format!("native_run:{id}")).as_deref() {
            Some("crash") => return Err(Stop::new("killed", format!("driver killed during {id}"))),
            Some("error") => {
                let _ = std::fs::remove_file(&lease);
                return Err(Stop::new(
                    "linux-host-unreachable",
                    format!("connection to {} lost during {id}", self.destination),
                ));
            }
            _ => {}
        }
        let script = self
            .dir
            .load()
            .script
            .native
            .get(id)
            .cloned()
            .unwrap_or_default();
        let outcome = script.outcome.unwrap_or_else(|| "passed".into());
        let mut tests: Vec<Value> = job.plan.tests.iter().map(|t| json!({"id": t, "outcome": if outcome == "failed" { "failed" } else { "passed" }, "duration_ms": 1})).collect();
        if script.omit_test {
            tests.pop();
        }
        let exit = match outcome.as_str() {
            "passed" => 0,
            "failed" => 1,
            _ => 2,
        };
        let kit: Value = read_json(&job.kit_dir.join("kit.json"))?;
        let expected = fake_inventory(&job.plan.tests.iter().map(|t| json!(t)).collect::<Vec<_>>());
        write_json(&job.out_dir.join("expected.json"), &expected)?;
        let mut xml = String::from("<testsuites><testsuite>");
        for t in &tests {
            let (class, name) = t["id"].as_str().unwrap().split_once(' ').unwrap();
            xml.push_str(&format!(
                "<testcase classname=\"{class}\" name=\"{name}\" time=\"0.001\"/>"
            ));
        }
        xml.push_str("</testsuite></testsuites>");
        std::fs::write(job.out_dir.join("junit.xml"), xml).map_err(anyhow::Error::from)?;
        std::fs::write(job.out_dir.join("runner.log"), "fake native execution\n")
            .map_err(anyhow::Error::from)?;
        let mut body = json!({"schema": 1, "native_id": id, "commit": job.commit, "tree": job.tree,
            "artifact_sha256": job.asset_sha256, "kit_sha256": job.kit_sha256, "outcome": outcome,
            "reason": script.reason_code.as_ref().map(|c| format!("injected {c}")), "reason_code": script.reason_code,
            "tests": tests, "host": {"os": if job.plan.host == "local" {"macos"} else {"linux"}, "os_build": "fake-1", "arch": if job.plan.host == "local" {"aarch64"} else {"x86_64"}},
            "manifest_sha256":job.manifest_sha256,"plan_sha256":job.plan_sha256,"policy_sha256":job.policy_sha256,
            "fixtures":kit["fixtures"],"previous_release":kit["previous_release"],"command_exit":exit,
            "env_contract_sha256":sha256_file(&job.kit_dir.join("capabilities.json"))?,
            "expected_inventory_sha256":sha256_file(&job.out_dir.join("expected.json"))?,
            "junit_sha256":sha256_file(&job.out_dir.join("junit.xml"))?,
            "runner_log_sha256":sha256_file(&job.out_dir.join("runner.log"))?,
            "started": world.clock.now().to_rfc3339(), "finished": world.clock.now().to_rfc3339()});
        if id == crate::manifest::release_policy::SBX_NATIVE && outcome == "passed" {
            let q = world.hosts.sbx_qualification()?;
            super::sbx::allocate(&job.out_dir, &job.out_dir)?;
            let owned: Value = read_json(&job.out_dir.join("sbx-owned.json"))?;
            for (agent, test, disposition, kit) in crate::manifest::release_policy::SBX_CASES {
                write_json(
                    &job.out_dir.join(format!("sbx-smoke.{agent}.json")),
                    &json!({"schema":1,"test":format!("awman-cli::artifact sbx_smoke::{test}"),"agent":agent,"disposition":disposition,"workload":if *disposition == "owned-workload" { Value::Null } else { json!(agent) },"kit_handle":kit,"kit_digest":"b".repeat(64),"sandbox":owned["cases"].as_array().unwrap().iter().find(|c| c["agent"] == *agent).unwrap()["sandbox"],"sbx_cli":q.cli_version,"inspect_sha256":null,"kits":null,"ready_exit":0,"task_exit":0,"output_bytes":7,"removed":true}),
                )?;
            }
            body["sbx"] = super::sbx::collect(&job.out_dir, &q)?;
        }
        if [
            crate::manifest::release_policy::APPLE_NATIVE,
            crate::manifest::release_policy::DOCKER_NATIVE,
        ]
        .contains(&id.as_str())
        {
            super::runtime_smoke::fake::native(job, &mut body)?;
        }
        let result = job.out_dir.join("result.json");
        std::fs::write(&result, serde_json::to_vec_pretty(&body).unwrap())
            .map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
        std::fs::write(job.out_dir.join("exit"), exit.to_string())
            .map_err(|e| Stop::new("state-corrupt", e.to_string()))?;
        let _ = std::fs::remove_file(&lease);
        Ok(Some((result, exit)))
    }
}

/// Assemble a `World` over a world directory.
pub fn world(path: &Path, interrupted: std::sync::Arc<std::sync::atomic::AtomicBool>) -> World {
    let dir = Dir {
        path: path.to_path_buf(),
    };
    World {
        forge: Box::new(DirForge { dir: dir.clone() }),
        source: Box::new(FakeSource { dir: dir.clone() }),
        tools: Box::new(FakeTools { dir: dir.clone() }),
        clock: Box::new(VirtualClock { dir: dir.clone() }),
        prompt: Box::new(ScriptPrompt { dir: dir.clone() }),
        hosts: Box::new(FakeHosts {
            dir,
            token: super::util::random_id().unwrap_or_default(),
        }),
        interrupted,
        poll_seconds: 30,
        production: false,
    }
}

/// A complete, valid starting world: canonical repo, `main` at M, a
/// prepared `release/<version>` source commit R on top of it, the plan,
/// active workflows and public pinned inputs.
pub fn seed(path: &Path, version: &str, plan: Value) -> Store {
    let dir = Dir {
        path: path.to_path_buf(),
    };
    std::fs::create_dir_all(path.join("blobs")).unwrap();
    let m = "1".repeat(40);
    let r = "2".repeat(40);
    let semver = version.trim_start_matches('v');
    let files = BTreeMap::from([
        (
            "Cargo.toml".to_string(),
            format!("[workspace.package]\nversion = \"{semver}\"\n"),
        ),
        (
            "Cargo.lock".to_string(),
            format!("[[package]]\nname = \"awman\"\nversion = \"{semver}\"\n"),
        ),
        (
            format!("docs/releases/{version}.md"),
            format!("# awman {version}\n\n- Changes.\n"),
        ),
    ]);
    let mut s = Store {
        repo: "example/awman".into(),
        repo_id: 42,
        default_branch: "main".into(),
        clock: DateTime::parse_from_rfc3339("2026-11-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        next_id: 1000,
        source: SourceState {
            origin: "example/awman".into(),
            head: r.clone(),
            branch: format!("release/{version}"),
            clean: true,
            remote_only: Default::default(),
            commits: BTreeMap::from([
                (
                    m.clone(),
                    Commit {
                        tree: "a".repeat(40),
                        parents: vec![],
                        files: BTreeMap::new(),
                    },
                ),
                (
                    r.clone(),
                    Commit {
                        tree: "b".repeat(40),
                        parents: vec![m.clone()],
                        files,
                    },
                ),
            ]),
            config: BTreeMap::new(),
        },
        plans: BTreeMap::from([("default".to_string(), plan.clone())]),
        refs: BTreeMap::from([("heads/main".to_string(), m)]),
        workflows: BTreeMap::from([
            ("ci.yml".to_string(), true),
            ("release.yml".to_string(), true),
        ]),
        script: Script {
            confirm: true,
            ci: RunScript {
                queue_s: 10,
                run_s: 720,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    for (asset, secs) in [
        ("awman-linux-amd64", 600),
        ("awman-linux-arm64", 540),
        ("awman-macos-arm64", 1080),
        ("awman-windows-amd64.exe", 900),
    ] {
        s.script.builds.insert(
            asset.into(),
            RunScript {
                queue_s: 15,
                run_s: secs,
                ..Default::default()
            },
        );
    }
    for n in plan["native"].as_array().cloned().unwrap_or_default() {
        for f in n["fixtures"].as_array().cloned().unwrap_or_default() {
            let body = format!("fixture {}", f["id"].as_str().unwrap_or(""));
            let blob = dir.put_blob(body.as_bytes());
            s.public
                .insert(f["url"].as_str().unwrap_or("").into(), blob);
        }
        if let Some(p) = n["previous_release"].as_object() {
            let blob = dir.put_blob(b"previous release");
            s.public
                .insert(p["url"].as_str().unwrap_or("").into(), blob);
        }
    }
    // Fixture pins in the plan must match the public bytes above.
    let mut plan = plan;
    if let Some(natives) = plan["native"].as_array_mut() {
        for n in natives {
            if let Some(fx) = n["fixtures"].as_array_mut() {
                for f in fx {
                    f["sha256"] =
                        sha256(format!("fixture {}", f["id"].as_str().unwrap_or("")).as_bytes())
                            .into();
                }
            }
            if n["previous_release"].is_object() {
                n["previous_release"]["sha256"] = sha256(b"previous release").into();
            }
        }
    }
    s.plans.insert("default".into(), plan);
    s.native_defaults();
    dir.save(&s);
    s
}

impl Store {
    fn native_defaults(&mut self) {
        let natives = self.plans["default"]["native"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for n in natives {
            self.script
                .native
                .entry(n["id"].as_str().unwrap_or("").into())
                .or_insert(NativeScript {
                    run_s: 240,
                    ..Default::default()
                });
        }
    }
}
