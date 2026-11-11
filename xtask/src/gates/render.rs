//! Own generated runner/protection files; emit documentation as fragments.
pub mod docs;
pub mod nextest;
pub mod workflows;
use crate::manifest::Manifest;
use anyhow::Result;
use std::{path::Path, process::ExitCode};
pub fn protection(m: &Manifest) -> Result<String> {
    Ok(format!(
        "{}\n",
        serde_json::to_string_pretty(
            &serde_json::json!({"schema":1,"required_checks":[m.gates["pr"].required_check],"merge_queue":true,"main_force_push":false,"main_deletion":false,"settings_verified":false,"pending_cutover":"P2: query actual repository protection before switching legacy statuses"})
        )?
    ))
}
pub fn outputs(m: &Manifest) -> Result<Vec<(String, String)>> {
    let mut out = vec![
        (".config/nextest.toml".into(), nextest::render(m)?),
        ("ci/branch-protection.expected.json".into(), protection(m)?),
    ];
    out.extend(workflows::outputs(m)?);
    Ok(out)
}
pub fn run(root: &Path, check: bool) -> Result<ExitCode> {
    let m = crate::manifest::load(root)?;
    let mut drift = false;
    for (path, text) in outputs(&m)? {
        let path = root.join(path);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(&text) {
            if check {
                eprintln!("generated file drift: {}", path.display());
                drift = true;
            } else {
                if let Some(p) = path.parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::fs::write(path, text)?;
            }
        }
    }
    let fragments = root.join("target/gates-render");
    if !check {
        std::fs::create_dir_all(&fragments)?;
        std::fs::write(fragments.join("devops.md"), docs::render(&m))?;
    }
    Ok(if drift {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}
