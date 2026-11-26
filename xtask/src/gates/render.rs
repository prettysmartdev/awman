//! Own generated runner files; emit documentation as fragments.
pub mod docs;
pub mod nextest;
pub mod workflows;
use crate::manifest::Manifest;
use anyhow::Result;
use std::{path::Path, process::ExitCode};
pub fn outputs(m: &Manifest) -> Result<Vec<(String, String)>> {
    let mut out = vec![(".config/nextest.toml".into(), nextest::render(m)?)];
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
    if !check {
        for name in ["cicd", "testing", "localdev"] {
            let path = root.join(format!("aspec/devops/{name}.md"));
            let text = std::fs::read_to_string(&path)?;
            let start = text
                .find("<!-- gates:begin contract -->")
                .ok_or_else(|| anyhow::anyhow!("missing generated block"))?;
            let end = text
                .find("<!-- gates:end contract -->")
                .ok_or_else(|| anyhow::anyhow!("missing generated block end"))?
                + "<!-- gates:end contract -->".len();
            std::fs::write(
                path,
                format!(
                    "{}{}{}",
                    &text[..start],
                    docs::render(&m).trim_end(),
                    &text[end..]
                ),
            )?;
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
