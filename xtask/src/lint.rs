//! Source hygiene and manifest-backed lint dispatch.
pub mod hygiene;
use std::{path::PathBuf, process::ExitCode};
pub fn run(args: &[String]) -> anyhow::Result<ExitCode> {
    match args.first().map(String::as_str) {
        Some("hygiene") => {
            let root = crate::gates::root();
            let path = if args.len() == 3 && args[1] == "--path" {
                PathBuf::from(&args[2])
            } else if args.len() == 1 {
                root.clone()
            } else {
                anyhow::bail!("usage: lint hygiene [--path <subtree>]");
            };
            let meta = crate::gates::discovery::metadata(&root)?;
            let m = crate::manifest::load(&root)?;
            let d = crate::gates::discovery::audit(&root, &m, &meta)?;
            let findings: Vec<_> = d
                .findings
                .iter()
                .filter(|f| {
                    f.contains("H5")
                        && (path == root || f.contains(&path.to_string_lossy().to_string()))
                })
                .collect();
            for f in &findings {
                eprintln!("{f}");
            }
            Ok(if findings.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Some("workflows") => {
            let root = crate::gates::root();
            let m = crate::manifest::load(&root)?;
            let errors = crate::gates::render::workflows::verify(&root, &m);
            for e in &errors {
                eprintln!("{e}");
            }
            Ok(if errors.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Some("pins") => {
            let m = crate::manifest::load(&crate::gates::root())?;
            let errors = crate::manifest::validate::pins(&m);
            for e in &errors {
                eprintln!("{e}");
            }
            Ok(if errors.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        // The shell and NOTICE lints are implemented with the CI helpers.
        Some("shell") => crate::ci::run(
            &std::iter::once("lint-shell".to_string())
                .chain(args[1..].iter().cloned())
                .collect::<Vec<_>>(),
        ),
        Some("notice") => crate::ci::run(&["lint-notice".to_string()]),
        Some("all") => all(),
        _ => {
            eprintln!("usage: cargo xtask lint hygiene [--path <subtree>] | shell | workflows | pins | notice | all");
            Ok(ExitCode::from(2))
        }
    }
}

/// Every `lint-*` suite of the manifest, in order, plus `gates check`: the
/// local equivalent of the pr gate's lint cell (`make check`). Two tools that
/// CI installs from pinned sources may be missing on a developer machine:
/// without shellcheck the shell lint still runs `bash -n`, and without
/// cargo-deny that suite is reported as not run. CI runs each suite on its own
/// and has neither exemption.
fn all() -> anyhow::Result<ExitCode> {
    use std::process::Command;
    let root = crate::gates::root();
    let m = crate::manifest::load(&root)?;
    let have = |program: &str, arg: &str| {
        Command::new(program)
            .arg(arg)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    let mut failed = Vec::new();
    let mut not_run = Vec::new();
    let mut steps: Vec<(String, Vec<String>)> = m
        .suites
        .iter()
        .filter(|(id, s)| id.starts_with("lint-") && s.kind == "command")
        .map(|(id, s)| (id.clone(), s.command.clone()))
        .collect();
    steps.push((
        "matrix-check".into(),
        ["cargo", "xtask", "matrix", "check"]
            .map(String::from)
            .to_vec(),
    ));

    steps.push((
        "gates-check".into(),
        ["cargo", "xtask", "gates", "check"]
            .map(String::from)
            .to_vec(),
    ));
    for (id, mut command) in steps {
        if command.get(1).map(String::as_str) == Some("deny") && !have("cargo-deny", "--version") {
            not_run.push(format!("{id} (cargo-deny not installed; CI runs it)"));
            continue;
        }
        if id == "lint-shell" && !have("shellcheck", "--version") {
            eprintln!(
                "xtask lint all: shellcheck not installed; running bash -n only (CI runs both)"
            );
            command.push("--allow-missing-shellcheck".into());
        }
        eprintln!("xtask lint all: {id}: {}", command.join(" "));
        let (program, rest) = command
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("suite {id} has no command"))?;
        let status = Command::new(program)
            .args(rest)
            .current_dir(&root)
            .status()?;
        if !status.success() {
            failed.push(id);
        }
    }
    for n in &not_run {
        eprintln!("xtask lint all: not run: {n}");
    }
    if failed.is_empty() {
        eprintln!("xtask lint all: passed");
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("xtask lint all: failed: {}", failed.join(", "));
        Ok(ExitCode::from(1))
    }
}
