//! Existing work-item editing. Resolution decides the source once; local files
//! stay in the checkout and remote issues use the shared update-only draft sync.

use std::path::Path;

use super::*;
use crate::command::commands::launch_policy::agent_session::{
    resolve_agent, resolve_credentials, run_agent_session, AgentSessionError, AgentSessionRequest,
};
use crate::command::commands::launch_policy::LaunchPolicy;
use crate::command::commands::prompt_templates::edit::{
    render_edit_prompt, EditLocation, EditPromptRequest, EditSubject,
};
use crate::command::work_item::draft_sync::{DraftSyncMode, FinalDraftSyncOutcome};
use crate::command::work_item::edit::reject_explicit_read_only_repo_context;
use crate::command::work_item::spec_service::{
    resolve_spec_edit_target, run_remote_spec_edit_with_overlays, RemoteSpecEdit, SpecEditTarget,
};
use crate::data::context::ContextRelPath;
use crate::data::fs::safe_tree::SafeTree;
use crate::data::fs::work_item_draft::MAX_WORK_ITEM_BYTES;

const LABEL: &str = "edit spec";

pub(super) async fn run(
    command: &EditCommand,
    f: EditSpecFlags,
    frontend: &mut dyn EditCommandFrontend,
) -> Result<EditOutcome, CommandError> {
    let engines = command.engines();
    let session = command.session();
    let flags = &f.common;
    // Always resolve the item before selecting or launching an agent. In
    // particular, a failed remote lookup must never fall back to a local number.
    let resolved =
        resolve_spec_edit_target(engines, session, &flags.reference, LABEL, frontend).await?;
    let identity = resolved.item().source_id(session.git_root());
    let cli_overlays = LaunchPolicy::parse_overlay_flags(&flags.overlays)?;
    if matches!(&resolved, SpecEditTarget::Remote { .. }) {
        if let Err(e) = reject_explicit_read_only_repo_context(&cli_overlays) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("{LABEL}: {e}"),
            });
            return Err(CommandError::WorkItem(e));
        }
    }
    engines.require_container_runtime()?;
    if matches!(&resolved, SpecEditTarget::Local { .. })
        && engines.agent_engine.read_only_workspace()
    {
        return Err(target_error(
            frontend,
            "a local spec cannot be edited through a read-only repository mount".into(),
        ));
    }
    require_repository_mount(session, frontend, LABEL, "a spec")?;
    let agent = resolve_agent(session, &flags.agent, LABEL, frontend)?;
    let credentials = resolve_credentials(engines, session, &agent, LABEL, frontend)?;
    frontend.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!(
            "{LABEL}: editing work item \"{}\" with agent '{agent}'",
            resolved.item().title
        ),
    });

    let collected =
        session
            .effective_config()
            .collected_overlays(cli_overlays.clone(), None, None)?;
    let mut run_opts = run_options(flags);
    run_opts.command_secrets = engines.command_secrets();
    run_opts.directory_overlays = collected.directories;
    run_opts.include_all_skills = collected.include_all_skills;
    run_opts.named_skills = collected.named_skills;
    run_opts.env_passthrough =
        (!collected.env_passthrough.is_empty()).then_some(collected.env_passthrough);

    match resolved {
        SpecEditTarget::Local {
            item,
            host_path,
            container_path,
        } => {
            // Re-read at launch, rather than using the resolver's earlier body.
            // Hash bytes, not metadata: rename saves and equal-size edits count.
            let (_, baseline) =
                read_local_spec(&host_path).map_err(|message| target_error(frontend, message))?;
            let (context_overlays, system_prompt) = LaunchPolicy::for_session(session)
                .with_git(&engines.git_engine)
                .resolve_context_overlays(
                    &collected.context_overlays,
                    &agent,
                    None,
                    None,
                    frontend,
                )?;
            run_opts.context_overlays = context_overlays;
            run_opts.system_prompt = system_prompt;
            run_opts.initial_prompt = Some(prompt(
                &item.title,
                &container_path,
                EditLocation::InPlace,
                flags,
            ));
            let target = EditTarget {
                kind: EditKind::Spec,
                identity,
                scope: EditScope::Local,
                persistence: EditPersistence::InPlace,
                files: vec![EditFile {
                    host_path: host_path.clone(),
                    container_path,
                    role: EditFileRole::Primary,
                }],
                bundle_dir: None,
            };
            let execution = match run_agent_session(
                engines,
                session,
                AgentSessionRequest {
                    label: LABEL,
                    agent: &agent,
                    run_opts,
                    credentials: Some(credentials),
                },
                command.cancel(),
                frontend,
            )
            .await
            {
                Err(AgentSessionError::Launch(error)) => return Err(error),
                Err(AgentSessionError::Wait(error)) => {
                    Err(format!("failed to wait for the agent: {error}"))
                }
                Ok(info) => Ok(info.exit_code),
            };
            let save = check_local_spec(&host_path, &baseline);
            conclude(LABEL, target, flags, execution, save, frontend)
        }
        SpecEditTarget::Remote {
            item,
            issue,
            source,
            url,
        } => {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!("{LABEL}: work item {url}"),
            });
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!(
                    "{LABEL}: draft edits are published to the tracker while the agent runs"
                ),
            });
            let seed = issue.body.clone();
            let provider = issue.provider.clone();
            let render = |path: &str| prompt(&item.title, path, EditLocation::RemoteDraft, flags);
            let end = run_remote_spec_edit_with_overlays(
                engines,
                session,
                RemoteSpecEdit {
                    agent: &agent,
                    run_opts,
                    credentials: Some(credentials),
                    source,
                    mode: DraftSyncMode::Update { issue },
                    seed: seed.clone(),
                    prompt: &render,
                    label: LABEL,
                },
                cli_overlays,
                command.cancel(),
                frontend,
            )
            .await?;
            // The shared runner allocates the UUID and reports the host path
            // before PTY ownership. Its returned path identifies that same draft,
            // even when successful finalization has already removed the file.
            let container_path = format!(
                "/awman/context/repo/scratch/work-items/{}",
                end.draft_host_path
                    .file_name()
                    .expect("generated draft filename")
                    .to_string_lossy()
            );
            let target = EditTarget {
                kind: EditKind::Spec,
                identity,
                scope: EditScope::Remote { provider },
                persistence: EditPersistence::RemoteDraft {
                    draft_path: end.draft_host_path.clone(),
                    issue_url: url.clone(),
                },
                files: vec![EditFile {
                    host_path: end.draft_host_path.clone(),
                    container_path,
                    role: EditFileRole::Primary,
                }],
                bundle_dir: None,
            };
            let save = end
                .sync
                .map(|sync| remote_summary(sync, &seed, &url))
                .map_err(|error| SaveFailure {
                    message: error.to_string(),
                    retained: vec![end.draft_host_path],
                    published: vec![],
                });
            let execution = end
                .execution
                .map(|info| info.exit_code)
                .map_err(|error| format!("failed to wait for the agent: {error}"));
            conclude(LABEL, target, flags, execution, save, frontend)
        }
    }
}

