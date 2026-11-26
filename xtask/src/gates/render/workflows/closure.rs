//! Local reusable-workflow calls cannot increase their caller's token grants.
use serde_yaml::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
fn grants(v: &Value) -> BTreeMap<String, u8> {
    v.as_mapping()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| {
            Some((
                k.as_str()?.into(),
                match v.as_str()? {
                    "read" => 1,
                    "write" => 2,
                    _ => 0,
                },
            ))
        })
        .collect()
}
fn requirements(
    root: &Path,
    path: &str,
    stack: &mut BTreeSet<String>,
) -> anyhow::Result<BTreeMap<String, u8>> {
    anyhow::ensure!(
        stack.insert(path.into()),
        "reusable workflow cycle at {path}"
    );
    let doc: Value = serde_yaml::from_str(&std::fs::read_to_string(root.join(path))?)?;
    let mut needed = BTreeMap::new();
    for (_, job) in doc["jobs"]
        .as_mapping()
        .ok_or_else(|| anyhow::anyhow!("{path}: no jobs"))?
    {
        let effective = grants(job.get("permissions").unwrap_or(&doc["permissions"]));
        if let Some(child) = job["uses"].as_str().and_then(|s| s.strip_prefix("./")) {
            anyhow::ensure!(
                child.starts_with(".github/workflows/") && !child.contains(".."),
                "unsafe local workflow path"
            );
            let nested = requirements(root, child, stack)?;
            for (scope, level) in nested {
                anyhow::ensure!(
                    effective.get(&scope).copied().unwrap_or(0) >= level,
                    "{path}: local call {child} does not grant required {scope}"
                );
            }
        }
        for (scope, level) in effective {
            needed
                .entry(scope)
                .and_modify(|n: &mut u8| *n = (*n).max(level))
                .or_insert(level);
        }
    }
    stack.remove(path);
    Ok(needed)
}
pub fn verify(root: &Path) -> Vec<String> {
    let mut errors = Vec::new();
    let Ok(entries) = std::fs::read_dir(root.join(".github/workflows")) else {
        return vec!["workflow directory missing".into()];
    };
    for entry in entries.flatten() {
        if !matches!(
            entry.path().extension().and_then(|s| s.to_str()),
            Some("yml" | "yaml")
        ) {
            continue;
        }
        let path = format!(".github/workflows/{}", entry.file_name().to_string_lossy());
        if let Err(e) = requirements(root, &path, &mut BTreeSet::new()) {
            errors.push(format!("workflow closure: {e:#}"));
        }
    }
    errors
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reusable_call_cannot_omit_a_nested_read_scope() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".github/workflows")).unwrap();
        let caller = tmp.path().join(".github/workflows/nightly.yml");
        std::fs::write(&caller,"permissions: {contents: read}\njobs:\n  full:\n    uses: ./.github/workflows/ci.yml\n    permissions: {contents: read, actions: read}\n").unwrap();
        std::fs::write(tmp.path().join(".github/workflows/ci.yml"),"permissions: {contents: read}\njobs:\n  plan:\n    permissions: {contents: read, actions: read, pull-requests: read}\n").unwrap();
        assert!(verify(tmp.path()).join(" ").contains("pull-requests"));
        let input = std::fs::read_to_string(&caller)
            .unwrap()
            .replace("actions: read}", "actions: read, pull-requests: read}");
        std::fs::write(caller, input).unwrap();
        assert!(verify(tmp.path()).is_empty());
    }
}
