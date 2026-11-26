//! `cargo xtask fixtures pin`: write the reviewed manifest pins for one
//! published, architecture-specific guest fixture set (design 7.8). It
//! replaces the old shell/Python pin step and is preparation work, never a
//! release step: `make release` only consumes pins.
//!
//! ```text
//! cargo xtask fixtures pin --arch arm64|amd64 --repo owner/name \
//!     --tag fixtures-<commit> --assets DIR [--skip-attestation-for-test]
//! ```
//!
//! `DIR` holds the downloaded release assets. Every check runs before the
//! manifest is touched: checksum list, base image, provenance platform,
//! recipe and image-contract digests, archive digests and (unless the
//! test-only switch is given) each archive's Actions attestation by
//! `fixtures.yml` at the tag's commit on a hosted runner.

use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, ExitCode};

/// `(arch) → [(fixture id, published file)]` and the provenance file name.
fn layout(arch: &str) -> Result<(Vec<(&'static str, &'static str)>, &'static str)> {
    Ok(match arch {
        "arm64" => (
            vec![
                ("guest-oci-base", "fixture-oci.tar"),
                ("guest-docker-base", "fixture-docker.tar"),
            ],
            "provenance.json",
        ),
        "amd64" => (
            vec![("guest-oci-base-amd64", "fixture-oci-amd64.tar")],
            "provenance-amd64.json",
        ),
        _ => bail!("--arch must be arm64 or amd64"),
    })
}

fn sha256(path: &Path) -> Result<String> {
    crate::release::util::sha256_file(path)
}

fn checksums(text: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (sha, name) = line.split_once("  ").context("malformed SHA256SUMS line")?;
        ensure!(
            crate::release::util::is_sha256(sha),
            "malformed digest in SHA256SUMS"
        );
        ensure!(
            out.insert(name.to_string(), sha.to_string()).is_none(),
            "{name} listed twice"
        );
    }
    Ok(out)
}

/// Replace `sha256`, `source` and `pending` in `[fixtures.<id>]`.
pub fn rewrite(manifest: &str, id: &str, sha: &str, source: &str) -> Result<String> {
    let header = format!("[fixtures.{id}]");
    let start = manifest
        .lines()
        .position(|l| l.trim() == header)
        .with_context(|| format!("ci/gates.toml has no {header} table"))?;
    let lines: Vec<&str> = manifest.lines().collect();
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map(|i| start + 1 + i)
        .unwrap_or(lines.len());
    let mut body: Vec<String> = lines[start + 1..end]
        .iter()
        .filter(|l| {
            let k = l.split('=').next().unwrap_or("").trim();
            !matches!(k, "sha256" | "source" | "pending")
        })
        .map(|l| l.to_string())
        .collect();
    while body.last().is_some_and(|l| l.trim().is_empty()) {
        body.pop();
    }
    body.push(format!("sha256 = \"{sha}\""));
    body.push(format!("source = \"{source}\""));
    body.push(String::new());
    let mut out: Vec<String> = lines[..=start].iter().map(|l| l.to_string()).collect();
    out.extend(body);
    out.extend(lines[end..].iter().map(|l| l.to_string()));
    let mut text = out.join("\n");
    if manifest.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

pub fn run(root: &Path, args: &[String]) -> Result<ExitCode> {
    let get = |name: &str| -> Result<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
            .with_context(|| format!("fixtures pin needs {name}"))
    };
    let arch = get("--arch")?;
    let repo = get("--repo")?;
    let tag = get("--tag")?;
    let assets = std::path::PathBuf::from(get("--assets")?);
    let attest = !args.iter().any(|a| a == "--skip-attestation-for-test");
    ensure!(
        repo.split('/').count() == 2
            && repo
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b)),
        "invalid repository {repo}"
    );
    let commit = tag.strip_prefix("fixtures-").unwrap_or("");
    ensure!(
        commit.len() == 40
            && commit
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "fixture tag must be fixtures-<full commit>"
    );
    let (files, provenance_name) = layout(&arch)?;
    let sums =
        checksums(&std::fs::read_to_string(assets.join("SHA256SUMS")).context("SHA256SUMS")?)?;
    for (name, want) in &sums {
        ensure!(
            sha256(&assets.join(name))? == *want,
            "{name} does not match SHA256SUMS"
        );
    }
    let base = std::fs::read_to_string(assets.join("base-image.txt")).context("base-image.txt")?;
    ensure!(
        base == std::fs::read_to_string(root.join("ci/fixture-base-image.txt"))?,
        "published base image differs from the reviewed ci/fixture-base-image.txt"
    );
    let p: Value = serde_json::from_slice(
        &std::fs::read(assets.join(provenance_name)).context(provenance_name)?,
    )?;
    ensure!(
        p["schema"] == 1
            && p["platform"]["os"] == "linux"
            && p["platform"]["architecture"] == arch.as_str(),
        "{provenance_name} has the wrong schema or platform (want linux/{arch})"
    );
    ensure!(
        p["recipe"] == "guest-contract-v1"
            && p["recipe_sha256"]
                == sha256(&root.join("xtask/src/fixtures/recipes/guest_image.rs"))?.as_str()
            && p["image_contract_sha256"]
                == sha256(&root.join("tests/guest/support/image-contract.sh"))?.as_str(),
        "{provenance_name} differs from the reviewed recipe or image contract"
    );
    for (_, file) in &files {
        let digest = sums
            .get(*file)
            .with_context(|| format!("{file} is not in SHA256SUMS"))?;
        let unsuffixed = file.replace("-amd64", "");
        ensure!(
            p["archives"][&unsuffixed] == digest.as_str()
                || p["archives"][*file] == digest.as_str(),
            "{provenance_name} does not bind {file}"
        );
        if attest {
            let status = Command::new("gh")
                .args(["attestation", "verify"])
                .arg(assets.join(file))
                .args([
                    "--repo",
                    &repo,
                    "--signer-workflow",
                    &format!("{repo}/.github/workflows/fixtures.yml"),
                ])
                .args(["--source-digest", commit, "--deny-self-hosted-runners"])
                .status()
                .context("gh attestation verify")?;
            ensure!(
                status.success(),
                "{file} is not attested by fixtures.yml at {commit}"
            );
        }
    }
    let path = root.join("ci/gates.toml");
    let mut text = std::fs::read_to_string(&path)?;
    for (id, file) in &files {
        let source = format!("https://github.com/{repo}/releases/download/{tag}/{file}");
        text = rewrite(&text, id, &sums[*file], &source)?;
    }
    std::fs::write(&path, text)?;
    println!("pinned {arch} guest fixtures from {tag}; run `cargo xtask gates check`, review and commit ci/gates.toml");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewriting_a_pin_replaces_only_the_pin_fields_of_that_table() {
        let m =
            "[fixtures.a]\narch = \"amd64\"\npending = \"x\"\n\n[fixtures.b]\nsha256 = \"old\"\n";
        let out = rewrite(m, "a", &"1".repeat(64), "https://e/x.tar").unwrap();
        assert_eq!(
            out,
            format!("[fixtures.a]\narch = \"amd64\"\nsha256 = \"{}\"\nsource = \"https://e/x.tar\"\n\n[fixtures.b]\nsha256 = \"old\"\n", "1".repeat(64))
        );
        assert!(rewrite(m, "missing", "x", "y").is_err());
        assert!(layout("riscv").is_err());
    }
}
