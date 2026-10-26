//! The overlay x settings-family x system-prompt matrix, resolved by the real
//! `AgentEngine` for every supported agent and then planned by the builtin
//! backend. Everything runs against a throwaway HOME and the in-memory driver.
use super::lifecycle_tests::Rig;
use super::*;
use crate::data::config::overlays::{ContextScope, DirectorySpec};
use crate::data::fs::auth_paths::AuthPathResolver;
use crate::data::image_tags::agent_image_tag;
use crate::data::session::{AgentName, Session, SessionOpenOptions, StaticGitRootResolver};
use crate::engine::agent::definition::{SettingsMount, SystemPromptMode};
use crate::engine::agent::first_party::{first_party, FIRST_PARTY_AGENTS};
use crate::engine::agent::{AgentEngine, AgentRunOptions};
use crate::engine::agent_runtime::{
    AgentRuntimeEngine, Capabilities, ReadyAgentOptions, ResolvedAgentOptions,
};
use crate::engine::auth::AgentCredentials;
use crate::engine::container::builtin::testing::*;
use crate::engine::overlay::{ContextOverlay, OverlayEngine};
use std::path::PathBuf;

const PROMPT: &str = "PROMPT with spaces\nand a second line";
const HOME: &str = "/home/probe";

/// Import-style runtime whose image HOME is what the imported image says.
struct ImportRuntime {
    home: Option<String>,
}

impl AgentRuntimeEngine for ImportRuntime {
    fn runtime_name(&self) -> &'static str {
        "builtin"
    }
    fn display_name(&self) -> &'static str {
        "Builtin"
    }
    fn capabilities(&self) -> &Capabilities {
        static CAPS: Capabilities = Capabilities {
            arbitrary_host_mounts: true,
            kit_declarative: false,
            has_image_store: true,
            image_acquisition: crate::engine::agent_runtime::ImageAcquisition::Import,
            per_launch_vm_settings: true,
            reports_resource_stats: true,
        };
        &CAPS
    }
    fn is_available(&self) -> bool {
        true
    }
    fn build(
        &self,
        _: ResolvedAgentOptions,
    ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
        unimplemented!("options are planned directly by the tests")
    }
    fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
    }
    fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
        unimplemented!()
    }
    fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
        Ok(())
    }
    fn attach(
        &self,
        _: &AgentHandle,
    ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
        unimplemented!()
    }
    fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
    }
    fn ready_agent(
        &self,
        _: &str,
        _: ReadyAgentOptions,
        _: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        Ok(self.home.clone())
    }
    fn build_image(
        &self,
        _request: &crate::engine::container::BuildRequest,
        _: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        Err(unsupported("image building"))
    }
}

struct World {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    git_root: PathBuf,
    ctx_global: PathBuf,
    ctx_workflow: PathBuf,
    extra_ro: PathBuf,
    extra_rw: PathBuf,
    session: Session,
    engine: AgentEngine,
    rig: Rig,
}

fn world(image_home: Option<&str>) -> World {
    let tmp = tempfile::tempdir_in("/tmp").unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("host-home");
    let git_root = root.join("repo");
    let ctx_global = root.join("ctx-global");
    let ctx_workflow = root.join("ctx-workflow");
    let extra_ro = root.join("extra-ro");
    let extra_rw = root.join("extra-rw");
    for dir in [
        &home,
        &git_root,
        &ctx_global,
        &ctx_workflow,
        &extra_ro,
        &extra_rw,
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // Every agent gets host settings, so every settings family is exercised.
    let resolver = AuthPathResolver::at_home(&home);
    for agent in FIRST_PARTY_AGENTS {
        if let SettingsMount::Direct(relative) = first_party(agent).unwrap().settings_mount {
            let dir = home.join(relative);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("settings.json"), b"{\"host\":true}").unwrap();
        }
        let paths = resolver.resolve(agent);
        if let Some(dir) = paths.settings_dir {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("settings.json"), b"{\"host\":true}").unwrap();
        }
        if let Some(file) = paths.config_file {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, b"{\"host\":true}").unwrap();
        }
    }
    let session = Session::open(
        git_root.clone(),
        &StaticGitRootResolver::new(&git_root),
        SessionOpenOptions::default(),
    )
    .unwrap();
    let engine = AgentEngine::new(
        Arc::new(OverlayEngine::with_auth_resolver(resolver)),
        Arc::new(ImportRuntime {
            home: image_home.map(str::to_owned),
        }),
    );
    let rig = Rig::new("session-a");
    for agent in FIRST_PARTY_AGENTS {
        rig.driver
            .state
            .lock()
            .unwrap()
            .images
            .insert(agent_image_tag(&git_root, agent), fixture_image());
    }
    World {
        _tmp: tmp,
        home,
        git_root,
        ctx_global,
        ctx_workflow,
        extra_ro,
        extra_rw,
        session,
        engine,
        rig,
    }
}

