//! `GithubIssueSource` â GitHub implementation of `IssueSource`.

use std::path::Path;

use crate::data::config::env::Env;
use crate::data::fs::remote_slug::RemoteSlug;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::engine::git::GitEngine;

use super::{Issue, IssueSource, IssueSourceError};

/// Hint embedded into `IssueSourceError::Unauthorized` by GitHub auth/403
/// failures. Provider-specific text lives here, not in the trait-level enum.
const GH_AUTH_HINT: &str = "ensure gh is authenticated (`gh auth login`) or GITHUB_TOKEN is set";

pub struct GithubIssueSource;

impl IssueSource for GithubIssueSource {
    fn provider_name(&self) -> &str {
        "GitHub"
    }

    fn provider_prefix(&self) -> &str {
        "ghb"
    }

    fn issue_identifier(&self, issue: &Issue) -> String {
        issue
            .numeric_id()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "0".to_string())
    }

    fn can_handle(&self, input: &str) -> bool {
        let input = input.trim();
        if input.is_empty() {
            return false;
        }
        // Bare integer
        if input.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
        // GitHub URL
        if input.starts_with("https://github.com/") || input.starts_with("http://github.com/") {
            return true;
        }
        // owner/repo#N short form
        if input.contains('#') && !input.contains("://") {
            return true;
        }
        false
    }

    fn fetch_issue(&self, input: &str, git_root: &Path) -> Result<Issue, IssueSourceError> {
        let env = Env::from_process();
        fetch_issue_with_dependencies(
            input,
            git_root,
            self.provider_name(),
            &GitEngine::new(),
            env.github_token(),
        )
    }

    fn fetch_issue_with_engine(
        &self,
        input: &str,
        git_root: &Path,
        git_engine: &GitEngine,
        github_token: Option<&str>,
    ) -> Result<Issue, IssueSourceError> {
        fetch_issue_with_dependencies(
            input,
            git_root,
            self.provider_name(),
            git_engine,
            github_token,
        )
    }

    fn fetch_issue_with_progress(
        &self,
        input: &str,
        git_root: &Path,
        sink: &mut dyn UserMessageSink,
    ) -> Result<Issue, IssueSourceError> {
        let env = Env::from_process();
        fetch_issue_with_progress_dependencies(
            input,
            git_root,
            self.provider_name(),
            sink,
            &GitEngine::new(),
            env.github_token(),
        )
    }

    fn fetch_issue_with_engine_progress(
        &self,
        input: &str,
        git_root: &Path,
        sink: &mut dyn UserMessageSink,
        git_engine: &GitEngine,
        github_token: Option<&str>,
    ) -> Result<Issue, IssueSourceError> {
        fetch_issue_with_progress_dependencies(
            input,
            git_root,
            self.provider_name(),
            sink,
            git_engine,
            github_token,
        )
    }
}

fn fetch_issue_with_dependencies(
    input: &str,
    git_root: &Path,
    provider: &str,
    git_engine: &GitEngine,
    github_token: Option<&str>,
) -> Result<Issue, IssueSourceError> {
    let input = input.trim();
    let (owner, repo, number) = parse_input(input, git_root, provider, git_engine)?;

    // Try gh CLI first
    if let Some(issue) =
        try_gh_cli_with_cmd_in(git_engine.env(), "gh", &owner, &repo, number, provider)
    {
        return Ok(issue);
    }

    // Fall back to REST API
    fetch_rest_api_with_token(&owner, &repo, number, provider, github_token)
}

/// External GitHub integrations consulted while fetching an issue: the `gh`
/// CLI command and the REST API base URL. Production code uses
/// [`GithubIntegrations::DEFAULT`]; tests substitute a fake CLI script and an
/// unreachable endpoint so they never change the process-global `PATH` or
/// reach the network.
#[derive(Clone, Copy, Debug)]
struct GithubIntegrations<'a> {
    gh_command: &'a str,
    api_base_url: &'a str,
}

impl GithubIntegrations<'static> {
    const DEFAULT: Self = Self {
        gh_command: "gh",
        api_base_url: "https://api.github.com",
    };
}

fn fetch_issue_with_progress_dependencies(
    input: &str,
    git_root: &Path,
    provider: &str,
    sink: &mut dyn UserMessageSink,
    git_engine: &GitEngine,
    github_token: Option<&str>,
) -> Result<Issue, IssueSourceError> {
    fetch_issue_with_progress_using(
        GithubIntegrations::DEFAULT,
        input,
        git_root,
        provider,
        sink,
        git_engine,
        github_token,
    )
}

