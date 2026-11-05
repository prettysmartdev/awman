//! GitHub pull-request lookup for `--pr`.
//!
//! Parses the user's PR reference, matches it to a configured git remote of
//! the current repository, and reads the PR's head branch, fork status and
//! state — `gh pr view --json` first, then the REST API, the same order the
//! GitHub issue source uses. Only github.com is supported.
//!
//! Everything returned here is untrusted metadata: `head_ref_name` is
//! attacker-controlled on any PR and must be validated with
//! [`GitEngine::validate_branch_name`] before it reaches a ref, argv or path.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::data::fs::remote_slug::RemoteSlug;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;

const GITHUB_HOST: &str = "github.com";
const GH_AUTH_HINT: &str = "ensure gh is authenticated (`gh auth login`) or GITHUB_TOKEN is set";

/// A pull request on github.com: its repository and number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestRef {
    pub repository: RemoteSlug,
    pub number: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

/// The PR metadata the session-worktree flow needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestInfo {
    /// Untrusted: validate before use.
    pub head_ref_name: String,
    /// The head lives in another repository, or its repository is gone.
    pub is_cross_repository: bool,
    pub state: PullRequestState,
}

fn invalid_input(input: &str, hint: &str) -> EngineError {
    EngineError::Git(format!(
        "invalid pull request reference '{}': {hint}",
        input.escape_debug()
    ))
}

const FORMS_HINT: &str =
    "expected a PR number, https://github.com/<owner>/<repo>/pull/<N>, or <owner>/<repo>#<N>";

fn parse_number(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u32>().ok().filter(|n| *n > 0)
}

