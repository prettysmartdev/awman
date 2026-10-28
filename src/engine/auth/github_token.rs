//! `GitHubTokenResolver` — every source of a GitHub token a workflow
//! prerequisite may draw on (WI 0149), in one place:
//!
//! - host environment variables, in the configured `envVars` order (the
//!   caller is told the variable **name** only);
//! - the host `gh` CLI (`gh auth status`, `gh auth token`, `gh auth login`),
//!   always as fixed argv through [`HostSecretCommand`];
//! - a configured `acquire` command, also through [`HostSecretCommand`];
//! - best-effort verification against the GitHub REST API.
//!
//! Nothing here prompts; Layer 2 decides which source to use and asks.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::data::config::env::{test_isolation_active, EnvSnapshot};
use crate::data::config::workflows::AcquirePolicy;

use super::credential::SecretString;
use super::host_secret_command::{
    HostSecretCommand, HostSecretCommandError, HostSecretCommandOptions, HostStdin,
    HOST_SECRET_TIMEOUT_MAX,
};

/// Bound on `gh auth status` and the verification request.
pub const GITHUB_TOKEN_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhAuthStatus {
    NotInstalled,
    LoggedIn { path: PathBuf },
    NotLoggedIn { path: PathBuf },
    Unknown { path: PathBuf, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenVerification {
    /// `repo_scope` is `Some` only for classic PATs (which report `X-OAuth-Scopes`).
    Verified {
        login: String,
        repo_scope: Option<bool>,
    },
    Rejected {
        status: u16,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct GitHubTokenResolverOptions {
    /// "github.com" or a GitHub Enterprise host name.
    pub host: String,
    pub env_vars: Vec<String>,
    pub verify: bool,
    /// 10 s by default.
    pub timeout: Duration,
}

impl Default for GitHubTokenResolverOptions {
    fn default() -> Self {
        Self {
            host: "github.com".into(),
            env_vars: crate::data::config::workflows::PREREQUISITES_DEFAULT_ENV_VARS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            verify: true,
            timeout: GITHUB_TOKEN_PROBE_TIMEOUT,
        }
    }
}

/// The token sources the prerequisite preflight probes. `GitHubTokenResolver`
/// is the production implementation; tests inject a fake. The preflight's
/// `sources.rs` re-exports this trait rather than declaring its own.
pub trait TokenProbeSource: Send + Sync {
    fn env_token_name(&self, vars: &[String]) -> Option<String>;
    fn gh_on_path(&self) -> Option<PathBuf>;
    fn host_gh_status(&self, host: &str) -> GhAuthStatus;
    fn verify(&self, token: &SecretString, host: &str) -> TokenVerification;
    /// Optional value access for verification and non-interactive refresh.
    /// Defaults let name-only probe fakes remain valid without reading the host env.
    fn env_token(&self, _var: &str) -> Option<SecretString> {
        None
    }
    fn acquire_token(
        &self,
        _acquire: &AcquirePolicy,
        _refused: &[String],
    ) -> Result<super::host_secret_command::HostSecretOutput, HostSecretCommandError> {
        Err(HostSecretCommandError::Spawn {
            program: "token source".into(),
            reason: "token acquisition is unsupported by this source".into(),
        })
    }
}

pub struct GitHubTokenResolver {
    opts: GitHubTokenResolverOptions,
    refused_binaries: Vec<String>,
    repository_root: Option<PathBuf>,
    /// `None` reads through `host_var` (daemon-overlay aware); `Some` reads a
    /// frozen snapshot only.
    env: Option<EnvSnapshot>,
}

impl std::fmt::Debug for GitHubTokenResolver {
    /// The snapshot may hold token values, so only its presence is shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubTokenResolver")
            .field("opts", &self.opts)
            .field("env_snapshot", &self.env.is_some())
            .finish()
    }
}

impl GitHubTokenResolver {
    pub fn new(opts: GitHubTokenResolverOptions) -> Self {
        Self {
            opts,
            env: None,
            refused_binaries: Vec::new(),
            repository_root: None,
        }
    }

    pub fn with_refused_binaries(mut self, binaries: Vec<String>) -> Self {
        self.refused_binaries = binaries;
        self
    }
    pub fn with_repository_root(mut self, root: PathBuf) -> Self {
        self.repository_root = Some(root);
        self
    }

    /// Read environment variables from `env` instead of the host.
    pub fn with_env_snapshot(mut self, env: EnvSnapshot) -> Self {
        self.env = Some(env);
        self
    }

    pub fn host(&self) -> &str {
        &self.opts.host
    }

    pub fn options(&self) -> &GitHubTokenResolverOptions {
        &self.opts
    }

    /// First var in `env_vars` order with a non-empty value, read through `data::config::env::host_var` (daemon overlay aware). Name only.
    pub fn env_token_name(&self) -> Option<String> {
        self.first_env_var(&self.opts.env_vars)
    }

    pub fn env_token(&self, var: &str) -> Option<SecretString> {
        self.env_value(var).map(SecretString::new)
    }

    /// `which("gh")`; under test isolation only a fake inside the temp dir counts.
    pub fn gh_on_path(&self) -> Option<PathBuf> {
        let found = which::which("gh").ok().filter(|p| p.is_absolute())?;
        if test_isolation_active() && !inside_temp_dir(&found) {
            return None;
        }
        Some(found)
    }

    /// `gh auth status --hostname <host>`, 10 s, stdin null.
    pub fn host_gh_status(&self) -> GhAuthStatus {
        let Some(path) = self.gh_on_path() else {
            return GhAuthStatus::NotInstalled;
        };
        let command = match self.gh_command(
            &path,
            &["auth", "status", "--hostname"],
            "gh auth status",
            HostStdin::Null,
            self.opts.timeout,
            false,
        ) {
            Ok(command) => command,
            Err(e) => {
                return GhAuthStatus::Unknown {
                    path,
                    reason: e.to_string(),
                }
            }
        };
        match command.run() {
            Ok(_) => GhAuthStatus::LoggedIn { path },
            Err(HostSecretCommandError::NonZeroExit { .. }) => GhAuthStatus::NotLoggedIn { path },
            Err(e) => GhAuthStatus::Unknown {
                path,
                reason: e.to_string(),
            },
        }
    }

    /// The moved `poll_ci::gh_is_available`: installed and `gh auth status --hostname host` succeeds.
    pub fn host_gh_authenticated(host: &str) -> bool {
        let resolver = Self::new(GitHubTokenResolverOptions {
            host: host.to_string(),
            ..Default::default()
        });
        matches!(resolver.host_gh_status(), GhAuthStatus::LoggedIn { .. })
    }

    /// `gh auth token --hostname <host>`.
    pub fn gh_auth_token_command(
        &self,
        stdin: HostStdin,
    ) -> Result<HostSecretCommand, HostSecretCommandError> {
        let path = self.gh_path()?;
        self.gh_command(
            &path,
            &["auth", "token", "--hostname"],
            "gh auth token",
            stdin,
            self.opts.timeout,
            true,
        )
    }

    /// `gh auth login --hostname <host> --web --git-protocol https`, attached
    /// to the terminal, 300 s, no token expected.
    pub fn gh_auth_login_command(&self) -> Result<HostSecretCommand, HostSecretCommandError> {
        let path = self.gh_path()?;
        let mut command = self.gh_options(&path, &["auth", "login", "--hostname"])?;
        command
            .argv
            .extend(["--web", "--git-protocol", "https"].map(String::from));
        command.label = "gh auth login".into();
        command.stdin = HostStdin::Tty;
        command.timeout = HOST_SECRET_TIMEOUT_MAX;
        command.expect_secret = false;
        HostSecretCommand::new(command)?.outside_repository(self.repository_root.as_deref())
    }

    /// The configured `acquire` argv. Approval of a repo-scoped command is
    /// Layer 2's job (by [`HostSecretCommand::sha256`]); this only builds it.
    pub fn acquire_command(
        &self,
        acquire: &AcquirePolicy,
        stdin: HostStdin,
        refused_binaries: &[String],
    ) -> Result<HostSecretCommand, HostSecretCommandError> {
        let program = acquire.argv.first().cloned().unwrap_or_default();
        let ellipsis = if acquire.argv.len() > 1 { " …" } else { "" };
        HostSecretCommand::new(HostSecretCommandOptions {
            argv: acquire.argv.clone(),
            label: format!("configured token command ({program}{ellipsis})"),
            timeout: Duration::from_secs(acquire.timeout_secs),
            stdin,
            refused_binaries: refused_binaries.to_vec(),
            expect_secret: true,
        })?
        .outside_repository(self.repository_root.as_deref())
    }

    /// GET https://api.github.com/user (or https://<host>/api/v3/user); reads `login` and, for classic PATs, `X-OAuth-Scopes` ∋ repo.
    /// Network/timeout ⇒ Unknown; disabled (Unknown{"test isolation"}) when test isolation is active. Never logs the token.
    pub fn verify(&self, token: &SecretString) -> TokenVerification {
        if !self.opts.verify {
            return TokenVerification::Unknown {
                reason: "verification disabled".into(),
            };
        }
        verify_token(token, &self.opts.host, self.opts.timeout)
    }

    fn first_env_var(&self, vars: &[String]) -> Option<String> {
        vars.iter().find(|v| self.env_value(v).is_some()).cloned()
    }

    fn env_value(&self, var: &str) -> Option<String> {
        let value = match &self.env {
            Some(env) => env.get(var).map(str::to_string),
            None => crate::data::config::env::host_var(var),
        };
        value.filter(|v| !v.trim().is_empty())
    }

    fn gh_path(&self) -> Result<PathBuf, HostSecretCommandError> {
        self.gh_on_path()
            .ok_or_else(|| HostSecretCommandError::NotFound {
                program: "gh".into(),
            })
    }

    /// `[<resolved gh>, <args…>, <host>]`. The resolved path is argv[0] so the
    /// command runs exactly the `gh` the test-isolation rule accepted.
    fn gh_options(
        &self,
        path: &Path,
        args: &[&str],
    ) -> Result<HostSecretCommandOptions, HostSecretCommandError> {
        if !valid_host(&self.opts.host) {
            return Err(HostSecretCommandError::Spawn {
                program: "gh".into(),
                reason: format!("invalid GitHub host {:?}", self.opts.host),
            });
        }
        let mut argv = vec![path.to_string_lossy().into_owned()];
        argv.extend(args.iter().map(|a| a.to_string()));
        argv.push(self.opts.host.clone());
        Ok(HostSecretCommandOptions {
            argv,
            label: String::new(),
            timeout: self.opts.timeout,
            stdin: HostStdin::Null,
            refused_binaries: self.refused_binaries.clone(),
            expect_secret: true,
        })
    }

    fn gh_command(
        &self,
        path: &Path,
        args: &[&str],
        label: &str,
        stdin: HostStdin,
        timeout: Duration,
        expect_secret: bool,
    ) -> Result<HostSecretCommand, HostSecretCommandError> {
        let mut options = self.gh_options(path, args)?;
        options.label = label.into();
        options.stdin = stdin;
        options.timeout = timeout;
        options.expect_secret = expect_secret;
        HostSecretCommand::new(options)?.outside_repository(self.repository_root.as_deref())
    }
}

impl TokenProbeSource for GitHubTokenResolver {
    fn env_token(&self, var: &str) -> Option<SecretString> {
        GitHubTokenResolver::env_token(self, var)
    }

    fn acquire_token(
        &self,
        acquire: &AcquirePolicy,
        refused: &[String],
    ) -> Result<super::host_secret_command::HostSecretOutput, HostSecretCommandError> {
        self.acquire_command(acquire, HostStdin::Null, refused)?
            .run()
    }
    fn env_token_name(&self, vars: &[String]) -> Option<String> {
        self.first_env_var(vars)
    }

    fn gh_on_path(&self) -> Option<PathBuf> {
        GitHubTokenResolver::gh_on_path(self)
    }

    fn host_gh_status(&self, host: &str) -> GhAuthStatus {
        if host == self.opts.host {
            return GitHubTokenResolver::host_gh_status(self);
        }
        let resolver = Self {
            opts: GitHubTokenResolverOptions {
                host: host.to_string(),
                ..self.opts.clone()
            },
            env: self.env.clone(),
            refused_binaries: self.refused_binaries.clone(),
            repository_root: self.repository_root.clone(),
        };
        resolver.host_gh_status()
    }

    fn verify(&self, token: &SecretString, host: &str) -> TokenVerification {
        if !self.opts.verify {
            return TokenVerification::Unknown {
                reason: "verification disabled".into(),
            };
        }
        verify_token(token, host, self.opts.timeout)
    }
}

/// A bare DNS name (optionally `:port`): nothing that could alter the URL
/// path or authority, and nothing `gh` could read as a flag.
pub(crate) fn valid_host(host: &str) -> bool {
    let (name, port) = match host.rsplit_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    !name.is_empty()
        && !name.starts_with(['-', '.'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && port
            .is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.chars().all(|c| c.is_ascii_digit()))
}

fn inside_temp_dir(path: &Path) -> bool {
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canonical(path).starts_with(canonical(&std::env::temp_dir()))
}

fn verify_url(host: &str) -> String {
    if host.eq_ignore_ascii_case("github.com") {
        "https://api.github.com/user".into()
    } else {
        format!("https://{host}/api/v3/user")
    }
}

fn verify_token(token: &SecretString, host: &str, timeout: Duration) -> TokenVerification {
    if test_isolation_active() {
        return TokenVerification::Unknown {
            reason: "test isolation".into(),
        };
    }
    if !valid_host(host) {
        return TokenVerification::Unknown {
            reason: "invalid GitHub host".into(),
        };
    }
    let url = verify_url(host);
    let token = token.clone();
    // The blocking client must not run on a tokio worker (it owns a runtime
    // of its own), so the request always runs on a plain thread.
    std::thread::spawn(move || verify_blocking(&url, &token, timeout))
        .join()
        .unwrap_or_else(|_| TokenVerification::Unknown {
            reason: "verification thread panicked".into(),
        })
}

fn verify_blocking(url: &str, token: &SecretString, timeout: Duration) -> TokenVerification {
    use reqwest::header::{HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
    let unknown = |reason: String| TokenVerification::Unknown { reason };
    let client = match reqwest::blocking::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(e) => return unknown(format!("http client: {}", e.without_url())),
    };
    let Ok(mut auth) = HeaderValue::from_str(&format!("Bearer {}", token.expose())) else {
        return unknown("token is not a valid header value".into());
    };
    auth.set_sensitive(true);
    let response = match client
        .get(url)
        .header(AUTHORIZATION, auth)
        .header(ACCEPT, "application/vnd.github+json")
        .header(USER_AGENT, "awman")
        .send()
    {
        Ok(response) => response,
        Err(e) => return unknown(format!("GitHub API unreachable: {}", e.without_url())),
    };
    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return TokenVerification::Rejected { status };
    }
    if !response.status().is_success() {
        return unknown(format!("GitHub API returned HTTP {status}"));
    }
    let repo_scope = response
        .headers()
        .get("x-oauth-scopes")
        .and_then(|v| v.to_str().ok())
        .map(|scopes| scopes.split(',').any(|s| s.trim() == "repo"));
    match response.json::<serde_json::Value>() {
        Ok(body) => match body.get("login").and_then(|l| l.as_str()) {
            Some(login) => TokenVerification::Verified {
                login: login.to_string(),
                repo_scope,
            },
            None => unknown("GitHub API response has no login".into()),
        },
        Err(_) => unknown("GitHub API response did not parse".into()),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn resolver(env: &[(&str, &str)], vars: &[&str]) -> GitHubTokenResolver {
        GitHubTokenResolver::new(GitHubTokenResolverOptions {
            env_vars: vars.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        })
        .with_env_snapshot(EnvSnapshot::with_overrides(env.iter().copied()))
    }

    #[test]
    fn env_lookup_follows_configured_order_and_skips_empty() {
        let r = resolver(
            &[
                ("GH_TOKEN", " "),
                ("GITHUB_TOKEN", "ghp_x1"),
                ("OTHER", "o"),
            ],
            &["GH_TOKEN", "GITHUB_TOKEN", "OTHER"],
        );
        assert_eq!(r.env_token_name().as_deref(), Some("GITHUB_TOKEN"));
        assert_eq!(r.env_token("GITHUB_TOKEN").unwrap().expose(), "ghp_x1");
        assert!(r.env_token("GH_TOKEN").is_none());
        assert_eq!(
            TokenProbeSource::env_token_name(&r, &["OTHER".into()]).as_deref(),
            Some("OTHER")
        );
        assert!(!format!("{r:?}").contains("ghp_x1"));
    }

    #[test]
    fn verification_is_disabled_under_test_isolation() {
        let r = resolver(&[], &[]);
        assert_eq!(
            r.verify(&SecretString::new("ghp_whatever")),
            TokenVerification::Unknown {
                reason: "test isolation".into()
            }
        );
        assert_eq!(verify_url("github.com"), "https://api.github.com/user");
        assert_eq!(
            verify_url("ghe.example.com"),
            "https://ghe.example.com/api/v3/user"
        );
    }

    #[test]
    fn hosts_are_validated() {
        assert!(valid_host("github.com"));
        assert!(valid_host("ghe.example.com:8443"));
        for bad in ["", "-x", "a/b", "a@b", "a b", "evil.com/x?", "h:", "h:abc"] {
            assert!(!valid_host(bad), "{bad}");
        }
    }

    #[test]
    fn acquire_and_gh_commands_use_fixed_argv() {
        let dir = tempfile::tempdir().unwrap();
        let op = dir.path().join("op");
        std::fs::write(&op, "#!/bin/sh\necho tok_from_op\n").unwrap();
        std::fs::set_permissions(&op, std::fs::Permissions::from_mode(0o700)).unwrap();
        let policy = AcquirePolicy {
            argv: vec![op.to_string_lossy().into(), "read".into()],
            timeout_secs: 5,
            refresh: Default::default(),
            scope: crate::data::workflow_prerequisites::AcquireScope::Global,
            sha256: String::new(),
        };
        let r = resolver(&[], &[]);
        let cmd = r.acquire_command(&policy, HostStdin::Null, &[]).unwrap();
        assert_eq!(cmd.review().timeout_secs, 5);
        assert_eq!(cmd.run().unwrap().secret.unwrap().expose(), "tok_from_op");

        let path = dir.path().join("gh");
        let options = r
            .gh_options(&path, &["auth", "token", "--hostname"])
            .unwrap();
        assert_eq!(
            options.argv[1..],
            ["auth", "token", "--hostname", "github.com"].map(String::from)
        );
    }
}

#[cfg(all(test, unix))]
mod custom_gh_review_tests {
    use super::*;
    #[test]
    fn builtin_gh_uses_the_complete_custom_agent_refusal_set() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("custom-agent-program");
        std::fs::write(&agent, "#!/bin/sh\necho fake_token\n").unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let gh = dir.path().join("gh");
        symlink(&agent, &gh).unwrap();
        let resolver = GitHubTokenResolver::new(Default::default())
            .with_refused_binaries(vec![agent.display().to_string()]);
        assert!(resolver
            .gh_command(
                &gh,
                &["auth", "status", "--hostname"],
                "fixture",
                HostStdin::Null,
                Duration::from_secs(1),
                false
            )
            .is_err());
    }
}
