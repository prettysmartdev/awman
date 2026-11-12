//! Candidate inputs: everything that, when changed, must invalidate a
//! candidate's evidence (WI 5.2, design 16.1). Computed from the candidate
//! checkout, never from the controller's own tree, and hashed as canonical
//! JSON with explicit nulls and a format version.

use crate::evidence::digest;
use crate::manifest::Manifest;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// The shipping entitlement file; the only one the release binary is signed with.
pub const ENTITLEMENTS: &str = "packaging/macos/awman.entitlements";
/// The signing wrapper; its bytes are a signing input.
pub const SIGN_SCRIPT: &str = "tools/macos-sign.sh";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetInputs {
    pub asset: String,
    pub build: String,
    pub triple: String,
    pub cargo_command: Vec<String>,
    pub profile: String,
    pub runner_image: String,
    pub runs_on: Vec<String>,
    pub sign: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateInputs {
    pub schema: u32,
    pub version: String,
    pub commit: String,
    pub tree: Option<String>,
    pub manifest_version: String,
    pub cargo_lock: String,
    pub cargo_manifests: BTreeMap<String, String>,
    pub rust_toolchain_file: Option<String>,
    pub cargo_config: Option<String>,
    pub build_scripts: BTreeMap<String, String>,
    pub toolchain: String,
    pub payloads: BTreeMap<String, String>,
    pub fixtures: BTreeMap<String, Option<String>>,
    pub previous_release: BTreeMap<String, Option<String>>,
    pub entitlements_sha256: Option<String>,
    pub signing_script_sha256: Option<String>,
    pub signing_command: Vec<String>,
    pub release_notes_sha256: Option<String>,
    pub targets: Vec<TargetInputs>,
}

impl CandidateInputs {
    /// The candidate input digest every evidence record is bound to.
    pub fn digest(&self) -> Result<String> {
        digest::input_digest(self)
    }

    /// One target's input digest: the global inputs plus that target only.
    pub fn target_digest(&self, asset: &str) -> Result<String> {
        let target = self
            .targets
            .iter()
            .find(|t| t.asset == asset)
            .with_context(|| format!("no release target for asset {asset}"))?;
        let mut global = self.clone();
        global.targets.clear();
        digest::input_digest(&serde_json::json!({
            "schema": 1, "global": global.digest()?, "target": target
        }))
    }

    /// Names of the top-level inputs that differ, for an actionable message.
    pub fn diff(&self, other: &CandidateInputs) -> Vec<String> {
        let a = serde_json::to_value(self).unwrap_or_default();
        let b = serde_json::to_value(other).unwrap_or_default();
        let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else {
            return vec!["inputs".into()];
        };
        let mut keys: Vec<String> = a.keys().chain(b.keys()).cloned().collect();
        keys.sort();
        keys.dedup();
        keys.into_iter().filter(|k| a.get(k) != b.get(k)).collect()
    }

    /// Pins that are still pending. A real or rehearsal candidate is blocked
    /// while any remains: no fake digest can stand in for a fixture.
    pub fn pending_pins(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (k, v) in &self.fixtures {
            if v.is_none() {
                out.push(format!("fixture {k} has no published digest"));
            }
        }
        for (k, v) in &self.previous_release {
            if v.is_none() {
                out.push(format!(
                    "previous release for {k} has no authenticated digest"
                ));
            }
        }
        if self.entitlements_sha256.is_none() {
            out.push(format!("{ENTITLEMENTS} is missing"));
        }
        if self.release_notes_sha256.is_none() {
            out.push(format!("docs/releases/{}.md is missing", self.version));
        }
        out
    }
}

fn file_digest(root: &Path, rel: &str) -> Result<Option<String>> {
    let path = root.join(rel);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(digest::sha256_file(&path)?))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let ty = entry.file_type()?;
        if ty.is_dir() {
            walk(&path, out)?;
        } else if ty.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Cargo features of a shipping build. Release builds enable only the
/// shipping runtime, never test drivers.
pub fn release_features(build_features: &str) -> Vec<String> {
    match build_features {
        "builtin" => vec!["--features".into(), "builtin-runtime".into()],
        _ => Vec::new(),
    }
}

/// The one optimized build command for a release target.
pub fn cargo_command(triple: &str, build_features: &str) -> Vec<String> {
    let mut cmd: Vec<String> = [
        "cargo",
        "build",
        "-p",
        "awman-cli",
        "--bin",
        "awman",
        "--release",
        "--locked",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    cmd.extend(release_features(build_features));
    cmd.extend(["--target".to_string(), triple.to_string()]);
    cmd
}

/// Release targets in manifest asset order. Fails when the build cells of
/// kind `release` do not cover exactly `release.assets`.
pub fn targets(m: &Manifest) -> Result<Vec<TargetInputs>> {
    let mut by_asset = BTreeMap::new();
    for (id, b) in &m.builds {
        if b.kind != "release" {
            continue;
        }
        let asset = b
            .asset
            .clone()
            .with_context(|| format!("release build {id} names no asset"))?;
        let p = m
            .platforms
            .get(&b.platform)
            .with_context(|| format!("release build {id}: unknown platform {}", b.platform))?;
        let triple = b.target.clone().unwrap_or_else(|| p.triple.clone());
        let t = TargetInputs {
            asset: asset.clone(),
            build: id.clone(),
            cargo_command: cargo_command(&triple, &b.features),
            triple: triple.clone(),
            profile: b.profile.clone(),
            runner_image: p.image.clone(),
            runs_on: p.runs_on.clone(),
            sign: triple.ends_with("apple-darwin"),
        };
        anyhow::ensure!(
            by_asset.insert(asset.clone(), t).is_none(),
            "two release builds produce {asset}"
        );
    }
    let built: Vec<&String> = by_asset.keys().collect();
    let mut wanted: Vec<&String> = m.release.assets.iter().collect();
    wanted.sort();
    anyhow::ensure!(
        built == wanted,
        "release target set {built:?} differs from release.assets {wanted:?}"
    );
    Ok(m.release
        .assets
        .iter()
        .map(|a| by_asset.remove(a).expect("checked above"))
        .collect())
}

/// Compute the inputs of a candidate from the checkout at `src`.
pub fn compute(src: &Path, m: &Manifest, version: &str, commit: &str) -> Result<CandidateInputs> {
    let mut cargo_manifests = BTreeMap::new();
    let mut build_scripts = BTreeMap::new();
    let mut files = Vec::new();
    for dir in ["crates", "xtask"] {
        walk(&src.join(dir), &mut files)?;
    }
    files.push(src.join("Cargo.toml"));
    files.push(src.join("build.rs"));
    for f in files {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let r = rel(src, &f);
        if r.contains("/target/") || !f.is_file() {
            continue;
        }
        if name == "Cargo.toml" {
            cargo_manifests.insert(r, digest::sha256_file(&f)?);
        } else if name == "build.rs" {
            build_scripts.insert(r, digest::sha256_file(&f)?);
        }
    }
    let mut payloads = BTreeMap::new();
    let mut payload_files = Vec::new();
    walk(&src.join("third_party/msb-payloads"), &mut payload_files)?;
    for f in payload_files {
        payloads.insert(rel(src, &f), digest::sha256_file(&f)?);
    }
    let fixtures = m
        .fixtures
        .iter()
        .filter(|(_, f)| f.verified_record.is_none())
        .map(|(k, f)| (k.clone(), f.sha256.clone()))
        .collect();
    let previous_release = m
        .previous_release
        .iter()
        .map(|(k, p)| (k.clone(), p.sha256.clone()))
        .collect();
    let targets = targets(m)?;
    let toolchain = targets
        .iter()
        .filter_map(|t| m.builds.get(&t.build))
        .filter_map(|b| m.platforms.get(&b.platform))
        .map(|p| p.toolchain.clone())
        .next()
        .unwrap_or_default();
    let tree = std::process::Command::new("git")
        .arg("-C")
        .arg(src)
        .args(["rev-parse", &format!("{commit}^{{tree}}")])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    Ok(CandidateInputs {
        schema: 1,
        version: version.into(),
        commit: commit.into(),
        tree,
        manifest_version: m.version().into(),
        cargo_lock: file_digest(src, "Cargo.lock")?.context("Cargo.lock is missing")?,
        cargo_manifests,
        rust_toolchain_file: file_digest(src, "rust-toolchain.toml")?,
        cargo_config: file_digest(src, ".cargo/config.toml")?,
        build_scripts,
        toolchain,
        payloads,
        fixtures,
        previous_release,
        entitlements_sha256: file_digest(src, ENTITLEMENTS)?,
        signing_script_sha256: file_digest(src, SIGN_SCRIPT)?,
        signing_command: vec!["bash".into(), SIGN_SCRIPT.into(), "<asset>".into()],
        release_notes_sha256: file_digest(src, &format!("docs/releases/{version}.md"))?,
        targets,
    })
}

/// The release policy inputs that must still match the protected main tip at
/// promotion: manifest, notes and entitlements (design 16.1). Lockfile and
/// toolchain are bound to the commit and need no comparison with main.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub manifest_version: String,
    pub release_notes_sha256: Option<String>,
    pub entitlements_sha256: Option<String>,
}

impl Policy {
    pub fn of(inputs: &CandidateInputs) -> Policy {
        Policy {
            manifest_version: inputs.manifest_version.clone(),
            release_notes_sha256: inputs.release_notes_sha256.clone(),
            entitlements_sha256: inputs.entitlements_sha256.clone(),
        }
    }
    /// Policy as it is now on a checkout of the main tip.
    pub fn current(main: &Path, version: &str) -> Result<Policy> {
        let manifest = crate::manifest::load(main)?;
        Ok(Policy {
            manifest_version: manifest.version().into(),
            release_notes_sha256: file_digest(main, &format!("docs/releases/{version}.md"))?,
            entitlements_sha256: file_digest(main, ENTITLEMENTS)?,
        })
    }
}
