//! Runner health. The first xtask step of every self-hosted job: an unhealthy
//! machine reports `BLOCKED: <reason>` at once instead of failing deep inside
//! an expensive step (or never being scheduled).

use super::{args::Args, verdict};
use crate::manifest::{self, Manifest};
use anyhow::{Context, Result};
use std::{
    path::Path,
    process::{Command, ExitCode},
};

#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub free_gib: Option<u64>,
    pub hv_support: Option<String>,
    pub rustc: Option<String>,
    pub os: String,
}

#[derive(Debug, Clone)]
pub struct Needs {
    pub min_free_gib: u64,
    pub hypervisor: bool,
    pub toolchain: String,
    pub macos: bool,
}

pub fn needs(m: &Manifest, platform: &str) -> Result<Needs> {
    let p = m
        .platforms
        .get(platform)
        .with_context(|| format!("unknown platform {platform}"))?;
    Ok(Needs {
        min_free_gib: 30,
        hypervisor: p.provides.iter().any(|c| c == "hypervisor"),
        toolchain: p.toolchain.clone(),
        macos: p.triple.contains("apple-darwin"),
    })
}

/// Reasons the machine cannot serve the job (empty = healthy).
pub fn evaluate(f: &Facts, n: &Needs) -> Vec<String> {
    let mut why = Vec::new();
    match f.free_gib {
        Some(g) if g >= n.min_free_gib => {}
        Some(g) => why.push(format!(
            "only {g} GiB free in the work directory (need {})",
            n.min_free_gib
        )),
        None => why.push("cannot determine free disk space".into()),
    }
    match &f.rustc {
        Some(v) if v.starts_with(&format!("rustc {}", n.toolchain)) => {}
        Some(v) => why.push(format!(
            "toolchain is `{v}`, the manifest pins {}",
            n.toolchain
        )),
        None => why.push("rustc is not runnable".into()),
    }
    if n.macos && f.os != "macos" {
        why.push(format!("expected a macOS host, found {}", f.os));
    }
    if n.hypervisor && n.macos && f.hv_support.as_deref() != Some("1") {
        why.push(format!(
            "kern.hv_support is {:?}, Hypervisor.framework is unavailable",
            f.hv_support
        ));
    }
    why
}

fn cmd_out(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn facts() -> Facts {
    let free_gib = cmd_out("df", &["-Pk", "."]).and_then(|t| {
        let kib: u64 = t.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
        Some(kib / 1024 / 1024)
    });
    Facts {
        free_gib,
        hv_support: cmd_out("sysctl", &["-n", "kern.hv_support"]),
        rustc: cmd_out("rustc", &["--version"]),
        os: std::env::consts::OS.into(),
    }
}

pub fn run(root: &Path, args: &[String]) -> Result<ExitCode> {
    let a = Args::parse(args, &["platform"], &[])?;
    let m = manifest::load(root)?;
    let platform = a.need("platform")?;
    let why = evaluate(&facts(), &needs(&m, platform)?);
    for w in &why {
        eprintln!("BLOCKED: runner for {platform}: {w}");
        println!("::error title=Runner blocked::{w}");
    }
    verdict(why.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac_needs() -> Needs {
        Needs {
            min_free_gib: 30,
            hypervisor: true,
            toolchain: "1.94.0".into(),
            macos: true,
        }
    }
    fn healthy() -> Facts {
        Facts {
            free_gib: Some(80),
            hv_support: Some("1".into()),
            rustc: Some("rustc 1.94.0 (abc 2026-01-01)".into()),
            os: "macos".into(),
        }
    }

    #[test]
    fn a_healthy_mac_has_no_findings() {
        assert!(evaluate(&healthy(), &mac_needs()).is_empty());
    }

    #[test]
    fn every_unhealthy_fact_is_a_named_reason() {
        let mut f = healthy();
        f.free_gib = Some(3);
        f.hv_support = Some("0".into());
        f.rustc = Some("rustc 1.80.0".into());
        let why = evaluate(&f, &mac_needs());
        assert_eq!(why.len(), 3, "{why:?}");
        assert!(why.iter().any(|w| w.contains("GiB")));
        assert!(why.iter().any(|w| w.contains("hv_support")));
        assert!(why.iter().any(|w| w.contains("toolchain")));
    }

    #[test]
    fn unknown_facts_block_rather_than_pass() {
        let why = evaluate(
            &Facts {
                os: "macos".into(),
                ..Facts::default()
            },
            &mac_needs(),
        );
        assert!(why.len() >= 3, "{why:?}");
    }

    #[test]
    fn manifest_ci_guest_platform_needs_the_hypervisor() {
        let m = manifest::load(&crate::gates::root()).unwrap();
        let n = needs(&m, "macos-arm64-ci-guest").unwrap();
        assert!(n.hypervisor && n.macos);
        assert!(!needs(&m, "linux-x64").unwrap().hypervisor);
    }
}
