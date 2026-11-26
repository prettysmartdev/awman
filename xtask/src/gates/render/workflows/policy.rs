//! Workflow security checks operate on YAML nodes, including resolved aliases.
//! Job dependencies may carry artifacts, outputs, or explicitly consumed verdicts;
//! a bare ordering edge needs a named rationale below.
use serde_yaml::Value;

fn text(v: &Value) -> String {
    serde_yaml::to_string(v).unwrap_or_default()
}
fn strings(v: &Value) -> Vec<&str> {
    if let Some(s) = v.as_str() {
        vec![s]
    } else {
        v.as_sequence()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect()
    }
}
/// A release may choose its hosted OS with a finite literal mapping. An
/// arbitrary output/expression is not evidence that a runner is hosted.
fn hosted_mapping(labels: &str) -> bool {
    let Some(expr) = labels
        .trim()
        .strip_prefix("${{")
        .and_then(|s| s.strip_suffix("}}"))
    else {
        return false;
    };
    let hosted = [
        "ubuntu-22.04",
        "ubuntu-24.04",
        "ubuntu-24.04-arm",
        "macos-15",
        "windows-2025",
    ];
    let literal = |s: &str| {
        s.trim()
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .map(str::to_owned)
    };
    expr.split("||").all(|arm| {
        if let Some((cond, runner)) = arm.split_once("&&") {
            let Some(asset) = cond
                .trim()
                .strip_prefix("inputs.asset == ")
                .and_then(literal)
            else {
                return false;
            };
            [
                "awman-linux-amd64",
                "awman-linux-arm64",
                "awman-macos-arm64",
                "awman-windows-amd64.exe",
            ]
            .contains(&asset.as_str())
                && literal(runner).is_some_and(|s| hosted.contains(&s.as_str()))
        } else {
            literal(arm).is_some_and(|s| hosted.contains(&s.as_str()))
        }
    })
}
fn allowed_write(path: &str, job: &str, scope: &str) -> bool {
    matches!(
        (path.rsplit('/').next().unwrap_or(path), job, scope),
        ("release.yml", "produce", "id-token" | "attestations")
            | ("fixtures.yml", "publish", "contents")
            | ("fixtures.yml", "build", "id-token" | "attestations")
            | ("nightly.yml", "flake-hunt", "issues")
    )
}
fn permissions(path: &str, job: &str, value: &Value, out: &mut Vec<String>) {
    if let Some(map) = value.as_mapping() {
        for (scope, level) in map {
            let scope = scope.as_str().unwrap_or("");
            let level = level.as_str().unwrap_or("");
            if !["none", "read", "write"].contains(&level)
                || (level == "write" && !allowed_write(path, job, scope))
            {
                out.push(format!(
                    "{path}: job {job} has unreviewed permission {scope}={level}"
                ));
            }
        }
    } else if value.as_str() != Some("read-all") {
        out.push(format!(
            "{path}: job {job} requires explicit read-only or reviewed permission mapping"
        ));
    }
}
fn ordering_reason(path: &str, job: &str, need: &str) -> Option<&'static str> {
    match (path.rsplit('/').next().unwrap_or(path), job, need) {
        // These are admission edges. Their jobs build from the checked-out commit,
        // not a preceding job's compiler output.
        // Trends intentionally sample only completed runs; full must finish first.
        ("nightly.yml", "ci-trends", "full") => Some("completed-run sampling barrier"),
        _ => None,
    }
}

