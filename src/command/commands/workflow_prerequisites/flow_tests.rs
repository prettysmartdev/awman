#![allow(clippy::field_reassign_with_default)]
//! The whole prerequisite flow through its frontend trait: probe
//! (`prepare` runs the production preflight against a temp repository and
//! home), review (the report the frontend is shown, and what it is told comes
//! next), grant (the answers a scripted frontend gives), and inject (the
//! grants turned into container env/mounts by the overlay engine).
//!
//! Hermetic: the repository and home are temp dirs, the token variables are
//! test-only names carried in each session snapshot; test isolation keeps the
//! container runtime, `gh` and GitHub verification out of reach, and nothing
//! sleeps.

use super::*;
use crate::command::dispatch::{catalogue::FrontendKind, Engines};
use crate::command::error::CommandError;
use crate::data::config::flags::*;
use crate::data::config::workflows::*;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::prompt::{Prompt, TextPrompt};
use crate::data::session::Session;
use crate::data::workflow_prerequisites::*;
use crate::engine::auth::credential::SecretString;
use crate::engine::workflow::prerequisite_grants::*;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// A test-only variable no developer machine sets.
const TOKEN_VAR: &str = "AWMAN_COVERAGE_FLOW_TOKEN";
const TOKEN_VALUE: &str = "ghp_coverage_flow_token_0001";

// ── Scripted frontend ────────────────────────────────────────────────────

#[derive(Default)]
struct Script {
    identity: VecDeque<IdentityChoice>,
    identity_text: VecDeque<Option<String>>,
    token_consent: VecDeque<TokenConsent>,
    token_source: VecDeque<TokenSourceChoice>,
    secret: VecDeque<Option<String>>,
    ssh: VecDeque<SshChoice>,
    gap: VecDeque<GapChoice>,
}

#[derive(Default)]
struct Scripted {
    interactive: bool,
    script: Script,
    /// Every question asked, by kind, in order.
    asked: Vec<&'static str>,
    shown: Vec<(WorkflowPrerequisiteReport, PrerequisiteReportNext)>,
    messages: Vec<String>,
    abort_on_show: bool,
}

impl Scripted {
    fn interactive(script: Script) -> Self {
        Self {
            interactive: true,
            script,
            ..Default::default()
        }
    }
    fn next<T>(queue: &mut VecDeque<T>, what: &str) -> T {
        queue
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected {what} question"))
    }
}

impl UserMessageSink for Scripted {
    fn replay_queued(&mut self) {}
    fn write_message(&mut self, message: UserMessage) {
        self.messages.push(message.text);
    }
}

impl WorkflowPrerequisiteFrontend for Scripted {
    fn show_workflow_prerequisites(
        &mut self,
        report: &WorkflowPrerequisiteReport,
        next: PrerequisiteReportNext,
    ) -> Result<(), CommandError> {
        self.shown.push((report.clone(), next));
        if self.abort_on_show {
            return Err(CommandError::Aborted);
        }
        Ok(())
    }
    fn can_ask_prerequisites(&self) -> bool {
        self.interactive
    }
    fn ask_prerequisite_identity(
        &mut self,
        _: &Prompt<IdentityChoice>,
    ) -> Result<IdentityChoice, CommandError> {
        self.asked.push("identity");
        Ok(Self::next(&mut self.script.identity, "identity"))
    }
    fn ask_prerequisite_identity_text(
        &mut self,
        _: &TextPrompt,
    ) -> Result<Option<String>, CommandError> {
        self.asked.push("identity_text");
        Ok(Self::next(&mut self.script.identity_text, "identity text"))
    }
    fn ask_prerequisite_ssh(&mut self, _: &Prompt<SshChoice>) -> Result<SshChoice, CommandError> {
        self.asked.push("ssh");
        Ok(Self::next(&mut self.script.ssh, "ssh"))
    }
    fn ask_prerequisite_token_consent(
        &mut self,
        _: &Prompt<TokenConsent>,
    ) -> Result<TokenConsent, CommandError> {
        self.asked.push("token_consent");
        Ok(Self::next(&mut self.script.token_consent, "token consent"))
    }
    fn ask_prerequisite_token_source(
        &mut self,
        _: &Prompt<TokenSourceChoice>,
    ) -> Result<TokenSourceChoice, CommandError> {
        self.asked.push("token_source");
        Ok(Self::next(&mut self.script.token_source, "token source"))
    }
    fn ask_prerequisite_secret(
        &mut self,
        prompt: &TextPrompt,
    ) -> Result<Option<SecretString>, CommandError> {
        assert!(prompt.masked, "a pasted token must be asked for masked");
        self.asked.push("secret");
        Ok(Self::next(&mut self.script.secret, "secret").map(SecretString::new))
    }
    fn ask_prerequisite_gap(&mut self, _: &Prompt<GapChoice>) -> Result<GapChoice, CommandError> {
        self.asked.push("gap");
        Ok(Self::next(&mut self.script.gap, "gap"))
    }
}

