//! CI status polling via `gh` CLI or GitHub REST API fallback.

use std::path::Path;
use std::sync::Arc;

use crate::data::ci_poll_event::CiPollEvent;
use crate::engine::auth::{
    credential::SecretString,
    github_token::{GhAuthStatus, GitHubTokenResolver, GitHubTokenResolverOptions},
    host_secret_command::HostStdin,
    redaction::RedactionSet,
};
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::remote::{HttpClientOptions, HttpCore};

/// Result of a single CI status check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiStatus {
    NotFound,
    Running,
    Success,
    Failed(String),
}

fn parse_github_owner_repo(url: &str) -> Result<(String, String), EngineError> {
    let remote = crate::data::workflow_prerequisites::GitRemoteUrl::parse(url);
    if remote.is_github_com() {
        if let Some((owner, repo)) = remote.owner_repo {
            if [&owner, &repo].iter().all(|part| {
                part.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            }) {
                return Ok((owner, repo));
            }
        }
    }
    Err(EngineError::Other(
        "poll_ci: cannot extract a GitHub owner/repo from the remote URL".into(),
    ))
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
    token: SecretString,
) -> Result<serde_json::Value, EngineError> {
    HttpCore::refuse_public_host_under_test_isolation(&url)
        .map_err(|e| EngineError::Other(format!("poll_ci: {e}")))?;
    // The shared builder with every option left at its default, which is the
    // untimed client this poller has always used (WI 0114 F-28).
    let client = HttpCore::client(&HttpClientOptions::default())
        .map_err(|e| EngineError::Other(format!("poll_ci: {e}")))?;
    let resp = client
        .get(&url)
        .header("Authorization", {
            let mut header =
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.expose()))
                    .map_err(|_| EngineError::Other("poll_ci: invalid token header".into()))?;
            header.set_sensitive(true);
            header
        })
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