impl World {
    fn run_options(&self, non_interactive: bool) -> AgentRunOptions {
        AgentRunOptions {
            non_interactive,
            initial_prompt: Some("task: fix it".into()),
            system_prompt: Some(PROMPT.into()),
            context_overlays: vec![
                ContextOverlay {
                    scope: ContextScope::Global,
                    host_path: self.ctx_global.clone(),
                    container_path: "/awman/context/global".into(),
                    permission: OverlayPermission::ReadOnly,
                },
                ContextOverlay {
                    scope: ContextScope::Workflow,
                    host_path: self.ctx_workflow.clone(),
                    container_path: "/awman/context/workflow".into(),
                    permission: OverlayPermission::ReadWrite,
                },
            ],
            directory_overlays: vec![
                DirectorySpec {
                    host: self.extra_ro.to_string_lossy().into(),
                    container: "/mnt/ro".into(),
                    permission: OverlayPermission::ReadOnly,
                },
                DirectorySpec {
                    host: self.extra_rw.to_string_lossy().into(),
                    container: "~/rw-tilde".into(),
                    permission: OverlayPermission::ReadWrite,
                },
            ],
            ..Default::default()
        }
    }

    /// The options the real `AgentEngine` resolves for `agent`, as every
    /// container-tier backend receives them.
    fn resolved(
        &self,
        agent: &str,
        run: &AgentRunOptions,
    ) -> Result<ResolvedContainerOptions, EngineError> {
        let agent = AgentName::new(agent).unwrap();
        let options = self.engine.build_options_with_credentials(
            &self.session,
            &agent,
            run,
            &AgentCredentials::default(),
        )?;
        let mut resolved = ResolvedContainerOptions::resolve(options).unwrap();
        resolved.name = Some(crate::engine::container::options::ContainerName::new(
            format!("awman-matrix-{}", agent.as_str()),
        ));
        Ok(resolved)
    }

    fn plan(&self, agent: &str, run: &AgentRunOptions) -> Result<LaunchPlan, EngineError> {
        self.rig.backend.plan(self.resolved(agent, run)?)
    }
}

fn mount<'a>(plan: &'a LaunchPlan, guest: &str) -> Option<&'a MountSpec> {
    plan.spec.mounts.iter().find(|m| m.guest == guest)
}

fn window(argv: &[String], first: &str, second: &str) -> bool {
    argv.windows(2).any(|w| w[0] == first && w[1] == second)
}