// ── Fixtures ─────────────────────────────────────────────────────────────

struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    /// A git repository with an `origin` remote, inside a temp dir that is
    /// also the session's home and data root.
    fn new(remote: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        if let Some(url) = remote {
            git(&["remote", "add", "origin", url]);
        }
        Self { dir }
    }
    fn root(&self) -> &Path {
        self.dir.path()
    }
}

fn base_policy() -> PrerequisitePolicy {
    let mut policy = PrerequisitePolicy::default();
    policy.github_token.env_vars = vec![TOKEN_VAR.to_string()];
    policy.github_token.verify = false;
    policy.unresolved = Some(UnresolvedMode::Warn);
    policy
}

fn flow_at(
    root: &Path,
    kind: FrontendKind,
    flags: PrerequisiteFlags,
    policy: PrerequisitePolicy,
) -> WorkflowPrerequisiteFlow {
    flow_at_env(root, kind, flags, policy, None)
}

fn flow_at_with_token(
    root: &Path,
    kind: FrontendKind,
    flags: PrerequisiteFlags,
    policy: PrerequisitePolicy,
) -> WorkflowPrerequisiteFlow {
    flow_at_env(root, kind, flags, policy, Some(TOKEN_VALUE))
}

fn flow_at_env(
    root: &Path,
    kind: FrontendKind,
    flags: PrerequisiteFlags,
    policy: PrerequisitePolicy,
    token: Option<&str>,
) -> WorkflowPrerequisiteFlow {
    let mut values = vec![
        ("HOME", root.to_str().unwrap()),
        ("AWMAN_HOME", root.to_str().unwrap()),
        ("AWMAN_TEST_ISOLATION", "1"),
    ];
    if let Some(token) = token {
        values.push((TOKEN_VAR, token));
    }
    let session = Session::for_tests_with_env(
        root,
        crate::data::config::env::EnvSnapshot::with_overrides(values),
    );
    let mut options = WorkflowPrerequisiteFlowOptions::for_session(
        &session,
        kind,
        &flags,
        None,
        PrerequisiteOrigin::Static,
        None,
    );
    options.policy = policy;
    WorkflowPrerequisiteFlow::new(&Engines::for_tests(root), &session, options)
}

fn need(prerequisite: WorkflowPrerequisite, step: PrerequisiteStepKind) -> PrerequisiteNeed {
    PrerequisiteNeed {
        prerequisite,
        needed_by: vec![StepRef {
            phase: crate::data::workflow_state::PhaseKind::Teardown,
            index: 0,
            kind: step,
            description: format!("{step:?}"),
        }],
    }
}

fn needs(list: Vec<PrerequisiteNeed>) -> WorkflowPrerequisites {
    WorkflowPrerequisites {
        origin: PrerequisiteOrigin::Static,
        needs: list,
        advisories: vec![],
    }
}

