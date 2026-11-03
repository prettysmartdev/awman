//! CI status polling via `gh` CLI o        let normalized = serde_json::Value::Array(runs.iter().map(normalize_api_run).collect()); GitHub REST API fallback.

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

/// The detail behind a CI failure: a report for the on_failure agent (failed
/// jobs, steps and log tails) and the failed jobs' names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CiFailure {
    pub report: String,
    pub failed_jobs: Vec<String>,
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

/// How many failed jobs a failure report includes, and how much of each
/// job's log. The report is for the on_failure agent: enough to see the
/// failing step's output without flooding its context.
const REPORT_MAX_JOBS: usize = 3;
const REPORT_LOG_TAIL_LINES: usize = 150;

/// A GitHub REST API client for one repository's Actions data.
struct GitHubActionsApi {
    base: String,
    owner: String,
    repo: String,
    token: SecretString,
}

impl GitHubActionsApi {
    fn new(host: &str, owner: &str, repo: &str, token: &str) -> Self {
        let base = if host == "github.com" {
            "https://api.github.com".into()
        } else {
            format!("https://{host}/api/v3")
        };
        Self {
            base,
            owner: owner.into(),
            repo: repo.into(),
            token: SecretString::new(token),
        }
    }

    fn repo_url(&self, path: &str) -> String {
        format!("{}/repos/{}/{}/{path}", self.base, self.owner, self.repo)
    }

    async fn get(&self, url: String) -> Result<reqwest::Response, EngineError> {
        HttpCore::refuse_public_host_under_test_isolation(&url)
            .map_err(|e| EngineError::Other(format!("poll_ci: {e}")))?;
        // Timed, so a stalled log download cannot hang the teardown.
        let client = HttpCore::client(&HttpClientOptions {
            connect_timeout: Some(std::time::Duration::from_secs(15)),
            read_timeout: Some(std::time::Duration::from_secs(60)),
            ..HttpClientOptions::default()
        })
        .map_err(|e| EngineError::Other(format!("poll_ci: {e}")))?;
        let resp = client
            .get(&url)
            .header("Authorization", {
                let mut header = reqwest::header::HeaderValue::from_str(&format!(
                    "Bearer {}",
                    self.token.expose()
                ))
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
        Ok(resp)
    }

    fn get_json(&self, url: String) -> Result<serde_json::Value, EngineError> {
        run_async(async {
            self.get(url)
                .await?
                .json::<serde_json::Value>()
                .await
                .map_err(|e| {
                    EngineError::Other(format!("poll_ci: failed to parse API response: {e}"))
                })
        })?
    }

    fn get_text(&self, url: String) -> Result<String, EngineError> {
        run_async(async {
            self.get(url)
                .await?
                .text()
                .await
                .map_err(|e| EngineError::Other(format!("poll_ci: failed to read response: {e}")))
        })?
    }

    /// The runs for `head_sha` on `branch`, normalised to the shape
    /// [`parse_run_list`] reads.
    fn runs(&self, branch: &str, head_sha: &str) -> Result<serde_json::Value, EngineError> {
        let url = self.repo_url(&format!(
            "actions/runs?branch={branch}&head_sha={head_sha}&per_page=100"
        ));
        let json = self.get_json(url)?;
        let runs = json
            .get("workflow_runs")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                EngineError::Other("poll_ci: unexpected GitHub API response structure".into())
            })?;
        Ok(serde_json::Value::Array(
            runs.iter().map(normalize_api_run).collect(),
        ))
    }

