//! Layer 2 — reviewing and approving custom agent definitions.
//!
//! The engine is fail-closed: `AgentRegistry::resolve` refuses a definition
//! that needs approval and has none (`AgentDefinitionUntrusted`). This module
//! is the only place an approval is *granted*. `ensure_agent_trusted` loads
//! the definition without the trust check, builds an [`AgentTrustReview`]
//! (the typed summary the CLI and TUI both show), asks through
//! [`AgentTrustFrontend`], and records the approval on an explicit yes.
//!
//! A frontend with no user present (the API, the squad daemon, a CLI run that
//! is non-interactive) keeps the default refusal. `--yes` is never consulted:
//! approving content that runs builds — and, with `[ping]`, a command on the
//! host — must be an explicit, per-definition act.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::command::error::CommandError;
use crate::data::config::agent_definition::{escape_for_display, is_bare_host_binary};
use crate::data::config::RuntimeSelection;
use crate::data::fs::agent_dirs::DefinitionScope;
use crate::data::prompt::Prompt;
use crate::data::session::{AgentName, Session};
use crate::engine::agent::definition::{
    AgentDefinition, AgentImageSource, AgentOrigin, SettingsMount,
};
use crate::engine::agent::first_party::{first_party, FIRST_PARTY_AGENTS};
use crate::engine::agent::registry::{AgentRegistry, TrustStatus};
use crate::engine::error::EngineError;
use crate::engine::ready::host_agent::GREETINGS;

/// The host command a definition's `[ping]` will run during `awman ready`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostCommandReview {
    /// `ping.command`, exactly as it will be passed (argv, no shell).
    pub argv: Vec<String>,
    /// Shell-quoted argv plus the literal `'<greeting>'` placeholder.
    pub rendered: String,
    /// Where `argv[0]` resolves on the host `PATH` right now. `None` for a
    /// bare name that is not found; always `None` for an absolute path,
    /// which needs no lookup.
    pub resolved_binary: Option<PathBuf>,
    /// `true` when `argv[0]` is a bare name (looked up on `PATH`).
    pub bare_name: bool,
}

impl HostCommandReview {
    pub fn from_argv(argv: &[String]) -> Self {
        let bare_name = argv.first().is_some_and(|bin| is_bare_host_binary(bin));
        let resolved_binary = if bare_name {
            which::which(&argv[0]).ok()
        } else {
            None
        };
        let mut rendered = shell_words::join(argv.iter().map(|t| escape_controls(t)));
        rendered.push_str(" '<greeting>'");
        Self {
            argv: argv.to_vec(),
            rendered,
            resolved_binary,
            bare_name,
        }
    }

    /// The block shown first in the review summary, and by `new agent`.
    pub fn render(&self) -> String {
        let mut out = String::from(
            "⚠ `awman ready` will run this command ON YOUR HOST (not in a container):\n",
        );
        out.push_str(&format!("    {}\n", self.rendered));
        if self.bare_name {
            match &self.resolved_binary {
                Some(path) => out.push_str(&format!(
                    "    ({} resolves to {} on your PATH)\n",
                    escape_controls(&self.argv[0]),
                    path.display()
                )),
                None => out.push_str(&format!(
                    "    ({} is not found on PATH)\n",
                    escape_controls(&self.argv[0])
                )),
            }
        }
        out.push_str(&format!(
            "  from an empty temporary directory outside the repo, where <greeting> is one of\n  \
             awman's built-in greetings (e.g. '{}'). Check that this is the agent you expect.",
            GREETINGS[0]
        ));
        out
    }
}

/// Everything a user needs to decide whether to approve a custom definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentTrustReview {
    pub name: String,
    pub display_name: String,
    pub scope: DefinitionScope,
    pub path: PathBuf,
    pub sha256: String,
    pub homepage: Option<String>,
    pub host_command: Option<HostCommandReview>,
    /// `image.install`, verbatim; empty for the escape hatch.
    pub install_steps: Vec<String>,
    /// Escape-hatch Dockerfile path and its line count.
    pub dockerfile: Option<(PathBuf, usize)>,
    pub host_dir: Option<String>,
    /// Set when `host_dir` is (inside) another first-party agent's settings dir.
    pub host_dir_warning: Option<String>,
    /// `auth.env` names; values are never read.
    pub auth_env: Vec<String>,
    /// `run.env` keys.
    pub run_env_keys: Vec<String>,
    /// Extra pre-approval notices (an existing generated Dockerfile, …).
    pub notices: Vec<String>,
}