fn token_need() -> PrerequisiteNeed {
    need(
        WorkflowPrerequisite::GitHubToken {
            host: "github.com".into(),
        },
        PrerequisiteStepKind::CreatePullRequest,
    )
}

fn identity_need() -> PrerequisiteNeed {
    need(
        WorkflowPrerequisite::GitIdentity,
        PrerequisiteStepKind::CommitChanges,
    )
}

fn status_of(report: &WorkflowPrerequisiteReport, p: &WorkflowPrerequisite) -> PrerequisiteStatus {
    report
        .checks
        .iter()
        .find(|c| &c.prerequisite == p)
        .unwrap_or_else(|| panic!("no check for {p:?}"))
        .status
}

fn overlay_engine(home: &Path) -> crate::engine::overlay::OverlayEngine {
    crate::engine::overlay::OverlayEngine::with_auth_resolver(
        crate::data::fs::auth_paths::AuthPathResolver::at_home(home),
    )
}

fn inject(
    prepared: &PreparedPrerequisites,
    home: &Path,
    kind: PrerequisiteStepKind,
) -> crate::engine::overlay::prerequisite_grants::GrantOverlays {
    let remote = prepared.remote("origin");
    overlay_engine(home)
        .prerequisite_grant_overlays(
            &crate::engine::overlay::prerequisite_grants::GrantTarget {
                repo: None,
                git_source: None,
                step_kind: kind,
                phase: crate::data::workflow_state::PhaseKind::Teardown,
                container_home: Some("/home/dev"),
                remote: remote.as_ref(),
            },
            &prepared.grants,
        )
        .unwrap()
}