fn prompt(title: &str, path: &str, location: EditLocation, flags: &EditCommonFlags) -> String {
    render_edit_prompt(&EditPromptRequest {
        subject: EditSubject::Spec { title },
        name: title,
        primary_path: path,
        companion_paths: &[],
        bundle_dir: None,
        location,
        global: false,
        interview: flags.interview,
        amend: flags.amend,
        headless: flags.headless,
        brief: flags.prompt.as_deref(),
    })
}

fn target_error(frontend: &mut dyn EditCommandFrontend, message: String) -> CommandError {
    frontend.write_message(UserMessage {
        level: MessageLevel::Error,
        text: format!("{LABEL}: {message}"),
    });
    EditError::Target(message).into()
}

/// The resolver returns a canonical host path. SafeTree holds directory
/// descriptors without following swapped symlinks and bounds every read.
fn read_local_spec(path: &Path) -> Result<(String, String), String> {
    let read = || {
        let root = path.parent().ok_or("work item path has no parent")?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("work item filename is not UTF-8")?;
        let relative = ContextRelPath::parse(name).map_err(|e| e.to_string())?;
        let (bytes, meta) = SafeTree::new(root)
            .read(&relative, MAX_WORK_ITEM_BYTES)
            .map_err(|e| e.to_string())?;
        let content = String::from_utf8(bytes).map_err(|_| "work item is not valid UTF-8")?;
        Ok((content, meta.sha256))
    };
    read().map_err(|reason: String| format!("cannot read work item {}: {reason}", path.display()))
}

