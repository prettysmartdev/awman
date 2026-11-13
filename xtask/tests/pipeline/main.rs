//! xtask `pipeline` integration tests (tier component).
//!
//! Owner: workflow step `candidate-promote-pipeline`.
//!
//! Each test drives the real `xtask release candidate|promote|verify|status|
//! abandon` commands against a directory store (`--store DIR`) that holds
//! release/draft/tag/run/artifact state the way GitHub does, with one-shot
//! fault injection. Nothing contacts a service and nothing is compiled: the
//! build step's outputs are written the way the native build jobs write them.
//! These are the recovery cases of WI 5.5 / design 12.4.

use awman_test_support::{Exit, TestCase, TestSandbox};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const VERSION: &str = "v0.14.0";
const C1: &str = "1111111111111111111111111111111111111111";
const C2: &str = "2222222222222222222222222222222222222222";
const REPO: &str = "example/awman";

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn sha_file(p: &Path) -> String {
    sha(&std::fs::read(p).unwrap())
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
    outputs: BTreeMap<String, String>,
}

impl Out {
    fn succeeded(&self) -> &Self {
        assert_eq!(
            self.code, 0,
            "stdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
        self
    }
    fn stopped(&self, prefix: &str) -> &Self {
        assert_eq!(
            self.code, 1,
            "stdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
        assert!(
            self.stderr.contains(prefix),
            "expected {prefix}:\n{}",
            self.stderr
        );
        self
    }
}

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    src: PathBuf,
    store: PathBuf,
    work: PathBuf,
    manifest: toml::Value,
    step: u32,
}

/// The repository manifest with every pending pin filled, so preflight is
/// not blocked (the unmodified manifest is tested separately).
fn pinned_manifest(text: &str) -> String {
    text.replace(
        "sha256 = {  }\npending = \"ci-workflows must supply verified per-platform installer digests before required use\"",
        &format!("sha256 = {{ x86_64-unknown-linux-gnu = \"{}\" }}", "a".repeat(64)),
    )
    .replace(
        "pending = \"P7: slice-guest-and-artifact-tiers publishes immutable archive and digest\"",
        &format!("sha256 = \"{}\"", "e".repeat(64)),
    )
    .replace(
        "pending = \"P7: authenticated published asset digest required\"",
        &format!("sha256 = \"{}\"", "f".repeat(64)),
    )
    .replace("image = \"pending-provisioned-image-digest\"", "image = \"provisioned-2026-11\"")
}

fn repo_root() -> PathBuf {
    // Runtime value first: a test binary reused from a target/ shared with a
    // checkout at another path still carries that path at compile time.
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    manifest_dir.parent().unwrap().to_path_buf()
}

impl World {
    fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let (src, store, work) = (root.join("src"), root.join("store"), root.join("work"));
        for d in [&src, &store, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        let real = std::fs::read_to_string(repo_root().join("ci/gates.toml")).unwrap();
        let mut w = World {
            _tmp: tmp,
            root,
            src,
            store,
            work,
            manifest: toml::Value::Boolean(false),
            step: 0,
        };
        w.write_src("ci/gates.toml", &pinned_manifest(&real));
        w.write_src(
            "Cargo.toml",
            "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.14.0\"\n",
        );
        w.write_src("Cargo.lock", "version = 4\n");
        w.write_src(
            "crates/awman-cli/Cargo.toml",
            "[package]\nname = \"awman-cli\"\n",
        );
        w.write_src(
            &format!("docs/releases/{VERSION}.md"),
            "# Release v0.14.0\n\n## Changes\n\n- Things.\n",
        );
        w.write_src(
            "packaging/macos/awman.entitlements",
            "<plist>hypervisor</plist>\n",
        );
        w.write_src("tools/macos-sign.sh", "#!/usr/bin/env bash\n");
        w.write_src(
            "third_party/msb-payloads/manifest.toml",
            "status = \"verified\"\n",
        );
        std::fs::write(w.store.join("main"), C1).unwrap();
        std::fs::write(w.store.join("ancestors"), format!("{C1}\n")).unwrap();
        std::fs::write(w.store.join("repository-id"), "42").unwrap();
        w.reload();
        w
    }

    fn write_src(&mut self, rel: &str, text: &str) {
        let p = self.src.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
        if rel == "ci/gates.toml" {
            self.reload();
        }
    }

    fn reload(&mut self) {
        if let Ok(t) = std::fs::read_to_string(self.src.join("ci/gates.toml")) {
            self.manifest = toml::from_str(&t).unwrap();
        }
    }

    fn xtask(&mut self, run_id: u64, attempt: u32, args: &[&str]) -> Out {
        self.step += 1;
        let out_file = self.work.join(format!("outputs-{}", self.step));
        let mut sandbox =
            TestSandbox::for_test(TestCase::component("pipeline::controller_invocation"));
        sandbox.register_exe("xtask", Path::new(awman_test_support::cargo_bin!("xtask")));
        let output = sandbox
            .command(awman_test_support::cargo_bin!("xtask"))
            .args(args)
            .args(["--repo", REPO, "--store"])
            .arg(&self.store)
            .current_dir(&self.root)
            .env("GITHUB_RUN_ID", run_id.to_string())
            .env("GITHUB_RUN_ATTEMPT", attempt.to_string())
            .env("GITHUB_JOB", "test")
            .env("GITHUB_OUTPUT", &out_file)
            .env_remove("GITHUB_STEP_SUMMARY")
            .env_remove("GITHUB_REPOSITORY")
            .env("TMPDIR", &self.root)
            .run();
        sandbox.finish();
        let mut outputs = BTreeMap::new();
        if let Ok(text) = std::fs::read_to_string(&out_file) {
            for l in text.lines() {
                if let Some((k, v)) = l.split_once('=') {
                    outputs.insert(k.to_owned(), v.to_owned());
                }
            }
        }
        Out {
            code: match output.exit {
                Exit::Code(code) => code,
                _ => -1,
            },
            stdout: String::from_utf8_lossy(&output.stdout).into(),
            stderr: String::from_utf8_lossy(&output.stderr).into(),
            outputs,
        }
    }