fn fetch_via_api(
    host: &str,
    owner: &str,
    repo: &str,
    branch: &str,
    head_sha: &str,
    token: &str,
) -> Result<CiStatus, EngineError> {
    let base = if host == "github.com" {
        "https://api.github.com".into()
    } else {
        format!("https://{host}/api/v3")
    };
    let url = format!("{base}/repos/{owner}/{repo}/actions/runs?branch={branch}&per_page=5");

    let json = run_async(fetch_workflow_runs_json(url, SecretString::new(token)))??;

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

// `PollMessage` is gone (WI 0114 F-45): the poller reports a typed
// `CiPollEvent` and the frontend decides the level and the wording. The doc
// block that sat here described `msg_info`/`msg_warning` closures that had
// already been replaced by the single `on_message` callback; it is now on
// `CiPoller::poll`, describing the parameters that actually exist.

/// Polls GitHub for the CI status of the current branch/HEAD.
///
/// Typed rather than a pair of free functions (Tenet 3), and holding its two
/// collaborators rather than reaching for them: branch, HEAD SHA and remote
/// URL come from [`GitEngine`] instead of three ad-hoc `git` shells, and the
/// GitHub token is supplied at construction instead of read from the
/// environment here. The command layer resolves and grants the credential.
///
/// The `reqwest` client stays local to this type until F-28 gives the engine
/// a shared one.
pub struct CiPoller {
    git: Arc<GitEngine>,
    token: Option<SecretString>,
    destination: Option<String>,
    refused_binaries: Vec<String>,
    redaction: RedactionSet,
}

impl CiPoller {
    pub fn new(git: Arc<GitEngine>, token: Option<SecretString>) -> Self {
        Self {
            git,
            token,
            destination: None,
            refused_binaries: vec![],
            redaction: RedactionSet::new(),
        }
    }
    pub fn with_destination(mut self, destination: Option<String>) -> Self {
        self.destination = destination;
        self
    }
    pub fn with_host_policy(
        mut self,
        refused_binaries: Vec<String>,
        redaction: RedactionSet,
    ) -> Self {
        self.refused_binaries = refused_binaries;
        self.redaction = redaction;
        self
    }

    /// Fetch the CI status for `git_root`'s current branch/HEAD.
    ///
    /// An explicit token selects the GitHub REST API. Otherwise use authenticated
    /// host `gh`; no environment token is implicitly granted by this poller.
    pub fn fetch_status(&self, git_root: &Path) -> Result<CiStatus, EngineError> {
        let remote_url = self
            .git
            .remote_url_redacted(git_root, "origin")?
            .ok_or_else(|| EngineError::Other("poll_ci: origin remote is missing".into()))?;
        if self
            .destination
            .as_ref()
            .is_some_and(|approved| approved != &remote_url)
        {
            return Err(EngineError::Other(
                "poll_ci: destination changed since prerequisite approval".into(),
            ));
        }
        // An ungranted host-gh login is only a github.com exception. Never let
        // checkout mutation select another host for ambient gh credentials.
        if self.token.is_none()
            && !crate::data::workflow_prerequisites::GitRemoteUrl::parse(&remote_url)
                .is_github_com()
        {
            return Err(EngineError::Other(
                "poll_ci: this host requires an explicit token grant".into(),
            ));
        }
        let branch = self.git.current_branch(git_root).ok_or_else(|| {
            EngineError::Other(
                "poll_ci: cannot determine the current branch (detached HEAD?)".into(),
            )
        })?;
        let head_sha = self.git.head_sha(git_root)?;

        let resolver = GitHubTokenResolver::new(GitHubTokenResolverOptions::default())
            .with_refused_binaries(self.refused_binaries.clone())
            .with_repository_root(git_root.to_path_buf());
        let host_token = if self.token.is_none()
            && matches!(resolver.host_gh_status(), GhAuthStatus::LoggedIn { .. })
        {
            resolver
                .gh_auth_token_command(HostStdin::Null)
                .and_then(|command| command.run())
                .ok()
                .and_then(|out| out.secret)
        } else {
            None
        };
        if let Some(token) = &host_token {
            self.redaction.add(token.clone());
        }

        let token = match self
            .token
            .as_ref()
            .or(host_token.as_ref())
            .map(SecretString::expose)
        {
            Some(t) if !t.is_empty() => t,
            _ => {
                return Err(EngineError::Other(
                    "poll_ci: neither `gh` CLI (authenticated) nor GITHUB_TOKEN env var is \
                     available; cannot poll CI status"
                        .into(),
                ));
            }
        };

        let remote = crate::data::workflow_prerequisites::GitRemoteUrl::parse(&remote_url);
        let host = match &remote.kind {
            crate::data::workflow_prerequisites::GitRemoteKind::Https { host }
            | crate::data::workflow_prerequisites::GitRemoteKind::Ssh { host } => host,
            _ => return Err(EngineError::Other("poll_ci: unsupported remote".into())),
        };
        if !crate::engine::auth::github_token::valid_host(host) {
            return Err(EngineError::Other("poll_ci: invalid host".into()));
        }
        let (owner, repo) = if host == "github.com" {
            parse_github_owner_repo(&remote_url)?
        } else {
            remote
                .owner_repo
                .clone()
                .filter(|(o, r)| {
                    [o, r].iter().all(|p| {
                        !p.is_empty()
                            && *p != "."
                            && *p != ".."
                            && p.chars()
                                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                    })
                })
                .ok_or_else(|| {
                    EngineError::Other("poll_ci: invalid enterprise repository".into())
                })?
        };
        fetch_via_api(host, &owner, &repo, &branch, &head_sha, token)
    }

    /// Poll until CI passes, fails, or `max_retries` attempts are spent.
    ///
    /// `on_event` receives each observation as a typed [`CiPollEvent`]; the
    /// poller no longer composes the narration itself (F-45). `CiPollEvent`'s
    /// `Display` is the wording it used to compose.
    pub fn poll(
        &self,
        git_root: &Path,
        interval_secs: u32,
        max_retries: u32,
        on_event: impl FnMut(CiPollEvent),
    ) -> Result<(), EngineError> {
        poll_with(
            || self.fetch_status(git_root),
            interval_secs,
            max_retries,
            on_event,
        )
    }
}

/// The retry loop behind [`CiPoller::poll`], with the status source injected
/// so the loop can be exercised without reaching the GitHub API.
fn poll_with(
    mut fetch_status: impl FnMut() -> Result<CiStatus, EngineError>,
    interval_secs: u32,
    max_retries: u32,
    mut on_event: impl FnMut(CiPollEvent),
) -> Result<(), EngineError> {
    let grace_not_found = 3u32.min(max_retries);

    for attempt in 1..=max_retries {
        on_event(CiPollEvent::Attempt {
            attempt,
            of: max_retries,
        });

        match fetch_status()? {
            CiStatus::Success => {
                on_event(CiPollEvent::Passed);
                return Ok(());
            }
            CiStatus::Running => {
                on_event(CiPollEvent::StillRunning);
            }
            CiStatus::NotFound if attempt <= grace_not_found => {
                on_event(CiPollEvent::NoRunYet);
            }
            CiStatus::NotFound => {
                return Err(EngineError::Container(
                    "poll_ci: no CI run found for this branch/commit".into(),
                ));
            }
            CiStatus::Failed(detail) => {
                on_event(CiPollEvent::Failed {
                    detail: detail.clone(),
                });
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

    use crate::engine::test_path::PathGuard;

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
        let mut events: Vec<CiPollEvent> = Vec::new();

        let result = test_poller(None).poll(tmp.path(), 0, 5, |event| {
            events.push(event);
        });

        assert!(result.is_err(), "must fail on a non-git directory");
        // The attempt banner is emitted before fetch_ci_status is called.
        assert_eq!(
            events.len(),
            1,
            "exactly one event before the error propagates"
        );
        assert_eq!(events[0], CiPollEvent::Attempt { attempt: 1, of: 5 });
        // The narration the default `report_ci_poll` will render.
        assert!(
            events[0].to_string().contains("attempt 1/5"),
            "first event must be the attempt banner: {}",
            events[0]
        );
        assert_eq!(events[0].level(), crate::data::message::MessageLevel::Info);
    }

    /// The runs a GitHub API adapter would hand `parse_run_list`, scripted so
    /// the poll loop can be driven without network access.
    fn scripted_status(runs_json: &str) -> Result<CiStatus, EngineError> {
        parse_run_list(&serde_json::from_str(runs_json).unwrap(), "any")
    }

    /// Exhausting all retries (CI remains running) must return a "did not
    /// complete" error.
    #[test]
    fn poll_loop_exhausts_max_retries_returns_error() {
        let mut messages: Vec<String> = Vec::new();
        let result = poll_with(
            || {
                scripted_status(
                    r#"[{"status":"in_progress","conclusion":"","name":"CI","headSha":"any"}]"#,
                )
            },
            0,
            3,
            |event| messages.push(event.to_string()),
        );

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

    /// When CI succeeds on the first poll, the loop returns `Ok` and emits a
    /// "CI passed" event.
    #[test]
    fn poll_loop_succeeds_on_first_poll() {
        let mut events: Vec<CiPollEvent> = Vec::new();
        let result = poll_with(
            || {
                scripted_status(
                    r#"[{"status":"completed","conclusion":"success","name":"CI","headSha":"any"}]"#,
                )
            },
            0,
            5,
            |event| events.push(event),
        );

        assert!(
            result.is_ok(),
            "should succeed when CI passes: {:?}",
            result
        );
        assert!(
            events.contains(&CiPollEvent::Passed),
            "must emit the CI-passed event: {events:?}"
        );
    }

    /// When CI has failed, the loop returns `Err` and emits a warning with the
    /// failure detail.
    #[test]
    fn poll_loop_fails_when_ci_failed() {
        let mut warnings: Vec<String> = Vec::new();
        let result = poll_with(
            || {
                scripted_status(
                    r#"[{"status":"completed","conclusion":"failure","name":"unit-tests","headSha":"any"}]"#,
                )
            },
            0,
            5,
            |event| {
                if event.level() == crate::data::message::MessageLevel::Warning {
                    warnings.push(event.to_string());
                }
            },
        );

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
        // Serialise against every sibling that mutates PATH to install a fake
        // `gh` — without this we can observe their PATH and find a "gh" we
        // shouldn't have. The guard is the process-wide one, so this also
        // covers the dsbx fake-`sbx` tests, not just this module's.
        let _path = PathGuard::acquire();

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

        let err = test_poller(None).fetch_status(repo.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("GITHUB_TOKEN"),
            "error must mention GITHUB_TOKEN: {msg}"
        );
        assert!(msg.contains("gh"), "error must mention gh CLI: {msg}");
    }

    /// A poller over a real `GitEngine` with an explicit token, so these
    /// tests never depend on the ambient environment.
    fn test_poller(token: Option<&str>) -> CiPoller {
        CiPoller::new(Arc::new(GitEngine::new()), token.map(SecretString::new))
    }

    // ── Helpers for real-git tests ────────────────────────────────────────────

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
        // fetch_status requires an `origin` remote, and a github.com host
        // when polling without an explicit token grant.
        run(&[
            "remote",
            "add",
            "origin",
            "https://github.com/example/repo.git",
        ]);
    }
}

#[cfg(test)]
mod destination_review_tests {
    use super::*;
    #[test]
    fn changed_destination_is_rejected_before_gh_or_token_network_use() {
        let root = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q"][..],
            &["remote", "add", "origin", "https://evil.test/o/r"][..],
        ] {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .unwrap()
                .status
                .success());
        }
        let poller = CiPoller::new(
            Arc::new(GitEngine::new()),
            Some(SecretString::new("fixture_secret")),
        )
        .with_destination(Some("https://github.com/o/r".into()));
        let error = poller.fetch_status(root.path()).unwrap_err().to_string();
        assert!(error.contains("destination changed"));
        assert!(!error.contains("fixture_secret"));
    }
}
