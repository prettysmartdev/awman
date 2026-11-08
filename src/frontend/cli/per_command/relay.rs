//! CLI rendering of an engine's dialog requests. Dispatch owns the socket,
//! submission, cancellation and backend policy.

use crate::data::dialog::{DialogRequest, DialogResponse};
use crate::frontend::cli::command_frontend::CliFrontend;
use tokio_util::sync::CancellationToken;

pub(crate) fn render_dialog(
    frontend: &mut CliFrontend,
    request: DialogRequest,
    interrupted: &CancellationToken,
) -> DialogResponse {
    use super::helpers;
    use DialogResponse as R;
    if interrupted.is_cancelled() {
        return R::Interrupted;
    }
    match request {
        DialogRequest::AgentSetup(state) => {
            use crate::command::commands::agent_setup::{AgentSetupDecision, AgentSetupFrontend};
            let Ok(agent) = crate::data::session::AgentName::new(&state.agent_name) else {
                return R::Cancel;
            };
            let fallback = state
                .fallback_name
                .as_deref()
                .and_then(|name| crate::data::session::AgentName::new(name).ok());
            match frontend.ask_agent_setup(
                &agent,
                fallback.as_ref().unwrap_or(&agent),
                state.has_fallback,
                state.image_only,
            ) {
                Ok(AgentSetupDecision::Setup) => R::Yes,
                Ok(AgentSetupDecision::FallbackToDefault) => R::Char('f'),
                _ => R::Cancel,
            }
        }
        DialogRequest::MountScope(state) => {
            use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
            match frontend.ask_mount_scope(
                std::path::Path::new(&state.git_root),
                std::path::Path::new(&state.cwd),
            ) {
                Ok(MountScopeDecision::MountGitRoot) => R::Char('r'),
                Ok(MountScopeDecision::MountCurrentDirOnly) => R::Char('c'),
                _ => R::Cancel,
            }
        }
        DialogRequest::AgentAuth(state) => {
            use crate::command::commands::agent_auth::{AgentAuthDecision, AgentAuthFrontend};
            let Ok(agent) = crate::data::session::AgentName::new(&state.agent_name) else {
                return R::Cancel;
            };
            let names: Vec<&str> = state.env_vars.iter().map(String::as_str).collect();
            match frontend.ask_agent_auth_consent(&agent, &names) {
                Ok(AgentAuthDecision::Accept) => R::Yes,
                Ok(AgentAuthDecision::Decline) => R::No,
                _ => R::Char('o'),
            }
        }
        DialogRequest::Loading { .. } => R::Dismissed,
        DialogRequest::KeySetupNotice { body, .. } => {
            eprintln!("{body}");
            R::Dismissed
        }
        DialogRequest::TerminalRestore => R::Dismissed,
        DialogRequest::TerminalRelease { .. } => R::Yes,
        // Headless answers are resolved by Layer 2 from the CLI profile.
        // A generic dialog renderer never invents an affirmative answer.
        _ if frontend.non_interactive => R::Dismissed,
        DialogRequest::YesNo { body, .. } => match helpers::yes_no_required(&body) {
            Some(true) => R::Yes,
            Some(false) => R::No,
            None => R::Dismissed,
        },
        DialogRequest::YesNoCancel { body, .. } => keyed(
            &body,
            &[
                ('y', "Yes".into()),
                ('n', "No".into()),
                ('c', "Cancel".into()),
            ],
        ),
        DialogRequest::TextInput {
            prompt,
            default_text,
            ..
        } => helpers::read_line(&prompt)
            .map(|text| {
                R::Text(if text.is_empty() {
                    default_text.unwrap_or_default()
                } else {
                    text
                })
            })
            .unwrap_or(R::Dismissed),
        DialogRequest::MultilineInput {
            prompt,
            default_text,
            ..
        } => helpers::read_multiline(&prompt)
            .map(|text| {
                R::Text(if text.is_empty() {
                    default_text.unwrap_or_default()
                } else {
                    text
                })
            })
            .unwrap_or(R::Dismissed),
        DialogRequest::SecretInput { title, prompt } => {
            helpers::read_secret_line(&format!("{title}: {prompt}"))
                .ok()
                .flatten()
                .map(R::Secret)
                .unwrap_or(R::Dismissed)
        }
        DialogRequest::ListPicker { title, items } => {
            let items: Vec<&str> = items.iter().map(String::as_str).collect();
            helpers::pick_numbered(&title, &items, 1)
                .and_then(|n| n.checked_sub(1))
                .filter(|n| *n < items.len())
                .map(R::Index)
                .unwrap_or(R::Dismissed)
        }
        DialogRequest::KindSelect { title, options } => {
            let labels: Vec<&str> = options.iter().map(|(_, label)| label.as_str()).collect();
            helpers::pick_numbered(&title, &labels, 1)
                .and_then(|n| n.checked_sub(1))
                .filter(|n| *n < options.len())
                .map(R::Index)
                .unwrap_or(R::Dismissed)
        }
        DialogRequest::Custom { title, body, keys } => {
            eprintln!("awman: {title}");
            keyed(&body, &keys)
        }
        DialogRequest::Review(review) => {
            use crate::data::dialog::ReviewBody;
            let body = match &review.body {
                ReviewBody::Exact(text) | ReviewBody::Prose(text) => text.clone(),
                ReviewBody::Prerequisites(report) => report.render_lines().join("\n"),
            };
            eprintln!("awman: {}", review.title);
            if !review.note.is_empty() {
                eprintln!("awman: {}", review.note);
            }
            keyed(&body, &review.keys)
        }
        DialogRequest::WorkflowStepConfirm(state) => keyed(
            &format!(
                "{} completed. Next: {}",
                state.completed_step, state.next_step
            ),
            &[('n', "Next step".into()), ('p', "Pause".into())],
        ),
        DialogRequest::WorkflowControlBoard(state) => {
            let mut keys = Vec::new();
            if state.can_dismiss {
                keys.push(('s', "Resume".into()));
            }
            if state.can_launch_next {
                keys.push((
                    'n',
                    state
                        .launch_next_label
                        .unwrap_or_else(|| "Next: new container".into()),
                ));
            }
            if state.can_continue_current {
                keys.push(('c', "Continue in current container".into()));
            }
            if state.can_restart {
                keys.push(('r', "Restart current step".into()));
            }
            if state.can_go_back {
                keys.push(('b', "Previous step".into()));
            }
            if state.can_finish {
                keys.push(('f', "Finish workflow".into()));
            }
            keys.push(('p', "Pause".into()));
            keys.push(('a', "Abort".into()));
            keyed(&state.step_name, &keys)
        }
        DialogRequest::WorkflowYoloCountdown(state) => {
            eprintln!(
                "awman: {}",
                crate::frontend::program_status_text::countdown_label(
                    state.kind,
                    state.remaining_secs
                )
            );
            R::Dismissed
        }
        DialogRequest::WorkflowCancelConfirm
        | DialogRequest::CloseTabConfirm
        | DialogRequest::QuitConfirm => {
            match helpers::yes_no_required("Cancel the running command?") {
                Some(true) => R::Yes,
                Some(false) => R::No,
                None => R::Dismissed,
            }
        }
        DialogRequest::ConfigShow { .. } => R::Dismissed,
    }
}