#[test]
fn every_settings_family_and_prompt_mode_belongs_to_some_agent() {
    let mut settings = [false; 4];
    // `Unsupported` delivery has no first-party agent since crush and maki
    // became custom definitions; it is covered through a custom fixture.
    let mut prompts = [false; 6];
    for agent in FIRST_PARTY_AGENTS {
        let matrix = first_party(agent).unwrap();
        settings[match matrix.settings_mount {
            SettingsMount::None => 0,
            SettingsMount::Direct(_) => 1,
            SettingsMount::Claude => 2,
            SettingsMount::Antigravity => 3,
        }] = true;
        match matrix.system_prompt_delivery {
            SystemPromptMode::Append => prompts[0] = true,
            SystemPromptMode::AppendInline { .. } => prompts[1] = true,
            SystemPromptMode::Replace => prompts[2] = true,
            SystemPromptMode::AgentsMd => prompts[3] = true,
            SystemPromptMode::EnvFile { .. } => prompts[4] = true,
            SystemPromptMode::AddDir { .. } => prompts[5] = true,
            SystemPromptMode::Unsupported => panic!("{agent}: unexpected Unsupported delivery"),
        }
    }
    assert_eq!(settings, [true; 4], "a SettingsMount family has no agent");
    assert_eq!(prompts, [true; 6], "a SystemPromptMode has no agent");
}

#[test]
fn unsupported_prompt_delivery_is_planned_for_a_custom_agent() {
    let mut world = world(Some(HOME));
    world.session = Session::for_tests_isolated(&world.git_root, &world.home.join(".awman"));
    crate::engine::agent::custom::install_crush_fixture(&world.git_root, world.session.env());
    world
        .rig
        .driver
        .state
        .lock()
        .unwrap()
        .images
        .insert(agent_image_tag(&world.git_root, "crush"), fixture_image());
    let plan = world.plan("crush", &world.run_options(false)).unwrap();
    assert!(!plan.command.argv.iter().any(|arg| arg.contains(PROMPT)));
    assert!(plan.command.argv.iter().any(|arg| arg == "crush"));
}

