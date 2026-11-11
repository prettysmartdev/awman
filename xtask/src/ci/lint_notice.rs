//! `NOTICE.third-party.md` must agree with `Cargo.lock`: every carried patch
//! (a lock package with no registry source that lives in `third_party/`) is
//! named at its locked version, no stale version is named, the embedded-runtime
//! pin (`Microsandbox vX`) and the `msb_krun =X` pin equal the lock, and the
//! notice itself is present.

use super::verdict;
use anyhow::{Context, Result};
use std::{collections::BTreeMap, path::Path, process::ExitCode};

#[derive(Debug, PartialEq, Eq)]
pub struct Pkg {
    pub name: String,
    pub version: String,
    pub registry: bool,
}

pub fn lock_packages(lock: &str) -> Result<Vec<Pkg>> {
    let v: toml::Value = toml::from_str(lock).context("parsing Cargo.lock")?;
    Ok(v["package"]
        .as_array()
        .context("Cargo.lock has no packages")?
        .iter()
        .map(|p| Pkg {
            name: p["name"].as_str().unwrap_or("").into(),
            version: p["version"].as_str().unwrap_or("").into(),
            registry: p.get("source").is_some(),
        })
        .collect())
}

/// Findings; empty means consistent. `carried` lists the `third_party/` directory
/// names that exist (e.g. `microsandbox-0.7.8`).
pub fn check(notice: &str, pkgs: &[Pkg], carried: &[String]) -> Vec<String> {
    let mut errors = Vec::new();
    let by_name: BTreeMap<&str, Vec<&Pkg>> = pkgs.iter().fold(BTreeMap::new(), |mut m, p| {
        m.entry(p.name.as_str()).or_default().push(p);
        m
    });
    for dir in carried {
        let Some(pkg) = pkgs
            .iter()
            .find(|p| !p.registry && format!("{}-{}", p.name, p.version) == *dir)
        else {
            errors.push(format!("third_party/{dir} is not a package of Cargo.lock at that version (stale directory?)"));
            continue;
        };
        if !notice.contains(&format!("third_party/{dir}")) {
            errors.push(format!(
                "NOTICE.third-party.md does not name carried patch third_party/{dir} ({} {})",
                pkg.name, pkg.version
            ));
        }
    }
    // A mentioned third_party/<name>-<version> must be a real carried directory.
    for (i, _) in notice.match_indices("third_party/") {
        let word: String = notice[i + "third_party/".len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.' || *c == '_')
            .collect();
        let word = word.trim_end_matches('.');
        if word.chars().next_back().is_some_and(|c| c.is_ascii_digit())
            && word.contains('-')
            && !carried.iter().any(|d| d == word)
        {
            errors.push(format!("NOTICE.third-party.md names third_party/{word}, which does not exist at that version"));
        }
    }
    if let Some(ms) = by_name.get("microsandbox").and_then(|v| v.first()) {
        if !notice.contains(&format!("Microsandbox v{}", ms.version)) {
            errors.push(format!(
                "NOTICE.third-party.md does not name `Microsandbox v{}` (the locked version)",
                ms.version
            ));
        }
    }
    if let Some(k) = by_name.get("msb_krun").and_then(|v| v.first()) {
        if !notice.contains(&format!("msb_krun =`{}", k.version))
            && !notice.contains(&format!("msb_krun ={}", k.version))
        {
            errors.push(format!(
                "NOTICE.third-party.md does not pin `msb_krun =` {} (the locked version)",
                k.version
            ));
        }
    }
    errors
}

pub fn run(root: &Path) -> Result<ExitCode> {
    let notice = std::fs::read_to_string(root.join("NOTICE.third-party.md"))
        .context("NOTICE.third-party.md is missing")?;
    let pkgs = lock_packages(&std::fs::read_to_string(root.join("Cargo.lock"))?)?;
    let carried: Vec<String> = std::fs::read_dir(root.join("third_party"))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().join("Cargo.toml").is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let errors = check(&notice, &pkgs, &carried);
    for e in &errors {
        eprintln!("notice: {e}");
    }
    verdict(errors.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkgs() -> Vec<Pkg> {
        vec![
            Pkg {
                name: "microsandbox".into(),
                version: "0.7.8".into(),
                registry: false,
            },
            Pkg {
                name: "msb_krun".into(),
                version: "0.1.41".into(),
                registry: true,
            },
            Pkg {
                name: "serde".into(),
                version: "1.0.0".into(),
                registry: true,
            },
        ]
    }
    const GOOD: &str = "Microsandbox v0.7.8 [SDK](third_party/microsandbox-0.7.8/PATCH.md). `msb_krun =0.1.41` is unmodified.";

    #[test]
    fn consistent_notice_passes() {
        assert!(check(GOOD, &pkgs(), &["microsandbox-0.7.8".into()]).is_empty());
    }

    #[test]
    fn a_version_bump_without_a_notice_update_fails() {
        let mut p = pkgs();
        p[0].version = "0.7.9".into();
        let e = check(GOOD, &p, &["microsandbox-0.7.9".into()]);
        assert!(
            e.iter().any(|e| e.contains("does not name carried patch")),
            "{e:?}"
        );
        assert!(e.iter().any(|e| e.contains("Microsandbox v0.7.9")), "{e:?}");
        let e = check(GOOD, &pkgs(), &[]);
        assert!(
            e.iter()
                .any(|e| e.contains("does not exist at that version")),
            "{e:?}"
        );
    }

    #[test]
    fn a_stale_krun_pin_fails() {
        let mut p = pkgs();
        p[1].version = "0.1.42".into();
        assert!(check(GOOD, &p, &["microsandbox-0.7.8".into()])
            .iter()
            .any(|e| e.contains("msb_krun")));
    }

    #[test]
    fn the_repository_notice_matches_its_lockfile() {
        let root = crate::gates::root();
        let notice = std::fs::read_to_string(root.join("NOTICE.third-party.md")).unwrap();
        let pkgs =
            lock_packages(&std::fs::read_to_string(root.join("Cargo.lock")).unwrap()).unwrap();
        let carried: Vec<String> = std::fs::read_dir(root.join("third_party"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().join("Cargo.toml").is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(check(&notice, &pkgs, &carried), Vec::<String>::new());
    }
}