fn keyed(body: &str, keys: &[(char, String)]) -> DialogResponse {
    eprintln!("awman: {body}");
    for (key, label) in keys {
        eprintln!("  [{key}] {label}");
    }
    super::helpers::read_line("choice:")
        .and_then(|line| {
            let mut chars = line.chars();
            let ch = chars.next()?;
            (chars.next().is_none() && keys.iter().any(|(key, _)| *key == ch)).then_some(ch)
        })
        .map(DialogResponse::Char)
        .unwrap_or(DialogResponse::Dismissed)
}

/// Render the wire completion using the same outcome renderers as a builtin
/// command. The wire exit code remains authoritative, including aggregate
/// failures and successful commands whose agent exited nonzero.
pub(crate) fn render_result(
    result: crate::data::frontend_protocol::CommandResultWire,
    json: bool,
) -> std::process::ExitCode {
    if let Some(error) = result.error {
        let message =
            super::super::format_error(&crate::command::error::CommandError::Other(error.message));
        if json {
            println!("{}", serde_json::json!({"error": message}));
        } else {
            eprintln!("{message}");
        }
    } else if let Some(outcome) = result.outcome {
        match decode_outcome(outcome, json) {
            Ok(outcome) => {
                if let Some(text) = super::render::render(&outcome, json) {
                    println!("{text}");
                }
            }
            Err(error) => {
                eprintln!("awman: invalid command outcome from the engine: {error}");
                return std::process::ExitCode::from(1);
            }
        }
    }
    std::process::ExitCode::from(u8::try_from(result.exit_code).unwrap_or(1))
}