#[test]
fn every_agent_plans_every_overlay_settings_and_prompt_variant() {
    let world = world(Some(HOME));
    for non_interactive in [false, true] {
        let run = world.run_options(non_interactive);
        for agent in FIRST_PARTY_AGENTS {
            let matrix = first_party(agent).unwrap();
            let plan = world
                .plan(agent, &run)
                .unwrap_or_else(|e| panic!("{agent} (non_interactive={non_interactive}): {e}"));
            let context = format!("{agent} (non_interactive={non_interactive})");
            let mounts = &plan.spec.mounts;
            let argv = &plan.command.argv;

            // Overlay types: workspace, ro/rw directories, `~/` expansion, contexts.
            let workspace = mount(&plan, "/workspace").unwrap_or_else(|| panic!("{context}"));
            assert_eq!(workspace.host, world.git_root);
            assert!(!workspace.read_only);
            assert!(mount(&plan, "/mnt/ro").unwrap().read_only, "{context}");
            let tilde = mount(&plan, "/home/probe/rw-tilde").expect("~/ expands to image HOME");
            assert!(!tilde.read_only, "{context}");
            assert!(mount(&plan, "/awman/context/global").unwrap().read_only);
            assert!(!mount(&plan, "/awman/context/workflow").unwrap().read_only);

            // Mount invariants: absolute, unique, parents before children, never /root.
            let mut seen = std::collections::BTreeSet::new();
            for m in mounts {
                assert!(m.guest.starts_with('/'), "{context}: {}", m.guest);
                assert!(seen.insert(&m.guest), "{context}: duplicate {}", m.guest);
                assert!(!m.guest.starts_with("/root"), "{context}: {}", m.guest);
            }
            let depths: Vec<usize> = mounts
                .iter()
                .map(|m| m.guest.matches('/').count())
                .collect();
            assert!(
                depths.windows(2).all(|w| w[0] <= w[1]),
                "{context}: {depths:?}"
            );

            // Settings family, rooted at the imported image's HOME.
            let resolver = AuthPathResolver::at_home(&world.home);
            let host_settings = resolver.resolve(agent).settings_dir;
            match &matrix.settings_mount {
                SettingsMount::None => {
                    assert!(
                        mounts.iter().all(|m| !m.guest.starts_with("/home/probe/.")),
                        "{context}: unexpected settings mount"
                    );
                }
                SettingsMount::Direct(relative) => {
                    let m = mount(&plan, &format!("{HOME}/{relative}"))
                        .unwrap_or_else(|| panic!("{context}: no Direct settings mount"));
                    assert_eq!(m.host, world.home.join(relative), "{context}");
                    assert!(!m.read_only);
                }
                SettingsMount::Claude => {
                    let dir = mount(&plan, "/home/probe/.claude").expect("staged .claude");
                    assert_ne!(
                        Some(&dir.host),
                        host_settings.as_ref(),
                        "staged, not the host dir"
                    );
                    assert!(dir.host.is_dir() && !dir.read_only);
                    let file = mount(&plan, "/home/probe/.claude.json").expect(".claude.json");
                    assert!(file.host.is_file(), "a single-file bind, not its parent");
                }
                SettingsMount::Antigravity => {
                    assert!(mount(&plan, "/home/probe/.gemini").is_some(), "{context}");
                }
            }

            // System prompt delivery.
            match &matrix.system_prompt_delivery {
                SystemPromptMode::Append => {
                    let flag = matrix
                        .system_prompt_flag
                        .as_deref()
                        .unwrap_or("--append-system-prompt-file");
                    let guest = argv
                        .windows(2)
                        .find(|w| w[0] == flag)
                        .map(|w| w[1].clone())
                        .unwrap_or_else(|| panic!("{context}: no {flag} in {argv:?}"));
                    let m = mount(&plan, &guest).expect("prompt file mounted");
                    assert!(m.read_only && m.host.is_file(), "{context}");
                    assert_eq!(std::fs::read_to_string(&m.host).unwrap(), PROMPT);
                }
                SystemPromptMode::AppendInline { key } => {
                    let flag = matrix.system_prompt_flag.as_deref().unwrap_or("--config");
                    assert!(
                        window(argv, flag, &format!("{key}={PROMPT}")),
                        "{context}: {argv:?}"
                    );
                }
                SystemPromptMode::Replace => {
                    let flag = matrix.system_prompt_flag.as_deref().unwrap_or("--system");
                    let text = argv
                        .windows(2)
                        .find(|w| w[0] == flag)
                        .map(|w| w[1].clone())
                        .unwrap_or_else(|| panic!("{context}: no {flag}"));
                    assert!(text.starts_with("You are ") && text.ends_with(PROMPT));
                }
                SystemPromptMode::AgentsMd => {
                    assert_eq!(
                        std::fs::read_to_string(world.ctx_global.join("AGENTS.md")).unwrap(),
                        PROMPT
                    );
                }
                SystemPromptMode::EnvFile { var } => {
                    let (_, guest) = plan
                        .command
                        .env
                        .iter()
                        .find(|(k, _)| k == var)
                        .unwrap_or_else(|| panic!("{context}: {var} unset"));
                    let m = mount(&plan, guest).expect("prompt file mounted");
                    assert!(m.read_only && m.host.is_file(), "{context}");
                    assert_eq!(std::fs::read_to_string(&m.host).unwrap(), PROMPT);
                }
                SystemPromptMode::AddDir { flag } => {
                    for dir in ["/awman/context/global", "/awman/context/workflow"] {
                        assert!(window(argv, flag, dir), "{context}: {argv:?}");
                    }
                }
                SystemPromptMode::Unsupported => {
                    let leaked = argv.iter().any(|a| a.contains("PROMPT"))
                        || plan.command.env.iter().any(|(_, v)| v.contains("PROMPT"));
                    assert!(!leaked, "{context}: nothing may be fabricated");
                }
            }

            // The non-interactive subcommand or flag appears exactly once.
            if let Some(flag) = matrix.non_interactive_flag.as_deref() {
                let count = argv.iter().filter(|a| a.as_str() == flag).count();
                assert_eq!(
                    count,
                    usize::from(non_interactive),
                    "{context}: `{flag}` in {argv:?}"
                );
            }
            // The seeded initial prompt.
            assert_eq!(argv[0], matrix.interactive_entrypoint[0], "{context}");
            if non_interactive {
                assert_eq!(
                    plan.seeded_prompt.as_deref(),
                    Some("task: fix it"),
                    "{context}"
                );
                assert!(!argv.iter().any(|a| a == "task: fix it"), "{context}");
            } else {
                assert_eq!(plan.seeded_prompt, None, "{context}");
                assert_eq!(argv.last().unwrap(), "task: fix it", "{context}");
            }
            for (key, value) in &matrix.static_env {
                assert!(
                    plan.command.env.contains(&(key.clone(), value.clone())),
                    "{context}: static env {key}"
                );
            }
        }
    }
}