impl AgentTrustReview {
    /// Build the review for a custom definition. A first-party definition
    /// never needs one; it yields an empty, path-less review.
    pub fn from_definition(def: &AgentDefinition) -> Self {
        let (scope, path, sha256) = match &def.origin {
            AgentOrigin::Custom {
                scope,
                path,
                sha256,
            } => (*scope, path.clone(), sha256.clone()),
            AgentOrigin::FirstParty => (DefinitionScope::Global, PathBuf::new(), String::new()),
        };
        let (install_steps, dockerfile) = match &def.image {
            AgentImageSource::Rendered { spec } => (spec.install.clone(), None),
            AgentImageSource::UserDockerfile { path } => {
                let lines = std::fs::read_to_string(path)
                    .map(|s| s.lines().count())
                    .unwrap_or(0);
                (Vec::new(), Some((path.clone(), lines)))
            }
            AgentImageSource::Download => (Vec::new(), None),
        };
        let host_dir = match &def.settings_mount {
            SettingsMount::Direct(dir) => Some(dir.clone()),
            _ => None,
        };
        let host_dir_warning = host_dir
            .as_deref()
            .and_then(|dir| first_party_settings_owner(dir, &def.name))
            .map(|(agent, dir)| {
                format!("this is {agent}'s settings directory (~/{dir}); {agent}'s settings and credentials would be mounted read-write")
            });
        Self {
            name: def.name.clone(),
            display_name: def.display_name.clone(),
            scope,
            path,
            sha256,
            homepage: def.homepage.clone(),
            host_command: def.ping_argv.as_deref().map(HostCommandReview::from_argv),
            install_steps,
            dockerfile,
            host_dir,
            host_dir_warning,
            auth_env: def.auth_env.clone(),
            run_env_keys: def.static_env.iter().map(|(k, _)| k.clone()).collect(),
            notices: Vec::new(),
        }
    }

    pub fn host_command_block(&self) -> Option<String> {
        self.host_command.as_ref().map(HostCommandReview::render)
    }

    /// The whole summary. The host-command block comes first when present;
    /// otherwise the "no host command" line does.
    pub fn render(&self) -> String {
        let mut out = match self.host_command_block() {
            Some(block) => block,
            None => "no host command: `awman ready` will skip the host check".to_string(),
        };
        out.push_str("\n\n");
        out.push_str(&format!(
            "agent:     {} ({})\n",
            escape_controls(&self.display_name),
            escape_controls(&self.name)
        ));
        out.push_str(&format!(
            "defined:   {}:{}\n",
            self.scope.as_str(),
            self.path.display()
        ));
        if let Some(homepage) = &self.homepage {
            out.push_str(&format!("homepage:  {}\n", escape_controls(homepage)));
        }
        match &self.dockerfile {
            Some((path, lines)) => out.push_str(&format!(
                "image:     hand-written Dockerfile {} ({lines} lines)\n",
                path.display()
            )),
            None => {
                out.push_str("image:     install steps (run as root in the image build):\n");
                for step in &self.install_steps {
                    out.push_str(&format!("    RUN {}\n", escape_controls(step)));
                }
            }
        }
        match &self.host_dir {
            Some(dir) => {
                out.push_str(&format!(
                    "settings:  ~/{} mounted read-write\n",
                    escape_controls(dir)
                ));
                if let Some(warning) = &self.host_dir_warning {
                    out.push_str(&format!("    ⚠ {warning}\n"));
                }
            }
            None => out.push_str("settings:  none\n"),
        }
        out.push_str(&format!("auth env:  {}\n", list_or_none(&self.auth_env)));
        out.push_str(&format!(
            "run env:   {}\n",
            list_or_none(&self.run_env_keys)
        ));
        for notice in &self.notices {
            out.push_str(&format!("note:      {}\n", escape_controls(notice)));
        }
        out.push_str(&format!("sha256:    {}", self.sha256));
        out
    }
}

