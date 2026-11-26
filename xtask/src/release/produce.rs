//! Steps of `.github/workflows/release.yml` (design 5.4(e), 7.7, 7.9). They
//! run on hosted runners from a checkout of R, take their selection from
//! the release plan and never hold a write token.
//!
//! ```text
//! release produce     --asset A --plan P --out DIR   build once, cache-free; sign once (macOS)
//! release kit-pack    --dir D --out FILE              deterministic tar of a test kit
//! release smoke       --asset A --plan P --artifact F --evidence DIR
//! release build-record --asset A --plan P --artifact F --version V --evidence DIR --out DIR
//! ```

use super::plan::{AssetPlan, Plan};
use super::util::{read_json, sha256_file, write_json};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::path::Path;

mod smoke_evidence;
use std::process::{Command, ExitCode};

fn flag(args: &[String], name: &str) -> Result<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
        .with_context(|| format!("missing {name}"))
}

fn load(args: &[String]) -> Result<(Plan, AssetPlan)> {
    let bytes = std::fs::read(flag(args, "--plan")?)?;
    let (plan, _) = Plan::parse(&bytes).map_err(|s| anyhow::anyhow!("{s}"))?;
    let asset = flag(args, "--asset")?;
    let a = plan
        .asset(&asset)
        .cloned()
        .with_context(|| format!("{asset} is not a release asset"))?;
    Ok((plan, a))
}

fn git(args: &[&str]) -> Result<String> {
    let out = Command::new("git").args(args).output()?;
    ensure!(out.status.success(), "git {} failed", args.join(" "));
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("starting {cmd:?}"))?;
    ensure!(status.success(), "{cmd:?} failed ({status})");
    Ok(())
}

/// Build once in a fresh target directory, sign once, check the ABI.
fn produce(args: &[String]) -> Result<ExitCode> {
    let (_, a) = load(args)?;
    let out = std::path::absolute(flag(args, "--out")?)?;
    std::fs::create_dir_all(&out)?;
    let builtin = a.cargo.iter().any(|c| c == "builtin-runtime");
    if builtin {
        run(Command::new("bash").args(["tools/msb-payloads/fetch.sh", &a.triple]))?;
        run(Command::new("bash").args(["tools/msb-payloads/verify.sh", &a.triple]))?;
    }
    // Shipping builds are cache-free: a new target directory per job.
    let target = out.join("cargo-target");
    if target.exists() {
        std::fs::remove_dir_all(&target)?;
    }
    println!("BUILD-ONCE {}: cargo {}", a.asset, a.cargo.join(" "));
    run(Command::new("cargo")
        .args(&a.cargo)
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_INCREMENTAL", "0"))?;
    let built = target.join(a.binary.strip_prefix("target/").unwrap_or(&a.binary));
    let file = out.join(&a.asset);
    std::fs::copy(&built, &file).with_context(|| format!("copy {}", built.display()))?;
    std::fs::remove_dir_all(&target).ok();
    let unsigned = sha256_file(&file)?;
    if a.sign {
        ensure!(
            cfg!(target_os = "macos"),
            "{} is signed on the hosted macOS runner",
            a.asset
        );
        println!("SIGN-ONCE {}", a.asset);
        run(Command::new("bash").arg("tools/macos-sign.sh").arg(&file))?;
    }
    let mut abi_ok = Value::Null;
    if a.abi.is_some() {
        super::abi::verify(&a.triple, &file, &out)?;
        abi_ok = json!(true);
    }
    let rustc = Command::new("rustc").arg("--version").output()?;
    write_json(
        &out.join("produce.json"),
        &json!({"schema": 1, "asset": a.asset, "sha256": sha256_file(&file)?, "unsigned_sha256": unsigned,
                "size": std::fs::metadata(&file)?.len(), "commit": git(&["rev-parse", "HEAD"])?,
                "tree": git(&["rev-parse", "HEAD^{tree}"])?, "toolchain": String::from_utf8_lossy(&rustc.stdout).trim(),
                "signed": a.sign, "abi_ok": abi_ok, "native": a.native}),
    )?;
    if let Ok(path) = std::env::var("GITHUB_OUTPUT") {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)?;
        writeln!(f, "native={}", a.native.clone().unwrap_or_default())?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Deterministic tar (sorted, mtime 0, uid/gid 0) of a kit directory;
/// regular files and directories only.
pub fn kit_pack(dir: &Path, out: &Path) -> Result<()> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let p = e?.path();
            let meta = std::fs::symlink_metadata(&p)?;
            if meta.is_dir() {
                stack.push(p);
            } else if meta.is_file() {
                files.push(p);
            } else {
                bail!("{} is not a regular file", p.display());
            }
        }
    }
    files.sort();
    let mut b = tar::Builder::new(std::fs::File::create(out)?);
    for p in files {
        let rel = p.strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
        let bytes = std::fs::read(&p)?;
        let mut h = tar::Header::new_gnu();
        h.set_size(bytes.len() as u64);
        #[cfg(unix)]
        let exec = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&p)?.permissions().mode() & 0o100 != 0
        };
        #[cfg(not(unix))]
        let exec = rel.starts_with("bin/");
        h.set_mode(if exec { 0o755 } else { 0o644 });
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_cksum();
        b.append_data(&mut h, &rel, bytes.as_slice())?;
    }
    b.finish()?;
    Ok(())
}