/// Docker/Apple vs builtin agent argv parity (C5, decision 8). Docker and
/// Apple share `docker::build_run_argv`; whatever follows the image there is
/// exactly the command builtin hands the sandbox, for every agent and every
/// launch shape. A tool list the agent has no flag for is refused by all
/// three, with the same typed error.
#[test]
fn docker_apple_and_builtin_render_the_same_agent_argv_for_every_agent() {
    use crate::data::config::repo::LaunchMode;
    use crate::engine::agent::definition::ModelFlagDelivery;
    use crate::engine::container::options::{AutoMode, PlanMode, YoloMode};
    use crate::engine::error::ToolRestriction;

    let world = world(Some(HOME));
    let mut checked = 0;
    for agent in FIRST_PARTY_AGENTS {
        let matrix = first_party(agent).unwrap();
        let model = (!matches!(matrix.model_flag, ModelFlagDelivery::Unsupported))
            .then(|| "some-model".to_string());
        let mut table: Vec<(&str, AgentRunOptions)> = vec![
            // The shape `awman ready`'s audit launches every agent with.
            (
                "ready",
                AgentRunOptions {
                    initial_prompt: Some("audit".into()),
                    ..Default::default()
                },
            ),
            ("interactive", world.run_options(false)),
            ("non-interactive", world.run_options(true)),
            (
                "yolo+model",
                AgentRunOptions {
                    yolo: Some(YoloMode::Enabled),
                    model: model.clone(),
                    ..world.run_options(false)
                },
            ),
            (
                "auto",
                AgentRunOptions {
                    auto: Some(AutoMode::Enabled),
                    ..world.run_options(true)
                },
            ),
            (
                "tools",
                AgentRunOptions {
                    allowed_tools: vec!["Read".into(), "Bash(git status)".into()],
                    disallowed_tools: vec!["Write".into()],
                    ..world.run_options(false)
                },
            ),
        ];
        // User `agentFlags` (WI 0127) land at the same place on every runtime:
        // after awman's own flags, before a seeded prompt. ACP keeps them too.
        table.push((
            "agent-flags+prompt",
            AgentRunOptions {
                agent_flags: Some(vec!["--user-a".into(), "value with spaces".into()]),
                initial_prompt: Some("seeded-prompt-mark".into()),
                model: model.clone(),
                ..world.run_options(false)
            },
        ));
        table.push((
            "agent-flags+non-interactive",
            AgentRunOptions {
                agent_flags: Some(vec!["--user-b".into()]),
                ..world.run_options(true)
            },
        ));
        if !matrix.plan_flags.is_empty() {
            table.push((
                "plan",
                AgentRunOptions {
                    plan: Some(PlanMode::Enabled),
                    ..world.run_options(false)
                },
            ));
        }
        if matrix.supports_acp {
            table.push((
                "acp",
                AgentRunOptions {
                    launch_mode: LaunchMode::Acp,
                    ..world.run_options(false)
                },
            ));
            for (label, allowed, disallowed) in [
                ("acp+allowed", vec!["Read".to_string()], vec![]),
                ("acp+disallowed", vec![], vec!["Write".to_string()]),
            ] {
                table.push((
                    label,
                    AgentRunOptions {
                        launch_mode: LaunchMode::Acp,
                        allowed_tools: allowed,
                        disallowed_tools: disallowed,
                        ..world.run_options(false)
                    },
                ));
            }
        }
        for (label, run) in table {
            let context = format!("{agent} ({label})");
            let resolved = match world.resolved(agent, &run) {
                Ok(resolved) => resolved,
                Err(EngineError::ToolRestrictionFlagMissing { restriction }) => {
                    let expected = if !run.disallowed_tools.is_empty()
                        && matrix.disallowed_tools_flag.is_none()
                    {
                        ToolRestriction::Disallowed
                    } else {
                        assert!(
                            !run.allowed_tools.is_empty() && matrix.allowed_tools_flag.is_none(),
                            "{context}"
                        );
                        ToolRestriction::Allowed
                    };
                    assert_eq!(restriction, expected, "{context}");
                    checked += 1;
                    continue;
                }
                Err(error) => panic!("{context}: {error}"),
            };
            let image = resolved.image.clone().expect("image");
            let docker = crate::engine::container::docker::build_run_argv(
                resolved.name.as_ref().unwrap(),
                &image,
                &resolved,
            );
            let builtin = world.rig.backend.plan(resolved);
            let lacks_flag = !run.disallowed_tools.is_empty()
                && matrix.disallowed_tools_flag.is_none()
                || !run.allowed_tools.is_empty() && matrix.allowed_tools_flag.is_none();
            let acp_restricted = matches!(run.launch_mode, LaunchMode::Acp)
                && (!run.allowed_tools.is_empty() || !run.disallowed_tools.is_empty());
            if acp_restricted {
                for (runtime, result) in [
                    ("docker/apple", docker.map(|_| ())),
                    ("builtin", builtin.map(|_| ())),
                ] {
                    assert!(
                        matches!(result, Err(EngineError::Config(_))),
                        "{context}: {runtime} must refuse a restricted ACP launch, got {result:?}"
                    );
                }
            } else if lacks_flag {
                let restriction = if matrix.disallowed_tools_flag.is_none() {
                    ToolRestriction::Disallowed
                } else {
                    ToolRestriction::Allowed
                };
                for (runtime, result) in [
                    ("docker/apple", docker.map(|_| ())),
                    ("builtin", builtin.map(|_| ())),
                ] {
                    match result {
                        Err(EngineError::ToolRestrictionFlagMissing { restriction: got }) => {
                            assert_eq!(got, restriction, "{context}: {runtime}")
                        }
                        other => panic!("{context}: {runtime} must refuse, got {other:?}"),
                    }
                }
            } else {
                let docker = docker.unwrap_or_else(|e| panic!("{context}: docker: {e}"));
                let plan = builtin.unwrap_or_else(|e| panic!("{context}: builtin: {e}"));
                let after_image = &docker[docker.iter().position(|a| *a == image.0).unwrap() + 1..];
                assert_eq!(after_image, plan.command.argv.as_slice(), "{context}");
                if let Some(flags) = run.agent_flags.as_ref() {
                    let argv = &plan.command.argv;
                    let at = argv
                        .windows(flags.len())
                        .position(|w| w == flags.as_slice())
                        .unwrap_or_else(|| panic!("{context}: agentFlags missing from {argv:?}"));
                    if run.initial_prompt.is_some()
                        && !run.non_interactive
                        && !matches!(run.launch_mode, LaunchMode::Acp)
                    {
                        let prompt = argv
                            .iter()
                            .rposition(|a| a.contains("seeded-prompt-mark"))
                            .unwrap_or_else(|| panic!("{context}: no prompt in {argv:?}"));
                        assert!(at < prompt, "{context}: agentFlags must precede the prompt");
                    }
                }
            }
            checked += 1;
        }
    }
    assert!(checked >= FIRST_PARTY_AGENTS.len() * 6);
}