fn valid_owner(owner: &str) -> bool {
    !owner.is_empty()
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn valid_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo != "."
        && repo != ".."
        && repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn github_slug(owner: &str, repo: &str) -> Option<RemoteSlug> {
    (valid_owner(owner) && valid_repo(repo)).then(|| RemoteSlug {
        host: GITHUB_HOST.to_string(),
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

/// Parse a `--pr` value: a bare number (resolved against `origin`, which
/// must then be a github.com remote), `https://github.com/<o>/<r>/pull/<N>`
/// (an optional single trailing `/`), or `<o>/<r>#<N>`. Anything else —
/// `/issues/` URLs, `http://`, other hosts, credentials, ports, queries,
/// fragments, percent escapes, extra path segments — is rejected.
pub fn parse_pull_request_input(
    input: &str,
    origin: Option<&RemoteSlug>,
) -> Result<PullRequestRef, EngineError> {
    if input.chars().any(char::is_control) {
        return Err(invalid_input(input, "contains control characters"));
    }
    let value = input.trim_matches(' ');
    if value.is_empty() {
        return Err(invalid_input(input, FORMS_HINT));
    }
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(invalid_input(
            input,
            "contains whitespace or control characters",
        ));
    }

    if value.bytes().all(|b| b.is_ascii_digit()) {
        let number = parse_number(value)
            .ok_or_else(|| invalid_input(input, "PR number must be between 1 and 4294967295"))?;
        let origin = origin.ok_or_else(|| {
            EngineError::Git(format!(
                "cannot resolve PR #{number}: no github.com 'origin' remote; use <owner>/<repo>#{number} or a PR URL"
            ))
        })?;
        if !origin.is_host(GITHUB_HOST) {
            return Err(EngineError::Git(format!(
                "cannot resolve PR #{number}: 'origin' is not a github.com remote (only github.com is supported)"
            )));
        }
        let repository = github_slug(&origin.owner, &origin.repo).ok_or_else(|| {
            EngineError::Git(format!(
                "cannot resolve PR #{number}: 'origin' does not name a valid GitHub repository"
            ))
        })?;
        return Ok(PullRequestRef { repository, number });
    }

    if value.contains("://") {
        let rest = value.strip_prefix("https://github.com/").ok_or_else(|| {
            invalid_input(
                input,
                "only https://github.com/ pull request URLs are supported",
            )
        })?;
        if rest.contains(['?', '#', '%', '@', ':', '\\']) {
            return Err(invalid_input(
                input,
                "URL must not contain a query, fragment, escapes or credentials",
            ));
        }
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        let segments: Vec<&str> = rest.split('/').collect();
        let [owner, repo, kind, number] = segments.as_slice() else {
            return Err(invalid_input(
                input,
                "expected https://github.com/<owner>/<repo>/pull/<N>",
            ));
        };
        if *kind != "pull" {
            return Err(invalid_input(
                input,
                "not a pull request URL (expected https://github.com/<owner>/<repo>/pull/<N>)",
            ));
        }
        let number =
            parse_number(number).ok_or_else(|| invalid_input(input, "invalid PR number"))?;
        let repository = github_slug(owner, repo)
            .ok_or_else(|| invalid_input(input, "invalid owner or repository name"))?;
        return Ok(PullRequestRef { repository, number });
    }

    if let Some((owner_repo, number)) = value.split_once('#') {
        let number =
            parse_number(number).ok_or_else(|| invalid_input(input, "invalid PR number"))?;
        let (owner, repo) = owner_repo
            .split_once('/')
            .ok_or_else(|| invalid_input(input, "expected <owner>/<repo>#<N>"))?;
        let repository = github_slug(owner, repo)
            .ok_or_else(|| invalid_input(input, "invalid owner or repository name"))?;
        return Ok(PullRequestRef { repository, number });
    }

    Err(invalid_input(input, FORMS_HINT))
}

/// Find the configured remote whose stored URL names the PR's repository
/// (host, owner and repo compared case-insensitively). `origin` wins when
/// several match; otherwise the first in name order. No match is an error
/// naming the repository: a PR from an unrelated repository cannot share
/// this object store.
pub fn match_pull_request_remote(
    git: &GitEngine,
    repo: &Path,
    pr: &PullRequestRef,
) -> Result<String, EngineError> {
    let mut names = git.remote_names(repo)?;
    names.sort();
    let mut matches = Vec::new();
    for name in names {
        if name.is_empty() || name.starts_with('-') {
            continue;
        }
        // A remote without a stored URL cannot be the PR's repository.
        let Ok(url) = git.remote_url(repo, &name) else {
            continue;
        };
        let Some(slug) = RemoteSlug::parse(&url) else {
            continue;
        };
        if slug.is_host(GITHUB_HOST)
            && slug.owner.eq_ignore_ascii_case(&pr.repository.owner)
            && slug.repo.eq_ignore_ascii_case(&pr.repository.repo)
        {
            matches.push(name);
        }
    }
    if let Some(origin) = matches.iter().find(|n| *n == "origin") {
        return Ok(origin.clone());
    }
    matches.into_iter().next().ok_or_else(|| {
        EngineError::Git(format!(
            "no git remote of this repository points at {}/{}; a pull request can only be opened from a remote of the same repository",
            pr.repository.owner, pr.repository.repo
        ))
    })
}

/// Reads pull-request metadata through `gh`, then the GitHub REST API.
#[derive(Debug, Clone)]
pub struct PullRequestClient {
    gh_command: PathBuf,
    api_base_url: String,
}

impl Default for PullRequestClient {
    fn default() -> Self {
        Self {
            gh_command: PathBuf::from("gh"),
            api_base_url: "https://api.github.com".to_string(),
        }
    }
}

impl PullRequestClient {
    /// Test seam: substitute a fake `gh` and a local REST endpoint. Never
    /// reachable from user flags or repository config.
    #[allow(dead_code)] // used only by test builds of the command layer
    pub(crate) fn with_integrations(gh_command: PathBuf, api_base_url: String) -> Self {
        Self {
            gh_command,
            api_base_url,
        }
    }

    /// Look up `pr`: `gh pr view` first; when `gh` is missing or fails, the
    /// REST API with `github_token` (anonymous when `None`, which works for
    /// public repositories). Metadata that `gh` returns successfully but
    /// malformed is an error, not a fallback.
    pub fn fetch(
        &self,
        pr: &PullRequestRef,
        github_token: Option<&str>,
        sink: &mut dyn UserMessageSink,
    ) -> Result<PullRequestInfo, EngineError> {
        let slug = format!("{}/{}", pr.repository.owner, pr.repository.repo);
        sink.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "running: gh pr view {} --repo {} --json headRefName,isCrossRepository,state",
                pr.number,
                gh_repo_argument(pr)
            ),
        });
        if let Some(info) = try_gh_cli_with_cmd(&self.gh_command, pr)? {
            return Ok(info);
        }
        let base = self.api_base_url.trim_end_matches('/');
        sink.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "gh CLI unavailable, falling back to REST API: GET {base}/repos/{slug}/pulls/{}",
                pr.number
            ),
        });
        fetch_rest_api_with_base_and_token(base, pr, github_token)
    }
}