    /// Failed jobs of the failed `runs` (id, name), their failed steps and
    /// the tail of each job's log. Best effort: what cannot be fetched is
    /// noted, not fatal. Every failed job is named; only the first few get
    /// their detail and log.
    fn failure(&self, runs: &[(u64, String)]) -> CiFailure {
        let mut report = String::new();
        let mut failed_jobs: Vec<String> = Vec::new();
        for (run_id, _) in runs {
            let jobs = match self.get_json(self.repo_url(&format!(
                "actions/runs/{run_id}/jobs?filter=latest&per_page=100"
            ))) {
                Ok(json) => json,
                Err(e) => {
                    report.push_str(&format!("Could not list jobs of run {run_id}: {e}\n"));
                    continue;
                }
            };
            let jobs = jobs
                .get("jobs")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for job in jobs
                .iter()
                .filter(|j| is_failed_conclusion(str_field(j, "conclusion")))
            {
                failed_jobs.push(str_field(job, "name").to_string());
                if failed_jobs.len() > REPORT_MAX_JOBS {
                    report.push_str(&format!("Also failed: {}\n\n", str_field(job, "name")));
                    continue;
                }
                report.push_str(&format_failed_job(job));
                if let Some(job_id) = job.get("id").and_then(serde_json::Value::as_u64) {
                    match self.get_text(self.repo_url(&format!("actions/jobs/{job_id}/logs"))) {
                        Ok(log) => {
                            report.push_str(&format!(
                                "Last {REPORT_LOG_TAIL_LINES} log lines:\n{}\n",
                                tail_lines(&log, REPORT_LOG_TAIL_LINES)
                            ));
                        }
                        Err(e) => report.push_str(&format!("Could not fetch the job log: {e}\n")),
                    }
                }
                report.push('\n');
            }
        }
        // Without job data, the failed workflows still say what broke.
        if failed_jobs.is_empty() {
            failed_jobs = runs.iter().map(|(_, name)| name.clone()).collect();
        }
        failed_jobs.retain(|name| !name.is_empty());
        failed_jobs.sort();
        failed_jobs.dedup();
        CiFailure {
            report,
            failed_jobs,
        }
    }
}

fn normalize_api_run(r: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": r.get("id").and_then(serde_json::Value::as_u64).unwrap_or(0),
        "workflowId": r.get("workflow_id").and_then(serde_json::Value::as_u64).unwrap_or(0),
        "event": str_field(r, "event"),
        "status": str_field(r, "status"),
        "conclusion": str_field(r, "conclusion"),
        "name": str_field(r, "name"),
        "headSha": str_field(r, "head_sha"),
        "htmlUrl": str_field(r, "html_url"),
    })
}

fn format_failed_job(job: &serde_json::Value) -> String {
    let failed_steps: Vec<&str> = job
        .get("steps")
        .and_then(|v| v.as_array())
        .map(|steps| {
            steps
                .iter()
                .filter(|s| is_failed_conclusion(str_field(s, "conclusion")))
                .map(|s| str_field(s, "name"))
                .collect()
        })
        .unwrap_or_default();
    let mut out = format!(
        "Failed job: {} ({})\n",
        str_field(job, "name"),
        str_field(job, "conclusion")
    );
    if !str_field(job, "html_url").is_empty() {
        out.push_str(&format!("URL: {}\n", str_field(job, "html_url")));
    }
    if !failed_steps.is_empty() {
        out.push_str(&format!("Failed steps: {}\n", failed_steps.join(", ")));
    }
    out
}

fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn str_field<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(serde_json::Value::as_str).unwrap_or("")
}

fn is_failed_conclusion(conclusion: &str) -> bool {
    !matches!(conclusion, "" | "success" | "skipped" | "neutral")
}

/// The runs that decide CI for `head_sha`.
///
/// Only runs of that exact commit count: right after a push GitHub may not
/// have queued the new runs yet, and judging the branch's previous commit
/// instead would report its old result. Of several runs of one workflow for
/// one trigger, only the newest counts (a re-run replaces the original). A
/// cancelled run is ignored when another run of the same workflow did not
/// cancel: that is a concurrency group replacing a duplicate run (a push and
/// a pull_request trigger for the same commit), not a CI failure.
fn decisive_runs<'a>(runs: &'a [serde_json::Value], head_sha: &str) -> Vec<&'a serde_json::Value> {
    let workflow = |r: &serde_json::Value| match r.get("workflowId").and_then(|v| v.as_u64()) {
        Some(id) if id != 0 => id.to_string(),
        _ => str_field(r, "name").to_string(),
    };
    let run_id = |r: &serde_json::Value| r.get("id").and_then(|v| v.as_u64()).unwrap_or(0);

    // GitHub lists runs newest first, so without ids the first one seen wins.
    let mut latest: Vec<&serde_json::Value> = Vec::new();
    for run in runs.iter().filter(|r| str_field(r, "headSha") == head_sha) {
        let key = (workflow(run), str_field(run, "event"));
        match latest
            .iter_mut()
            .find(|l| (workflow(l), str_field(l, "event")) == key)
        {
            Some(slot) if run_id(run) > run_id(slot) => *slot = run,
            Some(_) => {}
            None => latest.push(run),
        }
    }

    let superseded_cancel = |r: &serde_json::Value| {
        str_field(r, "conclusion") == "cancelled"
            && latest.iter().any(|other| {
                !std::ptr::eq(*other, r)
                    && workflow(other) == workflow(r)
                    && str_field(other, "conclusion") != "cancelled"
            })
    };
    latest
        .iter()
        .copied()
        .filter(|r| !superseded_cancel(r))
        .collect()
}