/// Inner implementation for `fetch_issue_with_progress`, parameterised so
/// tests can supply a deterministic CLI and REST endpoint without changing the
/// process-global `PATH` or reaching the network.
fn fetch_issue_with_progress_using(
    integrations: GithubIntegrations<'_>,
    input: &str,
    git_root: &Path,
    provider: &str,
    sink: &mut dyn UserMessageSink,
    git_engine: &GitEngine,
    github_token: Option<&str>,
) -> Result<Issue, IssueSourceError> {
    let input = input.trim();
    let (owner, repo, number) = parse_input(input, git_root, provider, git_engine)?;

    sink.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!(
            "running: gh issue view {number} --repo {owner}/{repo} --json number,title,body,url"
        ),
    });

    if let Some(issue) = try_gh_cli_with_cmd_in(
        git_engine.env(),
        integrations.gh_command,
        &owner,
        &repo,
        number,
        provider,
    ) {
        return Ok(issue);
    }

    let api_base_url = integrations.api_base_url.trim_end_matches('/');
    let api_url = format!("{api_base_url}/repos/{owner}/{repo}/issues/{number}");
    sink.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("gh CLI unavailable, falling back to REST API: GET {api_url}"),
    });

    fetch_rest_api_with_base_and_token(api_base_url, &owner, &repo, number, provider, github_token)
}

/// Parse user input into (owner, repo, number).
fn parse_input(
    input: &str,
    git_root: &Path,
    provider: &str,
    git_engine: &GitEngine,
) -> Result<(String, String, u32), IssueSourceError> {
    // Bare integer â resolve from git remote
    if input.chars().all(|c| c.is_ascii_digit()) {
        let number = input
            .parse::<u32>()
            .map_err(|_| IssueSourceError::InvalidRef {
                provider: provider.to_string(),
                input: input.to_string(),
                hint: "could not parse as issue number".to_string(),
            })?;
        let (owner, repo) = detect_github_remote(git_root, provider, git_engine)?;
        return Ok((owner, repo, number));
    }

    // Full URL: https://github.com/{owner}/{repo}/issues/{number}
    if input.starts_with("https://github.com/") || input.starts_with("http://github.com/") {
        return parse_github_url(input, provider);
    }

    // Non-GitHub URL
    if input.contains("://") {
        return Err(IssueSourceError::InvalidRef {
            provider: provider.to_string(),
            input: input.to_string(),
            hint: "URL does not point to github.com".to_string(),
        });
    }

    // owner/repo#N short form
    if let Some(hash_pos) = input.find('#') {
        let owner_repo = &input[..hash_pos];
        let number_str = &input[hash_pos + 1..];
        let number = number_str
            .parse::<u32>()
            .map_err(|_| IssueSourceError::InvalidRef {
                provider: provider.to_string(),
                input: input.to_string(),
                hint: format!("could not parse issue number from '{number_str}'"),
            })?;
        let parts: Vec<&str> = owner_repo.splitn(2, '/').collect();
        if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
            return Err(IssueSourceError::InvalidRef {
                provider: provider.to_string(),
                input: input.to_string(),
                hint: "expected format: owner/repo#number".to_string(),
            });
        }
        return Ok((parts[0].to_string(), parts[1].to_string(), number));
    }

    Err(IssueSourceError::InvalidRef {
        provider: provider.to_string(),
        input: input.to_string(),
        hint: "expected a number, GitHub URL, or owner/repo#number".to_string(),
    })
}

fn parse_github_url(url: &str, provider: &str) -> Result<(String, String, u32), IssueSourceError> {
    // Strip scheme + host
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .unwrap_or("");
    let segments: Vec<&str> = path.split('/').collect();
    // Expected: {owner}/{repo}/issues/{number}
    if segments.len() >= 4 && segments[2] == "issues" {
        let number = segments[3]
            .parse::<u32>()
            .map_err(|_| IssueSourceError::InvalidRef {
                provider: provider.to_string(),
                input: url.to_string(),
                hint: format!(
                    "could not parse issue number from URL segment '{}'",
                    segments[3]
                ),
            })?;
        Ok((segments[0].to_string(), segments[1].to_string(), number))
    } else {
        Err(IssueSourceError::InvalidRef {
            provider: provider.to_string(),
            input: url.to_string(),
            hint: "expected URL format: https://github.com/owner/repo/issues/NUMBER".to_string(),
        })
    }
}

/// Detect the GitHub remote's owner/repo from the current git repo.
fn detect_github_remote(
    git_root: &Path,
    provider: &str,
    git_engine: &GitEngine,
) -> Result<(String, String), IssueSourceError> {
    let url = git_engine.remote_url(git_root, "origin").map_err(|_| {
        IssueSourceError::NoRemoteDetected {
            provider: provider.to_string(),
        }
    })?;
    parse_owner_repo_from_remote(&url).ok_or_else(|| IssueSourceError::NoRemoteDetected {
        provider: provider.to_string(),
    })
}

