//! CI status polling via `gh` CLI or GitHub REST API fallback.

use std::path::Path;
use std::process::Command;

use crate::engine::error::EngineError;

/// Result of a single CI status check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiStatus {
    NotFound,
    Running,
    Success,
    Failed(String),
}

/// Fetch the CI status for the current branch/HEAD from GitHub.
///
/// Primary path: `gh run list` (if `gh` is installed and authenticated).
/// Fallback: GitHub REST API via `reqwest` with `GITHUB_TOKEN`.
pub fn fetch_ci_status(git_root: &Path) -> Result<CiStatus, EngineError> {
    let branch = detect_branch(git_root)?;
    let head_sha = detect_head_sha(git_root)?;

    if gh_is_available() {
        return fetch_via_gh(&branch, &head_sha, git_root);
    }

    let token = match std::env::var("GITHUB_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            return Err(EngineError::Other(
                "poll_ci: neither `gh` CLI (authenticated) nor GITHUB_TOKEN env var is available; \
                 cannot poll CI status"
                    .into(),
            ));
        }
    };

    let (owner, repo) = detect_github_repo(git_root)?;
    fetch_via_api(&owner, &repo, &branch, &head_sha, &token)
}

fn detect_branch(git_root: &Path) -> Result<String, EngineError> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(git_root)
        .output()
        .map_err(|e| EngineError::Other(format!("poll_ci: failed to run git rev-parse: {e}")))?;
    if !output.status.success() {
        return Err(EngineError::Other(
            "poll_ci: git rev-parse --abbrev-ref HEAD failed".into(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn detect_head_sha(git_root: &Path) -> Result<String, EngineError> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(git_root)
        .output()
        .map_err(|e| EngineError::Other(format!("poll_ci: failed to run git rev-parse: {e}")))?;
    if !output.status.success() {
        return Err(EngineError::Other(
            "poll_ci: git rev-parse HEAD failed".into(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn detect_github_repo(git_root: &Path) -> Result<(String, String), EngineError> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(git_root)
        .output()
        .map_err(|e| {
            EngineError::Other(format!("poll_ci: failed to run git remote get-url: {e}"))
        })?;
    if !output.status.success() {
        return Err(EngineError::Other(
            "poll_ci: git remote get-url origin failed; cannot determine GitHub repo".into(),
        ));
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_github_owner_repo(&url)
}

fn parse_github_owner_repo(url: &str) -> Result<(String, String), EngineError> {
    // SSH: git@github.com:owner/repo.git
    // HTTPS: https://github.com/owner/repo.git
    let path = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest.trim_end_matches(".git").to_string()
    } else if url.contains("github.com/") {
        let parts: Vec<&str> = url.splitn(2, "github.com/").collect();
        if parts.len() < 2 {
            return Err(EngineError::Other(format!(
                "poll_ci: cannot parse GitHub owner/repo from remote URL: {url}"
            )));
        }
        parts[1].trim_end_matches(".git").to_string()
    } else {
        return Err(EngineError::Other(format!(
            "poll_ci: remote URL does not appear to be a GitHub URL: {url}"
        )));
    };

    let segments: Vec<&str> = path.splitn(2, '/').collect();
    if segments.len() != 2 || segments[0].is_empty() || segments[1].is_empty() {
        return Err(EngineError::Other(format!(
            "poll_ci: cannot extract owner/repo from: {path}"
        )));
    }
    Ok((segments[0].to_string(), segments[1].to_string()))
}

/// Drive a future to completion from a sync context.
///
/// If a Tokio runtime handle exists for the current thread, reuse it; otherwise
/// build a small current-thread runtime for this one call. This lets
/// `fetch_via_api` use async `reqwest` regardless of whether the caller is
/// inside a runtime (production engine path, integration tests) or completely
/// sync (direct unit tests that don't actually hit the network).
fn run_async<F>(fut: F) -> Result<F::Output, EngineError>
where
    F: std::future::Future,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        return Ok(handle.block_on(fut));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| EngineError::Other(format!("poll_ci: failed to build tokio runtime: {e}")))?;
    Ok(rt.block_on(fut))
}

async fn fetch_workflow_runs_json(
    url: String,
    token: String,
) -> Result<serde_json::Value, EngineError> {
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", "awman")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| EngineError::Other(format!("poll_ci: GitHub API request failed: {e}")))?;

    let status_code = resp.status().as_u16();
    if status_code == 403 || status_code == 429 {
        return Err(EngineError::Other(format!(
            "poll_ci: GitHub API rate limit (HTTP {status_code}); \
             ensure GITHUB_TOKEN has sufficient permissions"
        )));
    }
    if !resp.status().is_success() {
        return Err(EngineError::Other(format!(
            "poll_ci: GitHub API returned HTTP {status_code}"
        )));
    }

    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| EngineError::Other(format!("poll_ci: failed to parse API response: {e}")))
}

fn gh_is_available() -> bool {
    if which::which("gh").is_err() {
        return false;
    }
    Command::new("gh")
        .args(["auth", "status"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn fetch_via_gh(branch: &str, head_sha: &str, git_root: &Path) -> Result<CiStatus, EngineError> {
    let output = Command::new("gh")
        .args([
            "run",
            "list",
            "--branch",
            branch,
            "--json",
            "status,conclusion,name,headSha",
            "--limit",
            "5",
        ])
        .current_dir(git_root)
        .output()
        .map_err(|e| EngineError::Other(format!("poll_ci: gh run list failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(EngineError::Other(format!(
            "poll_ci: gh run list exited {}: {stderr}",
            output.status
        )));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|e| {
        EngineError::Other(format!("poll_ci: failed to parse gh run list JSON: {e}"))
    })?;

    parse_run_list(&json, head_sha)
}

fn fetch_via_api(
    owner: &str,
    repo: &str,
    branch: &str,
    head_sha: &str,
    token: &str,
) -> Result<CiStatus, EngineError> {
    let url = format!(
        "https://api.github.com/repos/{owner}/{repo}/actions/runs?branch={branch}&per_page=5"
    );

    let json = run_async(fetch_workflow_runs_json(url, token.to_string()))??;

    let runs = json
        .get("workflow_runs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            EngineError::Other("poll_ci: unexpected GitHub API response structure".into())
        })?;

    if runs.is_empty() {
        return Ok(CiStatus::NotFound);
    }

    let as_array = serde_json::Value::Array(
        runs.iter()
            .map(|r| {
                serde_json::json!({
                    "status": r.get("status").and_then(|v| v.as_str()).unwrap_or(""),
                    "conclusion": r.get("conclusion").and_then(|v| v.as_str()).unwrap_or(""),
                    "name": r.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    "headSha": r.get("head_sha").and_then(|v| v.as_str()).unwrap_or(""),
                })
            })
            .collect(),
    );

    parse_run_list(&as_array, head_sha)
}

/// Parse the JSON array of runs (same shape from `gh` and from the API adapter)
/// and determine the overall CI status.
fn parse_run_list(json: &serde_json::Value, head_sha: &str) -> Result<CiStatus, EngineError> {
    let runs = json
        .as_array()
        .ok_or_else(|| EngineError::Other("poll_ci: expected JSON array of runs".into()))?;

    if runs.is_empty() {
        return Ok(CiStatus::NotFound);
    }

    // Prefer runs matching HEAD SHA; fall back to most recent run on branch.
    let matching: Vec<&serde_json::Value> = runs
        .iter()
        .filter(|r| {
            r.get("headSha")
                .and_then(serde_json::Value::as_str)
                .map(|s| s == head_sha)
                .unwrap_or(false)
        })
        .collect();

    let all_refs: Vec<&serde_json::Value> = runs.iter().collect();
    let target_runs: &[&serde_json::Value] = if matching.is_empty() {
        &all_refs
    } else {
        &matching
    };

    let mut any_running = false;
    let mut any_failed = false;
    let mut failure_detail = String::new();

    for run in target_runs {
        let status = run
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let conclusion = run
            .get("conclusion")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let name = run
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");

        match status {
            "queued" | "in_progress" | "waiting" | "pending" | "requested" => {
                any_running = true;
            }
            "completed" => match conclusion {
                "success" | "skipped" | "neutral" => {}
                "failure" | "timed_out" | "cancelled" | "action_required" => {
                    any_failed = true;
                    if failure_detail.is_empty() {
                        failure_detail = format!("{name}: {conclusion}");
                    }
                }
                other => {
                    any_failed = true;
                    if failure_detail.is_empty() {
                        failure_detail = format!("{name}: {other}");
                    }
                }
            },
            _ => {
                any_running = true;
            }
        }
    }

    if any_failed {
        Ok(CiStatus::Failed(failure_detail))
    } else if any_running {
        Ok(CiStatus::Running)
    } else {
        Ok(CiStatus::Success)
    }
}

/// Run the full poll_ci loop: poll at `interval_secs` for up to `max_retries`.
///
/// `msg_info` and `msg_warning` are closures for emitting status messages.
/// The first few `NotFound` results are treated as `Running` (the CI run
/// may not have been created yet).
/// Message level for poll_ci status updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollMessage {
    Info,
    Warning,
}

pub fn run_poll_ci_loop(
    git_root: &Path,
    interval_secs: u32,
    max_retries: u32,
    mut on_message: impl FnMut(PollMessage, String),
) -> Result<(), EngineError> {
    let grace_not_found = 3u32.min(max_retries);

    for attempt in 1..=max_retries {
        on_message(
            PollMessage::Info,
            format!("Polling CI (attempt {attempt}/{max_retries})..."),
        );

        let result = fetch_ci_status(git_root)?;

        match result {
            CiStatus::Success => {
                on_message(PollMessage::Info, "CI passed".to_string());
                return Ok(());
            }
            CiStatus::Running => {
                on_message(PollMessage::Info, "CI still running".to_string());
            }
            CiStatus::NotFound if attempt <= grace_not_found => {
                on_message(
                    PollMessage::Info,
                    "No CI run found yet (may not have been created); will retry".to_string(),
                );
            }
            CiStatus::NotFound => {
                return Err(EngineError::Container(
                    "poll_ci: no CI run found for this branch/commit".into(),
                ));
            }
            CiStatus::Failed(detail) => {
                on_message(PollMessage::Warning, format!("CI failed: {detail}"));
                return Err(EngineError::Container(format!(
                    "poll_ci: CI failed: {detail}"
                )));
            }
        }

        if attempt < max_retries {
            std::thread::sleep(std::time::Duration::from_secs(interval_secs.into()));
        }
    }

    Err(EngineError::Container(
        "poll_ci: CI did not complete within max_retries attempts".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_github_ssh_url() {
        let (owner, repo) = parse_github_owner_repo("git@github.com:acme/widgets.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_github_https_url() {
        let (owner, repo) = parse_github_owner_repo("https://github.com/acme/widgets.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_github_https_no_git_suffix() {
        let (owner, repo) = parse_github_owner_repo("https://github.com/acme/widgets").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_non_github_url_is_error() {
        let result = parse_github_owner_repo("https://gitlab.com/acme/widgets.git");
        assert!(result.is_err());
    }

    #[test]
    fn parse_run_list_success() {
        let json = serde_json::json!([
            {"status": "completed", "conclusion": "success", "name": "CI", "headSha": "abc123"}
        ]);
        let result = parse_run_list(&json, "abc123").unwrap();
        assert_eq!(result, CiStatus::Success);
    }

    #[test]
    fn parse_run_list_running() {
        let json = serde_json::json!([
            {"status": "in_progress", "conclusion": "", "name": "CI", "headSha": "abc123"}
        ]);
        let result = parse_run_list(&json, "abc123").unwrap();
        assert_eq!(result, CiStatus::Running);
    }

    #[test]
    fn parse_run_list_failed() {
        let json = serde_json::json!([
            {"status": "completed", "conclusion": "failure", "name": "CI", "headSha": "abc123"}
        ]);
        let result = parse_run_list(&json, "abc123").unwrap();
        assert!(matches!(result, CiStatus::Failed(_)));
    }

    #[test]
    fn parse_run_list_empty() {
        let json = serde_json::json!([]);
        let result = parse_run_list(&json, "abc123").unwrap();
        assert_eq!(result, CiStatus::NotFound);
    }

    #[test]
    fn parse_run_list_prefers_head_sha_match() {
        let json = serde_json::json!([
            {"status": "completed", "conclusion": "failure", "name": "old", "headSha": "old111"},
            {"status": "completed", "conclusion": "success", "name": "new", "headSha": "abc123"}
        ]);
        let result = parse_run_list(&json, "abc123").unwrap();
        assert_eq!(result, CiStatus::Success);
    }

    #[test]
    fn parse_run_list_falls_back_when_no_sha_match() {
        let json = serde_json::json!([
            {"status": "completed", "conclusion": "failure", "name": "CI", "headSha": "old111"}
        ]);
        let result = parse_run_list(&json, "abc123").unwrap();
        assert!(matches!(result, CiStatus::Failed(_)));
    }

    #[test]
    fn parse_run_list_mixed_running_and_success() {
        let json = serde_json::json!([
            {"status": "completed", "conclusion": "success", "name": "lint", "headSha": "abc123"},
            {"status": "in_progress", "conclusion": "", "name": "test", "headSha": "abc123"}
        ]);
        let result = parse_run_list(&json, "abc123").unwrap();
        assert_eq!(result, CiStatus::Running);
    }

    #[test]
    fn parse_run_list_failure_takes_precedence_over_running() {
        let json = serde_json::json!([
            {"status": "in_progress", "conclusion": "", "name": "slow", "headSha": "abc123"},
            {"status": "completed", "conclusion": "failure", "name": "fast", "headSha": "abc123"}
        ]);
        let result = parse_run_list(&json, "abc123").unwrap();
        assert!(matches!(result, CiStatus::Failed(_)));
    }

    // ── Additional status/conclusion coverage ─────────────────────────────────

    #[test]
    fn parse_run_list_queued_status_is_running() {
        let json = serde_json::json!([
            {"status": "queued", "conclusion": null, "name": "CI", "headSha": "abc"}
        ]);
        assert_eq!(parse_run_list(&json, "abc").unwrap(), CiStatus::Running);
    }

    #[test]
    fn parse_run_list_waiting_status_is_running() {
        let json = serde_json::json!([
            {"status": "waiting", "conclusion": null, "name": "CI", "headSha": "abc"}
        ]);
        assert_eq!(parse_run_list(&json, "abc").unwrap(), CiStatus::Running);
    }

    #[test]
    fn parse_run_list_pending_status_is_running() {
        let json = serde_json::json!([
            {"status": "pending", "conclusion": null, "name": "CI", "headSha": "abc"}
        ]);
        assert_eq!(parse_run_list(&json, "abc").unwrap(), CiStatus::Running);
    }

    #[test]
    fn parse_run_list_requested_status_is_running() {
        let json = serde_json::json!([
            {"status": "requested", "conclusion": null, "name": "CI", "headSha": "abc"}
        ]);
        assert_eq!(parse_run_list(&json, "abc").unwrap(), CiStatus::Running);
    }

    #[test]
    fn parse_run_list_unknown_status_treated_as_running() {
        let json = serde_json::json!([
            {"status": "blocked", "conclusion": null, "name": "CI", "headSha": "abc"}
        ]);
        assert_eq!(parse_run_list(&json, "abc").unwrap(), CiStatus::Running);
    }

    #[test]
    fn parse_run_list_cancelled_conclusion_is_failed() {
        let json = serde_json::json!([
            {"status": "completed", "conclusion": "cancelled", "name": "CI", "headSha": "abc"}
        ]);
        assert!(matches!(
            parse_run_list(&json, "abc").unwrap(),
            CiStatus::Failed(_)
        ));
    }

    #[test]
    fn parse_run_list_timed_out_conclusion_is_failed() {
        let json = serde_json::json!([
            {"status": "completed", "conclusion": "timed_out", "name": "CI", "headSha": "abc"}
        ]);
        assert!(matches!(
            parse_run_list(&json, "abc").unwrap(),
            CiStatus::Failed(_)
        ));
    }

    #[test]
    fn parse_run_list_action_required_conclusion_is_failed() {
        let json = serde_json::json!([
            {
                "status": "completed",
                "conclusion": "action_required",
                "name": "CI",
                "headSha": "abc"
            }
        ]);
        assert!(matches!(
            parse_run_list(&json, "abc").unwrap(),
            CiStatus::Failed(_)
        ));
    }

    #[test]
    fn parse_run_list_skipped_conclusion_is_success() {
        let json = serde_json::json!([
            {"status": "completed", "conclusion": "skipped", "name": "CI", "headSha": "abc"}
        ]);
        assert_eq!(parse_run_list(&json, "abc").unwrap(), CiStatus::Success);
    }

    #[test]
    fn parse_run_list_neutral_conclusion_is_success() {
        let json = serde_json::json!([
            {"status": "completed", "conclusion": "neutral", "name": "CI", "headSha": "abc"}
        ]);
        assert_eq!(parse_run_list(&json, "abc").unwrap(), CiStatus::Success);
    }

    #[test]
    fn parse_run_list_unknown_conclusion_is_failed() {
        let json = serde_json::json!([
            {
                "status": "completed",
                "conclusion": "unexpected_value",
                "name": "my-job",
                "headSha": "abc"
            }
        ]);
        let CiStatus::Failed(detail) = parse_run_list(&json, "abc").unwrap() else {
            panic!("expected CiStatus::Failed");
        };
        assert!(
            detail.contains("my-job"),
            "failure detail must include run name: {detail}"
        );
    }

    #[test]
    fn parse_run_list_failure_detail_includes_run_name_and_conclusion() {
        let json = serde_json::json!([
            {
                "status": "completed",
                "conclusion": "failure",
                "name": "my-ci-run",
                "headSha": "abc"
            }
        ]);
        let CiStatus::Failed(detail) = parse_run_list(&json, "abc").unwrap() else {
            panic!("expected Failed");
        };
        assert!(
            detail.contains("my-ci-run"),
            "detail must include run name: {detail}"
        );
        assert!(
            detail.contains("failure"),
            "detail must include conclusion: {detail}"
        );
    }

    #[test]
    fn parse_run_list_cancelled_detail_includes_run_name() {
        let json = serde_json::json!([
            {
                "status": "completed",
                "conclusion": "cancelled",
                "name": "deploy-check",
                "headSha": "abc"
            }
        ]);
        let CiStatus::Failed(detail) = parse_run_list(&json, "abc").unwrap() else {
            panic!("expected Failed");
        };
        assert!(detail.contains("deploy-check"), "detail: {detail}");
        assert!(detail.contains("cancelled"), "detail: {detail}");
    }

    /// Verify that the `fetch_via_api` JSON normalisation (head_sha → headSha)
    /// produces a shape that `parse_run_list` can interpret correctly.
    #[test]
    fn github_api_workflow_runs_json_normalized_and_parsed() {
        let api_response = serde_json::json!({
            "workflow_runs": [
                {
                    "status": "completed",
                    "conclusion": "success",
                    "name": "CI Pipeline",
                    "head_sha": "deadbeef"
                }
            ]
        });

        let runs = api_response
            .get("workflow_runs")
            .and_then(|v| v.as_array())
            .unwrap();

        // Replicate the normalization performed inside fetch_via_api.
        let normalized = serde_json::Value::Array(
            runs.iter()
                .map(|r| {
                    serde_json::json!({
                        "status":     r.get("status")    .and_then(|v| v.as_str()).unwrap_or(""),
                        "conclusion": r.get("conclusion").and_then(|v| v.as_str()).unwrap_or(""),
                        "name":       r.get("name")      .and_then(|v| v.as_str()).unwrap_or(""),
                        "headSha":    r.get("head_sha")  .and_then(|v| v.as_str()).unwrap_or(""),
                    })
                })
                .collect(),
        );

        let result = parse_run_list(&normalized, "deadbeef").unwrap();
        assert_eq!(result, CiStatus::Success);
    }

    #[test]
    fn github_api_empty_workflow_runs_array_is_not_found() {
        let api_response = serde_json::json!({"workflow_runs": []});
        let runs = api_response
            .get("workflow_runs")
            .and_then(|v| v.as_array())
            .unwrap();
        let normalized = serde_json::Value::Array(vec![]);
        let _ = runs; // confirm we consumed the API response
        let result = parse_run_list(&normalized, "any").unwrap();
        assert_eq!(result, CiStatus::NotFound);
    }

    // ── run_poll_ci_loop behaviour (no external git required) ─────────────────

    /// `run_poll_ci_loop` must emit the attempt banner BEFORE calling
    /// `fetch_ci_status`.  We verify this using a path that is not a git
    /// repository so `detect_branch` fails immediately, but the message
    /// callback has already been invoked once.
    #[test]
    fn run_poll_ci_loop_emits_attempt_message_before_fetch_fails() {
        let tmp = tempfile::tempdir().unwrap(); // NOT a git repo
        let mut messages: Vec<(PollMessage, String)> = Vec::new();

        let result = run_poll_ci_loop(tmp.path(), 0, 5, |level, msg| {
            messages.push((level, msg));
        });

        assert!(result.is_err(), "must fail on a non-git directory");
        // The attempt banner is emitted before fetch_ci_status is called.
        assert_eq!(
            messages.len(),
            1,
            "exactly one message before the error propagates"
        );
        assert!(
            messages[0].1.contains("attempt 1/5"),
            "first message must be the attempt banner: {:?}",
            messages[0].1
        );
        assert_eq!(messages[0].0, PollMessage::Info);
    }

    /// Exhausting all retries (CI remains running) must return a "did not
    /// complete" error.  We use a fake git repo + fake `gh` script so that
    /// `fetch_ci_status` always returns `CiStatus::Running`.
    #[test]
    #[cfg(unix)]
    fn real_git_run_poll_ci_loop_exhausts_max_retries_returns_error() {
        if !std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("SKIP: git not available");
            return;
        }

        let _lock = GH_SCRIPT_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let bin_dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_test_git_repo(repo.path());

        // Fake gh: auth status exits 0; run list outputs a running run.
        let script = "#!/bin/sh\n\
                      if [ \"$1\" = 'auth' ] && [ \"$2\" = 'status' ]; then exit 0; fi\n\
                      echo '[{\"status\":\"in_progress\",\"conclusion\":\"\",\
                            \"name\":\"CI\",\"headSha\":\"any\"}]'\n";
        write_executable(bin_dir.path().join("gh"), script);
        write_executable(bin_dir.path().join("which"), "#!/bin/sh\nexit 0\n");

        let orig_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{orig_path}", bin_dir.path().display()));

        let mut messages: Vec<String> = Vec::new();
        let result = run_poll_ci_loop(repo.path(), 0, 3, |_, msg| {
            messages.push(msg);
        });

        std::env::set_var("PATH", orig_path);

        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("max_retries"),
            "error must mention max_retries: {err}"
        );
        // Three attempt banners should have been emitted.
        let attempt_msgs: Vec<_> = messages
            .iter()
            .filter(|m| m.contains("Polling CI"))
            .collect();
        assert_eq!(
            attempt_msgs.len(),
            3,
            "must emit one attempt message per retry: {messages:?}"
        );
    }

    /// When CI succeeds on the first poll, `run_poll_ci_loop` returns `Ok` and
    /// emits a "CI passed" message.
    #[test]
    #[cfg(unix)]
    fn real_git_run_poll_ci_loop_succeeds_on_first_poll() {
        if !std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("SKIP: git not available");
            return;
        }

        let _lock = GH_SCRIPT_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let bin_dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_test_git_repo(repo.path());

        let script = "#!/bin/sh\n\
                      if [ \"$1\" = 'auth' ] && [ \"$2\" = 'status' ]; then exit 0; fi\n\
                      echo '[{\"status\":\"completed\",\"conclusion\":\"success\",\
                            \"name\":\"CI\",\"headSha\":\"any\"}]'\n";
        write_executable(bin_dir.path().join("gh"), script);
        write_executable(bin_dir.path().join("which"), "#!/bin/sh\nexit 0\n");

        let orig_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{orig_path}", bin_dir.path().display()));

        let mut messages: Vec<(PollMessage, String)> = Vec::new();
        let result = run_poll_ci_loop(repo.path(), 0, 5, |level, msg| {
            messages.push((level, msg));
        });

        std::env::set_var("PATH", orig_path);

        assert!(
            result.is_ok(),
            "should succeed when CI passes: {:?}",
            result
        );
        assert!(
            messages.iter().any(|(_, m)| m.contains("CI passed")),
            "must emit 'CI passed' message: {messages:?}"
        );
    }

    /// When CI has failed, `run_poll_ci_loop` returns `Err` and emits a
    /// warning with the failure detail.
    #[test]
    #[cfg(unix)]
    fn real_git_run_poll_ci_loop_fails_when_ci_failed() {
        if !std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("SKIP: git not available");
            return;
        }

        let _lock = GH_SCRIPT_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let bin_dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_test_git_repo(repo.path());

        let script = "#!/bin/sh\n\
                      if [ \"$1\" = 'auth' ] && [ \"$2\" = 'status' ]; then exit 0; fi\n\
                      echo '[{\"status\":\"completed\",\"conclusion\":\"failure\",\
                            \"name\":\"unit-tests\",\"headSha\":\"any\"}]'\n";
        write_executable(bin_dir.path().join("gh"), script);
        write_executable(bin_dir.path().join("which"), "#!/bin/sh\nexit 0\n");

        let orig_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{orig_path}", bin_dir.path().display()));

        let mut warnings: Vec<String> = Vec::new();
        let result = run_poll_ci_loop(repo.path(), 0, 5, |level, msg| {
            if level == PollMessage::Warning {
                warnings.push(msg);
            }
        });

        std::env::set_var("PATH", orig_path);

        assert!(result.is_err(), "should fail when CI fails");
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("CI failed"),
            "error must mention CI failed: {err_str}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("unit-tests")),
            "warning must contain run name 'unit-tests': {warnings:?}"
        );
    }

    /// When gh is unavailable and GITHUB_TOKEN is absent, `fetch_ci_status`
    /// must return an error mentioning both missing authentication paths.
    #[test]
    fn real_git_missing_github_token_and_no_gh_returns_descriptive_error() {
        // Serialise against sibling tests that mutate PATH to install a fake
        // `gh` — without this we can observe their PATH and find a "gh" we
        // shouldn't have.
        let _lock = GH_SCRIPT_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        if !std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("SKIP: git not available");
            return;
        }

        // Only meaningful when gh is NOT available/authenticated.
        if std::process::Command::new("gh")
            .args(["auth", "status"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("SKIP: gh is authenticated; test requires gh to be unavailable");
            return;
        }

        // Only meaningful when GITHUB_TOKEN is absent or empty.
        if std::env::var("GITHUB_TOKEN")
            .map(|t| !t.is_empty())
            .unwrap_or(false)
        {
            eprintln!("SKIP: GITHUB_TOKEN is set; test requires it to be absent");
            return;
        }

        let repo = tempfile::tempdir().unwrap();
        init_test_git_repo(repo.path());

        let err = fetch_ci_status(repo.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("GITHUB_TOKEN"),
            "error must mention GITHUB_TOKEN: {msg}"
        );
        assert!(msg.contains("gh"), "error must mention gh CLI: {msg}");
    }

    // ── Helpers for real-git tests ────────────────────────────────────────────

    /// Serialises tests that mutate the process-wide PATH.
    static GH_SCRIPT_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn init_test_git_repo(dir: &std::path::Path) {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .status()
                .expect("git invocation");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "--initial-branch=main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        run(&["add", "f.txt"]);
        run(&["commit", "-m", "init"]);
    }

    #[cfg(unix)]
    fn write_executable(path: std::path::PathBuf, content: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(&path, content).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
