//! Installing pinned tools from verified sources, and the actionlint config.
//!
//! * crates.io tools (`cargo-deny`, `cargo-llvm-cov`, `cargo-mutants`) are built
//!   with `cargo install --locked --version <pin>`; crates.io's index checksum
//!   and the crate's own lock file are the verification.
//! * Release-archive tools (`shellcheck`, `actionlint`) are downloaded from the
//!   manifest's source and accepted only if the archive's SHA-256 equals the
//!   manifest's per-triple digest. A missing digest ("pending") is a failure:
//!   an unverified binary is never installed.
//!
//! Everything lands in `target/xtask-tools/bin`, which is appended to
//! `$GITHUB_PATH` when set. nextest is installed by `cargo xtask test install-runner`.

use super::args::Args;
use crate::manifest::{self, Manifest, Tool};
use anyhow::{bail, ensure, Context, Result};
use std::{
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

pub const TOOL_DIR: &str = "target/xtask-tools";

/// `(url, archive suffix)` of a release-archive tool for a Rust host triple.
pub fn asset(name: &str, tool: &Tool, triple: &str) -> Result<(String, &'static str)> {
    let v = &tool.version;
    let src = tool.source.trim_end_matches('/');
    let (os, arch_sc, arch_go) = match triple {
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" => ("linux", "x86_64", "amd64"),
        "aarch64-unknown-linux-gnu" | "aarch64-unknown-linux-musl" => ("linux", "aarch64", "arm64"),
        "aarch64-apple-darwin" => ("darwin", "aarch64", "arm64"),
        "x86_64-apple-darwin" => ("darwin", "x86_64", "amd64"),
        other => bail!("{name}: no release archive is defined for host {other}"),
    };
    match name {
        "shellcheck" => Ok((
            format!("{src}/releases/download/v{v}/shellcheck-v{v}.{os}.{arch_sc}.tar.xz"),
            "tar.xz",
        )),
        "actionlint" => Ok((
            format!("{src}/releases/download/v{v}/actionlint_{v}_{os}_{arch_go}.tar.gz"),
            "tar.gz",
        )),
        other => bail!("{other}: not a release-archive tool"),
    }
}

pub fn is_crate(tool: &Tool) -> bool {
    tool.source.starts_with("https://crates.io/crates/")
}

/// The digest the manifest pins for this host, or why there is none.
pub fn pinned_digest<'a>(name: &str, tool: &'a Tool, triple: &str) -> Result<&'a str> {
    match tool.sha256.get(triple) {
        Some(d) if crate::evidence::digest::is_sha256(d) => Ok(d),
        _ => bail!(
            "{name} {}: no verified SHA-256 for {triple} in ci/gates.toml ({}); refusing to install an unverified binary",
            tool.version,
            tool.pending.as_deref().unwrap_or("digest absent")
        ),
    }
}

fn host_triple() -> Result<String> {
    let out = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("running rustc -vV")?;
    String::from_utf8(out.stdout)?
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(str::to_owned))
        .context("rustc -vV printed no host")
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let status = cmd.status().with_context(|| format!("starting {what}"))?;
    ensure!(status.success(), "{what} failed: {status}");
    Ok(())
}

fn install_crate(name: &str, tool: &Tool, root: &Path) -> Result<()> {
    run(
        Command::new("cargo")
            .args([
                "install",
                "--locked",
                "--version",
                &tool.version,
                name,
                "--root",
            ])
            .arg(root),
        &format!("cargo install {name}@{}", tool.version),
    )
}

fn install_archive(name: &str, tool: &Tool, bin: &Path) -> Result<()> {
    let triple = host_triple()?;
    let digest = pinned_digest(name, tool, &triple)?;
    let (url, kind) = asset(name, tool, &triple)?;
    let work = bin
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!(".download-{}", std::process::id()));
    std::fs::create_dir_all(&work)?;
    let result = fetch_and_unpack(name, &url, kind, digest, &work, bin);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn fetch_and_unpack(
    name: &str,
    url: &str,
    kind: &str,
    digest: &str,
    work: &Path,
    bin: &Path,
) -> Result<()> {
    let archive = work.join(format!("{name}.{kind}"));
    run(
        Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--retry",
                "3",
                "--proto",
                "=https",
                "--output",
            ])
            .arg(&archive)
            .arg(url),
        &format!("downloading {url}"),
    )?;
    let actual = crate::evidence::digest::sha256_file(&archive)?;
    ensure!(
        actual == digest,
        "{name}: downloaded archive has sha256 {actual}, manifest pins {digest}; refusing to unpack"
    );
    let flag = if kind == "tar.xz" { "-xJf" } else { "-xzf" };
    let unpacked = work.join("x");
    std::fs::create_dir_all(&unpacked)?;
    run(
        Command::new("tar")
            .arg(flag)
            .arg(&archive)
            .arg("-C")
            .arg(&unpacked),
        "tar",
    )?;
    let found =
        find_file(&unpacked, name).with_context(|| format!("{name} not found in the archive"))?;
    std::fs::create_dir_all(bin)?;
    let dest = bin.join(name);
    std::fs::copy(&found, &dest)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_file(&p, name) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|n| n == name) {
            return Some(p);
        }
    }
    None
}