/// Extract owner/repo from a GitHub remote URL.
///
/// The URL splitting is the shared Layer 0 `RemoteSlug` parser (WI 0114
/// F-29); what stays here is the GitHub-specific half — the host must be
/// `github.com`, and owner and repo are kept verbatim because they are path
/// segments in a GitHub API call, not a directory name.
fn parse_owner_repo_from_remote(remote_url: &str) -> Option<(String, String)> {
    RemoteSlug::parse(remote_url)
        .filter(|slug| slug.is_host("github.com"))
        .map(|slug| (slug.owner, slug.repo))
}

fn try_gh_cli_with_cmd_in(
    snapshot: &crate::data::config::env::EnvSnapshot,
    gh_cmd: &str,
    owner: &str,
    repo: &str,
    number: u32,
    provider: &str,
) -> Option<Issue> {
    let number = number.to_string();
    let owner_repo = format!("{owner}/{repo}");
    let mut retries = 0;
    let output = loop {
        let result = crate::engine::host_cli::command(snapshot, gh_cmd)
            .args([
                "issue",
                "view",
                &number,
                "--repo",
                &owner_repo,
                "--json",
                "number,title,body,url",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();
        match result {
            Ok(output) => break output,
            // Parallel tests can fork while another test is still writing its
            // fake gh executable. The fork inherits that write descriptor,
            // creating a short ETXTBSY window even after the writer closes it.
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy && retries < 20 =>
            {
                retries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    };

    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let title = json.get("title")?.as_str()?.to_string();
    let body = json
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let source_id = json
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let source_id = if source_id.is_empty() {
        format!("https://github.com/{owner}/{repo}/issues/{number}")
    } else {
        source_id
    };

    Some(Issue {
        source_id,
        title,
        body,
        provider: provider.to_string(),
    })
}

fn fetch_rest_api_with_token(
    owner: &str,
    repo: &str,
    number: u32,
    provider: &str,
    github_token: Option<&str>,
) -> Result<Issue, IssueSourceError> {
    fetch_rest_api_with_base_and_token(
        "https://api.github.com",
        owner,
        repo,
        number,
        provider,
        github_token,
    )
}

fn fetch_rest_api_with_base_and_token(
    base_url: &str,
    owner: &str,
    repo: &str,
    number: u32,
    provider: &str,
    github_token: Option<&str>,
) -> Result<Issue, IssueSourceError> {
    let url = format!("{base_url}/repos/{owner}/{repo}/issues/{number}");
    crate::engine::remote::HttpCore::refuse_public_host_under_test_isolation(&url).map_err(
        |detail| IssueSourceError::Network {
            provider: provider.to_string(),
            detail,
        },
    )?;
    let client = reqwest::blocking::Client::builder()
        .user_agent("awman")
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| IssueSourceError::Network {
            provider: provider.to_string(),
            detail: e.to_string(),
        })?;

    let mut request = client.get(&url);

    if let Some(token) = github_token.filter(|token| !token.is_empty()) {
        request = request.header("Authorization", format!("Bearer {token}"));
    }

    let response = request.send().map_err(|e| IssueSourceError::Network {
        provider: provider.to_string(),
        detail: e.to_string(),
    })?;

    let status = response.status().as_u16();
    match status {
        200 => {}
        401 | 403 => {
            // 403 can be rate limiting or an auth issue; disambiguate by body.
            let body = response.text().unwrap_or_default();
            if body.contains("rate limit") {
                return Err(IssueSourceError::RateLimited {
                    provider: provider.to_string(),
                });
            }
            return Err(IssueSourceError::Unauthorized {
                provider: provider.to_string(),
                hint: GH_AUTH_HINT.to_string(),
            });
        }
        404 => {
            return Err(IssueSourceError::NotFound {
                provider: provider.to_string(),
                source_id: format!("https://github.com/{owner}/{repo}/issues/{number}"),
            });
        }
        429 => {
            return Err(IssueSourceError::RateLimited {
                provider: provider.to_string(),
            });
        }
        _ => {
            let body = response.text().unwrap_or_default();
            return Err(IssueSourceError::ProviderError {
                provider: provider.to_string(),
                detail: format!("HTTP {status}: {body}"),
            });
        }
    }

    let json: serde_json::Value = response
        .json()
        .map_err(|e| IssueSourceError::ProviderError {
            provider: provider.to_string(),
            detail: format!("failed to parse response: {e}"),
        })?;

    let title = json
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let body = json
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok(Issue {
        source_id: format!("https://github.com/{owner}/{repo}/issues/{number}"),
        title,
        body,
        provider: provider.to_string(),
    })
}
