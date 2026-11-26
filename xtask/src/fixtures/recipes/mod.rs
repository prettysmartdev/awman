//! Explicit preparation jobs. Test execution never calls these recipes.
mod guest_image;
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

fn options(args: &[String]) -> Result<BTreeMap<String, String>> {
    if !args.len().is_multiple_of(2) {
        bail!("recipe arguments are --key VALUE pairs");
    }
    let mut out = BTreeMap::new();
    for pair in args.chunks_exact(2) {
        if !pair[0].starts_with("--") || out.insert(pair[0].clone(), pair[1].clone()).is_some() {
            bail!("invalid or duplicate option {}", pair[0]);
        }
    }
    Ok(out)
}
fn value<'a>(opts: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str> {
    opts.get(key)
        .map(String::as_str)
        .with_context(|| format!("missing {key}"))
}
fn digest(s: &str) -> Result<()> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("a literal SHA-256 digest is required");
    }
    Ok(())
}
fn verify(path: &Path, expected: &str) -> Result<()> {
    digest(expected)?;
    let actual = crate::evidence::digest::sha256_file(path)?;
    if actual != expected.to_lowercase() {
        bail!("{}: SHA-256 {actual}, expected {expected}", path.display());
    }
    Ok(())
}
fn checked(mut command: Command) -> Result<()> {
    let status = command.status().context("start fixture preparation tool")?;
    if !status.success() {
        bail!("fixture preparation tool failed: {status}");
    }
    Ok(())
}
/// Published assets are the normal upgrade input. Downloads are verified before
/// atomic placement; incomplete files can never become executable cache entries.
fn published(opts: &BTreeMap<String, String>) -> Result<()> {
    let hash = value(opts, "--sha256")?;
    digest(hash)?;
    let url = value(opts, "--url")?;
    if !url.starts_with("https://") || !url.contains("/releases/download/") {
        bail!("previous release must be an HTTPS published release asset URL");
    }
    let out = PathBuf::from(value(opts, "--out")?);
    if out.is_file() && verify(&out, hash).is_ok() {
        executable(&out)?;
        return Ok(());
    }
    std::fs::create_dir_all(out.parent().context("--out needs a parent directory")?)?;
    let partial = out.with_extension(format!("{}.part", std::process::id()));
    let mut curl = Command::new("curl");
    curl.args([
        "--fail",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--retry",
        "2",
        "--output",
    ])
    .arg(&partial)
    .arg(url);
    let result = checked(curl).and_then(|_| verify(&partial, hash));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    executable(&partial)?;
    std::fs::rename(&partial, &out)?;
    println!(
        "published previous release: {} sha256={hash}",
        out.display()
    );
    Ok(())
}
fn executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn guest(recipe: &str, opts: &BTreeMap<String, String>) -> Result<()> {
    let base = PathBuf::from(value(opts, "--base")?);
    verify(&base, value(opts, "--sha256")?)?;
    let out = PathBuf::from(value(opts, "--out")?);
    guest_image::build(&base, &out)?;
    let archive = out.join(if recipe == "guest-docker-base" {
        "fixture-docker.tar"
    } else {
        "fixture-oci.tar"
    });
    let hash = crate::evidence::digest::sha256_file(&archive)?;
    println!(
        "fixture {recipe}: {} sha256={hash}; publish these immutable bytes then commit the pin",
        archive.display()
    );
    Ok(())
}