fn env_value(
    o: &crate::engine::overlay::prerequisite_grants::GrantOverlays,
    name: &str,
) -> Option<String> {
    o.env_literals
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

fn grants_file(root: &Path) -> bool {
    walk(root).iter().any(|p| {
        p.file_name()
            .is_some_and(|n| n == "prerequisite-grants.json")
    })
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() && !path.ends_with(".git") {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

// ── Probe → review → grant → inject ──────────────────────────────────────

#[test]
fn interactive_prepare_reviews_first_then_grants_and_injects_identity_and_token() {
    std::env::remove_var(TOKEN_VAR);
    let repo = Repo::new(Some("https://github.com/o/r.git"));
    let flow = flow_at(
        repo.root(),
        FrontendKind::Cli,
        PrerequisiteFlags::default(),
        base_policy(),
    );
    let mut fe = Scripted::interactive(Script {
        identity: [IdentityChoice::Enter].into(),
        identity_text: [
            Some(" Ada Lovelace ".into()),
            Some("ada@example.test".into()),
        ]
        .into(),
        token_source: [TokenSourceChoice::Paste].into(),
        secret: [Some(format!("  {TOKEN_VALUE}\n"))].into(),
        ..Default::default()
    });
    let prepared = flow
        .prepare(&needs(vec![identity_need(), token_need()]), &mut fe)
        .unwrap();

    assert!(fe
        .messages
        .iter()
        .any(|m| m == "Checking 2 workflow prerequisites…"));
    assert!(fe
        .messages
        .iter()
        .any(|m| m == "Checking prerequisite: Git identity…"));
    assert!(fe
        .messages
        .iter()
        .any(|m| m.starts_with("Prerequisite: Git identity — ")));

    // Review: the report is shown once, before any question, and says that
    // questions follow.
    assert_eq!(fe.shown.len(), 1);
    let (shown, next) = &fe.shown[0];
    assert_eq!(
        *next,
        PrerequisiteReportNext::Questions {
            unresolved: UnresolvedMode::Warn
        }
    );
    assert!(shown
        .checks
        .iter()
        .all(|c| c.status != PrerequisiteStatus::Satisfied));
    assert_eq!(
        fe.asked,
        [
            "identity",
            "identity_text",
            "identity_text",
            "token_source",
            "secret"
        ]
    );

    // Grant: trimmed identity and token, both checks satisfied.
    let identity = prepared.grants.identity.as_ref().unwrap();
    assert_eq!(identity.name, "Ada Lovelace");
    assert_eq!(identity.email, "ada@example.test");
    assert_eq!(identity.source, IdentitySource::Entered);
    let token = prepared.grants.token.as_ref().unwrap();
    assert_eq!(token.secret.expose(), TOKEN_VALUE);
    assert_eq!(token.source, TokenGrantSource::Pasted);
    assert_eq!(token.host, "github.com");
    for p in [
        WorkflowPrerequisite::GitIdentity,
        WorkflowPrerequisite::GitHubToken {
            host: "github.com".into(),
        },
    ] {
        assert_eq!(
            status_of(&prepared.report, &p),
            PrerequisiteStatus::Satisfied
        );
    }
    // The token is in the run's redaction set and in no rendered report.
    assert!(!prepared.redaction.redact(TOKEN_VALUE).contains(TOKEN_VALUE));
    assert!(!format!("{:?}", prepared.report).contains(TOKEN_VALUE));
    assert!(!format!("{prepared:?}").contains(TOKEN_VALUE));
    // Nothing was remembered: the answers apply to this run only.
    assert!(!grants_file(repo.root()));

    // Inject: identity only into commit containers, the token only into the
    // pull-request container for the granted destination.
    let commit = inject(&prepared, repo.root(), PrerequisiteStepKind::CommitChanges);
    assert_eq!(
        env_value(&commit, "GIT_AUTHOR_NAME").as_deref(),
        Some("Ada Lovelace")
    );
    assert_eq!(
        env_value(&commit, "GIT_COMMITTER_EMAIL").as_deref(),
        Some("ada@example.test")
    );
    assert!(commit.secret_env.is_empty());

    let pr = inject(
        &prepared,
        repo.root(),
        PrerequisiteStepKind::CreatePullRequest,
    );
    assert_eq!(pr.secret_names(), ["GH_TOKEN"]);
    assert_eq!(pr.secret_env[0].1.expose(), TOKEN_VALUE);
    assert_eq!(env_value(&pr, "GH_REPO").as_deref(), Some("github.com/o/r"));
    assert!(env_value(&pr, "GIT_AUTHOR_NAME").is_none());

    let shell = inject(&prepared, repo.root(), PrerequisiteStepKind::RunShell);
    assert!(shell.is_empty(), "a shell step receives no credential");
}

#[test]
fn a_remembered_env_token_is_granted_on_later_runs_without_asking() {
    let repo = Repo::new(Some("https://github.com/o/r.git"));
    let run = |fe: &mut Scripted, policy: PrerequisitePolicy| {
        flow_at_with_token(
            repo.root(),
            FrontendKind::Cli,
            PrerequisiteFlags::default(),
            policy,
        )
        .prepare(&needs(vec![token_need()]), fe)
        .unwrap()
    };

    let mut first = Scripted::interactive(Script {
        token_consent: [TokenConsent::Remember].into(),
        ..Default::default()
    });
    let prepared = run(&mut first, base_policy());
    assert_eq!(first.asked, ["token_consent"]);
    let token = prepared.grants.token.unwrap();
    assert_eq!(token.source, TokenGrantSource::Env(TOKEN_VAR.into()));
    assert_eq!(token.secret.expose(), TOKEN_VALUE);
    assert!(grants_file(repo.root()), "the decision is remembered");
    assert!(
        first.messages.iter().any(|m| m
            .contains("Tip: this repo's prerequisite preference is stored in")
            && m.contains("prerequisite-grants.json")),
        "{:?}",
        first.messages
    );

    // Interactive again: nothing is asked.
    let mut second = Scripted::interactive(Script::default());
    let prepared = run(&mut second, base_policy());
    assert!(second.asked.is_empty(), "{:?}", second.asked);
    assert_eq!(
        prepared.grants.token.unwrap().source,
        TokenGrantSource::Env(TOKEN_VAR.into())
    );

    // Headless: the remembered decision still applies.
    let mut headless = Scripted::default();
    let prepared = run(&mut headless, base_policy());
    assert!(prepared.grants.token.is_some());
    assert!(prepared.report.needs_attention().is_empty());

    // A `deny` policy outranks the remembered decision and is never asked.
    let mut denied = base_policy();
    denied.github_token.use_ = CredentialUse::Deny;
    let mut fe = Scripted::interactive(Script::default());
    let prepared = run(&mut fe, denied);
    assert!(prepared.grants.token.is_none());
    assert!(fe.asked.is_empty());
}

#[test]
fn consent_for_this_run_only_is_not_remembered() {
    let repo = Repo::new(Some("https://github.com/o/r.git"));
    let mut fe = Scripted::interactive(Script {
        token_consent: [TokenConsent::ThisRun].into(),
        ..Default::default()
    });
    let prepared = flow_at_with_token(
        repo.root(),
        FrontendKind::Cli,
        PrerequisiteFlags::default(),
        base_policy(),
    )
    .prepare(&needs(vec![token_need()]), &mut fe)
    .unwrap();
    assert!(prepared.grants.token.is_some());
    assert!(!grants_file(repo.root()));
}

#[test]
fn declining_env_consent_falls_through_to_the_source_question() {
    let repo = Repo::new(Some("https://github.com/o/r.git"));
    let mut fe = Scripted::interactive(Script {
        token_consent: [TokenConsent::Decline].into(),
        token_source: [TokenSourceChoice::Decline].into(),
        ..Default::default()
    });
    let prepared = flow_at_with_token(
        repo.root(),
        FrontendKind::Cli,
        PrerequisiteFlags::default(),
        base_policy(),
    )
    .prepare(&needs(vec![token_need()]), &mut fe)
    .unwrap();
    assert_eq!(fe.asked, ["token_consent", "token_source"]);
    assert!(prepared.grants.token.is_none());
    let token = WorkflowPrerequisite::GitHubToken {
        host: "github.com".into(),
    };
    assert_eq!(
        status_of(&prepared.report, &token),
        PrerequisiteStatus::Missing
    );
    assert!(fe
        .messages
        .iter()
        .any(|m| m.contains("continuing without a GitHub token")));
}

#[test]
fn headless_prepare_never_asks_and_shows_the_decided_report() {
    std::env::remove_var(TOKEN_VAR);
    let repo = Repo::new(Some("https://github.com/o/r.git"));
    let mut fe = Scripted::default();
    let prepared = flow_at(
        repo.root(),
        FrontendKind::Cli,
        PrerequisiteFlags::default(),
        base_policy(),
    )
    .prepare(&needs(vec![identity_need(), token_need()]), &mut fe)
    .unwrap();
    assert!(fe.asked.is_empty());
    assert_eq!(fe.shown.len(), 1);
    assert_eq!(fe.shown[0].1, PrerequisiteReportNext::Decided);
    assert!(prepared.grants.is_empty());
    assert_eq!(prepared.report.needs_attention().len(), 2);
    // `unresolved: warn` turns each gap into one warning.
    assert_eq!(
        fe.messages
            .iter()
            .filter(|m| m.starts_with("workflow prerequisite not satisfied"))
            .count(),
        2
    );

    // Without the warn override the headless CLI default for a static
    // workflow is to fail, carrying the decided report.
    let mut policy = base_policy();
    policy.unresolved = None;
    let err = flow_at(
        repo.root(),
        FrontendKind::Cli,
        PrerequisiteFlags::default(),
        policy,
    )
    .prepare(&needs(vec![token_need()]), &mut Scripted::default())
    .expect_err("headless static gaps fail");
    match err {
        CommandError::WorkflowPrerequisitesUnresolved { report } => {
            assert_eq!(report.needs_attention().len(), 1)
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn stopping_at_the_review_asks_nothing() {
    std::env::remove_var(TOKEN_VAR);
    let repo = Repo::new(Some("https://github.com/o/r.git"));
    let mut fe = Scripted::interactive(Script::default());
    fe.abort_on_show = true;
    let err = flow_at(
        repo.root(),
        FrontendKind::Cli,
        PrerequisiteFlags::default(),
        base_policy(),
    )
    .prepare(&needs(vec![identity_need(), token_need()]), &mut fe)
    .err()
    .unwrap();
    assert!(matches!(err, CommandError::Aborted));
    assert!(fe.asked.is_empty());
}

#[test]
fn token_flags_decide_without_asking_even_on_a_tty() {
    let repo = Repo::new(Some("https://github.com/o/r.git"));
    let token = WorkflowPrerequisite::GitHubToken {
        host: "github.com".into(),
    };
    let run = |flag: TokenFlag| {
        let mut fe = Scripted::interactive(Script::default());
        let flags = PrerequisiteFlags {
            token: Some(flag),
            ..Default::default()
        };
        let prepared = flow_at_with_token(repo.root(), FrontendKind::Cli, flags, base_policy())
            .prepare(&needs(vec![token_need()]), &mut fe)
            .unwrap();
        assert!(fe.asked.is_empty(), "{:?}", fe.asked);
        prepared
    };

    let prepared = run(TokenFlag::Env(TOKEN_VAR.into()));
    assert_eq!(
        prepared.grants.token.unwrap().source,
        TokenGrantSource::Env(TOKEN_VAR.into())
    );

    let prepared = run(TokenFlag::Deny);
    assert!(prepared.grants.token.is_none());
    assert_eq!(
        status_of(&prepared.report, &token),
        PrerequisiteStatus::Missing
    );

    let prepared = run(TokenFlag::Env("AWMAN_COVERAGE_FLOW_UNSET_VAR".into()));
    assert!(prepared.grants.token.is_none());
    let check = prepared
        .report
        .checks
        .iter()
        .find(|c| c.prerequisite == token)
        .unwrap();
    assert!(
        check.explanation.contains("is not set"),
        "{}",
        check.explanation
    );
}

#[test]
fn invalid_pasted_tokens_are_retried_then_declined() {
    std::env::remove_var(TOKEN_VAR);
    let repo = Repo::new(Some("https://github.com/o/r.git"));
    let mut fe = Scripted::interactive(Script {
        token_source: [
            TokenSourceChoice::Paste,
            TokenSourceChoice::Paste,
            TokenSourceChoice::Paste,
        ]
        .into(),
        secret: [
            Some("has inner space".into()),
            None,
            Some("tab\tinside".into()),
        ]
        .into(),
        ..Default::default()
    });
    let prepared = flow_at(
        repo.root(),
        FrontendKind::Cli,
        PrerequisiteFlags::default(),
        base_policy(),
    )
    .prepare(&needs(vec![token_need()]), &mut fe)
    .unwrap();
    assert_eq!(
        fe.asked,
        [
            "token_source",
            "secret",
            "token_source",
            "secret",
            "token_source",
            "secret"
        ]
    );
    assert!(prepared.grants.token.is_none());
    assert_eq!(
        fe.messages
            .iter()
            .filter(|m| m.contains("is not a token"))
            .count(),
        2
    );
    let check = &prepared.report.checks[0];
    assert_eq!(check.status, PrerequisiteStatus::Missing);
    assert!(check.explanation.contains("no GitHub token was obtained"));
}

#[test]
fn an_invalid_entered_identity_is_declined_with_a_warning() {
    let repo = Repo::new(Some("https://github.com/o/r.git"));
    let mut fe = Scripted::interactive(Script {
        identity: [IdentityChoice::Enter].into(),
        identity_text: [Some("Ada".into()), Some("not an email".into())].into(),
        ..Default::default()
    });
    let prepared = flow_at(
        repo.root(),
        FrontendKind::Cli,
        PrerequisiteFlags::default(),
        base_policy(),
    )
    .prepare(&needs(vec![identity_need()]), &mut fe)
    .unwrap();
    assert!(prepared.grants.identity.is_none());
    assert!(fe.messages.iter().any(|m| m.contains("identity not used")));
    assert_eq!(
        status_of(&prepared.report, &WorkflowPrerequisite::GitIdentity),
        PrerequisiteStatus::Missing
    );

    // A dismissed name prompt declines too, without asking for the email.
    let mut fe = Scripted::interactive(Script {
        identity: [IdentityChoice::Enter].into(),
        identity_text: [None].into(),
        ..Default::default()
    });
    let prepared = flow_at(
        repo.root(),
        FrontendKind::Cli,
        PrerequisiteFlags::default(),
        base_policy(),
    )
    .prepare(&needs(vec![identity_need()]), &mut fe)
    .unwrap();
    assert_eq!(fe.asked, ["identity", "identity_text"]);
    assert!(prepared.grants.identity.is_none());
}

#[test]
fn a_missing_remote_is_a_gap_the_person_may_abort_on() {
    let repo = Repo::new(None);
    let remote_need = || {
        needs(vec![need(
            WorkflowPrerequisite::GitRemote {
                remote: "origin".into(),
            },
            PrerequisiteStepKind::PushBranch,
        )])
    };
    let mut fe = Scripted::interactive(Script {
        gap: [GapChoice::Abort].into(),
        ..Default::default()
    });
    let err = flow_at(
        repo.root(),
        FrontendKind::Cli,
        PrerequisiteFlags::default(),
        base_policy(),
    )
    .prepare(&remote_need(), &mut fe)
    .err()
    .unwrap();
    assert!(matches!(err, CommandError::Aborted));
    assert_eq!(fe.asked, ["gap"]);

    let mut fe = Scripted::interactive(Script {
        gap: [GapChoice::Continue].into(),
        ..Default::default()
    });
    let prepared = flow_at(
        repo.root(),
        FrontendKind::Cli,
        PrerequisiteFlags::default(),
        base_policy(),
    )
    .prepare(&remote_need(), &mut fe)
    .unwrap();
    assert!(prepared.grants.is_empty());
    assert_eq!(prepared.report.needs_attention().len(), 1);
    assert!(prepared.remote("origin").is_none());
}

#[test]
fn one_declined_token_covers_the_https_push_check_too() {
    std::env::remove_var(TOKEN_VAR);
    let repo = Repo::new(Some("https://github.com/o/r.git"));
    let mut flow = flow_at(
        repo.root(),
        FrontendKind::Cli,
        PrerequisiteFlags::default(),
        base_policy(),
    );
    let check = |p: WorkflowPrerequisite| PrerequisiteCheck {
        prerequisite: p,
        status: PrerequisiteStatus::Missing,
        explanation: "fixture".into(),
        needed_by: vec![],
        remediations: vec![Remediation::PasteToken],
    };
    let report = WorkflowPrerequisiteReport {
        phase: PrerequisitePhase::Start,
        origin: PrerequisiteOrigin::Static,
        checks: vec![
            check(WorkflowPrerequisite::GitHubToken {
                host: "github.com".into(),
            }),
            check(WorkflowPrerequisite::PushAuthHttps {
                remote: "origin".into(),
                host: "github.com".into(),
            }),
        ],
        advisories: vec![],
        notes: vec![],
    };
    let needs = needs(
        report
            .checks
            .iter()
            .map(|c| need(c.prerequisite.clone(), PrerequisiteStepKind::PushBranch))
            .collect(),
    );
    let mut fe = Scripted::interactive(Script {
        token_source: [TokenSourceChoice::Decline].into(),
        ..Default::default()
    });
    let outcome = flow
        .resolve(
            &needs,
            report,
            &crate::engine::auth::redaction::RedactionSet::new(),
            &mut fe,
        )
        .unwrap();
    assert_eq!(fe.asked, ["token_source"], "asked once for both checks");
    assert_eq!(outcome.unresolved.len(), 2);
    assert!(outcome.report.checks[1]
        .explanation
        .contains("declined above"));
}