#[test]
fn a_missing_image_home_is_refused_instead_of_guessing_root() {
    let world = world(None);
    let run = world.run_options(false);
    for agent in FIRST_PARTY_AGENTS {
        match world.plan(agent, &run) {
            Err(EngineError::Config(message)) => {
                assert!(message.contains("HOME"), "{agent}: {message}")
            }
            other => panic!(
                "{agent}: expected a Config error, got {:?}",
                other.map(|_| ())
            ),
        }
    }
    let relative = world_with_home("relative/home");
    assert!(relative
        .plan("claude", &relative.run_options(false))
        .is_err());
}

fn world_with_home(home: &str) -> World {
    world(Some(home))
}

#[test]
fn the_same_options_yield_the_same_mounts_for_the_same_image() {
    // The plan is a pure function of options + image config: repeating it
    // must not reorder mounts (Docker-argv-style reproducibility).
    let world = world(Some(HOME));
    let run = world.run_options(false);
    let key = |plan: &LaunchPlan| -> Vec<(String, bool)> {
        plan.spec
            .mounts
            .iter()
            .map(|m| (m.guest.clone(), m.read_only))
            .collect()
    };
    let first = key(&world.plan("codex", &run).unwrap());
    let second = key(&world.plan("codex", &run).unwrap());
    assert_eq!(first, second);
}