fn check_local_spec(path: &Path, baseline: &str) -> Result<SaveSummary, SaveFailure> {
    let failure = |message, changed| SaveFailure {
        message: format!(
            "{message}; in-place edits were not rolled back ({})",
            path.display()
        ),
        retained: vec![],
        published: if changed {
            vec![path.to_path_buf()]
        } else {
            vec![]
        },
    };
    let (content, hash) = read_local_spec(path).map_err(|message| failure(message, true))?;
    let changed = hash != baseline;
    if content.trim().is_empty() {
        return Err(failure("work item must not be blank".into(), changed));
    }
    Ok(SaveSummary {
        changed: if changed {
            vec![path.display().to_string()]
        } else {
            vec![]
        },
        notices: vec![],
    })
}

fn remote_summary(sync: FinalDraftSyncOutcome, seed: &str, url: &str) -> SaveSummary {
    // `uploaded` only describes final_upload. The acknowledged issue also
    // accounts for periodic uploads that left nothing for finalization to do.
    let changed = sync.issue.as_ref().is_some_and(|issue| issue.body != seed);
    SaveSummary {
        changed: if changed {
            vec![url.to_string()]
        } else {
            vec![]
        },
        notices: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awman_test_support::{TestCase, TestSandbox};

    fn sandbox(name: &'static str) -> TestSandbox {
        // This module is covered by the registered component-core-internals
        // suite. Its filesystem tests use private, owned state only.
        TestSandbox::for_test(TestCase::component(name))
    }

    #[test]
    fn local_noop_and_same_size_rename_are_distinguished() {
        let sandbox = sandbox(concat!(
            module_path!(),
            "::local_noop_and_same_size_rename_are_distinguished"
        ));
        let path = sandbox.work().canonicalize().unwrap().join("item.md");
        std::fs::write(&path, "original").unwrap();
        let (_, baseline) = read_local_spec(&path).unwrap();
        assert!(check_local_spec(&path, &baseline)
            .unwrap()
            .changed
            .is_empty());
        let replacement = path.with_extension("tmp");
        std::fs::write(&replacement, "modified").unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert_eq!(
            check_local_spec(&path, &baseline).unwrap().changed,
            vec![path.display().to_string()]
        );
    }

    #[test]
    fn invalid_local_edits_are_reported_and_left_for_recovery() {
        let sandbox = sandbox(concat!(
            module_path!(),
            "::invalid_local_edits_are_reported_and_left_for_recovery"
        ));
        let path = sandbox.work().canonicalize().unwrap().join("item.md");
        std::fs::write(&path, "original").unwrap();
        let (_, baseline) = read_local_spec(&path).unwrap();
        for bytes in [
            b" \n\t".to_vec(),
            vec![0xff],
            vec![b'x'; MAX_WORK_ITEM_BYTES as usize + 1],
        ] {
            std::fs::write(&path, &bytes).unwrap();
            let failure = check_local_spec(&path, &baseline).unwrap_err();
            assert!(failure.message.contains(&path.display().to_string()));
            assert_eq!(
                std::fs::read(&path).unwrap(),
                bytes,
                "validation must not roll back edits"
            );
        }
        std::fs::remove_file(&path).unwrap();
        assert!(check_local_spec(&path, &baseline).is_err());
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn replaced_local_file_or_parent_symlinks_are_not_followed() {
        let sandbox = sandbox(concat!(
            module_path!(),
            "::replaced_local_file_or_parent_symlinks_are_not_followed"
        ));
        let root = sandbox.work().canonicalize().unwrap();
        let dir = root.join("specs");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("item.md");
        std::fs::write(&path, "original").unwrap();
        let (_, baseline) = read_local_spec(&path).unwrap();
        let other = root.join("other.md");
        std::fs::write(&other, "unrelated").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&other, &path).unwrap();
        assert!(check_local_spec(&path, &baseline).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
        std::os::unix::fs::symlink(&root, &dir).unwrap();
        assert!(read_local_spec(&dir.join("other.md")).is_err());
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "unrelated");
    }

    #[test]
    fn remote_periodic_save_counts_when_finalization_is_a_noop() {
        let url = "https://example.test/items/42";
        for (body, changed) in [("original", false), ("modified", true)] {
            let summary = remote_summary(
                FinalDraftSyncOutcome {
                    issue: Some(crate::data::issue::Issue {
                        source_id: url.into(),
                        id: "42".into(),
                        number: Some(42),
                        title: "Title".into(),
                        body: body.into(),
                        provider: "http:test".into(),
                    }),
                    uploaded: false,
                },
                "original",
                url,
            );
            assert_eq!(
                summary.changed,
                if changed {
                    vec![url.to_string()]
                } else {
                    vec![]
                }
            );
        }
    }
}