    fn preflight(&mut self, run: u64, commit: &str, extra: &[&str]) -> Out {
        let out = self.work.join(format!("pf-{run}"));
        let src = self.src.clone();
        let mut args = vec![
            "release",
            "candidate",
            "preflight",
            "--version",
            VERSION,
            "--commit",
            commit,
            "--source",
            src.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        self.xtask(run, 1, &args)
    }

    fn pf(&self, run: u64) -> PathBuf {
        self.work.join(format!("pf-{run}/preflight.json"))
    }

    fn preflight_json(&self, run: u64) -> Value {
        serde_json::from_slice(&std::fs::read(self.pf(run)).unwrap()).unwrap()
    }

    /// What the native build jobs leave: bytes, build.json, sign.json (macOS),
    /// then the real `record` step. Also registers each attestation.
    fn build_all(&mut self, run: u64, seed: &str) {
        let pf = self.preflight_json(run);
        for t in pf["inputs"]["targets"].as_array().unwrap().clone() {
            let asset = t["asset"].as_str().unwrap();
            let dir = self.work.join(format!("build-{run}/{asset}"));
            std::fs::create_dir_all(&dir).unwrap();
            let unsigned = format!("binary {asset} {seed}");
            std::fs::write(dir.join(asset), &unsigned).unwrap();
            let target_digest = target_digest(&pf, asset);
            std::fs::write(
                dir.join("build.json"),
                serde_json::to_vec(
                    &json!({"asset": asset, "target": t["triple"], "build": t["build"],
                    "unsigned_sha256": sha(unsigned.as_bytes()), "bytes": unsigned.len(),
                    "toolchain": "rustc 1.94.0", "input_digest": target_digest,
                    "command": t["cargo_command"]}),
                )
                .unwrap(),
            )
            .unwrap();
            if t["sign"].as_bool().unwrap() {
                let signed = format!("{unsigned} + ad-hoc signature");
                std::fs::write(dir.join(asset), &signed).unwrap();
                std::fs::write(
                    dir.join("sign.json"),
                    serde_json::to_vec(&json!({"unsigned_sha256": sha(unsigned.as_bytes()),
                        "sha256": sha(signed.as_bytes()),
                        "entitlements_sha256": pf["inputs"]["entitlements_sha256"],
                        "command": ["bash", "tools/macos-sign.sh", asset]}))
                    .unwrap(),
                )
                .unwrap();
            }
            let pfp = self.pf(run);
            self.xtask(
                run,
                1,
                &[
                    "release",
                    "candidate",
                    "record",
                    "--asset",
                    asset,
                    "--preflight",
                    pfp.to_str().unwrap(),
                    "--out",
                    dir.to_str().unwrap(),
                    "--attestation",
                    "https://example.invalid/att",
                ],
            )
            .succeeded();
            let att = self.store.join("attestations");
            std::fs::create_dir_all(&att).unwrap();
            std::fs::write(att.join(sha_file(&dir.join(asset))), "attested").unwrap();
        }
    }

    fn assemble(&mut self, run: u64) -> Out {
        let (pf, input, out) = (
            self.pf(run),
            self.work.join(format!("build-{run}")),
            self.work.join(format!("asm-{run}")),
        );
        self.xtask(
            run,
            1,
            &[
                "release",
                "candidate",
                "assemble",
                "--preflight",
                pf.to_str().unwrap(),
                "--in",
                input.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ],
        )
    }

    /// A ci.yml push run on main for `commit` that uploaded passing main gate evidence.
    fn main_gate_run(&mut self, commit: &str, verdict: &str) {
        let manifest_version = sha_file(&self.src.join("ci/gates.toml"));
        let ev = json!({"schema": 1, "kind": "gate", "gate": "main", "commit": commit,
            "manifest_version": manifest_version, "plan_sha256": "p", "verdict": verdict,
            "cells": [], "missing": [], "errors": [], "release_critical": [], "quarantine": [],
            "overrides_applied": [], "queue_ms": 0, "execution_ms": 0, "p95_budget_minutes": null,
            "generated_at": "2026-11-11T00:00:00Z", "expires_at": "2099-01-01T00:00:00Z"});
        let file = self.work.join("gate-evidence.json");
        std::fs::write(&file, serde_json::to_vec(&ev).unwrap()).unwrap();
        put_run(
            &self.store,
            "ci.yml",
            50,
            &format!("CI {commit}"),
            "push",
            "success",
        );
        put_artifact(
            &self.store,
            &format!("gate-evidence-main-{commit}"),
            50,
            &[file],
        );
    }

    /// Cell evidence the artifact and guest jobs of `run` upload.
    fn cell_evidence(
        &mut self,
        run: u64,
        attempt: u32,
        mutate: &dyn Fn(&str, &mut Value),
    ) -> PathBuf {
        let pf = self.preflight_json(run);
        let rec: Value =
            serde_json::from_slice(&std::fs::read(self.state_asset_path(&pf)).unwrap()).unwrap();
        let dir = self.work.join(format!("ev-{run}-{attempt}"));
        let _ = std::fs::remove_dir_all(&dir);
        for (cell, suite, build, platform) in candidate_cells(&self.manifest) {
            let tier = self.manifest["suites"][&suite]["tier"]
                .as_str()
                .unwrap()
                .to_owned();
            let asset = self.manifest["builds"][&build]
                .get("asset")
                .and_then(|a| a.as_str())
                .map(str::to_owned);
            let artifacts: Vec<Value> = asset
                .iter()
                .filter_map(|a| {
                    rec["artifacts"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|x| x["name"] == *a)
                })
                .map(|a| json!({"name": a["name"], "sha256": a["sha256"], "bytes": a["bytes"]}))
                .collect();
            let mut tests: Vec<String> = self.manifest["release_critical"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| s["suite"].as_str() == Some(&suite))
                .flat_map(|s| {
                    s["tests"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|t| t.as_str().unwrap().to_owned())
                })
                .collect();
            if tests.is_empty() {
                tests.push(format!("awman-cli::{tier} {suite}::case"));
            }
            let results: Vec<Value> = tests
                .iter()
                .map(|t| {
                    json!({"test": t, "outcome": "passed", "duration_ms": 1, "classification": null,
                    "reason": null, "capabilities_lost": [], "assertions": ["contract"]})
                })
                .collect();
            let mut ev = json!({
                "schema": 1, "kind": "cell", "gate": "candidate", "cell": cell, "suite": suite,
                "tier": tier, "backend": self.manifest["suites"][&suite].get("backend"),
                "build": build, "platform": platform, "shard": {"index": 1, "of": 1},
                "commit": pf["commit"], "tree": "t", "manifest_version": pf["inputs"]["manifest_version"],
                "input_digest": "i", "inputs": {"schema": 1, "source_tree": "t", "cargo_manifests": {},
                    "cargo_lock": "l", "toolchain": "1.94.0", "tool_digests": {}, "rust_toolchain_file": null,
                    "cargo_config": "c", "build_scripts": {}, "payload_manifest": null, "features": [],
                    "target": "t", "profile": "test", "runner_image": "img", "fixtures": {},
                    "previous_release": null, "signing": null, "release_notes": null},
                "expected_set_sha256": "a".repeat(64), "archive_sha256": "b".repeat(64), "plan_sha256": rec["candidate_digest"],
                "artifacts": artifacts,
                "counts": {"expected": results.len(), "ran": results.len(), "passed": results.len(),
                    "failed": 0, "skipped": 0, "blocked": 0, "quarantined_failed": 0},
                "results": results, "release_critical": [], "outcome": "passed", "classification": null,
                "durations": {"queue_ms": 0, "setup_ms": 0, "fixtures_ms": 0, "run_ms": 0, "total_ms": 0,
                    "slowest": [], "compile_ms": null, "link_ms": null, "cache_restore_ms": null,
                    "previous_release_ms": null, "vm_boot_ms": null, "signing_ms": null, "upload_ms": null},
                "cache": null, "reproduce": "cargo xtask test run",
                "run": {"provider": "github-actions", "repository_id": "42",
                    "workflow_path": ".github/workflows/candidate.yml", "workflow_revision": "w",
                    "run_id": run, "attempt": attempt, "job": "artifact", "job_id": 9, "artifact_id": 10,
                    "artifact_service_digest": "c".repeat(64), "url": "u"},
                "started_at": "2026-11-11T00:00:00Z", "finished_at": "2026-11-11T00:10:00Z",
                "expires_at": "2099-01-01T00:00:00Z", "command_exit": 0,
                "phases": {"setup": "passed", "run": "passed", "sweep": "passed", "upload": "passed"},
                "capabilities_probed": []
            });
            ev["input_digest"] = json!(sha(&serde_json::to_vec(&ev["inputs"]).unwrap()));
            ev["capabilities_probed"] = json!(self.manifest["tiers"][&tier]["guarantees"]
                .as_array()
                .unwrap()
                .iter()
                .chain(
                    self.manifest["suites"][&suite]["requires"]
                        .as_array()
                        .unwrap()
                )
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>());
            mutate(&cell, &mut ev);
            let d = dir.join(cell.replace(['@', '+'], "_"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("evidence.json"), serde_json::to_vec(&ev).unwrap()).unwrap();
            // Independent fake service facts: identities come from the fixture's
            // job scheduler, never from the possibly-mutated producer JSON.
            let service = self.store.join("service-producers");
            std::fs::create_dir_all(&service).unwrap();
            use std::io::Write;
            writeln!(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(service.join(format!("{run}-{attempt}-9")))
                    .unwrap(),
                "{}",
                sha_file(&d.join("evidence.json"))
            )
            .unwrap();
        }
        dir
    }

    fn state_asset_path(&self, pf: &Value) -> PathBuf {
        let rel = draft(
            &self.store,
            pf["namespace"].as_str().unwrap(),
            pf["candidate"].as_u64().unwrap() as u32,
        )
        .expect("candidate draft");
        asset_path(&rel, "release-state.json").unwrap()
    }

    fn gate(&mut self, run: u64, attempt: u32, evidence: &Path) -> Out {
        let (pf, out, src) = (
            self.pf(run),
            self.work.join(format!("gate-{run}-{attempt}")),
            self.src.clone(),
        );
        let result = self.xtask(
            run,
            attempt,
            &[
                "release",
                "candidate",
                "gate",
                "--preflight",
                pf.to_str().unwrap(),
                "--evidence",
                evidence.to_str().unwrap(),
                "--main-checkout",
                src.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ],
        );
        let statement = out.join("validation.json");
        if result.code == 0 && statement.exists() {
            let attestations = self.store.join("attestations");
            std::fs::create_dir_all(&attestations).unwrap();
            std::fs::write(attestations.join(sha_file(&statement)), "trusted gate").unwrap();
        }
        result
    }

    fn promote(&mut self, run: u64, step: &str, candidate: u32, extra: &[&str]) -> Out {
        let (out, src) = (
            self.work.join(format!("promote-{run}-{step}")),
            self.src.clone(),
        );
        let c = candidate.to_string();
        let mut args = vec![
            "release",
            "promote",
            step,
            "--version",
            VERSION,
            "--candidate",
            &c,
            "--main-checkout",
            src.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        self.xtask(run, 1, &args)
    }

    fn status(&mut self) -> Value {
        let out = self.xtask(0, 0, &["release", "status", VERSION, "--json"]);
        out.succeeded();
        serde_json::from_str(&out.stdout).unwrap()
    }

    /// Candidate built and validated end to end for `commit` by run `run`.
    fn validated(&mut self, run: u64, commit: &str) {
        self.preflight(run, commit, &[]).succeeded();
        self.build_all(run, "a");
        self.assemble(run).succeeded();
        self.main_gate_run(commit, "passed");
        let ev = self.cell_evidence(run, 1, &|_, _| {});
        self.gate(run, 1, &ev).succeeded();
    }

    fn fault(&self, op: &str) {
        let p = self.store.join("faults");
        let mut t = std::fs::read_to_string(&p).unwrap_or_default();
        t.push_str(op);
        t.push('\n');
        std::fs::write(p, t).unwrap();
    }
}

fn target_digest(pf: &Value, asset: &str) -> String {
    // Mirrors CandidateInputs::target_digest: canonical JSON of the global
    // inputs digest and the target.
    let mut global = pf["inputs"].clone();
    global["targets"] = json!([]);
    let global_digest = sha(serde_json::to_string(&global).unwrap().as_bytes());
    let target = pf["inputs"]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["asset"] == asset)
        .unwrap();
    sha(
        serde_json::to_string(&json!({"schema": 1, "global": global_digest, "target": target}))
            .unwrap()
            .as_bytes(),
    )
}

fn candidate_cells(m: &toml::Value) -> Vec<(String, String, String, String)> {
    let mut out = Vec::new();
    for g in m["gates"]["candidate"]["cells"].as_array().unwrap() {
        for s in g["suites"].as_array().unwrap() {
            for b in g["builds"].as_array().unwrap() {
                let (s, b) = (s.as_str().unwrap(), b.as_str().unwrap());
                let platform = m["builds"][b]["platform"].as_str().unwrap();
                out.push((format!("{s}@{b}"), s.into(), b.into(), platform.into()));
            }
        }
    }
    out
}

fn put_run(store: &Path, workflow: &str, id: u64, title: &str, event: &str, conclusion: &str) {
    let p = store.join("runs").join(format!("{workflow}.json"));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut runs: Vec<Value> = std::fs::read(&p)
        .ok()
        .map(|b| serde_json::from_slice(&b).unwrap())
        .unwrap_or_default();
    runs.push(json!({"id": id, "title": title, "status": "completed", "conclusion": conclusion,
        "created_at": format!("2026-11-11T00:00:{:02}Z", id % 60), "head_branch": "main", "event": event,
        "url": format!("https://example.invalid/runs/{id}")}));
    std::fs::write(p, serde_json::to_vec(&runs).unwrap()).unwrap();
}

fn put_artifact(store: &Path, name: &str, run: u64, files: &[PathBuf]) {
    let p = store.join("next-id");
    let id: u64 = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1000);
    std::fs::write(&p, (id + 1).to_string()).unwrap();
    let dir = store.join("artifacts").join(id.to_string());
    std::fs::create_dir_all(dir.join("files")).unwrap();
    for f in files {
        std::fs::copy(f, dir.join("files").join(f.file_name().unwrap())).unwrap();
    }
    std::fs::write(
        dir.join("meta.json"),
        serde_json::to_vec(&json!({"id": id, "name": name, "run_id": run,
        "head_branch": "main", "expired": false, "digest": null}))
        .unwrap(),
    )
    .unwrap();
}

/// Release directories of the store.
fn releases(store: &Path) -> Vec<(PathBuf, Value)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(store.join("releases")) {
        for e in entries.flatten() {
            let v: Value =
                serde_json::from_slice(&std::fs::read(e.path().join("release.json")).unwrap())
                    .unwrap();
            out.push((e.path(), v));
        }
    }
    out
}

fn draft(store: &Path, namespace: &str, candidate: u32) -> Option<PathBuf> {
    let tag = if namespace == "production" {
        VERSION.to_owned()
    } else {
        format!("{namespace}/{VERSION}")
    };
    releases(store)
        .into_iter()
        .find(|(_, v)| {
            let name = v["name"].as_str().unwrap();
            v["tag"] == tag.as_str()
                && (name.ends_with(&format!("(candidate {candidate})"))
                    || (name == VERSION && v["draft"] == false))
        })
        .map(|(p, _)| p)
}

fn asset_path(release: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(release.join("assets"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .split_once('-')
                .map(|(_, n)| n == name)
                .unwrap_or(false)
        })
}

fn asset_names(release: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(release.join("assets"))
        .unwrap()
        .flatten()
        .map(|e| {
            e.file_name()
                .to_string_lossy()
                .split_once('-')
                .unwrap()
                .1
                .to_owned()
        })
        .collect();
    v.sort();
    v
}

fn tag(store: &Path) -> Option<String> {
    std::fs::read_to_string(store.join("tags").join(VERSION)).ok()
}

fn publish_receipt(w: &mut World, run: u64) -> Out {
    // What each native verify job leaves behind.
    let results = w.work.join(format!("verified-{run}"));
    std::fs::create_dir_all(&results).unwrap();
    let rel = draft(&w.store, "production", 1).unwrap();
    let rec: Value = serde_json::from_slice(
        &std::fs::read(asset_path(&rel, "release-state.json").unwrap()).unwrap(),
    )
    .unwrap();
    for a in rec["artifacts"].as_array().unwrap() {
        let mac = a["target"].as_str().unwrap().ends_with("apple-darwin");
        std::fs::write(
            results.join(format!("verified-{}.json", a["name"].as_str().unwrap())),
            serde_json::to_vec(&json!({"asset": a["name"], "sha256": a["sha256"], "sums_ok": true,
                "attestation_ok": true, "smoke_ok": true, "boot_ok": if mac { json!(true) } else { json!(null) }})).unwrap(),
        )
        .unwrap();
    }
    let out = w.work.join(format!("receipt-{run}"));
    let o = w.xtask(
        run,
        1,
        &[
            "release",
            "verify",
            "public",
            "--version",
            VERSION,
            "--candidate",
            "1",
            "--results",
            results.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
    );
    if o.code == 0 {
        put_run(
            &w.store,
            "promote.yml",
            run,
            "Release promote",
            "workflow_call",
            "success",
        );
        put_artifact(
            &w.store,
            &format!("release-verified-{VERSION}"),
            run,
            &[out.join("receipt.json")],
        );
    }
    o
}

// ── tests ────────────────────────────────────────────────────────────────────

#[test]
fn candidate_validates_promotes_and_verifies_the_same_bytes() {
    let mut w = World::new();
    assert_eq!(w.status()["state"], "none");
    let o = w.preflight(
        100,
        C1,
        &[
            "--operator-record",
            r#"{"local_tests":"skipped by operator"}"#,
        ],
    );
    o.succeeded();
    assert_eq!(o.outputs["decision"], "new");
    assert_eq!(o.outputs["candidate"], "1");
    w.build_all(100, "a");
    w.assemble(100).succeeded();
    let rel = draft(&w.store, "production", 1).unwrap();
    assert_eq!(
        asset_names(&rel),
        vec![
            "SHA256SUMS",
            "awman-linux-amd64",
            "awman-linux-arm64",
            "awman-macos-arm64",
            "awman-windows-amd64.exe",
            "release-state.json"
        ]
    );
    let built: BTreeMap<String, String> = [
        "awman-linux-amd64",
        "awman-linux-arm64",
        "awman-macos-arm64",
        "awman-windows-amd64.exe",
    ]
    .iter()
    .map(|a| {
        (
            a.to_string(),
            sha_file(&w.work.join(format!("build-100/{a}/{a}"))),
        )
    })
    .collect();
    let s = w.status();
    assert_eq!(
        (s["state"].as_str(), s["candidate"].as_u64()),
        (Some("candidate-built"), Some(1))
    );
    // The operator's skip is retained as advisory evidence only.
    let rec: Value = serde_json::from_slice(
        &std::fs::read(asset_path(&rel, "release-state.json").unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(rec["operator_record"]["local_tests"], "skipped by operator");
    // SHA256SUMS: 64 lowercase hex, two spaces, name.
    let sums = std::fs::read_to_string(asset_path(&rel, "SHA256SUMS").unwrap()).unwrap();
    for (name, digest) in &built {
        assert!(sums.contains(&format!("{digest}  {name}\n")));
    }

    w.main_gate_run(C1, "passed");
    let ev = w.cell_evidence(100, 1, &|_, _| {});
    w.gate(100, 1, &ev).succeeded();
    assert_eq!(w.status()["state"], "validated");

    w.promote(101, "verify-inputs", 1, &[]).succeeded();
    w.promote(101, "tag", 1, &[]).succeeded();
    assert_eq!(tag(&w.store).as_deref(), Some(C1));
    w.promote(101, "publish", 1, &[]).succeeded();
    let s = w.status();
    assert_eq!(
        (s["state"].as_str(), s["tag_on_origin"].as_bool()),
        (Some("promoted"), Some(true))
    );
    // Published bytes are the built bytes.
    for (name, digest) in &built {
        assert_eq!(
            &sha_file(&w.store.join("public").join(VERSION).join(name)),
            digest,
            "{name}"
        );
    }
    publish_receipt(&mut w, 101).succeeded();
    assert_eq!(w.status()["state"], "verified");
    // A second preflight for the published version starts nothing.
    let o = w.preflight(102, C1, &[]);
    o.succeeded();
    assert_eq!(o.outputs["decision"], "published");
}

#[test]
fn status_is_read_only() {
    let mut w = World::new();
    w.validated(100, C1);
    let snapshot = |store: &Path| {
        let mut files = Vec::new();
        fn walk(p: &Path, out: &mut Vec<(String, String)>) {
            for e in std::fs::read_dir(p).unwrap().flatten() {
                if e.path().is_dir() {
                    walk(&e.path(), out)
                } else {
                    out.push((e.path().display().to_string(), sha_file(&e.path())));
                }
            }
        }
        walk(store, &mut files);
        files.sort();
        files
    };
    let before = snapshot(&w.store);
    w.status();
    assert_eq!(before, snapshot(&w.store));
}

#[test]
fn failed_suite_reruns_against_the_same_artifacts_only() {
    let mut w = World::new();
    w.preflight(100, C1, &[]).succeeded();
    w.build_all(100, "a");
    w.assemble(100).succeeded();
    w.main_gate_run(C1, "passed");
    let rel = draft(&w.store, "production", 1).unwrap();
    let assets_before = asset_names(&rel)
        .iter()
        .filter(|n| n.starts_with("awman-"))
        .map(|n| sha_file(&asset_path(&rel, n).unwrap()))
        .collect::<Vec<_>>();
    let bad = w.cell_evidence(100, 1, &|cell, ev| {
        if cell.starts_with("artifact-smoke@linux-x64") {
            ev["results"][0]["outcome"] = json!("failed");
            ev["outcome"] = json!("failed");
        }
    });
    w.gate(100, 1, &bad)
        .stopped("FAILED: candidate gate failed");
    let s = w.status();
    assert_eq!(s["state"], "candidate-built");
    assert!(s["failing_gates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g["gate_id"] == "artifact-smoke@linux-x64-release" && g["outcome"] == "failed"));
    // `gh run rerun --failed`: attempt 2 of the same run, same draft bytes.
    let good = w.cell_evidence(100, 2, &|_, _| {});
    w.gate(100, 2, &good).succeeded();
    assert_eq!(w.status()["state"], "validated");
    let assets_after = asset_names(&rel)
        .iter()
        .filter(|n| n.starts_with("awman-"))
        .map(|n| sha_file(&asset_path(&rel, n).unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(assets_before, assets_after);
}

#[test]
fn runner_lost_mid_validation_is_blocked() {
    let mut w = World::new();
    w.preflight(100, C1, &[]).succeeded();
    w.build_all(100, "a");
    w.assemble(100).succeeded();
    w.main_gate_run(C1, "passed");
    let ev = w.cell_evidence(100, 1, &|cell, ev| {
        if cell.starts_with("guest-boot@") {
            ev["results"][0]["outcome"] = json!("blocked");
            ev["results"][0]["capabilities_lost"] = json!(["hypervisor"]);
            ev["counts"]["passed"] = json!(ev["counts"]["passed"].as_u64().unwrap() - 1);
            ev["counts"]["blocked"] = json!(1);
            ev["outcome"] = json!("blocked");
        }
    });
    w.gate(100, 1, &ev)
        .stopped("BLOCKED: candidate gate blocked");
    let s = w.status();
    assert!(s["failing_gates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g["outcome"] == "blocked"));
    w.promote(101, "verify-inputs", 1, &[]).stopped("FAILED");
}

#[test]
fn changed_inputs_never_reuse_stale_evidence() {
    for (what, rel, text) in [
        ("lockfile", "Cargo.lock", "version = 4\n# changed\n"),
        (
            "entitlement",
            "packaging/macos/awman.entitlements",
            "<plist>more</plist>\n",
        ),
        (
            "notes",
            "docs/releases/v0.14.0.md",
            "# Release v0.14.0\n\nOther notes.\n",
        ),
    ] {
        let mut w = World::new();
        w.validated(100, C1);
        w.write_src(rel, text);
        let o = w.preflight(110, C1, &[]);
        o.succeeded();
        assert_eq!(
            (
                o.outputs["decision"].as_str(),
                o.outputs["candidate"].as_str()
            ),
            ("new", "2"),
            "{what}"
        );
        if what != "lockfile" {
            // The main tip's policy no longer matches candidate 1.
            w.promote(111, "verify-inputs", 1, &[])
                .stopped("new candidate");
        }
    }
    // A manifest edit is a new identity too.
    let mut w = World::new();
    w.validated(100, C1);
    let t = std::fs::read_to_string(w.src.join("ci/gates.toml")).unwrap() + "\n# edited\n";
    w.write_src("ci/gates.toml", &t);
    assert_eq!(w.preflight(110, C1, &[]).outputs["decision"], "new");
    w.promote(111, "verify-inputs", 1, &[])
        .stopped("ci/gates.toml changed");
}

#[test]
fn same_identity_resumes_without_rebuilding() {
    let mut w = World::new();
    w.validated(100, C1);
    let o = w.preflight(120, C1, &[]);
    o.succeeded();
    assert_eq!(
        (
            o.outputs["decision"].as_str(),
            o.outputs["candidate"].as_str()
        ),
        ("resume", "1")
    );
    w.assemble(120).succeeded();
    assert_eq!(releases(&w.store).len(), 1);
}

#[test]
fn partial_asset_upload_resumes_without_duplicates() {
    let mut w = World::new();
    w.preflight(100, C1, &[]).succeeded();
    w.build_all(100, "a");
    w.fault("upload:awman-macos-arm64");
    let o = w.assemble(100);
    assert_eq!(o.code, 2, "{}", o.stderr);
    let rel = draft(&w.store, "production", 1).unwrap();
    assert!(!asset_names(&rel).contains(&"awman-macos-arm64".to_string()));
    assert_eq!(w.status()["state"], "prepared");
    // Rerun of the failed assemble job of the same run.
    w.assemble(100).succeeded();
    assert_eq!(releases(&w.store).len(), 1);
    assert_eq!(asset_names(&rel).len(), 6);
    assert_eq!(w.status()["state"], "candidate-built");
}

#[test]
fn interrupted_promotion_resumes_between_tag_and_publish() {
    let mut w = World::new();
    w.validated(100, C1);
    w.promote(101, "tag", 1, &[]).succeeded();
    w.fault("publish");
    let o = w.promote(101, "publish", 1, &[]);
    assert_eq!(o.code, 2, "{}", o.stderr);
    let s = w.status();
    assert_eq!(
        (s["state"].as_str(), s["tag_on_origin"].as_bool()),
        (Some("validated"), Some(true))
    );
    // Resume: the tag is present (not moved), then publish completes.
    let o = w.promote(102, "tag", 1, &[]);
    o.succeeded();
    assert!(o.stdout.contains("already on origin"));
    w.promote(102, "publish", 1, &[]).succeeded();
    assert_eq!(tag(&w.store).as_deref(), Some(C1));
    assert_eq!(w.status()["state"], "promoted");
    let o = w.promote(103, "publish", 1, &[]);
    o.succeeded();
    assert!(o.stdout.contains("already published"));
}

#[test]
fn tag_on_a_different_commit_fails_and_publishes_nothing() {
    let mut w = World::new();
    w.validated(100, C1);
    std::fs::create_dir_all(w.store.join("tags")).unwrap();
    std::fs::write(w.store.join("tags").join(VERSION), C2).unwrap();
    w.promote(101, "tag", 1, &[]).stopped("never moved");
    w.promote(101, "publish", 1, &[]).stopped("FAILED");
    assert!(releases(&w.store).iter().all(|(_, v)| v["draft"] == true));
    assert_eq!(tag(&w.store).as_deref(), Some(C2));
}

#[test]
fn concurrent_candidates_for_one_version_end_blocked_with_nothing_published() {
    let mut w = World::new();
    // Two operators start for the same version; both preflights see nothing.
    w.preflight(200, C1, &[]).succeeded();
    std::fs::write(w.store.join("main"), C2).unwrap();
    std::fs::write(w.store.join("ancestors"), format!("{C1}\n{C2}\n")).unwrap();
    w.preflight(300, C2, &[]).succeeded();
    assert_eq!(w.preflight_json(200)["candidate"], 1);
    assert_eq!(w.preflight_json(300)["candidate"], 1);
    w.build_all(200, "a");
    w.build_all(300, "b");
    w.assemble(200).succeeded();
    w.assemble(300).stopped("BLOCKED: candidate 1");
    assert_eq!(releases(&w.store).len(), 1);
    assert!(tag(&w.store).is_none());
    // A rerun allocates the next number and supersedes candidate 1.
    let o = w.preflight(301, C2, &[]);
    assert_eq!(o.outputs["candidate"], "2");
    w.build_all(301, "b");
    w.assemble(301).succeeded();
    let all = releases(&w.store);
    assert_eq!(all.len(), 1);
    assert!(all[0].1["name"]
        .as_str()
        .unwrap()
        .ends_with("(candidate 2)"));
    assert!(
        w.work
            .join("asm-301/superseded/c1/release-state.json")
            .exists(),
        "superseded evidence kept"
    );
}

#[test]
fn expired_evidence_is_refused() {
    let mut w = World::new();
    let t = std::fs::read_to_string(w.src.join("ci/gates.toml"))
        .unwrap()
        .replace("evidence_valid_days = 14", "evidence_valid_days = 0");
    w.write_src("ci/gates.toml", &t);
    w.preflight(100, C1, &[]).succeeded();
    w.build_all(100, "a");
    w.assemble(100).succeeded();
    w.main_gate_run(C1, "passed");
    let ev = w.cell_evidence(100, 1, &|_, _| {});
    w.gate(100, 1, &ev).stopped("expired");
    w.promote(101, "verify-inputs", 1, &[]).stopped("expired");
}

#[test]
fn commit_no_longer_an_ancestor_of_main_voids_the_candidate() {
    let mut w = World::new();
    w.validated(100, C1);
    // main was force-pushed past C1.
    std::fs::write(w.store.join("main"), C2).unwrap();
    std::fs::write(w.store.join("ancestors"), format!("{C2}\n")).unwrap();
    w.promote(101, "verify-inputs", 1, &[])
        .stopped("no longer the tip or an ancestor");
    w.promote(101, "tag", 1, &[]).stopped("FAILED");
    assert!(tag(&w.store).is_none());
    w.preflight(102, C1, &[])
        .stopped("neither the tip nor an ancestor");
}

#[test]
fn promotion_with_a_missing_gate_is_refused() {
    let mut w = World::new();
    w.preflight(100, C1, &[]).succeeded();
    w.build_all(100, "a");
    w.assemble(100).succeeded();
    w.promote(101, "verify-inputs", 1, &[])
        .stopped("not validated");
    // Without main evidence the gate is blocked, and promotion still refused.
    let ev = w.cell_evidence(100, 1, &|_, _| {});
    w.gate(100, 1, &ev).stopped("BLOCKED");
    w.promote(101, "verify-inputs", 1, &[]).stopped("FAILED");
    w.promote(101, "tag", 1, &[]).stopped("FAILED");
    assert!(tag(&w.store).is_none());
}

#[test]
fn forged_or_stale_evidence_cannot_validate() {
    let mut w = World::new();
    w.preflight(100, C1, &[]).succeeded();
    w.build_all(100, "a");
    w.assemble(100).succeeded();
    w.main_gate_run(C1, "passed");
    // Evidence produced by another run (or locally) is not authenticated.
    let ev = w.cell_evidence(100, 1, &|cell, ev| {
        if cell.starts_with("artifact-signed-boot@") {
            ev["run"]["run_id"] = json!(999);
        }
    });
    w.gate(100, 1, &ev).stopped("FAILED");
    // Evidence for other bytes is not bound to this candidate.
    let ev = w.cell_evidence(100, 2, &|cell, ev| {
        if cell.starts_with("artifact-entrypoints@windows") {
            ev["artifacts"][0]["sha256"] = json!("0".repeat(64));
        }
    });
    w.gate(100, 2, &ev).stopped("FAILED");
    assert_eq!(w.status()["state"], "candidate-built");
}

#[test]
fn override_on_a_not_overridable_cell_is_refused() {
    let mut w = World::new();
    let t = std::fs::read_to_string(w.src.join("ci/gates.toml")).unwrap().replace(
        "override = []",
        "override = [{ gate = \"candidate\", cell = \"artifact-signed-boot@macos-arm64-release-release\", \
         reason = \"no Mac\", expires = \"2099-01-01\", approved_by = \"@someone\" }]",
    );
    w.write_src("ci/gates.toml", &t);
    w.preflight(100, C1, &[]).stopped("override forbidden");
    assert!(releases(&w.store).is_empty());
}

#[test]
fn abandon_keeps_evidence_and_refuses_promoted_versions() {
    let mut w = World::new();
    w.validated(100, C1);
    let keep = w.work.join("kept");
    let o = w.xtask(
        0,
        0,
        &[
            "release",
            "abandon",
            VERSION,
            "--keep",
            keep.to_str().unwrap(),
        ],
    );
    o.succeeded();
    assert!(keep.join("release-state.json").exists());
    assert!(releases(&w.store).is_empty());
    put_run(
        &w.store,
        "candidate.yml",
        100,
        &format!("Release {VERSION} ({C1})"),
        "workflow_dispatch",
        "failure",
    );
    put_artifact(
        &w.store,
        &format!("candidate-record-{VERSION}-c1"),
        100,
        &[keep.join("release-state.json")],
    );
    let s = w.status();
    assert_eq!(
        (s["state"].as_str(), s["candidate"].as_u64()),
        (Some("abandoned"), Some(1))
    );
    // The next candidate never reuses number 1.
    assert_eq!(w.preflight(130, C1, &[]).outputs["candidate"], "2");

    let mut w = World::new();
    w.validated(100, C1);
    w.promote(101, "tag", 1, &[]).succeeded();
    w.promote(101, "publish", 1, &[]).succeeded();
    let o = w.xtask(0, 0, &["release", "abandon", VERSION]);
    o.stopped("never removed");
    assert_eq!(releases(&w.store).len(), 1);
}

#[test]
fn pending_pins_block_preflight_before_any_build() {
    let mut w = World::new();
    let real = std::fs::read_to_string(repo_root().join("ci/gates.toml")).unwrap();
    w.write_src("ci/gates.toml", &real);
    w.preflight(100, C1, &[])
        .stopped("BLOCKED: pins are not published yet");
    assert!(!w.pf(100).exists());
}

#[test]
fn preflight_refuses_bad_version_notes_and_triggers() {
    let mut w = World::new();
    w.write_src("Cargo.toml", "[workspace.package]\nversion = \"0.13.9\"\n");
    w.preflight(100, C1, &[])
        .stopped("Cargo.toml version is '0.13.9'");
    let mut w = World::new();
    w.write_src(
        &format!("docs/releases/{VERSION}.md"),
        "# Release\n\n_Write release notes here._\n",
    );
    w.preflight(100, C1, &[]).stopped("placeholder");
    let mut w = World::new();
    w.preflight(
        100,
        C1,
        &["--event", "pull_request", "--ref", "refs/pull/1/merge"],
    )
    .stopped("only from refs/heads/main");
    w.preflight(
        100,
        C1,
        &[
            "--event",
            "workflow_dispatch",
            "--ref",
            "refs/heads/feature",
        ],
    )
    .stopped("only from refs/heads/main");
    let t = std::fs::read_to_string(w.src.join("ci/gates.toml")).unwrap().replace(
        "assets = [\"awman-linux-amd64\", \"awman-linux-arm64\", \"awman-macos-arm64\", \"awman-windows-amd64.exe\"]",
        "assets = [\"awman-linux-amd64\", \"awman-linux-arm64\", \"awman-macos-arm64\"]",
    );
    w.write_src("ci/gates.toml", &t);
    w.preflight(100, C1, &[]).stopped("release target set");
}

#[test]
fn operator_journal_dispatch_records_and_allocates_nothing() {
    let mut w = World::new();
    let o = w.preflight(
        100,
        C1,
        &[
            "--operator-record",
            r#"{"record_only":true,"answers":[{"prompt":"retry","answer":"y"}]}"#,
        ],
    );
    o.succeeded();
    assert_eq!(o.outputs["decision"], "journal");
    assert!(w.work.join("pf-100/journal.json").exists());
    assert!(!w.pf(100).exists());
    assert!(releases(&w.store).is_empty());
}

#[test]
fn rehearsal_never_touches_production_and_cleans_up_its_own_draft() {
    let mut w = World::new();
    w.validated(100, C1);
    let o = w.preflight(400, C1, &["--dry-run", "--event", "schedule"]);
    o.succeeded();
    assert_eq!(o.outputs["namespace"], "rehearsal/400");
    w.build_all(400, "r");
    w.assemble(400).succeeded();
    assert_eq!(releases(&w.store).len(), 2);
    w.main_gate_run(C1, "passed");
    let ev = w.cell_evidence(400, 1, &|_, _| {});
    w.gate(400, 1, &ev).succeeded();
    // Dry promotion verifies everything, creates no tag, publishes nothing.
    let dry = ["--dry-run", "--namespace", "rehearsal/400"];
    w.promote(400, "verify-inputs", 1, &dry).succeeded();
    w.promote(400, "tag", 1, &dry).succeeded();
    w.promote(400, "publish", 1, &dry).succeeded();
    assert!(tag(&w.store).is_none());
    // Rehearsal evidence cannot promote production, and vice versa.
    w.promote(400, "verify-inputs", 1, &["--namespace", "rehearsal/400"])
        .stopped("FAILED");
    let pf = w.pf(400);
    w.xtask(
        400,
        1,
        &[
            "release",
            "candidate",
            "cleanup",
            "--preflight",
            pf.to_str().unwrap(),
        ],
    )
    .succeeded();
    let left = releases(&w.store);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].1["tag"], VERSION);
    assert_eq!(w.status()["state"], "validated");
}

#[test]
fn test_jobs_get_exact_bytes_from_run_artifacts_and_refuse_anything_else() {
    let mut w = World::new();
    w.preflight(100, C1, &[]).succeeded();
    w.build_all(100, "a");
    w.assemble(100).succeeded();
    let pf = w.pf(100);
    let record = w.work.join("asm-100/candidate-record.json");
    let fetch = |w: &mut World, input: &Path, record: &Path, out: &str| {
        let out = w.work.join(out);
        w.xtask(
            100,
            1,
            &[
                "release",
                "candidate",
                "fetch",
                "--preflight",
                pf.to_str().unwrap(),
                "--asset",
                "awman-linux-amd64",
                "--record",
                record.to_str().unwrap(),
                "--in",
                input.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ],
        )
    };
    let builds = w.work.join("build-100");
    let o = fetch(&mut w, &builds, &record, "fetched");
    o.succeeded();
    let rec: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    assert_eq!(
        o.outputs["sha256"],
        rec["artifacts"][0]["sha256"].as_str().unwrap()
    );
    assert!(std::fs::metadata(w.work.join("fetched/awman-linux-amd64"))
        .unwrap()
        .permissions()
        .readonly());
    // Other bytes under the same name are refused.
    let tampered = w.work.join("tampered");
    std::fs::create_dir_all(&tampered).unwrap();
    std::fs::write(tampered.join("awman-linux-amd64"), "rebuilt bytes").unwrap();
    fetch(&mut w, &tampered, &record, "fetched-2").stopped("never overridable");
    // A record of another candidate is refused.
    let mut other = rec.clone();
    other["candidate"] = json!(2);
    let other_path = w.work.join("other-record.json");
    std::fs::write(&other_path, serde_json::to_vec(&other).unwrap()).unwrap();
    fetch(&mut w, &builds, &other_path, "fetched-3").stopped("another candidate");
}

#[test]
fn mutable_draft_cannot_forge_or_replay_validation() {
    let mut w = World::new();
    w.validated(100, C1);
    let ev = w.cell_evidence(100, 2, &|_, e| {
        e["outcome"] = json!("failed");
    });
    assert_ne!(w.gate(100, 2, &ev).code, 0);
    let pf = w.preflight_json(100);
    let state = w.state_asset_path(&pf);
    let rel = draft(&w.store, "production", 1).unwrap();
    let gp = asset_path(&rel, "gate-evidence-candidate.json").unwrap();
    let mut g: Value = serde_json::from_slice(&std::fs::read(&gp).unwrap()).unwrap();
    g["verdict"] = json!("passed");
    g["errors"] = json!([]);
    g["missing"] = json!([]);
    g["plan_sha256"] = json!("EVIDENCE-FOR-ANOTHER-CANDIDATE");
    for c in g["cells"].as_array_mut().unwrap() {
        c["outcome"] = json!("passed");
    }
    for c in g["release_critical"].as_array_mut().unwrap() {
        c["outcome"] = json!("passed");
    }
    std::fs::write(&gp, serde_json::to_vec(&g).unwrap()).unwrap();
    let mut rec: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    rec["state"] = json!("validated");
    for g in rec["gates"].as_array_mut().unwrap() {
        g["verdict"] = json!("passed");
        g["run_id"] = json!(9999999);
        if g["id"] == "candidate" {
            g["gate_evidence_sha256"] = json!(sha_file(&gp));
        }
    }
    std::fs::write(&state, serde_json::to_vec(&rec).unwrap()).unwrap();
    let r = w.promote(101, "verify-inputs", 1, &[]);
    assert_ne!(r.code, 0, "forged validation accepted");
}

#[cfg(unix)]
#[test]
fn fetched_artifact_is_absolute_and_executable_from_another_directory() {
    use std::os::unix::fs::PermissionsExt;
    let mut w = World::new();
    w.validated(100, C1);
    let pf = w.pf(100);
    let o = w.xtask(
        100,
        1,
        &[
            "release",
            "candidate",
            "fetch",
            "--preflight",
            pf.to_str().unwrap(),
            "--asset",
            "awman-linux-amd64",
            "--out",
            "artifact",
        ],
    );
    o.succeeded();
    let p = std::path::PathBuf::from(&o.outputs["path"]);
    assert!(p.is_absolute());
    assert!(p.exists());
    let mode = std::fs::metadata(w.root.join(&p))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o100, 0);
    assert_eq!(mode & 0o222, 0);
    println!(
        "FETCH OUTPUT unusable after working-directory change: path={}, mode={:o}",
        p.display(),
        mode & 0o777
    );
}

#[test]
fn live_main_policy_wins_over_a_stale_checkout_before_tag_and_publish() {
    let mut w = World::new();
    w.validated(100, C1);
    let live = w.store.join("main-policy");
    for path in [
        "ci/gates.toml",
        "packaging/macos/awman.entitlements",
        "docs/releases/v0.14.0.md",
    ] {
        let dest = live.join(path);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(w.src.join(path), dest).unwrap();
    }
    w.promote(101, "tag", 1, &[]).succeeded();
    std::fs::write(
        live.join("docs/releases/v0.14.0.md"),
        "changed live main notes",
    )
    .unwrap();
    let result = w.promote(102, "publish", 1, &[]);
    assert_ne!(result.code, 0, "{}", result.stdout);
    assert!(
        result.stderr.contains("changed") || result.stderr.contains("policy"),
        "{}",
        result.stderr
    );
    assert!(releases(&w.store)
        .iter()
        .all(|(_, release)| release["draft"] == true));
}

#[test]
fn failed_commands_missing_shards_skipped_counts_and_empty_assertions_cannot_validate() {
    let mut w = World::new();
    w.preflight(100, C1, &[]).succeeded();
    w.build_all(100, "a");
    w.assemble(100).succeeded();
    w.main_gate_run(C1, "passed");
    let ev = w.cell_evidence(100, 1, &|_, e| {
        e["command_exit"] = json!(137);
        e["shard"] = json!({"index": 1, "of": 9});
        e["counts"]["expected"] = json!(10000);
        e["counts"]["skipped"] = json!(9999);
        e["run"]["job_id"] = json!(0);
        e["run"]["artifact_id"] = json!(0);
        for t in e["results"].as_array_mut().unwrap() {
            t["assertions"] = json!([]);
        }
    });
    let result = w.gate(100, 1, &ev);
    assert_ne!(result.code, 0, "{}", result.stdout);
    assert!(
        result.stderr.contains("command exit") || result.stderr.contains("shards"),
        "{}",
        result.stderr
    );
    assert_ne!(w.promote(101, "verify-inputs", 1, &[]).code, 0);
}

#[test]
fn mutable_journal_cannot_extend_authorization_or_invent_gate_runs() {
    for field in ["expiry", "run"] {
        let mut w = World::new();
        w.validated(100, C1);
        let pf = w.preflight_json(100);
        let state = w.state_asset_path(&pf);
        let mut record: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
        if field == "expiry" {
            record["expires_at"] = json!("2099-01-01T00:00:00Z");
        } else {
            record["gates"][0]["run_id"] = json!(999999);
        }
        std::fs::write(state, serde_json::to_vec(&record).unwrap()).unwrap();
        w.promote(101, "verify-inputs", 1, &[])
            .stopped("validation statement");
    }
}
