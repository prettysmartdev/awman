//! GitHub transport: trusted host gh first, then authenticated REST.
use super::{transport, Issue, IssueOperation, IssueSource, IssueSourceError};
use crate::data::{
    config::env::EnvSnapshot,
    fs::path_guard::canonicalize_lenient,
    fs::remote_slug::RemoteSlug,
    message::{MessageLevel, UserMessage, UserMessageSink},
    work_item_ref::{GithubRepository, WorkItemRef},
};
use crate::engine::{git::GitEngine, host_cli};
use reqwest::{Method, Url};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
mod cli;

const PROVIDER: &str = "github";
const AUTH_HINT: &str = "run `gh auth login` or set GITHUB_TOKEN";

pub struct GithubIssueSource {
    git_engine: Arc<GitEngine>,
    env: EnvSnapshot,
    gh_command: PathBuf,
    api_base: Url,
}
impl GithubIssueSource {
    pub fn new(git_engine: Arc<GitEngine>, env: EnvSnapshot) -> Self {
        // A repository-controlled PATH entry must never become a host tool.
        let paths = env
            .path_dirs()
            .into_iter()
            .filter(|path| path.is_absolute() && !path.starts_with(env.cwd()))
            .collect();
        let gh_command = host_cli::find_program(&env, "gh", paths)
            .unwrap_or_else(|| PathBuf::from("/nonexistent/awman-missing-program/gh"));
        Self {
            git_engine,
            env,
            gh_command,
            api_base: Url::parse("https://api.github.com").expect("static API URL"),
        }
    }
    pub fn with_endpoints(
        git_engine: Arc<GitEngine>,
        env: EnvSnapshot,
        gh_command: PathBuf,
        api_base: Url,
    ) -> Result<Self, IssueSourceError> {
        super::http::validate_url(&api_base, PROVIDER)?;
        if api_base.query().is_some() || api_base.fragment().is_some() {
            return Err(transport::error(
                PROVIDER,
                "API base must not have query or fragment",
            ));
        }
        transport::outbound(PROVIDER, &api_base)?;
        if !gh_command.is_absolute() || gh_command.starts_with(env.cwd()) {
            return Err(transport::error(
                PROVIDER,
                "gh must be an absolute trusted host executable outside the checkout",
            ));
        }
        Ok(Self {
            git_engine,
            env,
            gh_command,
            api_base,
        })
    }
    pub fn origin_repository(&self, git_root: &Path) -> Result<GithubRepository, IssueSourceError> {
        let missing = || IssueSourceError::NoRemoteDetected {
            provider: PROVIDER.into(),
        };
        let remote = self
            .git_engine
            .remote_url(git_root, "origin")
            .map_err(|_| missing())?;
        let slug = RemoteSlug::parse(&remote)
            .filter(|slug| slug.is_host("github.com"))
            .ok_or_else(missing)?;
        let repository = GithubRepository {
            owner: slug.owner,
            repo: slug.repo,
        };
        validate_repository(&repository)?;
        Ok(repository)
    }
    fn parse_input(
        &self,
        input: &str,
        git_root: &Path,
    ) -> Result<(GithubRepository, u32), IssueSourceError> {
        // Numeric input is a provider-ready argument from the parsed router,
        // never a default-source decision.
        let reference = if !input.is_empty() && input.bytes().all(|c| c.is_ascii_digit()) {
            format!("github:{input}")
        } else {
            input.to_string()
        };
        match WorkItemRef::parse(&reference) {
            Ok(WorkItemRef::Github { repository, number }) => {
                let repository = match repository {
                    Some(repository) => repository,
                    None => self.origin_repository(git_root)?,
                };
                validate_repository(&repository)?;
                Ok((repository, number))
            }
            _ => Err(IssueSourceError::InvalidRef {
                provider: PROVIDER.into(),
                input: String::new(),
                hint: "expected a GitHub issue reference".into(),
            }),
        }
    }
    fn run_cli(
        &self,
        args: &[String],
        body: Option<&str>,
        operation: IssueOperation,
        git_root: &Path,
    ) -> Result<Option<Vec<u8>>, IssueSourceError> {
        let executable = canonicalize_lenient(&self.gh_command);
        let checkout = canonicalize_lenient(git_root);
        if !checkout.as_os_str().is_empty() && executable.starts_with(&checkout) {
            return Ok(None);
        }
        // Under isolation only a fake inside the snapshot's private world is
        // eligible. An installed host gh must never contact a real tracker.
        let private_fake = self
            .env
            .home_dir()
            .and_then(|home| home.parent().map(Path::to_owned))
            .filter(|root| root.components().count() > 2)
            .is_some_and(|root| executable.starts_with(canonicalize_lenient(&root)));
        if (cfg!(test) || self.env.test_isolation()) && !private_fake {
            return Ok(None);
        }
        let mut cmd = host_cli::command(
            &self.env,
            self.gh_command
                .to_str()
                .ok_or_else(|| transport::error(PROVIDER, "invalid host executable path"))?,
        );
        cmd.args(args)
            .current_dir(self.env.temp_dir())
            .env_remove("GH_HOST")
            .env_remove("GH_REPO");
        match cli::run(&mut cmd, body.map(str::as_bytes)) {
            Ok(output) if output.status.success() => Ok(Some(output.stdout)),
            Ok(output) => {
                if operation == IssueOperation::Create && !output.stdout.is_empty() {
                    return Err(transport::unknown(PROVIDER));
                }
                // Examine diagnostics only to classify them. Never expose their
                // raw bytes: gh can echo bodies, tokens and query strings.
                let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
                let rejection = if output.status.code() == Some(4)
                    || stderr.contains("gh auth login")
                    || stderr.contains("not logged")
                    || stderr.contains("authentication")
                    || stderr.contains("http 401")
                {
                    Some(IssueSourceError::Unauthorized {
                        provider: PROVIDER.into(),
                        hint: AUTH_HINT.into(),
                    })
                } else if stderr.contains("rate limit") || stderr.contains("http 429") {
                    Some(IssueSourceError::RateLimited {
                        provider: PROVIDER.into(),
                    })
                } else if stderr.contains("permission")
                    || stderr.contains("http 403")
                    || stderr.contains("resource not accessible")
                {
                    Some(IssueSourceError::Forbidden {
                        provider: PROVIDER.into(),
                        hint: "check repository write permission and token scopes".into(),
                    })
                } else if stderr.contains("http 404")
                    || stderr.contains("http 422")
                    || stderr.contains("unknown flag:")
                {
                    Some(transport::error(PROVIDER, "gh rejected the request"))
                } else {
                    None
                };
                if operation == IssueOperation::Create && rejection.is_none() {
                    return Err(transport::unknown(PROVIDER));
                }
                if let Some(error @ IssueSourceError::RateLimited { .. }) = rejection {
                    return Err(error);
                }
                // A create that gh started may have succeeded before a later
                // step failed. Only "not logged in" proves nothing was created,
                // so every other classified create failure is final: never a
                // second create over REST.
                if operation == IssueOperation::Create
                    && !matches!(rejection, Some(IssueSourceError::Unauthorized { .. }))
                {
                    return Err(rejection.unwrap_or_else(|| transport::unknown(PROVIDER)));
                }
                if self
                    .env
                    .github_token()
                    .is_some_and(|token| !token.is_empty())
                {
                    return Ok(None);
                }
                Err(rejection
                    .unwrap_or_else(|| transport::error(PROVIDER, "gh issue command failed")))
            }
            Err(cli::Failure::NotStarted) => Ok(None),
            Err(_) if operation == IssueOperation::Create => Err(transport::unknown(PROVIDER)),
            Err(cli::Failure::Timeout) => Err(IssueSourceError::Timeout {
                provider: PROVIDER.into(),
                operation,
            }),
            Err(cli::Failure::TooLarge) => Err(IssueSourceError::ResponseTooLarge {
                provider: PROVIDER.into(),
                limit: transport::RESPONSE_LIMIT,
            }),
            Err(cli::Failure::Io) => Err(transport::error(PROVIDER, "gh communication failed")),
        }
    }
    fn rest(
        &self,
        repository: &GithubRepository,
        number: Option<u32>,
        method: Method,
        payload: Option<serde_json::Value>,
        operation: IssueOperation,
    ) -> Result<Vec<u8>, IssueSourceError> {
        let token = self
            .env
            .github_token()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| IssueSourceError::Unauthorized {
                provider: PROVIDER.into(),
                hint: AUTH_HINT.into(),
            })?;
        let mut url = self.api_base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| transport::error(PROVIDER, "invalid API base"))?;
            path.pop_if_empty()
                .push("repos")
                .push(&repository.owner)
                .push(&repository.repo)
                .push("issues");
            if let Some(number) = number {
                path.push(&number.to_string());
            }
        }
        transport::outbound(PROVIDER, &url)?;
        let client = transport::client(PROVIDER)?;
        let mut request = client
            .request(method, url)
            .header("Accept", "application/vnd.github+json")
            .bearer_auth(token);
        if let Some(payload) = payload {
            request = request.json(&payload);
        }
        let response = request
            .send()
            .map_err(|e| transport::request_error(PROVIDER, operation, e))?;
        transport::status(
            PROVIDER,
            &response,
            &number.map(|n| issue_url(repository, n)).unwrap_or_default(),
            AUTH_HINT,
            operation,
        )?;
        transport::read_response(PROVIDER, response, operation)
    }
    fn fetch(
        &self,
        repository: &GithubRepository,
        number: u32,
        git_root: &Path,
    ) -> Result<Issue, IssueSourceError> {
        let args = vec![
            "issue".into(),
            "view".into(),
            number.to_string(),
            "--repo".into(),
            cli_repo_arg(repository),
            "--json".into(),
            "number,title,body,url".into(),
        ];
        if let Some(bytes) = self.run_cli(&args, None, IssueOperation::Fetch, git_root)? {
            if let Ok(issue) = json_issue(&bytes, repository, Some(number)) {
                return Ok(issue);
            }
        }
        let bytes = self.rest(
            repository,
            Some(number),
            Method::GET,
            None,
            IssueOperation::Fetch,
        )?;
        json_issue(&bytes, repository, Some(number))
    }
}
fn repo_arg(repository: &GithubRepository) -> String {
    format!("{}/{}", repository.owner, repository.repo)
}
/// `--repo` value for `gh`. The explicit `github.com/` host keeps `gh` from
/// following `GH_HOST` to a different server than the one awman reports.
fn cli_repo_arg(repository: &GithubRepository) -> String {
    format!("github.com/{}", repo_arg(repository))
}
fn issue_url(repository: &GithubRepository, number: u32) -> String {
    format!(
        "https://github.com/{}/issues/{number}",
        repo_arg(repository)
    )
}
fn validate_repository(repository: &GithubRepository) -> Result<(), IssueSourceError> {
    let safe = |part: &str| {
        !part.is_empty()
            && !matches!(part, "." | "..")
            && part
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    };
    if !safe(&repository.owner) || !safe(&repository.repo) {
        return Err(transport::error(PROVIDER, "invalid GitHub repository"));
    }
    Ok(())
}
#[derive(Deserialize)]
struct GithubIssue {
    number: u32,
    title: String,
    #[serde(default)]
    body: Option<String>,
}
fn json_issue(
    bytes: &[u8],
    repository: &GithubRepository,
    expected: Option<u32>,
) -> Result<Issue, IssueSourceError> {
    let value: GithubIssue = serde_json::from_slice(bytes)
        .map_err(|_| transport::error(PROVIDER, "invalid GitHub response"))?;
    if value.number == 0 || expected.is_some_and(|n| n != value.number) {
        return Err(transport::error(
            PROVIDER,
            "GitHub response changed issue number",
        ));
    }
    Ok(Issue {
        source_id: issue_url(repository, value.number),
        id: value.number.to_string(),
        number: Some(value.number),
        title: value.title,
        body: value.body.unwrap_or_default(),
        provider: PROVIDER.into(),
    })
}
impl IssueSource for GithubIssueSource {
    fn provider_name(&self) -> &str {
        "GitHub"
    }
    fn provider_prefix(&self) -> &str {
        "ghb"
    }
    fn issue_identifier(&self, issue: &Issue) -> String {
        issue.id.clone()
    }
    fn fetch_issue(&self, input: &str, git_root: &Path) -> Result<Issue, IssueSourceError> {
        let (repository, number) = self.parse_input(input, git_root)?;
        self.fetch(&repository, number, git_root)
    }
    fn fetch_issue_with_progress(
        &self,
        input: &str,
        git_root: &Path,
        sink: &mut dyn UserMessageSink,
    ) -> Result<Issue, IssueSourceError> {
        let (repository, number) = self.parse_input(input, git_root)?;
        sink.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("fetching GitHub issue {}#{number}", repo_arg(&repository)),
        });
        self.fetch(&repository, number, git_root)
    }
    fn create_issue(
        &self,
        title: &str,
        body: &str,
        git_root: &Path,
    ) -> Result<Issue, IssueSourceError> {
        transport::check_body(PROVIDER, body)?;
        let repository = self.origin_repository(git_root)?;
        let args = vec![
            "issue".into(),
            "create".into(),
            "--repo".into(),
            cli_repo_arg(&repository),
            format!("--title={title}"),
            "--body-file".into(),
            "-".into(),
        ];
        if let Some(bytes) = self.run_cli(&args, Some(body), IssueOperation::Create, git_root)? {
            let output = std::str::from_utf8(&bytes).map_err(|_| transport::unknown(PROVIDER))?;
            let found: Vec<_> = output
                .split_whitespace()
                .filter_map(|word| match WorkItemRef::parse(word) {
                    Ok(WorkItemRef::Github {
                        repository: Some(repo),
                        number,
                    }) if word.starts_with("https://") && repo.same_as(&repository) => Some(number),
                    _ => None,
                })
                .collect();
            if found.len() != 1 {
                return Err(transport::unknown(PROVIDER));
            }
            let number = found[0];
            return Ok(Issue {
                source_id: issue_url(&repository, number),
                id: number.to_string(),
                number: Some(number),
                title: title.into(),
                body: body.into(),
                provider: PROVIDER.into(),
            });
        }
        let bytes = self.rest(
            &repository,
            None,
            Method::POST,
            Some(serde_json::json!({"title":title,"body":body})),
            IssueOperation::Create,
        )?;
        json_issue(&bytes, &repository, None).map_err(|_| transport::unknown(PROVIDER))
    }
    fn update_issue(&self, issue: &Issue, body: &str) -> Result<Issue, IssueSourceError> {
        transport::check_body(PROVIDER, body)?;
        if issue.provider != PROVIDER {
            return Err(transport::error(
                PROVIDER,
                "issue belongs to another provider",
            ));
        }
        let (repository, number) = self.parse_input(&issue.source_id, Path::new(""))?;
        if issue.number != Some(number) || issue.id != number.to_string() {
            return Err(transport::error(PROVIDER, "inconsistent issue identity"));
        }
        let args = vec![
            "issue".into(),
            "edit".into(),
            number.to_string(),
            "--repo".into(),
            cli_repo_arg(&repository),
            "--body-file".into(),
            "-".into(),
        ];
        let git_root = self
            .git_engine
            .resolve_root(self.env.cwd())
            .unwrap_or_else(|_| self.env.cwd().into());
        if self
            .run_cli(&args, Some(body), IssueOperation::Update, &git_root)?
            .is_some()
        {
            return Ok(Issue {
                body: body.into(),
                ..issue.clone()
            });
        }
        let bytes = self.rest(
            &repository,
            Some(number),
            Method::PATCH,
            Some(serde_json::json!({"body":body})),
            IssueOperation::Update,
        )?;
        if bytes.is_empty() {
            return Ok(Issue {
                body: body.into(),
                ..issue.clone()
            });
        }
        let mut updated = json_issue(&bytes, &repository, Some(number))?;
        // Some write endpoints return no body; use the uploaded raw body.
        if serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("body").cloned())
            .is_none_or(|v| v.is_null())
        {
            updated.body = body.into();
        }
        updated.source_id = issue.source_id.clone();
        Ok(updated)
    }
}