/// The user's answer to the trust prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustDecision {
    Approve,
    Decline,
}

/// What `ensure_agent_trusted` found or did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustOutcome {
    FirstParty,
    NotRequired,
    AlreadyApproved,
    Approved,
}

/// The frontend surface for the trust prompt.
///
/// A supertrait of every command frontend that launches or readies an agent.
/// The default refuses: only a frontend with a person at it (the interactive
/// CLI and the TUI) overrides it.
pub trait AgentTrustFrontend {
    /// Show `review` (its `render()` is `prompt.body`) and ask.
    fn confirm_agent_definition(
        &mut self,
        review: &AgentTrustReview,
        _prompt: &Prompt<TrustDecision>,
    ) -> Result<TrustDecision, CommandError> {
        Err(untrusted_error(review))
    }
}

/// The refusal for a context that cannot ask.
pub fn untrusted_error(review: &AgentTrustReview) -> CommandError {
    CommandError::Engine(EngineError::AgentDefinitionUntrusted {
        name: review.name.clone(),
        path: review.path.clone(),
        sha256: review.sha256.clone(),
    })
}

/// Make sure `agent` may be resolved for a build or launch, asking the user
/// when it needs an approval it does not have.
///
/// Runs before any `AgentRegistry::resolve` in `ready`, `chat`, `exec prompt`
/// and `exec workflow`. An unknown or invalid definition fails here with the
/// same error resolve would give. A decline is exit 1.
pub fn ensure_agent_trusted(
    session: &Session,
    agent: &AgentName,
    frontend: &mut dyn AgentTrustFrontend,
) -> Result<TrustOutcome, CommandError> {
    let env = session.env();
    let root = Some(session.definition_root());
    let untrusted =
        AgentRegistry::load_untrusted(env, agent, root).map_err(|error| match error {
            // Rebuild with the runtime note, so a former built-in on sbx learns
            // up front that recreating it will not make it run there.
            EngineError::UnknownAgent { .. } => {
                AgentRegistry::unknown(env, agent.as_str(), root, runtime_is_sandbox(session))
            }
            error => error,
        })?;
    let def = untrusted.definition();
    match AgentRegistry::trust_status(env, def, root)? {
        TrustStatus::FirstParty => return Ok(TrustOutcome::FirstParty),
        TrustStatus::NotRequired => return Ok(TrustOutcome::NotRequired),
        TrustStatus::Approved => return Ok(TrustOutcome::AlreadyApproved),
        TrustStatus::Pending => {}
    }
    let mut review = AgentTrustReview::from_definition(def);
    let paths = crate::data::RepoDockerfilePaths::new(session.git_root());
    review
        .notices
        .extend(untrusted.existing_dockerfile_notice(&paths));
    let prompt = crate::command::prompts::agent_trust_confirm(&review);
    match frontend.confirm_agent_definition(&review, &prompt)? {
        TrustDecision::Approve => {
            AgentRegistry::record_approval(env, def, root)?;
            Ok(TrustOutcome::Approved)
        }
        TrustDecision::Decline => Err(CommandError::Other(format!(
            "agent definition '{}' was not approved",
            agent.as_str()
        ))),
    }
}

/// Whether the configured runtime is Docker Sandboxes, which cannot run
/// custom agents. Engines are built from the same effective config.
fn runtime_is_sandbox(session: &Session) -> bool {
    matches!(
        RuntimeSelection::parse(session.effective_config().runtime().as_deref()),
        Ok(RuntimeSelection::DockerSbxExperimental)
    )
}

/// [`ensure_agent_trusted`] once per distinct agent, in order.
pub fn ensure_workflow_agents_trusted(
    session: &Session,
    agents: &[AgentName],
    frontend: &mut dyn AgentTrustFrontend,
) -> Result<(), CommandError> {
    let mut seen: Vec<&str> = Vec::new();
    for agent in agents {
        if seen.contains(&agent.as_str()) {
            continue;
        }
        seen.push(agent.as_str());
        ensure_agent_trusted(session, agent, frontend)?;
    }
    Ok(())
}