pub fn verify(path: &str, input: &str) -> Vec<String> {
    let doc: Value = match serde_yaml::from_str(input) {
        Ok(v) => v,
        Err(e) => return vec![format!("{path}: invalid workflow YAML: {e}")],
    };
    let mut out = Vec::new();
    let triggers = &doc["on"];
    if triggers.get("pull_request_target").is_some()
        || strings(triggers).contains(&"pull_request_target")
    {
        out.push(format!("{path}: pull_request_target is forbidden"));
    }
    permissions(path, "<workflow>", &doc["permissions"], &mut out);
    let Some(jobs) = doc["jobs"].as_mapping() else {
        out.push(format!("{path}: missing jobs mapping"));
        return out;
    };
    for (id, job) in jobs {
        let id = id.as_str().unwrap_or("<invalid>");
        let effective = job.get("permissions").unwrap_or(&doc["permissions"]);
        permissions(path, id, effective, &mut out);
        let body = text(job);
        let labels = text(&job["runs-on"]);
        let hardware = labels.contains("self-hosted")
            || (labels.contains("${{")
                && !(path.ends_with("release.yml")
                    && hosted_mapping(job["runs-on"].as_str().unwrap_or(""))));
        if hardware {
            out.push(format!(
                "{path}: self-hosted/dynamic job {id} is forbidden; use explicit hosted labels"
            ));
        }
        if job.get("environment").is_some() {
            out.push(format!("{path}: job {id} must not require an environment"));
        }
        if path.ends_with("release.yml") {
            if body.contains("secrets.")
                || body.contains("secrets[")
                || job.get("secrets").is_some()
            {
                out.push(format!("{path}: job {id} must not consume secrets"));
            }
            for step in job["steps"].as_sequence().into_iter().flatten() {
                if step["uses"].as_str().is_some_and(|s| s.starts_with("./")) {
                    out.push(format!("{path}: job {id} must not execute local actions"));
                }
            }
        }
        let download = body.contains("actions/download-artifact@")
            || body.contains("gh run download")
            || body.contains("*download");
        for need in strings(&job["needs"]) {
            if !jobs.contains_key(Value::String(need.into())) {
                out.push(format!("{path}: job {id} needs nonexistent job {need}"));
            }
            let consumes = download
                || body.contains(&format!("needs.{need}.outputs"))
                || body.contains(&format!("needs.{need}.result"))
                || body.contains("toJSON(needs)")
                || body.contains("toJson(needs)");
            if !consumes && ordering_reason(path, id, need).is_none() {
                out.push(format!("{path}: job {id} needs {need} without artifacts, outputs, verdict, or reviewed ordering reason"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unreviewed_write_scopes_and_privileged_trigger() {
        let errors = verify("w.yml", "on: [pull_request_target]\npermissions: write-all\njobs:\n  x:\n    permissions: {contents: write}\n").join("\n");
        assert!(errors.contains("pull_request_target"));
        assert!(errors.contains("explicit read-only"));
        assert!(errors.contains("contents=write"));
    }
    #[test]
    fn rejects_unprotected_hardware_and_unconsumed_dependencies() {
        let errors = verify("w.yml", "permissions: {contents: read}\njobs:\n  a: {}\n  x:\n    needs: [a, absent]\n    runs-on: [self-hosted, macOS]\n").join("\n");
        assert!(errors.contains("self-hosted/dynamic"));
        assert!(errors.contains("needs a without"));
        assert!(errors.contains("nonexistent job absent"));
    }
    #[test]
    fn consumed_outputs_and_verdicts_need_no_artifact_transfer() {
        assert!(verify("w.yml", "permissions: {contents: read}\njobs:\n  a: {}\n  x:\n    needs: a\n    if: ${{ needs.a.result == 'success' }}\n").is_empty());
    }
}

#[cfg(test)]
mod hosted_mapping_tests {
    use super::*;
    #[test]
    fn only_literal_hosted_choices_are_admitted() {
        assert!(hosted_mapping(
            "${{ inputs.asset == 'awman-macos-arm64' && 'macos-15' || 'ubuntu-22.04' }}"
        ));
        assert!(!hosted_mapping("${{ inputs.runner }}"));
        assert!(!hosted_mapping(
            "${{ inputs.asset == 'awman-macos-arm64' && 'self-hosted' || 'ubuntu-22.04' }}"
        ));
    }
}
