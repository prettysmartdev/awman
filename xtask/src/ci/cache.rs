//! Cache keys and statistics. A key covers every input that can change what a
//! cached `target/` or registry means, and is used WITHOUT `restore-keys`, so a
//! toolchain or lockfile change can never be served a stale prefix match.

use super::args::Args;
use crate::manifest::{self, Manifest};
use anyhow::{Context, Result};
use serde::Serialize;
use std::{path::Path, process::ExitCode};

const KEY_SCHEMA: u32 = 1;

/// Everything declared as an input of a build cell's cache.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct KeyInputs {
    pub schema: u32,
    pub scope: String,
    pub build: String,
    pub platform: String,
    pub image: String,
    pub observed_image: Option<String>,
    pub rustflags: Vec<Option<String>>,
    pub triple: String,
    pub toolchain: String,
    pub profile: String,
    pub features: Vec<String>,
    pub packages: Vec<String>,
    pub nextest: String,
    /// Hash of the manifest bytes: a changed contract invalidates caches.
    pub manifest: String,
    pub cargo_lock: Option<String>,
    pub rust_toolchain_file: Option<String>,
    pub cargo_config: Option<String>,
}

fn file_digest(root: &Path, rel: &str) -> Result<Option<String>> {
    let p = root.join(rel);
    if p.is_file() {
        Ok(Some(crate::evidence::digest::sha256_file(&p)?))
    } else {
        Ok(None)
    }
}

pub fn inputs(root: &Path, m: &Manifest, build: &str, scope: &str) -> Result<KeyInputs> {
    let b = m
        .builds
        .get(build)
        .with_context(|| format!("unknown build {build}"))?;
    let p = &m.platforms[&b.platform];
    Ok(KeyInputs {
        schema: KEY_SCHEMA,
        scope: scope.into(),
        build: build.into(),
        platform: b.platform.clone(),
        image: p.image.clone(),
        observed_image: std::env::var("ImageVersion").ok(),
        rustflags: ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"]
            .into_iter()
            .map(|key| std::env::var(key).ok())
            .collect(),
        triple: b.target.clone().unwrap_or_else(|| p.triple.clone()),
        toolchain: p.toolchain.clone(),
        profile: b.profile.clone(),
        features: m.features[&b.features].cargo_args.clone(),
        packages: b.packages.clone(),
        nextest: m
            .tools
            .get("nextest")
            .map(|t| t.version.clone())
            .unwrap_or_default(),
        manifest: m.version().into(),
        cargo_lock: file_digest(root, "Cargo.lock")?,
        rust_toolchain_file: file_digest(root, "rust-toolchain.toml")?,
        cargo_config: file_digest(root, ".cargo/config.toml")?,
    })
}

/// `awman-v1-<scope>-<build>-<toolchain>-<digest>`; the readable prefix is for
/// humans, the digest for correctness.
pub fn key(i: &KeyInputs) -> Result<String> {
    let digest = crate::evidence::digest::input_digest(i)?;
    Ok(format!(
        "awman-v{}-{}-{}-{}-{}",
        i.schema,
        i.scope,
        i.build,
        i.toolchain,
        &digest[..40]
    ))
}

pub fn key_command(root: &Path, args: &[String]) -> Result<ExitCode> {
    let a = Args::parse(args, &["build", "scope"], &[])?;
    let m = manifest::load(root)?;
    let i = inputs(
        root,
        &m,
        a.need("build")?,
        a.get("scope").unwrap_or("build"),
    )?;
    println!("{}", key(&i)?);
    Ok(ExitCode::SUCCESS)
}

#[derive(Serialize)]
struct Stats<'a> {
    schema: u32,
    build: &'a str,
    key: &'a str,
    hit: bool,
    target_bytes: u64,
}