/// The first-party agent whose host settings dir `dir` is, contains, or is
/// inside, with that dir. `own` (the definition's own name) is never reported.
fn first_party_settings_owner(dir: &str, own: &str) -> Option<(&'static str, String)> {
    let dir = dir.trim_end_matches('/');
    FIRST_PARTY_AGENTS.iter().find_map(|agent| {
        if *agent == own {
            return None;
        }
        let settings = match first_party(agent)?.settings_mount {
            SettingsMount::Direct(d) => d,
            SettingsMount::Claude => ".claude".to_string(),
            SettingsMount::Antigravity => ".gemini".to_string(),
            SettingsMount::None => return None,
        };
        let overlaps =
            Path::new(dir).starts_with(&settings) || Path::new(&settings).starts_with(dir);
        overlaps.then_some((*agent, settings))
    })
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items
            .iter()
            .map(|s| escape_controls(s))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Defence in depth: validation already rejects terminal controls and bidi
/// formatting in definition strings, but nothing from a definition reaches
/// the terminal raw. Line breaks are escaped too: one field is one line.
fn escape_controls(s: &str) -> String {
    escape_for_display(s).replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::prompts::agent_trust_confirm;

    const BASE: &str =
        "schema=1\ndisplay_name='Pi'\n[run]\ncommand=['pi']\n[image]\ninstall=['npm i -g pi']\n";

    struct Fixture {
        root: tempfile::TempDir,
        home: PathBuf,
        session: Session,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("global");
        std::fs::create_dir_all(&home).unwrap();
        let session = Session::for_tests_isolated(root.path(), &home);
        Fixture {
            root,
            home,
            session,
        }
    }

    impl Fixture {
        fn write(&self, global: bool, name: &str, body: &str) {
            let dir = if global {
                self.home.join("agents")
            } else {
                self.root.path().join(".awman/agents")
            };
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{name}.toml")), body).unwrap();
        }
        fn review(&self, name: &str) -> AgentTrustReview {
            let name = AgentName::new(name).unwrap();
            let u = AgentRegistry::load_untrusted(
                self.session.env(),
                &name,
                Some(self.session.definition_root()),
            )
            .unwrap();
            AgentTrustReview::from_definition(u.definition())
        }
    }

    /// Records every prompt and answers with a fixed decision.
    struct Recording {
        answer: Option<TrustDecision>,
        seen: Vec<(String, String, Vec<char>)>,
    }
    impl Recording {
        fn new(answer: TrustDecision) -> Self {
            Self {
                answer: Some(answer),
                seen: Vec::new(),
            }
        }
    }
    impl AgentTrustFrontend for Recording {
        fn confirm_agent_definition(
            &mut self,
            review: &AgentTrustReview,
            prompt: &Prompt<TrustDecision>,
        ) -> Result<TrustDecision, CommandError> {
            assert_eq!(prompt.body, review.render());
            self.seen.push((
                prompt.title.clone(),
                prompt.body.clone(),
                prompt.choices.iter().map(|c| c.key).collect(),
            ));
            Ok(self.answer.unwrap())
        }
    }
    /// A frontend with no user: keeps the default refusal.
    struct NoUser;
    impl AgentTrustFrontend for NoUser {}

    fn pi() -> AgentName {
        AgentName::new("pi").unwrap()
    }

    #[test]
    fn prompt_is_shown_with_the_full_summary() {
        let fx = fixture();
        fx.write(false, "pi", BASE);
        let mut fe = Recording::new(TrustDecision::Approve);
        ensure_agent_trusted(&fx.session, &pi(), &mut fe).unwrap();
        assert_eq!(fe.seen.len(), 1);
        let (title, body, keys) = &fe.seen[0];
        assert!(title.contains("pi"));
        assert_eq!(keys, &vec!['y', 'n']);
        for needle in [
            "agent:",
            "defined:",
            "RUN npm i -g pi",
            "sha256:",
            "auth env:",
        ] {
            assert!(body.contains(needle), "missing {needle}: {body}");
        }
    }

    #[test]
    fn approval_writes_the_entry_and_is_not_asked_twice() {
        let fx = fixture();
        fx.write(false, "pi", BASE);
        let mut fe = Recording::new(TrustDecision::Approve);
        assert_eq!(
            ensure_agent_trusted(&fx.session, &pi(), &mut fe).unwrap(),
            TrustOutcome::Approved
        );
        assert!(fx.home.join("trusted-agents.json").exists());
        assert_eq!(
            ensure_agent_trusted(&fx.session, &pi(), &mut fe).unwrap(),
            TrustOutcome::AlreadyApproved
        );
        assert_eq!(fe.seen.len(), 1);
        // The engine now resolves it.
        AgentRegistry::resolve(fx.session.env(), &pi(), Some(fx.session.definition_root()))
            .unwrap();
    }

    #[test]
    fn an_edited_definition_must_be_reapproved() {
        let fx = fixture();
        fx.write(false, "pi", BASE);
        let mut fe = Recording::new(TrustDecision::Approve);
        ensure_agent_trusted(&fx.session, &pi(), &mut fe).unwrap();
        fx.write(false, "pi", &BASE.replace("npm i -g pi", "npm i -g pi2"));
        ensure_agent_trusted(&fx.session, &pi(), &mut fe).unwrap();
        assert_eq!(fe.seen.len(), 2);
    }

    #[test]
    fn decline_exits_1_and_writes_nothing() {
        let fx = fixture();
        fx.write(false, "pi", BASE);
        let mut fe = Recording::new(TrustDecision::Decline);
        let err = ensure_agent_trusted(&fx.session, &pi(), &mut fe).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("was not approved"), "{err}");
        assert!(!fx.home.join("trusted-agents.json").exists());
    }

    #[test]
    fn no_user_fails_with_the_ready_hint_and_exit_2() {
        let fx = fixture();
        fx.write(false, "pi", BASE);
        let err = ensure_agent_trusted(&fx.session, &pi(), &mut NoUser).unwrap_err();
        assert!(matches!(
            err,
            CommandError::Engine(EngineError::AgentDefinitionUntrusted { .. })
        ));
        assert!(err.to_string().contains("awman ready --agent pi"), "{err}");
        assert_eq!(err.exit_code(), 2);
        assert_eq!(err.http_status(), 403);
        assert!(!fx.home.join("trusted-agents.json").exists());
    }

    #[test]
    fn yes_flag_never_approves() {
        // `--yes` lives on the session's config; the policy never reads it,
        // so a no-user frontend still refuses.
        let fx = fixture();
        fx.write(false, "pi", BASE);
        let mut fe = NoUser;
        assert!(ensure_agent_trusted(&fx.session, &pi(), &mut fe).is_err());
        assert!(!fx.home.join("trusted-agents.json").exists());
        let src = include_str!("agent_trust.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            !prod.contains(".yes("),
            "trust policy must not consult --yes"
        );
    }

    #[test]
    fn first_party_needs_no_prompt() {
        let fx = fixture();
        let mut fe = Recording::new(TrustDecision::Decline);
        let out =
            ensure_agent_trusted(&fx.session, &AgentName::new("claude").unwrap(), &mut fe).unwrap();
        assert_eq!(out, TrustOutcome::FirstParty);
        assert!(fe.seen.is_empty());
    }

    #[test]
    fn unknown_agent_fails_with_unknown_agent_exit_4() {
        let fx = fixture();
        let mut fe = Recording::new(TrustDecision::Approve);
        let err = ensure_agent_trusted(&fx.session, &AgentName::new("nope").unwrap(), &mut fe)
            .unwrap_err();
        assert!(matches!(
            err,
            CommandError::Engine(EngineError::UnknownAgent { .. })
        ));
        assert_eq!(err.exit_code(), 4);
        assert_eq!(err.http_status(), 404);
    }

    #[test]
    fn missing_former_built_in_on_sbx_carries_both_migration_notes() {
        for name in ["crush", "maki"] {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().join("global");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(
                home.join("config.json"),
                r#"{"runtime":"docker-sbx-experimental"}"#,
            )
            .unwrap();
            let session = Session::for_tests_isolated(root.path(), &home);
            let err = ensure_agent_trusted(&session, &AgentName::new(name).unwrap(), &mut NoUser)
                .unwrap_err();
            assert_eq!(err.exit_code(), 4, "{err}");
            let text = err.to_string();
            assert!(
                text.contains(&format!("awman new agent {name} --from {name}")),
                "{text}"
            );
            assert!(text.contains("Docker Sandboxes"), "{text}");
        }
        // Without sbx configured the runtime note is absent.
        let fx = fixture();
        let err = ensure_agent_trusted(&fx.session, &AgentName::new("crush").unwrap(), &mut NoUser)
            .unwrap_err();
        assert!(!err.to_string().contains("Docker Sandboxes"), "{err}");
    }

    #[test]
    fn invalid_definition_fails_with_its_own_error_exit_1() {
        let fx = fixture();
        fx.write(false, "pi", "schema=1\nbogus=1\n");
        let err = ensure_agent_trusted(&fx.session, &pi(), &mut NoUser).unwrap_err();
        assert!(matches!(
            err,
            CommandError::Engine(EngineError::AgentDefinitionInvalid { .. })
        ));
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn global_definition_with_ping_prompts_and_without_does_not() {
        let fx = fixture();
        fx.write(true, "pi", BASE);
        let mut fe = Recording::new(TrustDecision::Approve);
        assert_eq!(
            ensure_agent_trusted(&fx.session, &pi(), &mut fe).unwrap(),
            TrustOutcome::NotRequired
        );
        assert!(fe.seen.is_empty());

        fx.write(true, "pi", &format!("{BASE}[ping]\ncommand=['pi','run']\n"));
        assert_eq!(
            ensure_agent_trusted(&fx.session, &pi(), &mut fe).unwrap(),
            TrustOutcome::Approved
        );
        assert_eq!(fe.seen.len(), 1);
        assert!(fe.seen[0]
            .1
            .starts_with("⚠ `awman ready` will run this command ON YOUR HOST"));
    }

    #[test]
    fn workflow_agents_are_asked_once_each_in_order() {
        let fx = fixture();
        fx.write(false, "pi", BASE);
        fx.write(false, "zed", BASE);
        let mut fe = Recording::new(TrustDecision::Approve);
        let agents = [
            pi(),
            AgentName::new("claude").unwrap(),
            AgentName::new("zed").unwrap(),
            pi(),
        ];
        ensure_workflow_agents_trusted(&fx.session, &agents, &mut fe).unwrap();
        assert_eq!(fe.seen.len(), 2);
        assert!(fe.seen[0].0.contains("pi") && fe.seen[1].0.contains("zed"));
    }

    // ── review summary ───────────────────────────────────────────────────

    #[test]
    fn ping_host_command_block_comes_first_with_quoted_argv_and_placeholder() {
        let fx = fixture();
        fx.write(
            false,
            "pi",
            &format!("{BASE}[ping]\ncommand=['/bin/echo','a b']\n"),
        );
        let text = fx.review("pi").render();
        assert!(text.starts_with("⚠ `awman ready` will run this command ON YOUR HOST"));
        assert!(
            text.contains("    /bin/echo 'a b' '<greeting>'\n"),
            "{text}"
        );
        // Absolute path: no PATH lookup line.
        assert!(!text.contains("on your PATH") && !text.contains("not found on PATH"));
        assert!(text.find("agent:").unwrap() > text.find("<greeting>").unwrap());
    }

    #[test]
    fn bare_ping_binary_shows_resolved_path_or_not_found() {
        let found = HostCommandReview::from_argv(&["sh".to_string()]);
        let text = found.render();
        assert!(found.resolved_binary.is_some());
        assert!(text.contains("(sh resolves to "), "{text}");
        assert!(text.contains("on your PATH)"));

        let missing = HostCommandReview::from_argv(&["no-such-awman-binary-xyz".to_string()]);
        assert!(missing.resolved_binary.is_none());
        assert!(missing
            .render()
            .contains("(no-such-awman-binary-xyz is not found on PATH)"));
    }

    #[test]
    fn without_ping_the_no_host_command_line_is_first() {
        let fx = fixture();
        fx.write(false, "pi", BASE);
        let review = fx.review("pi");
        assert!(review.host_command.is_none());
        assert!(review
            .render()
            .starts_with("no host command: `awman ready` will skip the host check"));
        assert!(review.host_command_block().is_none());
    }

    #[test]
    fn settings_dir_of_another_agent_is_flagged() {
        let fx = fixture();
        fx.write(
            false,
            "pi",
            &format!("{BASE}[settings]\nhost_dir='.codex'\n"),
        );
        let text = fx.review("pi").render();
        assert!(
            text.contains("settings:  ~/.codex mounted read-write"),
            "{text}"
        );
        assert!(
            text.contains("⚠ this is codex's settings directory"),
            "{text}"
        );

        fx.write(false, "pi", &format!("{BASE}[settings]\nhost_dir='.pi'\n"));
        let text = fx.review("pi").render();
        assert!(text.contains("settings:  ~/.pi mounted read-write"));
        assert!(!text.contains("⚠ this is"));
    }

    #[test]
    fn env_values_are_never_shown() {
        let fx = fixture();
        fx.write(
            false,
            "pi",
            &format!("{BASE}[run.env]\nPI_MODE='super-secret-value'\n[auth]\nenv=['PI_API_KEY']\n"),
        );
        let text = fx.review("pi").render();
        assert!(
            text.contains("PI_MODE") && text.contains("PI_API_KEY"),
            "{text}"
        );
        assert!(!text.contains("super-secret-value"));
    }

    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(escape_controls("a\x1b[2Jb"), "a\\u{1b}[2Jb");
        assert_eq!(escape_controls("plain"), "plain");
        assert_eq!(escape_controls("a\u{202E}b\nc"), "a\\u{202e}b\\nc");
    }

    /// A rejected definition's diagnostics never carry a raw ESC (or any
    /// other control) to the terminal, through either error type.
    #[test]
    fn invalid_env_key_diagnostic_has_no_raw_terminal_controls() {
        let fx = fixture();
        fx.write(
            false,
            "pi",
            &format!("{BASE}[run.env]\n\"BAD\\u001b[2J\\u202e\" = 'x'\n"),
        );
        let err = ensure_agent_trusted(&fx.session, &pi(), &mut NoUser).unwrap_err();
        let text = err.to_string();
        assert!(matches!(
            err,
            CommandError::Engine(EngineError::AgentDefinitionInvalid { .. })
        ));
        assert!(text.contains("\\u{1b}[2J"), "{text}");
        assert!(text.contains("\\u{202e}"), "{text}");
        assert!(
            !text
                .chars()
                .any(|c| c != '\n' && (c.is_control() || c == '\u{202E}')),
            "{text:?}"
        );
    }

    #[test]
    fn bidi_controls_in_a_ping_token_are_rejected() {
        let fx = fixture();
        fx.write(
            false,
            "pi",
            &format!("{BASE}[ping]\ncommand=['/bin/echo', \"safe\\u202Etxt.sh\"]\n"),
        );
        let err = ensure_agent_trusted(&fx.session, &pi(), &mut NoUser).unwrap_err();
        assert!(err.to_string().contains("control characters"), "{err}");
        assert!(!err.to_string().contains('\u{202E}'), "{err:?}");
    }

    #[test]
    fn prompt_default_on_dismiss_is_decline() {
        let fx = fixture();
        fx.write(false, "pi", BASE);
        let p = agent_trust_confirm(&fx.review("pi"));
        assert_eq!(p.default_on_dismiss, Some(TrustDecision::Decline));
    }

    #[test]
    fn untrusted_error_carries_name_path_and_hash() {
        let fx = fixture();
        fx.write(false, "pi", BASE);
        let review = fx.review("pi");
        match untrusted_error(&review) {
            CommandError::Engine(EngineError::AgentDefinitionUntrusted {
                name, sha256, ..
            }) => {
                assert_eq!(name, "pi");
                assert_eq!(sha256, review.sha256);
            }
            other => panic!("{other:?}"),
        }
    }
}