#[test]
fn acp_launch_keeps_the_agent_entrypoint_and_is_persistent() {
    let world = world(Some(HOME));
    let mut run = world.run_options(false);
    run.launch_mode = crate::data::config::repo::LaunchMode::Acp;
    let plan = world.plan("cline", &run).unwrap();
    assert_eq!(plan.command.argv, ["cline", "--acp"]);
    assert!(plan.persistent_stdin);
    assert_eq!(plan.seeded_prompt, None);
}

#[test]
fn named_and_all_skill_overlays_reach_the_builtin_plan() {
    let config = tempfile::tempdir().unwrap();
    let skill = config.path().join("skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "synthetic review skill").unwrap();
    let mut world = world(Some(HOME));
    world.session = Session::for_tests_isolated(&world.git_root, config.path());
    for all in [false, true] {
        let mut run = world.run_options(false);
        run.include_all_skills = all;
        run.named_skills = vec!["review".into()];
        let plan = world.plan("codex", &run).unwrap();
        let base = format!(
            "{HOME}/{}",
            first_party("codex").unwrap().skills_mount.unwrap()
        );
        let destination = if all { base } else { format!("{base}/review") };
        let mounted = mount(&plan, &destination).expect("resolved skill reaches builtin");
        assert!(mounted.read_only);
        let expected = if all {
            config.path().join("skills")
        } else {
            skill.clone()
        };
        assert_eq!(mounted.host, expected.canonicalize().unwrap());
    }
}

#[test]
fn environment_overlay_keeps_boundaries_and_literal_precedence() {
    use crate::engine::container::options::{EnvLiteral, EnvVar};
    // PATH is only read, never modified. This exercises the same passthrough
    // transport as env(NAME) without touching credentials or process globals.
    let world = world(Some(HOME));
    let mut options = super::lifecycle_tests::options("awman-env-overlay");
    options.env_passthrough = vec![EnvVar("PATH".into())];
    options.env_literal = vec![EnvLiteral {
        key: "PATH".into(),
        value: "literal with spaces\nand newline".into(),
    }];
    let plan = world.rig.backend.plan(options).unwrap();
    let effective: HashMap<_, _> = plan.command.env.into_iter().collect();
    assert_eq!(
        effective.get("PATH").map(String::as_str),
        Some("literal with spaces\nand newline")
    );
}

#[test]
fn claude_credentials_are_leased_per_execution_and_refresh_atomically_for_every_consumer() {
    use crate::engine::auth::credential::{
        claude_spec, CredentialFile, CredentialFingerprint, CredentialSnapshot, SecretString,
    };
    use crate::engine::auth::RefreshableCredentialDelivery;
    use crate::engine::credential_refresh::{CredentialLease, LeaseFactoryHandle, LeaseRegistry};
    use crate::engine::overlay::write_credential_file_atomic;

    struct Registry(LeaseRegistry);
    impl crate::engine::credential_refresh::CredentialLeaseFactory for Registry {
        fn register_lease(
            &self,
            delivery: &RefreshableCredentialDelivery,
            container: &str,
        ) -> CredentialLease {
            self.0.register(delivery, container)
        }
    }
    fn payload(token: &str) -> Vec<u8> {
        format!(r#"{{"claudeAiOauth":{{"accessToken":"{token}","expiresAt":4102444800000}}}}"#)
            .into_bytes()
    }

    let world = world(Some(HOME));
    let factory = Arc::new(Registry(LeaseRegistry::new()));
    let handle = LeaseFactoryHandle::new(factory.clone());
    let spec = claude_spec();
    let mut staged = Vec::new();
    let mut plans = Vec::new();
    for consumer in 0..3 {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let path = root.path().join(".credentials.json");
        std::fs::write(&path, payload("token-v1")).unwrap();
        let delivery = RefreshableCredentialDelivery {
            agent: AgentName::new("claude").unwrap(),
            spec_agent: spec.agent,
            credential_env_key: spec.credential_env_key,
            staged_path: path.clone(),
            staged_root: root.path().to_path_buf(),
            initial_fingerprint: CredentialFingerprint::of(&CredentialSnapshot {
                secret: SecretString::new("token-v1"),
                expires_at: None,
                extra: Default::default(),
            }),
        };
        let mut options = super::lifecycle_tests::options(&format!("awman-lease-{consumer}"));
        options.refreshable_credentials = vec![delivery];
        options.lease_factory = Some(handle.clone());
        options.overlays = vec![OverlaySpec {
            host_path: root.path().to_path_buf(),
            container_path: "/home/probe/.claude".into(),
            permission: OverlayPermission::ReadWrite,
        }];
        plans.push(world.rig.backend.plan(options).unwrap());
        staged.push((root, path));
    }
    assert_eq!(factory.0.len(), 3, "one lease per credentialed execution");
    assert!(plans.iter().all(|p| p.leases.len() == 1));
    let snapshot = factory.0.snapshot();
    let distinct: std::collections::BTreeSet<_> =
        snapshot.iter().map(|s| s.staged_path.clone()).collect();
    assert_eq!(distinct.len(), 3);

    // Guest-side readers (here: host threads over the same directories) must
    // only ever see a complete v1 or v2 file while the host refreshes.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = staged
        .iter()
        .flat_map(|(_, path)| [path.clone(), path.clone()])
        .map(|path| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut torn = 0usize;
                let mut reads = 0usize;
                loop {
                    if let Ok(bytes) = std::fs::read(&path) {
                        reads += 1;
                        if bytes != payload("token-v1") && bytes != payload("token-v2") {
                            torn += 1;
                        }
                    }
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                }
                (reads, torn)
            })
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(30));
    for (root, path) in &staged {
        let before = std::fs::metadata(path).unwrap();
        let wrote = write_credential_file_atomic(
            root.path(),
            &CredentialFile {
                relative_path: ".credentials.json".into(),
                contents: payload("token-v2"),
                mode: 0o600,
            },
        )
        .unwrap();
        assert!(wrote);
        let after = std::fs::metadata(path).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_ne!(
            before.ino(),
            after.ino(),
            "replaced by rename, never truncated in place"
        );
        assert_eq!(after.mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(path).unwrap(), payload("token-v2"));
    }
    std::thread::sleep(std::time::Duration::from_millis(30));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for reader in readers {
        let (reads, torn) = reader.join().unwrap();
        assert!(reads > 0);
        assert_eq!(torn, 0, "a consumer observed a partial credential file");
    }

    // The lease brackets the execution: dropping the plan releases it.
    drop(plans);
    assert!(factory.0.is_empty());
}