fn dir_bytes(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_bytes(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

pub fn stats_command(args: &[String]) -> Result<ExitCode> {
    let a = Args::parse(args, &["build", "key", "hit", "out"], &[])?;
    let out = Path::new(a.need("out")?);
    std::fs::create_dir_all(out)?;
    let hit = a.get("hit") == Some("true");
    let stats = Stats {
        schema: 1,
        build: a.need("build")?,
        key: a.need("key")?,
        hit,
        target_bytes: if hit {
            dir_bytes(Path::new("target"))
        } else {
            0
        },
    };
    std::fs::write(
        out.join("cache-stats.json"),
        serde_json::to_vec_pretty(&stats)?,
    )?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> KeyInputs {
        KeyInputs {
            schema: 1,
            scope: "build".into(),
            build: "linux-x64-default".into(),
            platform: "linux-x64".into(),
            image: "20261101.360".into(),
            observed_image: Some("20261101.360.1".into()),
            rustflags: vec![None, None],
            triple: "x86_64-unknown-linux-gnu".into(),
            toolchain: "1.94.0".into(),
            profile: "test".into(),
            features: vec![],
            packages: vec!["awman".into()],
            nextest: "0.9.150".into(),
            manifest: "m".into(),
            cargo_lock: Some("l".into()),
            rust_toolchain_file: None,
            cargo_config: None,
        }
    }

    /// Guards: f462fa2e (cache keys cover every effective build input).
    #[test]
    fn every_declared_input_changes_the_key() {
        let k0 = key(&base()).unwrap();
        assert_eq!(k0, key(&base()).unwrap(), "key is deterministic");
        type Mutation = Box<dyn Fn(&mut KeyInputs)>;
        let mutations: Vec<(&str, Mutation)> = vec![
            ("scope", Box::new(|i| i.scope = "lint".into())),
            ("build", Box::new(|i| i.build = "other".into())),
            ("platform", Box::new(|i| i.platform = "other".into())),
            ("image", Box::new(|i| i.image = "20261102.1".into())),
            (
                "observed_image",
                Box::new(|i| i.observed_image = Some("new-image".into())),
            ),
            (
                "rustflags",
                Box::new(|i| i.rustflags = vec![Some("-C target-cpu=native".into()), None]),
            ),
            (
                "triple",
                Box::new(|i| i.triple = "aarch64-apple-darwin".into()),
            ),
            ("toolchain", Box::new(|i| i.toolchain = "1.95.0".into())),
            ("profile", Box::new(|i| i.profile = "release".into())),
            (
                "features",
                Box::new(|i| i.features = vec!["--features".into(), "x".into()]),
            ),
            ("packages", Box::new(|i| i.packages.push("xtask".into()))),
            ("nextest", Box::new(|i| i.nextest = "0.9.151".into())),
            ("manifest", Box::new(|i| i.manifest = "m2".into())),
            ("cargo_lock", Box::new(|i| i.cargo_lock = Some("l2".into()))),
            (
                "rust_toolchain_file",
                Box::new(|i| i.rust_toolchain_file = Some("t".into())),
            ),
            (
                "cargo_config",
                Box::new(|i| i.cargo_config = Some("c".into())),
            ),
        ];
        for (name, mutate) in mutations {
            let mut i = base();
            mutate(&mut i);
            assert_ne!(key(&i).unwrap(), k0, "input `{name}` must change the key");
        }
    }

    #[test]
    fn a_toolchain_change_never_shares_a_prefix_with_the_old_key_digest() {
        let mut i = base();
        i.toolchain = "1.95.0".into();
        let (a, b) = (key(&base()).unwrap(), key(&i).unwrap());
        assert_ne!(a.rsplit('-').next(), b.rsplit('-').next());
    }

    #[test]
    fn real_manifest_yields_a_key_for_every_ci_build() {
        let root = crate::gates::root();
        let m = manifest::load(&root).unwrap();
        for id in crate::gates::render::workflows::ci_builds(&m).unwrap() {
            let k = key(&inputs(&root, &m, &id, "build").unwrap()).unwrap();
            assert!(k.contains(&id));
        }
    }
}