/// Every smoke suite of the plan on the exact downloaded bytes.
fn smoke(args: &[String]) -> Result<ExitCode> {
    let (_, a) = load(args)?;
    if a.cargo.iter().any(|arg| arg == "builtin-runtime") {
        // Validation is a fresh hosted job: its test harness needs the same
        // reviewed build inputs as production, even though the asset is retained.
        run(Command::new("bash").args(["tools/msb-payloads/fetch.sh", &a.triple]))?;
        run(Command::new("bash").args(["tools/msb-payloads/verify.sh", &a.triple]))?;
    }
    let artifact = std::path::absolute(flag(args, "--artifact")?)?;
    let evidence = std::path::absolute(flag(args, "--evidence")?)?;
    let sha = sha256_file(&artifact)?;
    if cfg!(windows) {
        // The wrapper probes real outbound denial and removes its owned
        // firewall rules even when the test command fails.
        let status = Command::new("pwsh")
            .args([
                "-NoProfile",
                "-File",
                "ci/artifact-offline.ps1",
                "-Artifact",
            ])
            .arg(&artifact)
            .args([
                "-Sha256",
                &sha,
                "-Build",
                &a.build,
                "-Suites",
                &a.smoke_suites.join(","),
                "-Evidence",
            ])
            .arg(&evidence)
            .status()?;
        return Ok(ExitCode::from(status.code().unwrap_or(2) as u8));
    }
    let exe = std::env::current_exe()?;
    let mut failed = Vec::new();
    for suite in &a.smoke_suites {
        let status = Command::new(&exe)
            .args([
                "test",
                "run",
                "--suite",
                suite,
                "--build",
                &a.build,
                "--artifact",
            ])
            .arg(&artifact)
            .args(["--artifact-sha256", &sha, "--evidence-dir"])
            .arg(evidence.join(suite))
            .env("AWMAN_TEST_HOST_RESOURCES", "offline")
            .status()?;
        if !status.success() {
            failed.push(suite.clone());
        }
    }
    if failed.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("release smoke: failed suites {failed:?}");
        Ok(ExitCode::from(1))
    }
}

/// `build.json`: exact bytes, commit, tree, toolchain, runner image, ABI and
/// the complete expected/actual smoke inventory (design 5.4(e), 7.2).
fn build_record(args: &[String]) -> Result<ExitCode> {
    let (_, a) = load(args)?;
    let artifact = std::path::absolute(flag(args, "--artifact")?)?;
    let version = flag(args, "--version")?;
    let out = std::path::absolute(flag(args, "--out")?)?;
    std::fs::create_dir_all(&out)?;
    let produced: Value = read_json(&artifact.with_file_name("produce.json"))?;
    let sha = sha256_file(&artifact)?;
    ensure!(
        produced["sha256"] == sha.as_str(),
        "{} differs from the produced bytes",
        a.asset
    );
    let commit = git(&["rev-parse", "HEAD"])?;
    ensure!(
        produced["commit"] == commit.as_str(),
        "produced for another commit"
    );
    let tree = git(&["rev-parse", "HEAD^{tree}"])?;
    let smoke = smoke_evidence::read(Path::new(&flag(args, "--evidence")?), &a, &commit, &tree)?;
    let mut version_output = String::new();
    if !a.triple.contains("windows") || cfg!(windows) {
        let o = Command::new(&artifact).arg("--version").output()?;
        version_output = String::from_utf8_lossy(&o.stdout).trim().to_string();
        ensure!(
            version_output
                .split_whitespace()
                .any(|w| w == version.trim_start_matches('v')),
            "{} --version prints `{version_output}`, not {version}",
            a.asset
        );
    }
    std::fs::copy(&artifact, out.join(&a.asset))?;
    write_json(
        &out.join("build.json"),
        &json!({"schema": 1, "asset": a.asset, "sha256": sha, "size": std::fs::metadata(&artifact)?.len(),
                "commit": commit, "tree": git(&["rev-parse", "HEAD^{tree}"])?, "triple": a.triple,
                "toolchain": produced["toolchain"], "runner_image": std::env::var("ImageVersion").unwrap_or_default(),
                "smoke": smoke,
                "abi": if a.abi.is_some() { json!({"ok": produced["abi_ok"] == true, "policy": a.abi}) } else { Value::Null },
                "version_output": version_output}),
    )?;
    Ok(ExitCode::SUCCESS)
}

pub fn run_cmd(cmd: &str, args: &[String]) -> Result<ExitCode> {
    match cmd {
        "produce" => produce(args),
        "kit-pack" => {
            kit_pack(
                Path::new(&flag(args, "--dir")?),
                Path::new(&flag(args, "--out")?),
            )?;
            Ok(ExitCode::SUCCESS)
        }
        "smoke" => smoke(args),
        "produce-native" => {
            let (_, a) = load(args)?;
            println!("{}", a.native.unwrap_or_default());
            Ok(ExitCode::SUCCESS)
        }
        "build-record" => build_record(args),
        _ => bail!("unknown release.yml step {cmd}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_kits_are_deterministic_and_extract_through_the_safe_extractor() {
        let tmp = tempfile::tempdir().unwrap();
        let kit = tmp.path().join("kit");
        std::fs::create_dir_all(kit.join("bin")).unwrap();
        std::fs::write(kit.join("kit.json"), "{}").unwrap();
        std::fs::write(kit.join("bin/xtask"), "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                kit.join("bin/xtask"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let (a, b) = (tmp.path().join("a.tar"), tmp.path().join("b.tar"));
        kit_pack(&kit, &a).unwrap();
        kit_pack(&kit, &b).unwrap();
        assert_eq!(sha256_file(&a).unwrap(), sha256_file(&b).unwrap());
        super::super::native::safe_extract(&a, &tmp.path().join("out")).unwrap();
        assert!(tmp.path().join("out/bin/xtask").is_file());
    }
}