pub fn install(m: &Manifest, name: &str, root: &Path) -> Result<()> {
    let tool = m
        .tools
        .get(name)
        .with_context(|| format!("{name} is not declared in [tools]"))?;
    if is_crate(tool) {
        install_crate(name, tool, root)
    } else {
        install_archive(name, tool, &root.join("bin"))
    }
}

pub fn install_command(repo: &Path, args: &[String]) -> Result<ExitCode> {
    let a = Args::parse(args, &[], &[])?;
    let [name] = a.positional.as_slice() else {
        bail!("usage: ci install-tool NAME");
    };
    let m = manifest::load(repo)?;
    let root = repo.join(TOOL_DIR);
    install(&m, name, &root)?;
    let bin = root.join("bin");
    if let Ok(path_file) = std::env::var("GITHUB_PATH") {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path_file)?;
        writeln!(f, "{}", bin.display())?;
    }
    println!(
        "installed {name} {} into {}",
        m.tools[name].version,
        bin.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// actionlint knows only GitHub's labels; the self-hosted label sets are
/// declared once, in the manifest, and handed to it from there.
pub fn actionlint_config(repo: &Path, args: &[String]) -> Result<ExitCode> {
    let a = Args::parse(args, &["out"], &[])?;
    let m = manifest::load(repo)?;
    let mut labels: Vec<&str> = m
        .release
        .ci_guest_labels
        .iter()
        .chain(&m.release.release_labels)
        .map(String::as_str)
        .collect();
    labels.sort_unstable();
    labels.dedup();
    let mut text = String::from("self-hosted-runner:\n  labels:\n");
    for l in labels {
        text.push_str(&format!("    - {l}\n"));
    }
    std::fs::write(a.need("out")?, text)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Tool {
        let m = manifest::load(&crate::gates::root()).unwrap();
        m.tools[name].clone()
    }

    #[test]
    fn asset_urls_follow_the_release_naming() {
        let (u, k) = asset(
            "shellcheck",
            &tool("shellcheck"),
            "x86_64-unknown-linux-gnu",
        )
        .unwrap();
        assert_eq!(u, "https://github.com/koalaman/shellcheck/releases/download/v0.11.0/shellcheck-v0.11.0.linux.x86_64.tar.xz");
        assert_eq!(k, "tar.xz");
        let (u, _) = asset("actionlint", &tool("actionlint"), "aarch64-apple-darwin").unwrap();
        assert_eq!(u, "https://github.com/rhysd/actionlint/releases/download/v1.7.7/actionlint_1.7.7_darwin_arm64.tar.gz");
        assert!(asset("shellcheck", &tool("shellcheck"), "x86_64-pc-windows-msvc").is_err());
    }

    #[test]
    fn a_pending_or_malformed_digest_refuses_installation() {
        let mut t = tool("shellcheck");
        t.sha256.clear();
        let e = pinned_digest("shellcheck", &t, "x86_64-unknown-linux-gnu").unwrap_err();
        assert!(e
            .to_string()
            .contains("refusing to install an unverified binary"));
        t.sha256
            .insert("x86_64-unknown-linux-gnu".into(), "not-a-digest".into());
        assert!(pinned_digest("shellcheck", &t, "x86_64-unknown-linux-gnu").is_err());
        let good = "a".repeat(64);
        t.sha256
            .insert("x86_64-unknown-linux-gnu".into(), good.clone());
        assert_eq!(
            pinned_digest("shellcheck", &t, "x86_64-unknown-linux-gnu").unwrap(),
            good
        );
    }

    #[test]
    fn crate_tools_are_recognised_by_source() {
        assert!(is_crate(&tool("cargo-deny")));
        assert!(!is_crate(&tool("actionlint")));
    }
}
