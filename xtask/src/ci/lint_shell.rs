//! `bash -n` and shellcheck over every tracked script, found by glob and by
//! shebang rather than from a hand-kept list, so a new script is linted the day
//! it is added. Vendored trees and intentionally broken lint fixtures are
//! excluded by name below, with the reason.

use super::{args::Args, verdict};
use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

/// Path components that are never linted: build output, VCS data, node
/// dependencies, vendored upstream trees and vendored upstream trees.
const SKIP_COMPONENTS: [&str; 4] = ["target", ".git", "node_modules", "third_party"];

pub fn skipped(path: &Path) -> bool {
    path.components()
        .any(|c| SKIP_COMPONENTS.contains(&c.as_os_str().to_string_lossy().as_ref()))
}

/// `Some("bash")`/`Some("sh")` when the file is a shell script.
pub fn shell_kind(path: &Path, first_line: &str) -> Option<&'static str> {
    let ext = path.extension().and_then(|e| e.to_str());
    if first_line.starts_with("#!") {
        let l = first_line;
        if l.contains("bash") {
            return Some("bash");
        }
        if l.ends_with("/sh") || l.contains("/sh ") || l.contains("env sh") || l.contains("dash") {
            return Some("sh");
        }
    }
    match ext {
        Some("sh") | Some("bash") => Some("bash"),
        _ => None,
    }
}

fn tracked(root: &Path) -> Result<Vec<PathBuf>> {
    if let Ok(out) = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "-z"])
        .output()
    {
        if out.status.success() && !out.stdout.is_empty() {
            return Ok(out
                .stdout
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| PathBuf::from(String::from_utf8_lossy(s).into_owned()))
                .collect());
        }
    }
    // Not a git checkout (an unpacked source tree): walk it.
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap_or(&p).to_path_buf();
            if skipped(&rel) {
                continue;
            }
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    Ok(out)
}

/// Every script to lint, relative to `root`.
pub fn scripts(root: &Path) -> Result<Vec<(PathBuf, &'static str)>> {
    let mut out = Vec::new();
    for rel in tracked(root)? {
        if skipped(&rel) {
            continue;
        }
        let abs = root.join(&rel);
        let Ok(bytes) = std::fs::read(&abs) else {
            continue;
        };
        let first = String::from_utf8_lossy(bytes.split(|b| *b == b'\n').next().unwrap_or(b""))
            .into_owned();
        if let Some(kind) = shell_kind(&rel, &first) {
            out.push((rel, kind));
        }
    }
    out.sort();
    Ok(out)
}

pub fn run(root: &Path, args: &[String]) -> Result<ExitCode> {
    let a = Args::parse(args, &[], &["allow-missing-shellcheck"])?;
    let list = scripts(root)?;
    anyhow::ensure!(
        !list.is_empty(),
        "found no shell scripts: the glob is broken, not the repository clean"
    );
    let shellcheck_ok = Command::new("shellcheck")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    let mut failures = 0;
    if !shellcheck_ok {
        if a.flag("allow-missing-shellcheck") {
            eprintln!("warning: shellcheck is not installed; only bash -n runs");
        } else {
            anyhow::bail!("shellcheck is not installed (cargo xtask ci install-tool shellcheck); refusing to pass without it");
        }
    }
    for (rel, kind) in &list {
        let syntax = Command::new(kind)
            .arg("-n")
            .arg(rel)
            .current_dir(root)
            .output()
            .with_context(|| format!("running {kind} -n"))?;
        if !syntax.status.success() {
            failures += 1;
            eprintln!(
                "{}: {kind} -n failed:\n{}",
                rel.display(),
                String::from_utf8_lossy(&syntax.stderr)
            );
        }
        if shellcheck_ok {
            let sc = Command::new("shellcheck")
                .args(["--severity=warning", "-s", kind, "-x"])
                .arg(rel)
                .current_dir(root)
                .output()?;
            if !sc.status.success() {
                failures += 1;
                eprintln!(
                    "{}: shellcheck:\n{}",
                    rel.display(),
                    String::from_utf8_lossy(&sc.stdout)
                );
            }
        }
    }
    println!("checked {} scripts, {failures} failing", list.len());
    verdict(failures == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shebang_and_extension_identify_scripts() {
        assert_eq!(
            shell_kind(Path::new("tools/x"), "#!/usr/bin/env bash"),
            Some("bash")
        );
        assert_eq!(shell_kind(Path::new("tools/x"), "#!/bin/sh"), Some("sh"));
        assert_eq!(shell_kind(Path::new("a.sh"), ""), Some("bash"));
        assert_eq!(shell_kind(Path::new("a.rs"), "fn main() {}"), None);
        assert_eq!(
            shell_kind(Path::new("a.py"), "#!/usr/bin/env python3"),
            None
        );
    }

    #[test]
    fn vendored_trees_are_skipped_but_test_scripts_are_linted() {
        assert!(skipped(Path::new("third_party/x/build.sh")));
        assert!(!skipped(Path::new("tests/guest/support/mount-contract.sh")));
        assert!(skipped(Path::new("web/node_modules/y/z.sh")));
        assert!(!skipped(Path::new("tools/install.sh")));
    }

    #[test]
    fn finds_scripts_by_glob_in_an_untracked_tree() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("tools")).unwrap();
        std::fs::write(d.path().join("tools/a.sh"), "#!/usr/bin/env bash\ntrue\n").unwrap();
        std::fs::write(d.path().join("tools/noext"), "#!/bin/sh\ntrue\n").unwrap();
        std::fs::write(d.path().join("tools/readme.md"), "# no\n").unwrap();
        std::fs::create_dir_all(d.path().join("target")).unwrap();
        std::fs::write(d.path().join("target/skip.sh"), "x").unwrap();
        let found: Vec<_> = scripts(d.path())
            .unwrap()
            .into_iter()
            .map(|(p, k)| (p.to_string_lossy().replace('\\', "/"), k))
            .collect();
        assert_eq!(
            found,
            vec![
                ("tools/a.sh".to_string(), "bash"),
                ("tools/noext".to_string(), "sh")
            ]
        );
    }

    #[test]
    fn a_syntax_error_fails_the_lint_even_without_shellcheck() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("bad.sh"), "#!/usr/bin/env bash\nif then fi\n").unwrap();
        let code = run(d.path(), &["--allow-missing-shellcheck".to_string()]).unwrap();
        assert_ne!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
    }
}