fn field<T: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
    name: &str,
) -> Result<T, serde_json::Error> {
    serde_json::from_value(value.get(name).cloned().unwrap_or(serde_json::Value::Null))
}

/// Decode just the relayable command outcomes. This translation is kept
/// beside their CLI renderers; no wire payload is treated as display text.
fn decode_outcome(
    value: serde_json::Value,
    json: bool,
) -> Result<crate::command::CommandOutcome, serde_json::Error> {
    use crate::command::commands::{
        chat::ChatOutcome, exec_prompt::ExecPromptOutcome, exec_workflow::ExecWorkflowOutcome,
        new::*, ready::ReadyOutcome,
    };
    use crate::command::CommandOutcome as O;
    let payload = &value["payload"];
    Ok(match value["kind"].as_str() {
        Some("Empty" | "Init") => O::Empty,
        Some("Chat") => O::Chat(ChatOutcome {
            agent: field(payload, "agent")?,
            exit_code: field(payload, "exit_code")?,
        }),
        Some("ExecPrompt") => O::ExecPrompt(ExecPromptOutcome {
            agent: field(payload, "agent")?,
            exit_code: field(payload, "exit_code")?,
        }),
        Some("ExecWorkflow") => O::ExecWorkflow(ExecWorkflowOutcome {
            workflow: field(payload, "workflow")?,
            exit_code: field(payload, "exit_code")?,
            worktree_used: field(payload, "worktree_used")?,
            prerequisites: field(payload, "prerequisites")?,
        }),
        Some("Ready") => O::Ready(ReadyOutcome {
            runtime: field(payload, "runtime")?,
            dockerfile: field(payload, "dockerfile")?,
            base_image: field(payload, "base_image")?,
            agent_image: field(payload, "agent_image")?,
            image_source: field(payload, "image_source")?,
            agent_definition: field(payload, "agent_definition")?,
            local_agent: field(payload, "local_agent")?,
            audit: field(payload, "audit")?,
            image_rebuild: field(payload, "image_rebuild")?,
            agent_credentials: field(payload, "agent_credentials")?,
            non_default_agent_images: field(payload, "non_default_agent_images")?,
            image_results: field::<Option<_>>(payload, "image_results")?.unwrap_or_default(),
            base_image_results: field::<Option<_>>(payload, "base_image_results")?
                .unwrap_or_default(),
            ready: field(payload, "ready")?,
            json_requested: json,
            refresh_requested: payload["refresh_requested"].as_bool().unwrap_or(false),
        }),
        Some("New") => {
            let body = &payload["payload"];
            O::New(match payload["kind"].as_str() {
                Some("Spec") => NewOutcome::Spec(NewSpecOutcome {
                    interview: field(body, "interview")?,
                    path: field(body, "path")?,
                }),
                Some("Workflow") => NewOutcome::Workflow(NewWorkflowOutcome {
                    interview: field(body, "interview")?,
                    global: field(body, "global")?,
                    format: field(body, "format")?,
                    path: field(body, "path")?,
                }),
                Some("Skill") => NewOutcome::Skill(NewSkillOutcome {
                    interview: field(body, "interview")?,
                    global: field(body, "global")?,
                    path: field(body, "path")?,
                    pull: field(body, "pull")?,
                    libraries: body["libraries"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|library| {
                            Ok(PullLibraryOutcome {
                                slug: field(library, "slug")?,
                                dir: field(library, "dir")?,
                                updated: field(library, "updated")?,
                                skills_found: field(library, "skills_found")?,
                                error: field(library, "error")?,
                            })
                        })
                        .collect::<Result<_, serde_json::Error>>()?,
                }),
                Some("Agent") => NewOutcome::Agent(NewAgentOutcome {
                    name: field(body, "name")?,
                    path: field(body, "path")?,
                    scope: field(body, "scope")?,
                    skeleton: field(body, "skeleton")?,
                    files_written: field(body, "files_written")?,
                    approved: field(body, "approved")?,
                    host_command_block: field(body, "host_command_block")?,
                    next_step: field(body, "next_step")?,
                }),
                _ => {
                    return Err(<serde_json::Error as serde::de::Error>::custom(
                        "unsupported new outcome",
                    ))
                }
            })
        }
        _ => {
            return Err(<serde_json::Error as serde::de::Error>::custom(
                "unsupported relayed outcome",
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::commands::exec_prompt::ExecPromptOutcome;
    use crate::command::commands::exec_workflow::ExecWorkflowOutcome;
    use crate::command::commands::new::*;
    use crate::command::CommandOutcome;

    #[test]
    fn wire_outcomes_use_the_builtin_renderers_and_exit_codes() {
        for outcome in [
            CommandOutcome::ExecPrompt(ExecPromptOutcome {
                agent: Some("fixture".into()),
                exit_code: Some(17),
            }),
            CommandOutcome::ExecWorkflow(ExecWorkflowOutcome {
                workflow: "a b.toml".into(),
                exit_code: Some(3),
                worktree_used: true,
                prerequisites: None,
            }),
            CommandOutcome::New(NewOutcome::Workflow(NewWorkflowOutcome {
                interview: false,
                global: false,
                format: "toml".into(),
                path: Some("a b.toml".into()),
            })),
            CommandOutcome::New(NewOutcome::Skill(NewSkillOutcome {
                interview: false,
                global: true,
                path: None,
                pull: true,
                libraries: vec![PullLibraryOutcome {
                    slug: "fixture".into(),
                    dir: "/fixture".into(),
                    updated: false,
                    skills_found: vec![],
                    error: Some("failed".into()),
                }],
            })),
        ] {
            let value = serde_json::to_value(&outcome).unwrap();
            for json in [false, true] {
                let decoded = decode_outcome(value.clone(), json).unwrap();
                assert_eq!(
                    super::super::render::render(&decoded, json),
                    super::super::render::render(&outcome, json)
                );
                assert_eq!(decoded.exit_code(), outcome.exit_code());
            }
        }
    }

    #[test]
    fn headless_dialogs_use_existing_cli_agent_renderers() {
        use crate::command::dispatch::catalogue::CommandCatalogue;
        use crate::command::dispatch::CliRelayFrontend;
        use crate::data::dialog::{AgentAuthState, AgentSetupState, MountScopeState};
        let matches = CommandCatalogue::get()
            .build_clap_command()
            .try_get_matches_from(["awman", "exec", "prompt", "--headless", "hello"])
            .unwrap();
        let mut frontend = CliFrontend::new(matches);
        let interrupted = CancellationToken::new();
        assert_eq!(
            frontend.relay_dialog(
                DialogRequest::AgentAuth(AgentAuthState {
                    agent_name: "claude".into(),
                    env_vars: vec!["SECRET".into()]
                }),
                &interrupted
            ),
            DialogResponse::Char('o')
        );
        assert_eq!(
            frontend.relay_dialog(
                DialogRequest::AgentSetup(AgentSetupState {
                    agent_name: "claude".into(),
                    image_only: false,
                    has_fallback: false,
                    fallback_name: None
                }),
                &interrupted
            ),
            DialogResponse::Yes
        );
        assert_eq!(
            frontend.relay_dialog(
                DialogRequest::MountScope(MountScopeState {
                    git_root: "/repo".into(),
                    cwd: "/repo/sub".into()
                }),
                &interrupted
            ),
            DialogResponse::Char('r')
        );
        interrupted.cancel();
        assert_eq!(
            frontend.relay_dialog(
                DialogRequest::YesNo {
                    title: "test".into(),
                    body: "test?".into()
                },
                &interrupted
            ),
            DialogResponse::Interrupted
        );
    }
}