/// `HOST/OWNER/REPO` for `gh --repo`. Without the host, `gh` would use
/// `GH_HOST` (or its configured default host), which can be a GitHub
/// Enterprise server holding a same-named repository.
fn gh_repo_argument(pr: &PullRequestRef) -> String {
    format!(
        "{GITHUB_HOST}/{}/{}",
        pr.repository.owner, pr.repository.repo
    )
}

fn pr_label(pr: &PullRequestRef) -> String {
    format!(
        "{}/{}#{}",
        pr.repository.owner, pr.repository.repo, pr.number
    )
}

fn malformed(pr: &PullRequestRef, what: &str) -> EngineError {
    EngineError::Git(format!(
        "malformed metadata for pull request {}: {what}",
        pr_label(pr)
    ))
}

/// `gh pr view <N> --repo github.com/<o>/<r> --json headRefName,isCrossRepository,state`.
/// `Ok(None)` when `gh` cannot be run or exits non-zero (fall back to REST).
fn try_gh_cli_with_cmd(
    gh_command: &Path,
    pr: &PullRequestRef,
) -> Result<Option<PullRequestInfo>, EngineError> {
    let number = pr.number.to_string();
    let repo = gh_repo_argument(pr);
    let mut retries = 0;
    let output = loop {
        let result = Command::new(gh_command)
            .args([
                "pr",
                "view",
                &number,
                "--repo",
                &repo,
                "--json",
                "headRefName,isCrossRepository,state",
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        match result {
            Ok(output) => break output,
            // Parallel tests can fork while another test is still writing its
            // fake gh executable (ETXTBSY), as in the issue source.
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy && retries < 20 =>
            {
                retries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => return Ok(None),
        }
    };
    if !output.status.success() {
        return Ok(None);
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| malformed(pr, &format!("gh returned invalid JSON: {e}")))?;
    let head_ref_name = json
        .get("headRefName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| malformed(pr, "missing headRefName"))?
        .to_string();
    let is_cross_repository = json
        .get("isCrossRepository")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| malformed(pr, "missing isCrossRepository"))?;
    let state = match json.get("state").and_then(|v| v.as_str()) {
        Some(s) if s.eq_ignore_ascii_case("OPEN") => PullRequestState::Open,
        Some(s) if s.eq_ignore_ascii_case("CLOSED") => PullRequestState::Closed,
        Some(s) if s.eq_ignore_ascii_case("MERGED") => PullRequestState::Merged,
        _ => return Err(malformed(pr, "missing or unknown state")),
    };
    Ok(Some(PullRequestInfo {
        head_ref_name,
        is_cross_repository,
        state,
    }))
}

/// `GET <base>/repos/<o>/<r>/pulls/<N>`, bearer token when given.
fn fetch_rest_api_with_base_and_token(
    base_url: &str,
    pr: &PullRequestRef,
    github_token: Option<&str>,
) -> Result<PullRequestInfo, EngineError> {
    let label = pr_label(pr);
    let url = format!(
        "{base_url}/repos/{}/{}/pulls/{}",
        pr.repository.owner, pr.repository.repo, pr.number
    );
    crate::engine::remote::HttpCore::refuse_public_host_under_test_isolation(&url).map_err(
        |detail| EngineError::Git(format!("cannot look up pull request {label}: {detail}")),
    )?;
    let network = |e: reqwest::Error| {
        EngineError::Git(format!(
            "cannot look up pull request {label}: network error: {}",
            e.without_url()
        ))
    };
    let client = reqwest::blocking::Client::builder()
        .user_agent("awman")
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(network)?;
    let mut request = client
        .get(&url)
        .header("Accept", "application/vnd.github+json");
    if let Some(token) = github_token.filter(|t| !t.is_empty()) {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request.send().map_err(network)?;
    let status = response.status().as_u16();
    match status {
        200 => {}
        401 | 403 => {
            let body = response.text().unwrap_or_default();
            if body.to_ascii_lowercase().contains("rate limit") {
                return Err(EngineError::Git(format!(
                    "cannot look up pull request {label}: GitHub API rate limit exceeded; {GH_AUTH_HINT}"
                )));
            }
            return Err(EngineError::Git(format!(
                "cannot look up pull request {label}: not authorized; {GH_AUTH_HINT}"
            )));
        }
        404 => {
            return Err(EngineError::Git(format!(
                "pull request {label} not found (the number may be an issue rather than a pull request, or the repository is private: {GH_AUTH_HINT})"
            )));
        }
        429 => {
            return Err(EngineError::Git(format!(
                "cannot look up pull request {label}: GitHub API rate limit exceeded; {GH_AUTH_HINT}"
            )));
        }
        _ => {
            return Err(EngineError::Git(format!(
                "cannot look up pull request {label}: GitHub API returned HTTP {status}"
            )));
        }
    }
    let json: serde_json::Value = response
        .json()
        .map_err(|e| malformed(pr, &format!("invalid JSON: {}", e.without_url())))?;
    parse_rest_pull_request(pr, &json)
}

/// Interpret a REST `pulls/<N>` body. A missing or null `head.repo` (the
/// head repository was deleted) counts as cross-repository so the caller
/// falls back to `refs/pull/<N>/head`.
fn parse_rest_pull_request(
    pr: &PullRequestRef,
    json: &serde_json::Value,
) -> Result<PullRequestInfo, EngineError> {
    let head = json
        .get("head")
        .filter(|h| h.is_object())
        .ok_or_else(|| malformed(pr, "missing head"))?;
    let head_ref_name = head
        .get("ref")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| malformed(pr, "missing head.ref"))?
        .to_string();
    let head_repo = head
        .get("repo")
        .and_then(|r| r.get("full_name"))
        .and_then(|v| v.as_str());
    let is_cross_repository = match head_repo {
        None => true,
        Some(head_full) => {
            let base_full = json
                .get("base")
                .and_then(|b| b.get("repo"))
                .and_then(|r| r.get("full_name"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| malformed(pr, "missing base.repo.full_name"))?;
            !head_full.eq_ignore_ascii_case(base_full)
        }
    };
    let merged = json
        .get("merged")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || json.get("merged_at").is_some_and(|v| !v.is_null());
    let state = match json.get("state").and_then(|v| v.as_str()) {
        _ if merged => PullRequestState::Merged,
        Some("open") => PullRequestState::Open,
        Some("closed") => PullRequestState::Closed,
        _ => return Err(malformed(pr, "missing or unknown state")),
    };
    Ok(PullRequestInfo {
        head_ref_name,
        is_cross_repository,
        state,
    })
}

#[cfg(test)]
mod tests;
