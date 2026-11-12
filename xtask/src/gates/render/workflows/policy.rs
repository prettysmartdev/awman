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
fn allowed_write(path: &str, job: &str, scope: &str) -> bool {
    matches!(
        (path.rsplit('/').next().unwrap_or(path), job, scope),
        (
            "candidate.yml",
            "preflight" | "assemble" | "gate" | "cleanup" | "promote",
            "contents"
        ) | (
            "candidate.yml",
            "controller" | "build" | "gate",
            "id-token" | "attestations"
        ) | ("promote.yml", "verify-inputs" | "receipt", "contents")
            | ("fixtures.yml", "publish", "contents")
            | ("nightly.yml", "flake-hunt", "issues")
            | (
                "nightly.yml",
                "rehearsal",
                "contents" | "id-token" | "attestations"
            )
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
        ("candidate.yml", "runner-preflight", "preflight") => {
            Some("candidate trigger/source admission")
        }
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
        let hardware = labels.contains("self-hosted") || labels.contains("${{");
        if hardware {
            let guard = job["if"].as_str().unwrap_or("");
            if !guard.contains("github.ref == 'refs/heads/main'") {
                out.push(format!(
                    "{path}: self-hosted/dynamic job {id} lacks protected-main guard"
                ));
            }
            if path.ends_with("ci.yml") {
                if !guard.contains("outputs.hardware == 'true'")
                    || !guard.contains("github.event_name != 'pull_request'")
                    || !guard.contains("github.event_name != 'merge_group'")
                {
                    out.push(format!(
                        "{path}: self-hosted job {id} lacks event/hardware admission guards"
                    ));
                }
            } else if job["environment"].as_str() != Some("release") {
                out.push(format!(
                    "{path}: self-hosted/dynamic job {id} requires release environment"
                ));
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
        assert!(errors.contains("protected-main guard"));
        assert!(errors.contains("release environment"));
        assert!(errors.contains("needs a without"));
        assert!(errors.contains("nonexistent job absent"));
    }
    #[test]
    fn consumed_outputs_and_verdicts_need_no_artifact_transfer() {
        assert!(verify("w.yml", "permissions: {contents: read}\njobs:\n  a: {}\n  x:\n    needs: a\n    if: ${{ needs.a.result == 'success' }}\n").is_empty());
    }
}
