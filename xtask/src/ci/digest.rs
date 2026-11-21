//! Digests of build outputs. The build job records them next to the archive and
//! as job outputs (a channel the artifact bytes cannot write to); test jobs
//! verify the downloaded bytes against both before executing anything.

use super::{args::Args, verdict};
use crate::evidence::digest::{is_sha256, sha256_file};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path, process::ExitCode};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Digests {
    pub schema: u32,
    pub files: BTreeMap<String, Recorded>,
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Recorded {
    /// File name next to the digest file.
    pub file: String,
    pub sha256: String,
    pub bytes: u64,
}

pub fn run(args: &[String]) -> Result<ExitCode> {
    match args.first().map(String::as_str) {
        Some("record") => record(&args[1..]),
        Some("verify") => verify_command(&args[1..]),
        _ => bail!("usage: ci digest record --out FILE name=path... | verify --file FILE [--expect name=sha256]..."),
    }
}

pub fn record_files(out: &Path, named: &[(String, std::path::PathBuf)]) -> Result<Digests> {
    let dir = out.parent().unwrap_or(Path::new("."));
    let mut files = BTreeMap::new();
    for (name, path) in named {
        let rel = path
            .strip_prefix(dir)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.file_name().map(Into::into).unwrap_or_default());
        anyhow::ensure!(
            rel.components().count() == 1,
            "{}: digested files must sit next to {}",
            path.display(),
            out.display()
        );
        let sha = sha256_file(path).with_context(|| format!("digesting {}", path.display()))?;
        files.insert(
            name.clone(),
            Recorded {
                file: rel.to_string_lossy().into_owned(),
                sha256: sha,
                bytes: std::fs::metadata(path)?.len(),
            },
        );
    }
    Ok(Digests { schema: 1, files })
}

fn record(args: &[String]) -> Result<ExitCode> {
    let a = Args::parse(args, &["out"], &[])?;
    let out = Path::new(a.need("out")?);
    let mut named = Vec::new();
    for p in &a.positional {
        let (n, path) = p.split_once('=').context("expected name=path")?;
        named.push((n.to_owned(), std::path::PathBuf::from(path)));
    }
    anyhow::ensure!(!named.is_empty(), "nothing to record");
    let d = record_files(out, &named)?;
    std::fs::write(out, serde_json::to_vec_pretty(&d)?)?;
    // GITHUB_OUTPUT lines: archive_sha256=…
    for (name, r) in &d.files {
        println!("{name}_sha256={}", r.sha256);
    }
    Ok(ExitCode::SUCCESS)
}

/// Every problem found; empty means the directory's bytes are exactly what was
/// recorded and what the independent channel expects.
pub fn verify(file: &Path, expect: &[(String, String)]) -> Result<Vec<String>> {
    let d: Digests = serde_json::from_slice(&std::fs::read(file)?)
        .with_context(|| format!("reading {}", file.display()))?;
    anyhow::ensure!(d.schema == 1, "unsupported digest schema");
    let dir = file.parent().unwrap_or(Path::new("."));
    let mut errors = Vec::new();
    for (name, r) in &d.files {
        if r.file.contains('/') || r.file.contains('\\') || r.file.contains("..") {
            errors.push(format!(
                "{name}: recorded path {} escapes the archive directory",
                r.file
            ));
            continue;
        }
        match sha256_file(&dir.join(&r.file)) {
            Ok(actual) if actual == r.sha256 => {}
            Ok(actual) => errors.push(format!(
                "{name}: {} has sha256 {actual}, recorded {}",
                r.file, r.sha256
            )),
            Err(e) => errors.push(format!("{name}: {}: {e}", r.file)),
        }
    }
    if expect.is_empty() {
        errors.push(
            "no independent digests supplied (--expect): the archive cannot vouch for itself"
                .into(),
        );
    }
    for (name, want) in expect {
        if !is_sha256(want) {
            errors.push(format!(
                "{name}: expected digest `{want}` is not a SHA-256 (build job output missing?)"
            ));
            continue;
        }
        match d.files.get(name) {
            None => errors.push(format!("{name}: not recorded in {}", file.display())),
            Some(r) if r.sha256 != *want => errors.push(format!(
                "{name}: archive records {} but the build job reported {want}",
                r.sha256
            )),
            Some(_) => {}
        }
    }
    Ok(errors)
}