fn is_running(run: &serde_json::Value) -> bool {
    str_field(run, "status") != "completed"
}

fn is_failed(run: &serde_json::Value) -> bool {
    !is_running(run) && is_failed_conclusion(str_field(run, "conclusion"))
}

/// Ids and names of the failed decisive runs for `head_sha`.
fn failed_runs(json: &serde_json::Value, head_sha: &str) -> Vec<(u64, String)> {
    let runs = json.as_array().map(Vec::as_slice).unwrap_or_default();
    decisive_runs(runs, head_sha)
        .into_iter()
        .filter(|r| is_failed(r))
        .filter_map(|r| {
            let id = r.get("id").and_then(|v| v.as_u64()).filter(|id| *id != 0)?;
            Some((id, str_field(r, "name").to_string()))
        })
        .collect()
}

/// Parse the JSON array of runs (the normalised API shape) and determine
/// the overall CI status of `head_sha`.
fn parse_run_list(json: &serde_json::Value, head_sha: &str) -> Result<CiStatus, EngineError> {
    let runs = json
        .as_array()
        .ok_or_else(|| EngineError::Other("poll_ci: expected JSON array of runs".into()))?;

    let decisive = decisive_runs(runs, head_sha);
    if decisive.is_empty() {
        return Ok(CiStatus::NotFound);
    }

    let failures: Vec<String> = decisive
        .iter()
        .filter(|r| is_failed(r))
        .map(|r| {
            let name = match str_field(r, "name") {
                "" => "unknown",
                name => name,
            };
            match str_field(r, "htmlUrl") {
                "" => format!("{name}: {}", str_field(r, "conclusion")),
                url => format!("{name}: {} ({url})", str_field(r, "conclusion")),
            }
        })
        .collect();

    if !failures.is_empty() {
        Ok(CiStatus::Failed(failures.join("; ")))
    } else if decisive.iter().any(|r| is_running(r)) {
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
    /// What the last failed status check learned about the failure, for the
    /// on_failure agent and its fix commit.
    failure: std::sync::Mutex<Option<CiFailure>>,
}

impl CiPoller {
    pub fn new(git: Arc<GitEngine>, token: Option<SecretString>) -> Self {
        Self {
            git,
            token,
            destination: None,
            refused_binaries: vec![],
            redaction: RedactionSet::new(),
            failure: std::sync::Mutex::new(None),
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
        // The approved destination is a `GitRemoteUrl` redaction (no userinfo,
        // `host:path` for scp-style remotes); compare in that same form.
        let current = crate::data::workflow_prerequisites::GitRemoteUrl::parse(&remote_url);
        if self
            .destination
            .as_ref()
            .is_some_and(|approved| approved != &current.redacted)
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
        let api = GitHubActionsApi::new(host, &owner, &repo, token);
        let runs = api.runs(&branch, &head_sha)?;
        let status = parse_run_list(&runs, &head_sha)?;
        if matches!(status, CiStatus::Failed(_)) {
            let mut failure = api.failure(&failed_runs(&runs, &head_sha));
            failure.report = self.redaction.redact(&failure.report);
            *self.failure.lock().unwrap() = Some(failure);
        }
        Ok(status)
    }

    /// What the last CI failure this poller saw was made of, if any.
    pub fn take_failure(&self) -> Option<CiFailure> {
        self.failure.lock().unwrap().take()
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
    fn parse_run_list_ignores_runs_of_other_commits() {
        // Right after a push the new commit has no run yet; the previous
        // commit's failed run must not be reported as this commit's result.
        let json = serde_json::json!([
            {"status": "completed", "conclusion": "failure", "name": "CI", "headSha": "old111"}
        ]);
        let result = parse_run_list(&json, "abc123").unwrap();
        assert_eq!(result, CiStatus::NotFound);
    }

    #[test]
    fn parse_run_list_counts_only_the_newest_run_per_workflow_and_trigger() {
        let json = serde_json::json!([
            {"id": 9, "workflowId": 1, "event": "push", "status": "completed", "conclusion": "success", "name": "CI", "headSha": "abc"},
            {"id": 7, "workflowId": 1, "event": "push", "status": "completed", "conclusion": "failure", "name": "CI", "headSha": "abc"}
        ]);
        assert_eq!(parse_run_list(&json, "abc").unwrap(), CiStatus::Success);
    }

    #[test]
    fn parse_run_list_ignores_a_run_cancelled_by_its_concurrency_group() {
        let json = serde_json::json!([
            {"id": 9, "workflowId": 1, "event": "pull_request", "status": "completed", "conclusion": "success", "name": "CI", "headSha": "abc"},
            {"id": 8, "workflowId": 1, "event": "push", "status": "completed", "conclusion": "cancelled", "name": "CI", "headSha": "abc"}
        ]);
        assert_eq!(parse_run_list(&json, "abc").unwrap(), CiStatus::Success);
    }

    #[test]
    fn parse_run_list_still_fails_a_real_failure_on_another_trigger() {
        let json = serde_json::json!([
            {"id": 9, "workflowId": 1, "event": "pull_request", "status": "completed", "conclusion": "failure", "name": "CI", "headSha": "abc", "htmlUrl": "https://github.com/o/r/actions/runs/9"},
            {"id": 8, "workflowId": 1, "event": "push", "status": "completed", "conclusion": "success", "name": "CI", "headSha": "abc"}
        ]);
        let CiStatus::Failed(detail) = parse_run_list(&json, "abc").unwrap() else {
            panic!("expected Failed");
        };
        assert_eq!(
            detail,
            "CI: failure (https://github.com/o/r/actions/runs/9)"
        );
        assert_eq!(failed_runs(&json, "abc"), [(9, "CI".to_string())]);
    }

    #[test]
    fn failed_job_report_names_the_failed_steps() {
        let job = serde_json::json!({
            "name": "test", "conclusion": "failure", "html_url": "https://github.com/o/r/actions/runs/9/job/3",
            "steps": [
                {"name": "checkout", "conclusion": "success"},
                {"name": "cargo test", "conclusion": "failure"}
            ]
        });
        assert_eq!(
            format_failed_job(&job),
            "Failed job: test (failure)\nURL: https://github.com/o/r/actions/runs/9/job/3\nFailed steps: cargo test\n"
        );
        assert_eq!(tail_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(tail_lines("a", 5), "a");
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

    /// Verify that the API run normalisation (head_sha → headSha)
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

        let normalized = serde_json::Value::Array(runs.iter().map(normalize_api_run).collect());

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

    #[test]
    fn an_unchanged_ssh_destination_matches_its_approved_redaction() {
        for url in ["git@example.test:o/r.git", "ssh://git@example.test/o/r.git"] {
            let root = tempfile::tempdir().unwrap();
            for args in [&["init", "-q"][..], &["remote", "add", "origin", url][..]] {
                assert!(std::process::Command::new("git")
                    .args(args)
                    .current_dir(root.path())
                    .output()
                    .unwrap()
                    .status
                    .success());
            }
            // The prerequisite flow approves `GitRemoteUrl::parse(..).redacted`.
            let approved = crate::data::workflow_prerequisites::GitRemoteUrl::parse(url).redacted;
            let poller =
                CiPoller::new(Arc::new(GitEngine::new()), None).with_destination(Some(approved));
            // Past the destination check, a non-github.com host without a token
            // stops before any network use.
            let error = poller.fetch_status(root.path()).unwrap_err().to_string();
            assert!(!error.contains("destination changed"), "{url}: {error}");
            assert!(error.contains("explicit token grant"), "{url}: {error}");
        }
    }
}