/// Verify publication outputs against a separately reviewed base pin. This
/// proves reproducibility; the maintainer also authenticates Actions provenance
/// before committing the public archive pins.
pub fn verify_guest(args: &[String]) -> Result<ExitCode> {
    let opts = options(args)?;
    let base = PathBuf::from(value(&opts, "--base")?);
    verify(&base, value(&opts, "--sha256")?)?;
    guest_image::verify(&base, Path::new(value(&opts, "--out")?))?;
    println!("both fixture archives and provenance match the reviewed base and recipe");
    Ok(ExitCode::SUCCESS)
}
fn history(opts: &BTreeMap<String, String>) -> Result<()> {
    let hash = value(opts, "--sha256")?;
    digest(hash)?;
    let revision = value(opts, "--revision")?;
    if revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("history fallback requires a full immutable commit ID");
    }
    if value(opts, "--reason")?.trim().is_empty() {
        bail!("history fallback requires the missing published-asset reason");
    }
    let target = value(opts, "--target")?;
    if ![
        "aarch64-apple-darwin",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
    ]
    .contains(&target)
    {
        bail!("unsupported builtin target");
    }
    let cache = PathBuf::from(value(opts, "--cache")?).join(hash);
    let exe = cache.join("awman");
    if exe.is_file() {
        verify(&exe, hash)?;
        println!("{}", exe.display());
        return Ok(());
    }
    std::fs::create_dir_all(&cache)?;
    let source = cache.join("source");
    std::fs::create_dir_all(&source)?;
    let archive = cache.join("source.tar");
    let mut git = Command::new("git");
    git.args(["archive", "--format=tar", "--output"])
        .arg(&archive)
        .arg(revision);
    checked(git)?;
    let mut tar = Command::new("tar");
    tar.arg("-xf").arg(&archive).arg("-C").arg(&source);
    checked(tar)?;
    let mut build = Command::new("cargo");
    build
        .current_dir(&source)
        .args([
            "build",
            "--locked",
            "--release",
            "--bin",
            "awman",
            "--features",
            "builtin-runtime",
            "--target",
            target,
            "--target-dir",
        ])
        .arg(cache.join("build"));
    checked(build)?;
    let built = cache.join("build").join(target).join("release/awman");
    verify(&built, hash)?;
    std::fs::rename(built, &exe)?;
    println!("fallback only: {} sha256={hash}", exe.display());
    Ok(())
}
pub fn run(recipe: &str, args: &[String]) -> Result<ExitCode> {
    let opts = options(args)?;
    match recipe {
        "previous-published" => published(&opts)?,
        // The amd64 fixture uses the same recipe; the base image decides the architecture.
        "guest-oci-base" | "guest-oci-base-amd64" | "guest-docker-base" => guest(recipe, &opts)?,
        "previous-from-history" => history(&opts)?,
        _ => bail!("unknown fixture recipe {recipe}"),
    }
    Ok(ExitCode::SUCCESS)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mutable_or_missing_pins_are_rejected() {
        for s in ["", "latest", "pending", &"x".repeat(64)] {
            assert!(digest(s).is_err());
        }
        assert!(digest(&"a".repeat(64)).is_ok());
    }
    #[test]
    fn malformed_and_duplicate_options_are_rejected() {
        assert!(options(&["--out".into()]).is_err());
        assert!(options(&["--out".into(), "a".into(), "--out".into(), "b".into()]).is_err());
    }

    #[test]
    fn corrupt_cached_fixture_is_rejected_by_digest() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("fixture.tar");
        std::fs::write(&archive, b"fixture bytes").unwrap();
        let hash = crate::evidence::digest::sha256_file(&archive).unwrap();
        verify(&archive, &hash).unwrap();
        std::fs::write(&archive, b"replaced bytes").unwrap();
        assert!(verify(&archive, &hash).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn cached_published_executable_regains_its_execute_bit() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let asset = root.path().join("awman");
        std::fs::write(&asset, b"published binary").unwrap();
        std::fs::set_permissions(&asset, std::fs::Permissions::from_mode(0o644)).unwrap();
        let opts = BTreeMap::from([
            ("--out".into(), asset.display().to_string()),
            (
                "--sha256".into(),
                crate::evidence::digest::sha256_file(&asset).unwrap(),
            ),
            (
                "--url".into(),
                "https://github.com/example/awman/releases/download/v1/awman".into(),
            ),
        ]);
        published(&opts).unwrap();
        assert_ne!(
            std::fs::metadata(&asset).unwrap().permissions().mode() & 0o111,
            0
        );
    }

    #[test]
    fn published_asset_requires_https_and_history_requires_immutable_revision() {
        let pin = "a".repeat(64);
        let published = options(&[
            "--sha256".into(),
            pin.clone(),
            "--url".into(),
            "http://example.test/releases/download/v1/awman".into(),
        ])
        .unwrap();
        assert!(super::published(&published)
            .unwrap_err()
            .to_string()
            .contains("HTTPS"));
        let history =
            options(&["--sha256".into(), pin, "--revision".into(), "main".into()]).unwrap();
        assert!(super::history(&history)
            .unwrap_err()
            .to_string()
            .contains("immutable commit"));
    }
}