fn verify_command(args: &[String]) -> Result<ExitCode> {
    let a = Args::parse(args, &["file", "expect"], &[])?;
    let mut expect = Vec::new();
    for e in a.all("expect") {
        let (n, v) = e.split_once('=').context("expected name=sha256")?;
        expect.push((n.to_owned(), v.to_owned()));
    }
    let errors = verify(Path::new(a.need("file")?), &expect)?;
    for e in &errors {
        eprintln!("digest: {e}");
    }
    verdict(errors.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, std::path::PathBuf, Digests) {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("archive.tar.zst");
        let b = d.path().join("awman-debug");
        std::fs::write(&a, b"archive bytes").unwrap();
        std::fs::write(&b, b"binary bytes").unwrap();
        let out = d.path().join("digests.json");
        let rec = record_files(&out, &[("archive".into(), a), ("binary".into(), b)]).unwrap();
        std::fs::write(&out, serde_json::to_vec(&rec).unwrap()).unwrap();
        (d, out, rec)
    }

    #[test]
    fn matching_bytes_and_expectations_verify() {
        let (_d, out, rec) = setup();
        let expect = vec![
            ("archive".to_string(), rec.files["archive"].sha256.clone()),
            ("binary".to_string(), rec.files["binary"].sha256.clone()),
        ];
        assert_eq!(verify(&out, &expect).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn tampered_archive_bytes_are_rejected() {
        let (d, out, rec) = setup();
        std::fs::write(d.path().join("archive.tar.zst"), b"evil").unwrap();
        let expect = vec![("archive".to_string(), rec.files["archive"].sha256.clone())];
        let errors = verify(&out, &expect).unwrap();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("archive") && e.contains("recorded")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_substituted_digest_file_cannot_vouch_for_itself() {
        let (d, out, _) = setup();
        // An attacker replaces both the archive and its digest file consistently…
        std::fs::write(d.path().join("archive.tar.zst"), b"evil").unwrap();
        let forged = record_files(
            &out,
            &[("archive".into(), d.path().join("archive.tar.zst"))],
        )
        .unwrap();
        std::fs::write(&out, serde_json::to_vec(&forged).unwrap()).unwrap();
        // …but the build job's output (independent channel) still names the original.
        let original = crate::evidence::digest::sha256(b"archive bytes");
        let errors = verify(&out, &[("archive".to_string(), original)]).unwrap();
        assert!(
            errors.iter().any(|e| e.contains("build job reported")),
            "{errors:?}"
        );
    }

    #[test]
    fn missing_or_malformed_expectations_fail_closed() {
        let (_d, out, _) = setup();
        assert!(!verify(&out, &[]).unwrap().is_empty());
        assert!(!verify(&out, &[("archive".into(), String::new())])
            .unwrap()
            .is_empty());
        let sha = crate::evidence::digest::sha256(b"x");
        assert!(!verify(&out, &[("nope".into(), sha)]).unwrap().is_empty());
    }

    #[test]
    fn recorded_path_traversal_is_rejected() {
        let (d, out, mut rec) = setup();
        rec.files.get_mut("archive").unwrap().file = "../x".into();
        std::fs::write(&out, serde_json::to_vec(&rec).unwrap()).unwrap();
        let sha = rec.files["archive"].sha256.clone();
        let errors = verify(&out, &[("archive".into(), sha)]).unwrap();
        assert!(errors.iter().any(|e| e.contains("escapes")), "{errors:?}");
        drop(d);
    }
}
