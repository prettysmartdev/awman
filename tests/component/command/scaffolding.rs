use awman::command::commands::new::{
    NewCommand, NewOutcome, NewSkillFlags, NewSubcommand, NewWorkflowFlags,
};
use awman::command::commands::Command;
use awman::command::commands::{
    agent_auth::*, agent_setup::*, agent_trust::AgentTrustFrontend, mount_scope::*,
    specs::SpecsCommandFrontend,
};
use awman::command::dispatch::Engines;
use awman::command::error::CommandError;
use awman::data::session::{Session, SessionOpenOptions};
use awman::data::{
    message::{UserMessage, UserMessageSink},
    session::AgentName,
};

struct Scaffolder;
impl UserMessageSink for Scaffolder {
    fn write_message(&mut self, _: UserMessage) {}
    fn replay_queued(&mut self) {}
}
impl MountScopeFrontend for Scaffolder {
    fn ask_mount_scope(
        &mut self,
        _: &std::path::Path,
        _: &std::path::Path,
    ) -> Result<MountScopeDecision, CommandError> {
        panic!("scaffolding must not mount a runtime")
    }
}
impl AgentSetupFrontend for Scaffolder {
    fn ask_agent_setup(
        &mut self,
        _: &AgentName,
        _: &AgentName,
        _: bool,
        _: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        panic!("scaffolding must not set up an agent")
    }
    fn record_fallback(&mut self, _: &AgentName, _: &AgentName) {
        panic!("scaffolding must not resolve an agent fallback")
    }
}
impl AgentAuthFrontend for Scaffolder {
    fn ask_agent_auth_consent(
        &mut self,
        _: &AgentName,
        _: &[&str],
    ) -> Result<AgentAuthDecision, CommandError> {
        panic!("scaffolding must not request credentials")
    }
}
impl HasAgentFrontend for Scaffolder {
    fn container_frontend(
        &mut self,
    ) -> Box<dyn awman::engine::agent_runtime::frontend::AgentFrontend> {
        panic!("scaffolding must not launch an agent")
    }
}
impl AgentTrustFrontend for Scaffolder {}
impl AgentLaunchFrontend for Scaffolder {
    fn set_pty_active(&mut self, _: bool) {
        panic!("scaffolding must not acquire terminal IO")
    }
}
impl SpecsCommandFrontend for Scaffolder {}
impl awman::command::commands::new::NewCommandFrontend for Scaffolder {
    fn ask_workflow_step_name(&mut self) -> Result<String, CommandError> {
        Ok("one".into())
    }
    fn ask_workflow_step_prompt(&mut self) -> Result<String, CommandError> {
        Ok("literal scaffold prompt".into())
    }
}

/// Guards: #144 global scaffolding must use the invoking session's roots.
#[tokio::test(flavor = "multi_thread")]
async fn global_scaffolds_and_empty_pull_all_use_only_the_session_home() {
    let s = crate::support::sandbox(
        "command::scaffolding::global_scaffolds_and_empty_pull_all_use_only_the_session_home",
    );
    let env = crate::support::env_snapshot(&s);
    let session = Session::open_at_git_root(
        s.work(),
        s.work(),
        SessionOpenOptions {
            env: Some(env.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let engines = Engines::for_env(env).unwrap();
    let command = NewCommand::new(
        NewSubcommand::Workflow(NewWorkflowFlags {
            interview: false,
            non_interactive: true,
            global: true,
            format: "toml".into(),
        }),
        engines.clone(),
        session.clone(),
    );
    let outcome = command
        .run_with_frontend(Box::new(Scaffolder))
        .await
        .unwrap();
    let NewOutcome::Workflow(outcome) = outcome else {
        panic!("expected workflow")
    };
    let path = std::path::PathBuf::from(outcome.path.unwrap());
    assert!(
        path.starts_with(s.awman_home().join("workflows")),
        "{}",
        path.display()
    );
    assert!(path.is_file());
    let flags = NewSkillFlags {
        interview: false,
        non_interactive: true,
        global: true,
        pull: None,
        pull_all: false,
        subdir: None,
    };
    let command = NewCommand::new(
        NewSubcommand::Skill(flags.clone()),
        engines.clone(),
        session.clone(),
    );
    let outcome = command
        .run_with_frontend(Box::new(Scaffolder))
        .await
        .unwrap();
    let NewOutcome::Skill(outcome) = outcome else {
        panic!("expected skill")
    };
    let path = std::path::PathBuf::from(outcome.path.unwrap());
    assert!(
        path.starts_with(s.awman_home().join("skills")),
        "{}",
        path.display()
    );
    assert!(path.is_file());
    let command = NewCommand::new(
        NewSubcommand::Skill(NewSkillFlags {
            pull_all: true,
            ..flags
        }),
        engines,
        session,
    );
    let outcome = command
        .run_with_frontend(Box::new(Scaffolder))
        .await
        .unwrap();
    let NewOutcome::Skill(outcome) = outcome else {
        panic!("expected pull-all")
    };
    assert!(outcome.pull);
    assert!(outcome.libraries.is_empty());
    s.finish();
}
