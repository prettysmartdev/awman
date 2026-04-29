use crate::cli::Agent;
use crate::commands::auth::resolve_auth;
use crate::commands::download;
use crate::commands::output::OutputSink;
use crate::commands::ready::{audit_entrypoint, print_interactive_notice, StepStatus};
use crate::config::{load_repo_config, save_repo_config, WorkItemsConfig};
use crate::runtime::{agent_image_tag, format_build_cmd, generate_container_name, project_image_tag, AgentRuntime};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ─── Traits ───────────────────────────────────────────────────────────────────

/// All Q&A interactions the init flow needs from the caller.
///
/// CLI implements these with stdin prompts; TUI implements them by returning
/// pre-collected answers from modal dialogs.
pub trait InitQa {
    fn ask_replace_aspec(&mut self) -> Result<bool>;
    fn ask_run_audit(&mut self) -> Result<bool>;
    /// Called only when the work-items setup offer is applicable.
    /// Return `None` to skip setup; return `Some(config)` to configure.
    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>>;
}

/// Container build/run operations delegated to the caller.
///
/// CLI blocks synchronously; TUI blocks inside the spawned task thread.
pub trait InitContainerLauncher {
    fn build_image(
        &self,
        tag: &str,
        dockerfile: &Path,
        context: &Path,
        sink: &OutputSink,
    ) -> Result<()>;
    fn run_audit(&self, agent: Agent, cwd: &Path, sink: &OutputSink) -> Result<()>;
}

// ─── Params / Summary ─────────────────────────────────────────────────────────

/// Parameters for the init flow.
pub struct InitParams {
    pub agent: Agent,
    pub aspec: bool,
    pub git_root: PathBuf,
}

/// Summary of what happened during `amux init`.
#[derive(Clone, Debug)]
pub struct InitSummary {
    pub config: StepStatus,
    pub aspec_folder: StepStatus,
    pub dockerfile: StepStatus,
    pub audit: StepStatus,
    pub image_build: StepStatus,
    pub work_items_setup: StepStatus,
}

impl Default for InitSummary {
    fn default() -> Self {
        Self {
            config: StepStatus::Pending,
            aspec_folder: StepStatus::Pending,
            dockerfile: StepStatus::Pending,
            audit: StepStatus::Pending,
            image_build: StepStatus::Pending,
            work_items_setup: StepStatus::Pending,
        }
    }
}

// ─── CLI adapters ─────────────────────────────────────────────────────────────

/// Q&A implementation for CLI mode — uses `OutputSink` for I/O so tests can
/// inject mock answers via `OutputSink::MockInput`.
pub struct CliInitQa {
    git_root: PathBuf,
    out: OutputSink,
}

impl CliInitQa {
    pub fn new(git_root: &Path, out: OutputSink) -> Self {
        Self {
            git_root: git_root.to_path_buf(),
            out,
        }
    }
}

impl InitQa for CliInitQa {
    fn ask_replace_aspec(&mut self) -> Result<bool> {
        let aspec_dir = self.git_root.join("aspec");
        self.out
            .println(format!("aspec folder already exists at: {}", aspec_dir.display()));
        Ok(self
            .out
            .ask_yes_no("Replace existing aspec folder with fresh templates?"))
    }

    fn ask_run_audit(&mut self) -> Result<bool> {
        let dockerfile_path = self.git_root.join("Dockerfile.dev");
        if dockerfile_path.exists() {
            self.out.println(format!(
                "Dockerfile.dev already exists at: {}",
                dockerfile_path.display()
            ));
            self.out.println(
                "\nThe agent audit container will scan your project and update Dockerfile.dev"
                    .to_string(),
            );
            self.out.println(
                "to ensure all tools needed to build, run, and test your project are installed."
                    .to_string(),
            );
            Ok(self.out.ask_yes_no("Run the agent audit container now?"))
        } else {
            self.out
                .println("No Dockerfile.dev found — a default template will be downloaded.".to_string());
            self.out.println(
                "\nThe agent audit container will scan your project and update Dockerfile.dev"
                    .to_string(),
            );
            self.out.println(
                "to ensure all tools needed to build, run, and test your project are installed."
                    .to_string(),
            );
            Ok(self
                .out
                .ask_yes_no("Run the agent audit container after creating Dockerfile.dev?"))
        }
    }

    fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>> {
        let do_setup = self
            .out
            .ask_yes_no("Would you like to configure a work items directory?");
        if !do_setup {
            return Ok(None);
        }

        self.out
            .print("Work items directory path (relative to repo root): ");
        let dir_input = self.out.read_line().trim().to_string();
        if dir_input.is_empty() {
            return Ok(None);
        }

        self.out
            .print("Work item template path (leave blank to skip): ");
        let tmpl_input = self.out.read_line().trim().to_string();
        let template = if tmpl_input.is_empty() {
            None
        } else {
            Some(tmpl_input)
        };

        Ok(Some(WorkItemsConfig {
            dir: Some(dir_input),
            template,
        }))
    }
}

/// Container launcher for CLI mode — blocking synchronous calls.
pub struct CliContainerLauncher {
    runtime: Arc<dyn AgentRuntime>,
}

impl CliContainerLauncher {
    pub fn new(runtime: Arc<dyn AgentRuntime>) -> Self {
        Self { runtime }
    }
}

impl InitContainerLauncher for CliContainerLauncher {
    fn build_image(
        &self,
        tag: &str,
        dockerfile: &Path,
        context: &Path,
        sink: &OutputSink,
    ) -> Result<()> {
        let build_cmd = format_build_cmd(
            self.runtime.cli_binary(),
            tag,
            dockerfile.to_str().unwrap_or(""),
            context.to_str().unwrap_or(""),
        );
        sink.println(format!("$ {}", build_cmd));
        let sink_clone = sink.clone();
        self.runtime
            .build_image_streaming(tag, dockerfile, context, false, &mut |line| {
                sink_clone.println(line);
            })
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    fn run_audit(&self, agent: Agent, cwd: &Path, sink: &OutputSink) -> Result<()> {
        let git_root = cwd;
        let agent_img = agent_image_tag(git_root, agent.as_str());
        let agent_df_path = git_root
            .join(".amux")
            .join(format!("Dockerfile.{}", agent.as_str()));
        let mount_path = git_root.to_str().unwrap_or("").to_string();

        let credentials = resolve_auth(git_root, agent.as_str()).unwrap_or_default();
        let mut env_vars = credentials.env_vars;
        let passthrough_names = crate::config::effective_env_passthrough(git_root);
        for name in &passthrough_names {
            if env_vars.iter().any(|(k, _)| k == name) {
                continue;
            }
            if let Ok(val) = std::env::var(name) {
                env_vars.push((name.clone(), val));
            }
        }
        let host_settings =
            crate::passthrough::passthrough_for_agent(agent.as_str()).prepare_host_settings();

        print_interactive_notice(sink, agent.as_str());
        let entrypoint = audit_entrypoint(agent.as_str());
        let entrypoint_refs: Vec<&str> = entrypoint.iter().map(String::as_str).collect();
        let container_name = generate_container_name();

        let modified_settings: Option<crate::runtime::HostSettings> =
            host_settings.as_ref().and_then(|settings| {
                let mut new_settings = settings.clone_view();
                if let Some(msg) =
                    crate::runtime::apply_dockerfile_user(&mut new_settings, &agent_df_path)
                {
                    sink.println(msg);
                    Some(new_settings)
                } else {
                    None
                }
            });
        let effective_settings: Option<&crate::runtime::HostSettings> =
            modified_settings.as_ref().or(host_settings.as_ref());

        self.runtime
            .run_container(
                &agent_img,
                &mount_path,
                &entrypoint_refs,
                &env_vars,
                effective_settings,
                false,
                Some(&container_name),
                None,
            )
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
}

// ─── TUI Phase-Split Types ────────────────────────────────────────────────────

/// Context produced by `execute_init_pre_audit`. Carries everything the TUI needs to:
/// 1. Launch the PTY audit container (image, entrypoint, credentials, host settings)
/// 2. Run the post-audit image rebuild and work-items setup
pub struct InitAuditHandoff {
    pub agent: crate::cli::Agent,
    pub git_root: PathBuf,
    pub image_tag: String,
    pub agent_image_tag: String,
    pub aspec: bool,
    pub summary: InitSummary,
    pub env_vars: Vec<(String, String)>,
    pub host_settings: Option<crate::runtime::HostSettings>,
    pub runtime: Arc<dyn AgentRuntime>,
    /// Pre-collected work-items setup answer from the TUI dialog.
    /// `None` if work-items setup was not offered or was declined.
    pub work_items: Option<crate::config::WorkItemsConfig>,
}

/// Result of `execute_init_pre_audit`.
pub enum InitPreAuditResult {
    /// No audit required (or runtime unavailable); summary has already been printed.
    Done { summary: InitSummary },
    /// An audit should be run; all context is in the handoff.
    NeedsAudit(InitAuditHandoff),
}

// ─── execute_init_pre_audit / execute_init_post_audit ─────────────────────────

/// Run the pre-audit phase of the init flow (stages 1-7b).
///
/// Writes config, Dockerfiles, checks the runtime, and builds the project/agent
/// images. Returns `NeedsAudit` when images were successfully built and the
/// caller should launch the PTY audit container. Returns `Done` for all other
/// paths (user declined, runtime unavailable, build failures).
///
/// `replace_aspec`, `run_audit`, and `work_items` are pre-collected TUI dialog
/// answers; the CLI's `execute()` calls this via the trait-based `qa` path instead.
pub async fn execute_init_pre_audit<L>(
    params: InitParams,
    replace_aspec: bool,
    run_audit: bool,
    work_items: Option<crate::config::WorkItemsConfig>,
    sink: &OutputSink,
    launcher: &L,
    runtime: Arc<dyn AgentRuntime>,
) -> Result<InitPreAuditResult>
where
    L: InitContainerLauncher,
{
    let git_root = params.git_root.clone();
    let agent = params.agent;
    let aspec = params.aspec;
    let mut summary = InitSummary::default();

    sink.println(format!("Initializing amux in: {}", git_root.display()));
    sink.println(format!("Agent: {}", agent.as_str()));

    // ── Stage 2: Load and update repo config ─────────────────────────────────
    let mut config = load_repo_config(&git_root).unwrap_or_default();
    config.agent = Some(agent.as_str().to_string());
    save_repo_config(&git_root, &config)?;
    sink.println(format!(
        "Config written to: {}",
        git_root.join(".amux/config.json").display()
    ));
    summary.config = StepStatus::Ok("saved".into());

    // ── Stage 3: Download or skip aspec folder ───────────────────────────────
    let aspec_dir = git_root.join("aspec");
    if aspec {
        if !aspec_dir.exists() || replace_aspec {
            match download::download_aspec_folder(&git_root, sink).await {
                Ok(()) => {
                    summary.aspec_folder = StepStatus::Ok("downloaded".into());
                }
                Err(e) => {
                    sink.println(format!(
                        "Warning: failed to download aspec folder from GitHub: {}",
                        e
                    ));
                    sink.println(
                        "You can manually download it from https://github.com/cohix/aspec"
                            .to_string(),
                    );
                    summary.aspec_folder = StepStatus::Failed("download failed".into());
                }
            }
        } else {
            sink.println(format!(
                "aspec folder already exists at: {} (keeping existing)",
                aspec_dir.display()
            ));
            summary.aspec_folder = StepStatus::Ok("already exists".into());
        }
    } else if aspec_dir.exists() {
        summary.aspec_folder = StepStatus::Ok("already exists".into());
    } else {
        summary.aspec_folder = StepStatus::Skipped("use --aspec to download".into());
    }

    // ── Stage 4: Write Dockerfile.dev ────────────────────────────────────────
    let dockerfile_was_new = write_project_dockerfile(&git_root, sink).await?;
    if dockerfile_was_new {
        sink.println(format!(
            "Dockerfile.dev written to: {}",
            git_root.join("Dockerfile.dev").display()
        ));
        summary.dockerfile = StepStatus::Ok("created".into());
    } else {
        sink.println(format!(
            "Dockerfile.dev already exists at: {} (not overwritten)",
            git_root.join("Dockerfile.dev").display()
        ));
        summary.dockerfile = StepStatus::Ok("already exists".into());
    }

    // ── Stage 5: Write .amux/Dockerfile.{agent} ──────────────────────────────
    let agent_dockerfile_was_new = write_agent_dockerfile(&git_root, &agent, sink).await?;

    // ── Stages 6-7b: Runtime check + build both images ───────────────────────
    let image_tag = project_image_tag(&git_root);
    let agent_image_tag_val = agent_image_tag(&git_root, agent.as_str());
    let dockerfile_path = git_root.join("Dockerfile.dev");
    let agent_df_path = git_root
        .join(".amux")
        .join(format!("Dockerfile.{}", agent.as_str()));

    if run_audit {
        // Stage 6: Check runtime availability
        sink.print(format!("Checking {} runtime... ", runtime.name()));
        if !runtime.is_available() {
            sink.println("FAILED".to_string());
            sink.println(format!(
                "{} runtime is not running. Skipping audit and image build.",
                runtime.name()
            ));
            summary.audit = StepStatus::Failed(format!("{} not running", runtime.name()));
            summary.image_build =
                StepStatus::Failed(format!("{} not running", runtime.name()));
            summary.work_items_setup = StepStatus::Skipped("runtime not running".into());
            print_init_summary(sink, &summary, agent.as_str());
            print_whats_next(sink);
            return Ok(InitPreAuditResult::Done { summary });
        }
        sink.println("OK".to_string());

        // Stage 7a: Build project base image before audit.
        sink.println(format!("Building image {}...", image_tag));
        match launcher.build_image(&image_tag, &dockerfile_path, &git_root, sink) {
            Ok(()) => {
                sink.println(format!("Image {} built successfully.", image_tag));
            }
            Err(e) => {
                sink.println(format!("Warning: failed to build image: {}", e));
                summary.audit = StepStatus::Failed("image build failed before audit".into());
                summary.image_build = StepStatus::Failed("build failed".into());
                summary.work_items_setup = StepStatus::Skipped("build failed".into());
                print_init_summary(sink, &summary, agent.as_str());
                print_whats_next(sink);
                return Ok(InitPreAuditResult::Done { summary });
            }
        }

        // Stage 7b: Build agent image before audit.
        sink.println(format!("Building agent image {}...", agent_image_tag_val));
        match launcher.build_image(&agent_image_tag_val, &agent_df_path, &git_root, sink) {
            Ok(()) => {
                sink.println(format!(
                    "Agent image {} built successfully.",
                    agent_image_tag_val
                ));
            }
            Err(e) => {
                sink.println(format!("Warning: failed to build agent image: {}", e));
                summary.audit =
                    StepStatus::Failed("agent image build failed before audit".into());
                summary.image_build = StepStatus::Failed("agent build failed".into());
                summary.work_items_setup = StepStatus::Skipped("build failed".into());
                print_init_summary(sink, &summary, agent.as_str());
                print_whats_next(sink);
                return Ok(InitPreAuditResult::Done { summary });
            }
        }

        // Both images built — gather credentials and host settings for the PTY audit.
        let credentials = resolve_auth(&git_root, agent.as_str()).unwrap_or_default();
        let mut env_vars = credentials.env_vars;
        let passthrough_names = crate::config::effective_env_passthrough(&git_root);
        for name in &passthrough_names {
            if env_vars.iter().any(|(k, _)| k == name) {
                continue;
            }
            if let Ok(val) = std::env::var(name) {
                env_vars.push((name.clone(), val));
            }
        }
        let mut host_settings =
            crate::passthrough::passthrough_for_agent(agent.as_str()).prepare_host_settings();

        // Apply USER directive so settings files mount at the correct home directory.
        if let Some(ref mut settings) = host_settings {
            if let Some(msg) = crate::runtime::apply_dockerfile_user(settings, &agent_df_path) {
                sink.println(msg);
            }
        }

        return Ok(InitPreAuditResult::NeedsAudit(InitAuditHandoff {
            agent,
            git_root,
            image_tag,
            agent_image_tag: agent_image_tag_val,
            aspec,
            summary,
            env_vars,
            host_settings,
            runtime,
            work_items,
        }));
    }

    // ── No-audit paths (stages 8-9) ───────────────────────────────────────────
    if dockerfile_was_new || agent_dockerfile_was_new {
        // Stage 8: New Dockerfiles, no audit — build both images.
        sink.print(format!("Checking {} runtime... ", runtime.name()));
        if !runtime.is_available() {
            sink.println("not running (skipping image build)".to_string());
            summary.audit = StepStatus::Skipped("declined".into());
            summary.image_build =
                StepStatus::Skipped(format!("{} not running", runtime.name()));
        } else {
            sink.println("OK".to_string());

            sink.println(format!("Building image {}...", image_tag));
            match launcher.build_image(&image_tag, &dockerfile_path, &git_root, sink) {
                Ok(()) => {
                    sink.println(format!("Image {} built successfully.", image_tag));

                    sink.println(format!("Building agent image {}...", agent_image_tag_val));
                    match launcher.build_image(
                        &agent_image_tag_val,
                        &agent_df_path,
                        &git_root,
                        sink,
                    ) {
                        Ok(()) => {
                            sink.println(format!(
                                "Agent image {} built successfully.",
                                agent_image_tag_val
                            ));
                            summary.audit = StepStatus::Skipped("declined".into());
                            summary.image_build = StepStatus::Ok("built".into());
                        }
                        Err(e) => {
                            sink.println(format!(
                                "Warning: failed to build agent image: {}",
                                e
                            ));
                            summary.audit = StepStatus::Skipped("declined".into());
                            summary.image_build =
                                StepStatus::Failed("agent build failed".into());
                        }
                    }
                }
                Err(e) => {
                    sink.println(format!("Warning: failed to build image: {}", e));
                    summary.audit = StepStatus::Skipped("declined".into());
                    summary.image_build = StepStatus::Failed("build failed".into());
                }
            }
        }
    } else {
        // Existing Dockerfiles, user declined audit — skip build.
        summary.audit = StepStatus::Skipped("declined".into());
        summary.image_build = StepStatus::Skipped("no changes".into());
    }

    // Stage 9: Work items setup (no audit path — use pre-collected answer).
    run_init_work_items_setup(&git_root, aspec, work_items, sink, &mut summary)?;

    print_init_summary(sink, &summary, agent.as_str());
    print_whats_next(sink);
    Ok(InitPreAuditResult::Done { summary })
}

/// Run the post-audit phase of the init flow (stages 7d-9).
///
/// Called by the TUI after the PTY audit container exits. Rebuilds images,
/// runs work-items setup, and prints the final summary.
/// `audit_exit_code = 0` means the audit succeeded; non-zero is treated as a
/// warning (the audit may still have modified Dockerfile.dev).
pub async fn execute_init_post_audit<L>(
    sink: &OutputSink,
    mut handoff: InitAuditHandoff,
    audit_exit_code: i32,
    launcher: &L,
) -> Result<InitSummary>
where
    L: InitContainerLauncher,
{
    let image_tag = &handoff.image_tag;
    let agent_image_tag_val = &handoff.agent_image_tag;
    let dockerfile_path = handoff.git_root.join("Dockerfile.dev");
    let agent_df_path = handoff
        .git_root
        .join(".amux")
        .join(format!("Dockerfile.{}", handoff.agent.as_str()));

    if audit_exit_code == 0 {
        handoff.summary.audit = StepStatus::Ok("completed".into());
    } else {
        handoff.summary.audit =
            StepStatus::Failed(format!("agent exited with code {}", audit_exit_code));
    }

    // Stage 7d: Rebuild project base after audit.
    sink.println(format!("Rebuilding image {} after audit...", image_tag));
    match launcher.build_image(image_tag, &dockerfile_path, &handoff.git_root, sink) {
        Ok(()) => {
            sink.println(format!("Image {} rebuilt successfully.", image_tag));
        }
        Err(e) => {
            sink.println(format!("Warning: failed to rebuild image: {}", e));
            handoff.summary.image_build = StepStatus::Failed("rebuild failed".into());
            handoff.summary.work_items_setup = StepStatus::Skipped("build failed".into());
            print_init_summary(sink, &handoff.summary, handoff.agent.as_str());
            print_whats_next(sink);
            return Ok(handoff.summary);
        }
    }

    // Stage 7e: Rebuild agent image after audit.
    sink.println(format!(
        "Rebuilding agent image {} after audit...",
        agent_image_tag_val
    ));
    match launcher.build_image(
        agent_image_tag_val,
        &agent_df_path,
        &handoff.git_root,
        sink,
    ) {
        Ok(()) => {
            sink.println(format!(
                "Agent image {} rebuilt successfully.",
                agent_image_tag_val
            ));
            handoff.summary.image_build = StepStatus::Ok("built".into());
        }
        Err(e) => {
            sink.println(format!("Warning: failed to rebuild agent image: {}", e));
            handoff.summary.image_build = StepStatus::Failed("agent rebuild failed".into());
        }
    }

    // Stage 9: Work items setup.
    run_init_work_items_setup(
        &handoff.git_root,
        handoff.aspec,
        handoff.work_items.take(),
        sink,
        &mut handoff.summary,
    )?;

    print_init_summary(sink, &handoff.summary, handoff.agent.as_str());
    print_whats_next(sink);
    Ok(handoff.summary)
}

/// Process the work-items setup step (stage 9) using a pre-collected answer.
fn run_init_work_items_setup(
    git_root: &Path,
    aspec: bool,
    work_items: Option<crate::config::WorkItemsConfig>,
    sink: &OutputSink,
    summary: &mut InitSummary,
) -> Result<()> {
    let current_config = load_repo_config(git_root).unwrap_or_default();
    let work_items_already_set = current_config
        .work_items
        .as_ref()
        .and_then(|w| w.dir.as_deref())
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    let aspec_dir_now = git_root.join("aspec");
    if !aspec && !aspec_dir_now.exists() && !work_items_already_set {
        match work_items {
            None => {
                summary.work_items_setup = StepStatus::Skipped("declined".into());
            }
            Some(wi_config) => {
                let dir = wi_config.dir.as_deref().unwrap_or("").to_string();
                if dir.is_empty() {
                    summary.work_items_setup =
                        StepStatus::Skipped("no path provided".into());
                } else {
                    match crate::commands::config::validate_path_within_git_root(&dir, git_root) {
                        Err(e) => {
                            sink.println(format!(
                                "Invalid path: {}. Skipping work items setup.",
                                e
                            ));
                            summary.work_items_setup =
                                StepStatus::Failed("invalid path".into());
                        }
                        Ok(()) => {
                            let mut updated = load_repo_config(git_root).unwrap_or_default();
                            let wi = updated.work_items.get_or_insert_with(WorkItemsConfig::default);
                            wi.dir = Some(dir.clone());
                            if let Some(tmpl) = &wi_config.template {
                                if !tmpl.is_empty() {
                                    match crate::commands::config::validate_path_within_git_root(
                                        tmpl, git_root,
                                    ) {
                                        Ok(()) => {
                                            wi.template = Some(tmpl.clone());
                                        }
                                        Err(e) => {
                                            sink.println(format!(
                                                "Invalid template path: {}. Skipping template.",
                                                e
                                            ));
                                        }
                                    }
                                }
                            }
                            save_repo_config(git_root, &updated)?;
                            sink.println(format!(
                                "Work items directory configured: {}",
                                dir
                            ));
                            summary.work_items_setup = StepStatus::Ok("configured".into());
                        }
                    }
                }
            }
        }
    } else {
        summary.work_items_setup = StepStatus::Skipped("not needed".into());
    }
    Ok(())
}

// ─── execute() ────────────────────────────────────────────────────────────────

/// Run the full init flow.
///
/// All business logic lives here; CLI and TUI differ only through their `qa` and
/// `launcher` implementations. The `runtime` is used for availability checks
/// (stages 6-8); container operations are delegated to `launcher`.
pub async fn execute<Q, L>(
    params: InitParams,
    qa: &mut Q,
    launcher: &L,
    sink: &OutputSink,
    runtime: Arc<dyn AgentRuntime>,
) -> Result<InitSummary>
where
    Q: InitQa,
    L: InitContainerLauncher,
{
    let git_root = &params.git_root;
    let agent = params.agent;
    let aspec = params.aspec;
    let mut summary = InitSummary::default();

    sink.println(format!("Initializing amux in: {}", git_root.display()));
    sink.println(format!("Agent: {}", agent.as_str()));

    // ── Stage 1: Collect Q&A ─────────────────────────────────────────────────
    let replace_aspec = if aspec && git_root.join("aspec").exists() {
        qa.ask_replace_aspec()?
    } else {
        false
    };
    let run_audit = qa.ask_run_audit()?;

    // ── Stage 2: Load and update repo config ─────────────────────────────────
    let mut config = load_repo_config(git_root).unwrap_or_default();
    config.agent = Some(agent.as_str().to_string());
    save_repo_config(git_root, &config)?;
    sink.println(format!(
        "Config written to: {}",
        git_root.join(".amux/config.json").display()
    ));
    summary.config = StepStatus::Ok("saved".into());

    // ── Stage 3: Download or skip aspec folder ───────────────────────────────
    let aspec_dir = git_root.join("aspec");
    if aspec {
        if !aspec_dir.exists() || replace_aspec {
            match download::download_aspec_folder(git_root, sink).await {
                Ok(()) => {
                    summary.aspec_folder = StepStatus::Ok("downloaded".into());
                }
                Err(e) => {
                    sink.println(format!(
                        "Warning: failed to download aspec folder from GitHub: {}",
                        e
                    ));
                    sink.println(
                        "You can manually download it from https://github.com/cohix/aspec"
                            .to_string(),
                    );
                    summary.aspec_folder = StepStatus::Failed("download failed".into());
                }
            }
        } else {
            sink.println(format!(
                "aspec folder already exists at: {} (keeping existing)",
                aspec_dir.display()
            ));
            summary.aspec_folder = StepStatus::Ok("already exists".into());
        }
    } else if aspec_dir.exists() {
        summary.aspec_folder = StepStatus::Ok("already exists".into());
    } else {
        summary.aspec_folder = StepStatus::Skipped("use --aspec to download".into());
    }

    // ── Stage 4: Write Dockerfile.dev ────────────────────────────────────────
    let dockerfile_was_new = write_project_dockerfile(git_root, sink).await?;
    if dockerfile_was_new {
        sink.println(format!(
            "Dockerfile.dev written to: {}",
            git_root.join("Dockerfile.dev").display()
        ));
        summary.dockerfile = StepStatus::Ok("created".into());
    } else {
        sink.println(format!(
            "Dockerfile.dev already exists at: {} (not overwritten)",
            git_root.join("Dockerfile.dev").display()
        ));
        summary.dockerfile = StepStatus::Ok("already exists".into());
    }

    // ── Stage 5: Write .amux/Dockerfile.{agent} ──────────────────────────────
    let agent_dockerfile_was_new = write_agent_dockerfile(git_root, &agent, sink).await?;

    // ── Stages 6-8: Container runtime check and image builds ─────────────────
    let image_tag = project_image_tag(git_root);
    let agent_image_tag_val = agent_image_tag(git_root, agent.as_str());
    let dockerfile_path = git_root.join("Dockerfile.dev");
    let agent_df_path = git_root
        .join(".amux")
        .join(format!("Dockerfile.{}", agent.as_str()));

    if run_audit {
        // Stage 6: Check runtime availability
        sink.print(format!("Checking {} runtime... ", runtime.name()));
        if !runtime.is_available() {
            sink.println("FAILED".to_string());
            sink.println(format!(
                "{} runtime is not running. Skipping audit and image build.",
                runtime.name()
            ));
            summary.audit = StepStatus::Failed(format!("{} not running", runtime.name()));
            summary.image_build =
                StepStatus::Failed(format!("{} not running", runtime.name()));
        } else {
            sink.println("OK".to_string());

            // Stage 7a: Build project base image before audit.
            sink.println(format!("Building image {}...", image_tag));
            match launcher.build_image(&image_tag, &dockerfile_path, git_root, sink) {
                Ok(()) => {
                    sink.println(format!("Image {} built successfully.", image_tag));
                }
                Err(e) => {
                    sink.println(format!("Warning: failed to build image: {}", e));
                    summary.audit =
                        StepStatus::Failed("image build failed before audit".into());
                    summary.image_build = StepStatus::Failed("build failed".into());
                    summary.work_items_setup = StepStatus::Skipped("build failed".into());
                    print_init_summary(sink, &summary, agent.as_str());
                    print_whats_next(sink);
                    return Ok(summary);
                }
            }

            // Stage 7b: Build agent image before audit.
            sink.println(format!("Building agent image {}...", agent_image_tag_val));
            match launcher.build_image(&agent_image_tag_val, &agent_df_path, git_root, sink) {
                Ok(()) => {
                    sink.println(format!(
                        "Agent image {} built successfully.",
                        agent_image_tag_val
                    ));
                }
                Err(e) => {
                    sink.println(format!("Warning: failed to build agent image: {}", e));
                    summary.audit = StepStatus::Failed(
                        "agent image build failed before audit".into(),
                    );
                    summary.image_build = StepStatus::Failed("agent build failed".into());
                    summary.work_items_setup = StepStatus::Skipped("build failed".into());
                    print_init_summary(sink, &summary, agent.as_str());
                    print_whats_next(sink);
                    return Ok(summary);
                }
            }

            // Stage 7c: Run the audit container.
            match launcher.run_audit(agent.clone(), git_root, sink) {
                Ok(()) => {
                    summary.audit = StepStatus::Ok("completed".into());
                }
                Err(e) => {
                    sink.println(format!("Warning: audit container failed: {}", e));
                    summary.audit = StepStatus::Failed("container error".into());
                }
            }

            // Stage 7d: Rebuild project base after audit (audit may modify Dockerfile.dev).
            sink.println(format!("Rebuilding image {} after audit...", image_tag));
            match launcher.build_image(&image_tag, &dockerfile_path, git_root, sink) {
                Ok(()) => {
                    sink.println(format!("Image {} rebuilt successfully.", image_tag));
                }
                Err(e) => {
                    sink.println(format!("Warning: failed to rebuild image: {}", e));
                    summary.image_build = StepStatus::Failed("rebuild failed".into());
                    summary.work_items_setup = StepStatus::Skipped("build failed".into());
                    print_init_summary(sink, &summary, agent.as_str());
                    print_whats_next(sink);
                    return Ok(summary);
                }
            }

            // Stage 7e: Rebuild agent image after audit.
            sink.println(format!(
                "Rebuilding agent image {} after audit...",
                agent_image_tag_val
            ));
            match launcher.build_image(&agent_image_tag_val, &agent_df_path, git_root, sink) {
                Ok(()) => {
                    sink.println(format!(
                        "Agent image {} rebuilt successfully.",
                        agent_image_tag_val
                    ));
                    summary.image_build = StepStatus::Ok("built".into());
                }
                Err(e) => {
                    sink.println(format!("Warning: failed to rebuild agent image: {}", e));
                    summary.image_build = StepStatus::Failed("agent rebuild failed".into());
                }
            }
        }
    } else if dockerfile_was_new || agent_dockerfile_was_new {
        // Stage 8: New Dockerfiles, no audit — build both images.
        sink.print(format!("Checking {} runtime... ", runtime.name()));
        if !runtime.is_available() {
            sink.println("not running (skipping image build)".to_string());
            summary.audit = StepStatus::Skipped("declined".into());
            summary.image_build =
                StepStatus::Skipped(format!("{} not running", runtime.name()));
        } else {
            sink.println("OK".to_string());

            sink.println(format!("Building image {}...", image_tag));
            match launcher.build_image(&image_tag, &dockerfile_path, git_root, sink) {
                Ok(()) => {
                    sink.println(format!("Image {} built successfully.", image_tag));

                    sink.println(format!(
                        "Building agent image {}...",
                        agent_image_tag_val
                    ));
                    match launcher.build_image(
                        &agent_image_tag_val,
                        &agent_df_path,
                        git_root,
                        sink,
                    ) {
                        Ok(()) => {
                            sink.println(format!(
                                "Agent image {} built successfully.",
                                agent_image_tag_val
                            ));
                            summary.audit = StepStatus::Skipped("declined".into());
                            summary.image_build = StepStatus::Ok("built".into());
                        }
                        Err(e) => {
                            sink.println(format!(
                                "Warning: failed to build agent image: {}",
                                e
                            ));
                            summary.audit = StepStatus::Skipped("declined".into());
                            summary.image_build =
                                StepStatus::Failed("agent build failed".into());
                        }
                    }
                }
                Err(e) => {
                    sink.println(format!("Warning: failed to build image: {}", e));
                    summary.audit = StepStatus::Skipped("declined".into());
                    summary.image_build = StepStatus::Failed("build failed".into());
                }
            }
        }
    } else {
        // Existing Dockerfiles, user declined audit — skip build.
        summary.audit = StepStatus::Skipped("declined".into());
        summary.image_build = StepStatus::Skipped("no changes".into());
    }

    // ── Stage 9: Work items setup ─────────────────────────────────────────────
    {
        // Re-read config so we see the current work_items state after any prior saves.
        let current_config = load_repo_config(git_root).unwrap_or_default();
        let work_items_already_set = current_config
            .work_items
            .as_ref()
            .and_then(|w| w.dir.as_deref())
            .map(|s| !s.is_empty())
            .unwrap_or(false);

        // Offer work-items setup when: not using aspec, aspec/ dir absent, and
        // work_items.dir not yet configured.
        let aspec_dir_now = git_root.join("aspec");
        if !aspec && !aspec_dir_now.exists() && !work_items_already_set {
            match qa.ask_work_items_setup()? {
                None => {
                    summary.work_items_setup = StepStatus::Skipped("declined".into());
                }
                Some(wi_config) => {
                    let dir = wi_config.dir.as_deref().unwrap_or("").to_string();
                    if dir.is_empty() {
                        summary.work_items_setup =
                            StepStatus::Skipped("no path provided".into());
                    } else {
                        match crate::commands::config::validate_path_within_git_root(
                            &dir, git_root,
                        ) {
                            Err(e) => {
                                sink.println(format!(
                                    "Invalid path: {}. Skipping work items setup.",
                                    e
                                ));
                                summary.work_items_setup =
                                    StepStatus::Failed("invalid path".into());
                            }
                            Ok(()) => {
                                let mut updated =
                                    load_repo_config(git_root).unwrap_or_default();
                                let wi = updated
                                    .work_items
                                    .get_or_insert_with(WorkItemsConfig::default);
                                wi.dir = Some(dir.clone());
                                if let Some(tmpl) = &wi_config.template {
                                    if !tmpl.is_empty() {
                                        match crate::commands::config::validate_path_within_git_root(
                                            tmpl, git_root,
                                        ) {
                                            Ok(()) => {
                                                wi.template = Some(tmpl.clone());
                                            }
                                            Err(e) => {
                                                sink.println(format!(
                                                    "Invalid template path: {}. Skipping template.",
                                                    e
                                                ));
                                            }
                                        }
                                    }
                                }
                                save_repo_config(git_root, &updated)?;
                                sink.println(format!(
                                    "Work items directory configured: {}",
                                    dir
                                ));
                                summary.work_items_setup = StepStatus::Ok("configured".into());
                            }
                        }
                    }
                }
            }
        } else {
            summary.work_items_setup = StepStatus::Skipped("not needed".into());
        }
    }

    // ── Stage 10: Print summary and "What's Next?" ────────────────────────────
    print_init_summary(sink, &summary, agent.as_str());
    print_whats_next(sink);

    Ok(summary)
}

// ─── Helper functions ─────────────────────────────────────────────────────────

/// Walks upward from the given directory to find the nearest `.git` folder.
pub fn find_git_root_from(cwd: &Path) -> Option<PathBuf> {
    let mut dir = cwd.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Walks upward from CWD to find the nearest directory containing a `.git` folder.
pub fn find_git_root() -> Option<PathBuf> {
    find_git_root_from(&std::env::current_dir().ok()?)
}

/// Write Dockerfile.dev to the git root using the project base template.
/// Returns `true` if a new file was created, `false` if an existing file was preserved.
/// Public so other commands (e.g. ready) can initialize a missing Dockerfile.dev.
pub async fn write_project_dockerfile(git_root: &Path, out: &OutputSink) -> Result<bool> {
    let path = git_root.join("Dockerfile.dev");
    if path.exists() {
        return Ok(false);
    }
    let content = project_dockerfile_embedded();
    std::fs::write(&path, &content)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    out.println(format!("Project Dockerfile.dev written to: {}", path.display()));
    Ok(true)
}

/// Write the agent-specific Dockerfile to `.amux/Dockerfile.{agent}`.
/// Downloads the template from GitHub; falls back to the embedded template.
/// Substitutes the project base image tag into the FROM directive.
/// Returns `true` if a new file was created, `false` if an existing file was preserved.
pub async fn write_agent_dockerfile(
    git_root: &Path,
    agent: &Agent,
    out: &OutputSink,
) -> Result<bool> {
    let amux_dir = git_root.join(".amux");
    std::fs::create_dir_all(&amux_dir)
        .with_context(|| format!("Failed to create directory {}", amux_dir.display()))?;

    let agent_name = agent.as_str();
    let path = amux_dir.join(format!("Dockerfile.{}", agent_name));
    if path.exists() {
        return Ok(false);
    }

    let base_tag = project_image_tag(git_root);
    let template = download_or_fallback_agent_dockerfile(agent, out).await;
    let content = template.replace("{{AMUX_BASE_IMAGE}}", &base_tag);

    std::fs::write(&path, &content)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    out.println(format!("Agent Dockerfile written to: {}", path.display()));
    Ok(true)
}

/// Try to download the agent Dockerfile template from GitHub; fall back to embedded template.
async fn download_or_fallback_agent_dockerfile(agent: &Agent, out: &OutputSink) -> String {
    match download::download_dockerfile_template(agent, out).await {
        Ok(content) => content,
        Err(e) => {
            out.println(format!(
                "Warning: failed to download Dockerfile template from GitHub: {}. Using bundled template.",
                e
            ));
            dockerfile_for_agent_embedded(agent)
        }
    }
}

/// Embedded project base Dockerfile template compiled into the binary.
pub fn project_dockerfile_embedded() -> String {
    include_str!("../../templates/Dockerfile.project").to_string()
}

/// Embedded agent Dockerfile templates compiled into the binary (used as fallback).
/// Templates use `{{AMUX_BASE_IMAGE}}` as a placeholder for the project base image tag.
pub fn dockerfile_for_agent_embedded(agent: &Agent) -> String {
    match agent {
        Agent::Claude => include_str!("../../templates/Dockerfile.claude").to_string(),
        Agent::Codex => include_str!("../../templates/Dockerfile.codex").to_string(),
        Agent::Opencode => include_str!("../../templates/Dockerfile.opencode").to_string(),
        Agent::Maki => include_str!("../../templates/Dockerfile.maki").to_string(),
        Agent::Gemini => include_str!("../../templates/Dockerfile.gemini").to_string(),
        Agent::Copilot => include_str!("../../templates/Dockerfile.copilot").to_string(),
        Agent::Crush => include_str!("../../templates/Dockerfile.crush").to_string(),
        Agent::Cline => include_str!("../../templates/Dockerfile.cline").to_string(),
    }
}

/// Print the init summary table.
fn print_init_summary(out: &OutputSink, summary: &InitSummary, agent_name: &str) {
    out.println(String::new());
    out.println("┌──────────────────────────────────────────────────┐");
    out.println(format!(
        "│              Init Summary ({:>12})         │",
        agent_name
    ));
    out.println("├───────────────────┬──────────────────────────────┤");
    print_init_row(out, "Config", &summary.config);
    print_init_row(out, "aspec folder", &summary.aspec_folder);
    print_init_row(out, "Dockerfile.dev", &summary.dockerfile);
    print_init_row(out, "Agent audit", &summary.audit);
    print_init_row(out, "Docker image", &summary.image_build);
    print_init_row(out, "Work items", &summary.work_items_setup);
    out.println("└───────────────────┴──────────────────────────────┘");
}

fn print_init_row(out: &OutputSink, label: &str, status: &StepStatus) {
    let (symbol, text) = match status {
        StepStatus::Pending => ("-", "pending".to_string()),
        StepStatus::Ok(msg) => ("✓", msg.clone()),
        StepStatus::Skipped(msg) => ("–", msg.clone()),
        StepStatus::Failed(msg) => ("✗", msg.clone()),
        StepStatus::Warn(msg) => ("⚠", msg.clone()),
    };
    out.println(format!("│ {:>17} │ {} {:<27} │", label, symbol, text));
}

/// Returns `text` with each non-space character wrapped in a cycling ANSI rainbow colour.
/// Used only when the sink supports colour output (i.e. stdout terminal).
fn rainbow_text(text: &str) -> String {
    // red, yellow, green, cyan, blue, magenta
    const COLORS: &[&str] = &[
        "\x1b[31m", "\x1b[33m", "\x1b[32m", "\x1b[36m", "\x1b[34m", "\x1b[35m",
    ];
    let mut result = String::from("\x1b[1m"); // bold
    let mut color_idx = 0usize;
    for ch in text.chars() {
        if ch == ' ' {
            result.push(' ');
        } else {
            result.push_str(COLORS[color_idx % COLORS.len()]);
            result.push(ch);
            color_idx += 1;
        }
    }
    result.push_str("\x1b[0m"); // reset
    result
}

/// Print a "What's Next?" section with a stylized title and spaced command list.
pub fn print_whats_next(out: &OutputSink) {
    let title = if out.supports_color() {
        rainbow_text("  What's Next?")
    } else {
        "  What's Next?".to_string()
    };

    out.println(String::new());
    out.println(title);
    out.println(String::new());
    out.println("  Run `amux` to launch the interactive TUI.".to_string());
    out.println(String::new());
    out.println("  Available commands:".to_string());
    out.println(String::new());
    out.println(
        "    amux chat        —  Start a freeform chat session with the agent".to_string(),
    );
    out.println(
        "    amux new         —  Create a new work item from the aspec template".to_string(),
    );
    out.println(
        "    amux implement   —  Implement a work item inside a container".to_string(),
    );
    out.println(String::new());
    out.println(
        "  Any amux command can also be run as a plain CLI command without".to_string(),
    );
    out.println("  launching the TUI.".to_string());
    out.println(String::new());
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::sync::mpsc::unbounded_channel;

    // ── Helper: a temp git repo with Dockerfile.dev pre-created ───────────────

    fn setup_temp_repo() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join("Dockerfile.dev"), "FROM ubuntu:22.04\n").unwrap();
        tmp
    }

    // ── find_git_root_from ────────────────────────────────────────────────────

    #[test]
    fn find_git_root_finds_git_dir() {
        let src_dir = std::path::Path::new(file!()).parent().unwrap().parent().unwrap();
        let root = find_git_root_from(src_dir);
        assert!(root.is_some());
        assert!(root.unwrap().join(".git").exists());
    }

    #[test]
    fn find_git_root_returns_none_outside_repo() {
        let tmp = TempDir::new().unwrap();
        let result = find_git_root_from(tmp.path());
        assert!(result.is_none());
    }

    // ── InitSummary ───────────────────────────────────────────────────────────

    #[test]
    fn init_summary_default_all_pending() {
        let summary = InitSummary::default();
        assert_eq!(summary.config, StepStatus::Pending);
        assert_eq!(summary.aspec_folder, StepStatus::Pending);
        assert_eq!(summary.dockerfile, StepStatus::Pending);
        assert_eq!(summary.audit, StepStatus::Pending);
        assert_eq!(summary.image_build, StepStatus::Pending);
        assert_eq!(summary.work_items_setup, StepStatus::Pending);
    }

    // ── print_init_summary / print_whats_next ─────────────────────────────────

    #[test]
    fn print_init_summary_outputs_table() {
        let (tx, mut rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let summary = InitSummary {
            config: StepStatus::Ok("saved".into()),
            aspec_folder: StepStatus::Skipped("use --aspec to download".into()),
            dockerfile: StepStatus::Ok("created".into()),
            audit: StepStatus::Skipped("declined".into()),
            image_build: StepStatus::Ok("built".into()),
            work_items_setup: StepStatus::Skipped("not needed".into()),
        };
        print_init_summary(&sink, &summary, "claude");

        let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let all = messages.join("\n");
        assert!(all.contains("Init Summary"), "Missing header");
        assert!(all.contains("Config"), "Missing config row");
        assert!(all.contains("saved"), "Missing saved status");
        assert!(all.contains("aspec folder"), "Missing aspec row");
        assert!(all.contains("Dockerfile.dev"), "Missing dockerfile row");
        assert!(all.contains("Agent audit"), "Missing audit row");
        assert!(all.contains("Docker image"), "Missing image row");
    }

    #[test]
    fn print_whats_next_outputs_box() {
        let (tx, mut rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        print_whats_next(&sink);

        let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let all = messages.join("\n");
        assert!(all.contains("amux"), "Missing amux TUI mention");
        assert!(all.contains("chat"), "Missing chat command");
        assert!(all.contains("new"), "Missing new command");
        assert!(all.contains("implement"), "Missing implement command");
    }

    // ── execute() integration ─────────────────────────────────────────────────

    #[tokio::test]
    async fn execute_streams_output() {
        let (tx, mut rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let cwd = std::env::current_dir().unwrap();
        let runtime = std::sync::Arc::new(crate::runtime::DockerRuntime::new());
        let git_root = find_git_root_from(&cwd).unwrap();
        let mut qa = CliInitQa::new(&git_root, sink.clone());
        let launcher = CliContainerLauncher::new(runtime.clone());
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root,
        };
        let result = execute(params, &mut qa, &launcher, &sink, runtime).await;
        drop(result);
        // Should have received at least one message via the channel.
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn execute_skips_aspec_when_flag_false() {
        let (tx, mut rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let cwd = std::env::current_dir().unwrap();
        let runtime = std::sync::Arc::new(crate::runtime::DockerRuntime::new());
        let git_root = find_git_root_from(&cwd).unwrap();
        let mut qa = CliInitQa::new(&git_root, sink.clone());
        let launcher = CliContainerLauncher::new(runtime.clone());
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root,
        };
        let result = execute(params, &mut qa, &launcher, &sink, runtime).await;
        drop(result);
        let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let all = messages.join("\n");
        assert!(
            all.contains("already exists") || all.contains("use --aspec"),
            "Should report aspec folder status when --aspec is not passed. Got: {:?}",
            messages
        );
    }

    #[tokio::test]
    async fn execute_preserves_work_items_config_on_reinit() {
        let tmp = setup_temp_repo();
        let root = tmp.path();

        let pre_config = crate::config::RepoConfig {
            work_items: Some(crate::config::WorkItemsConfig {
                dir: Some("my-items".to_string()),
                template: None,
            }),
            ..Default::default()
        };
        crate::config::save_repo_config(root, &pre_config).unwrap();

        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let runtime = std::sync::Arc::new(crate::runtime::DockerRuntime::new());
        let mut qa = CliInitQa::new(root, sink.clone());
        let launcher = CliContainerLauncher::new(runtime.clone());
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };
        let _ = execute(params, &mut qa, &launcher, &sink, runtime).await;

        let loaded = crate::config::load_repo_config(root).unwrap();
        assert_eq!(
            loaded.work_items.as_ref().and_then(|w| w.dir.as_deref()),
            Some("my-items"),
            "work_items.dir must survive an amux init re-run"
        );
    }

    #[tokio::test]
    async fn execute_work_items_offer_saves_dir_to_config() {
        let tmp = setup_temp_repo();
        let root = tmp.path();

        // Sequence: "n" declines audit, "y" accepts work items, "my/items" dir, "" no template.
        let (tx, mut rx) = unbounded_channel();
        let sink = OutputSink::mock_input(tx, vec!["n", "y", "my/items", ""]);
        let runtime = std::sync::Arc::new(crate::runtime::DockerRuntime::new());
        let mut qa = CliInitQa::new(root, sink.clone());
        let launcher = CliContainerLauncher::new(runtime.clone());
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };
        let _ = execute(params, &mut qa, &launcher, &sink, runtime).await;

        let loaded = crate::config::load_repo_config(root).unwrap();
        assert_eq!(
            loaded.work_items.as_ref().and_then(|w| w.dir.as_deref()),
            Some("my/items"),
            "work_items.dir should be persisted after accepting the init offer"
        );
        assert!(
            loaded
                .work_items
                .as_ref()
                .and_then(|w| w.template.as_deref())
                .is_none(),
            "template should be None when left blank"
        );

        let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let output = messages.join("\n");
        assert!(
            output.contains("Work items directory configured"),
            "expected confirmation message; got: {}",
            output
        );
    }

    #[tokio::test]
    async fn execute_work_items_offer_skips_when_already_configured() {
        let tmp = setup_temp_repo();
        let root = tmp.path();

        let pre_config = crate::config::RepoConfig {
            work_items: Some(crate::config::WorkItemsConfig {
                dir: Some("existing/items".to_string()),
                template: None,
            }),
            ..Default::default()
        };
        crate::config::save_repo_config(root, &pre_config).unwrap();

        // MockInput with no queued inputs — if offer fires it would return "" (not panic).
        let (tx, mut rx) = unbounded_channel();
        let sink = OutputSink::mock_input(tx, vec![] as Vec<String>);
        let runtime = std::sync::Arc::new(crate::runtime::DockerRuntime::new());
        let mut qa = CliInitQa::new(root, sink.clone());
        let launcher = CliContainerLauncher::new(runtime.clone());
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };
        let result = execute(params, &mut qa, &launcher, &sink, runtime).await;
        assert!(result.is_ok(), "init should succeed: {:?}", result.err());

        let loaded = crate::config::load_repo_config(root).unwrap();
        assert_eq!(
            loaded.work_items.as_ref().and_then(|w| w.dir.as_deref()),
            Some("existing/items"),
            "work_items.dir should not change when already configured"
        );

        let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let output = messages.join("\n");
        assert!(
            !output.contains("configure a work items directory"),
            "the offer prompt should not appear when already configured; got: {}",
            output
        );
    }

    // ── write_project_dockerfile ──────────────────────────────────────────────

    #[tokio::test]
    async fn write_project_dockerfile_creates_when_missing() {
        let tmp = TempDir::new().unwrap();
        let (tx, _rx) = unbounded_channel();
        let out = OutputSink::Channel(tx);
        let result = write_project_dockerfile(tmp.path(), &out).await.unwrap();
        assert!(result, "should return true when creating a new file");
        let path = tmp.path().join("Dockerfile.dev");
        assert!(path.exists(), "Dockerfile.dev should be created");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("debian:bookworm-slim"),
            "project Dockerfile should use debian:bookworm-slim base"
        );
    }

    #[tokio::test]
    async fn write_project_dockerfile_does_not_overwrite_existing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("Dockerfile.dev");
        std::fs::write(&path, "CUSTOM CONTENT").unwrap();
        let (tx, _rx) = unbounded_channel();
        let out = OutputSink::Channel(tx);
        let result = write_project_dockerfile(tmp.path(), &out).await.unwrap();
        assert!(!result, "should return false when file already exists");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "CUSTOM CONTENT");
    }

    // ── write_agent_dockerfile ────────────────────────────────────────────────

    #[tokio::test]
    async fn write_agent_dockerfile_creates_amux_dir_if_missing() {
        let tmp = TempDir::new().unwrap();
        let amux_dir = tmp.path().join(".amux");
        assert!(!amux_dir.exists());
        let (tx, _rx) = unbounded_channel();
        let out = OutputSink::Channel(tx);
        write_agent_dockerfile(tmp.path(), &Agent::Claude, &out)
            .await
            .unwrap();
        assert!(amux_dir.exists(), ".amux dir should have been created");
    }

    #[tokio::test]
    async fn write_agent_dockerfile_creates_file_in_correct_location() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().join("myapp");
        std::fs::create_dir_all(&project_dir).unwrap();
        let (tx, _rx) = unbounded_channel();
        let out = OutputSink::Channel(tx);
        let result = write_agent_dockerfile(&project_dir, &Agent::Claude, &out)
            .await
            .unwrap();
        assert!(result, "should return true when creating a new file");
        assert!(project_dir
            .join(".amux")
            .join("Dockerfile.claude")
            .exists());
    }

    #[tokio::test]
    async fn write_agent_dockerfile_does_not_overwrite_existing() {
        let tmp = TempDir::new().unwrap();
        let amux_dir = tmp.path().join(".amux");
        std::fs::create_dir_all(&amux_dir).unwrap();
        std::fs::write(amux_dir.join("Dockerfile.claude"), "CUSTOM").unwrap();
        let (tx, _rx) = unbounded_channel();
        let out = OutputSink::Channel(tx);
        let result = write_agent_dockerfile(tmp.path(), &Agent::Claude, &out)
            .await
            .unwrap();
        assert!(!result, "should return false when file already exists");
    }

    #[tokio::test]
    async fn write_agent_dockerfile_codex_uses_agent_name_in_path() {
        let tmp = TempDir::new().unwrap();
        let (tx, _rx) = unbounded_channel();
        let out = OutputSink::Channel(tx);
        write_agent_dockerfile(tmp.path(), &Agent::Codex, &out)
            .await
            .unwrap();
        let expected = tmp.path().join(".amux").join("Dockerfile.codex");
        assert!(
            expected.exists(),
            ".amux/Dockerfile.codex should be created for codex agent"
        );
    }

    /// Verifies the `{{AMUX_BASE_IMAGE}}` substitution logic.
    #[test]
    fn agent_dockerfile_embedded_substitution_replaces_placeholder() {
        use std::path::Path;
        let base_tag = crate::runtime::project_image_tag(Path::new("/work/myapp"));
        assert_eq!(base_tag, "amux-myapp:latest");

        for agent in &[
            Agent::Claude,
            Agent::Codex,
            Agent::Opencode,
            Agent::Maki,
            Agent::Gemini,
            Agent::Copilot,
            Agent::Crush,
            Agent::Cline,
        ] {
            let template = dockerfile_for_agent_embedded(agent);
            let content = template.replace("{{AMUX_BASE_IMAGE}}", &base_tag);
            assert!(
                !content.contains("{{AMUX_BASE_IMAGE}}"),
                "{:?}: placeholder should be gone after substitution",
                agent
            );
            assert!(
                content.contains("FROM amux-myapp:latest"),
                "{:?}: substituted content should have FROM amux-myapp:latest; got:\n{}",
                agent,
                content
            );
        }
    }

    // ── Embedded template checks ──────────────────────────────────────────────

    #[test]
    fn project_dockerfile_embedded_uses_debian_slim_base() {
        let content = project_dockerfile_embedded();
        assert!(
            content.contains("debian:bookworm-slim"),
            "project template should use debian:bookworm-slim base image"
        );
    }

    #[test]
    fn dockerfile_for_agent_embedded_uses_base_image_placeholder() {
        for agent in &[Agent::Claude, Agent::Codex, Agent::Opencode, Agent::Maki, Agent::Gemini, Agent::Copilot, Agent::Crush, Agent::Cline] {
            let content = dockerfile_for_agent_embedded(agent);
            assert!(
                content.contains("{{AMUX_BASE_IMAGE}}"),
                "{:?} template should use {{AMUX_BASE_IMAGE}} placeholder",
                agent
            );
        }
    }

    #[test]
    fn dockerfile_for_agent_embedded_does_not_use_npm_install() {
        // Agents that do NOT use npm install as their distribution method.
        // Agent::Gemini, Agent::Crush, and Agent::Cline are explicitly excluded:
        // npm install -g is the official distribution method for those agents
        // (gemini-cli, @charmland/crush, and cline respectively).
        for agent in &[Agent::Claude, Agent::Codex, Agent::Opencode, Agent::Maki, Agent::Copilot] {
            let content = dockerfile_for_agent_embedded(agent);
            assert!(
                !content.contains("npm install"),
                "{:?} template should not use npm install",
                agent
            );
        }
    }

    #[test]
    fn dockerfile_templates_install_via_apt_or_direct_download() {
        for agent in &[Agent::Claude, Agent::Codex, Agent::Opencode, Agent::Maki, Agent::Gemini, Agent::Copilot, Agent::Crush, Agent::Cline] {
            let content = dockerfile_for_agent_embedded(agent);
            assert!(
                content.contains("apt-get") || content.contains("curl"),
                "{:?} template should install packages via apt-get or direct download",
                agent
            );
        }
    }

    #[test]
    fn dockerfile_for_agent_embedded_maki_uses_official_installer() {
        let content = dockerfile_for_agent_embedded(&Agent::Maki);
        assert!(
            content.contains("maki.sh/install.sh"),
            "Dockerfile.maki must install maki via the official maki.sh/install.sh installer"
        );
    }

    #[test]
    fn dockerfile_for_agent_embedded_gemini_contains_expected_strings() {
        let content = dockerfile_for_agent_embedded(&Agent::Gemini);
        assert!(
            content.contains("{{AMUX_BASE_IMAGE}}"),
            "Dockerfile.gemini must use {{AMUX_BASE_IMAGE}} placeholder"
        );
        assert!(
            content.contains("nodesource"),
            "Dockerfile.gemini must install Node.js via NodeSource"
        );
        assert!(
            content.contains("@google/gemini-cli"),
            "Dockerfile.gemini must install @google/gemini-cli"
        );
    }

    #[test]
    fn dockerfile_for_agent_embedded_uses_debian_slim_base() {
        // All agent Dockerfiles must use the {{AMUX_BASE_IMAGE}} placeholder, which is
        // derived from the project base image (itself based on debian:bookworm-slim).
        // The substitution is verified by agent_dockerfile_embedded_substitution_replaces_placeholder.
        for agent in &[
            Agent::Claude,
            Agent::Codex,
            Agent::Opencode,
            Agent::Maki,
            Agent::Gemini,
            Agent::Copilot,
            Agent::Crush,
            Agent::Cline,
        ] {
            let content = dockerfile_for_agent_embedded(agent);
            assert!(
                content.contains("{{AMUX_BASE_IMAGE}}"),
                "{:?} template must use {{AMUX_BASE_IMAGE}} placeholder (derived from debian:bookworm-slim)",
                agent
            );
        }
    }

    #[test]
    fn dockerfile_for_agent_embedded_copilot_contains_expected_strings() {
        let content = dockerfile_for_agent_embedded(&Agent::Copilot);
        assert!(
            content.contains("{{AMUX_BASE_IMAGE}}"),
            "Dockerfile.copilot must use {{AMUX_BASE_IMAGE}} placeholder"
        );
        assert!(
            content.contains("gh.io/copilot-install"),
            "Dockerfile.copilot must install copilot via the official gh.io/copilot-install script"
        );
    }

    #[test]
    fn dockerfile_for_agent_embedded_crush_contains_expected_strings() {
        let content = dockerfile_for_agent_embedded(&Agent::Crush);
        assert!(
            content.contains("{{AMUX_BASE_IMAGE}}"),
            "Dockerfile.crush must use {{AMUX_BASE_IMAGE}} placeholder"
        );
        assert!(
            content.contains("nodesource"),
            "Dockerfile.crush must install Node.js via NodeSource"
        );
        assert!(
            content.contains("@charmland/crush"),
            "Dockerfile.crush must install @charmland/crush"
        );
    }

    #[test]
    fn dockerfile_for_agent_embedded_cline_contains_expected_strings() {
        let content = dockerfile_for_agent_embedded(&Agent::Cline);
        assert!(
            content.contains("{{AMUX_BASE_IMAGE}}"),
            "Dockerfile.cline must use {{AMUX_BASE_IMAGE}} placeholder"
        );
        assert!(
            content.contains("nodesource"),
            "Dockerfile.cline must install Node.js via NodeSource"
        );
        let installs_cline = content
            .lines()
            .any(|line| line.contains("npm install -g") && line.contains("cline"));
        assert!(
            installs_cline,
            "Dockerfile.cline must install the cline package via npm install -g"
        );
    }

    // ── Mock types ────────────────────────────────────────────────────────────

    /// Minimal `AgentRuntime` stub for unit tests.
    ///
    /// `execute()` only calls `name()` and `is_available()` directly; everything
    /// else goes through the `InitContainerLauncher`. All remaining methods
    /// return inert defaults so they are safe if accidentally reached.
    struct MockRuntime {
        available: bool,
    }

    impl crate::runtime::AgentRuntime for MockRuntime {
        fn is_available(&self) -> bool {
            self.available
        }
        fn name(&self) -> &'static str {
            "mock"
        }
        fn cli_binary(&self) -> &'static str {
            "mock"
        }
        fn check_socket(&self) -> anyhow::Result<std::path::PathBuf> {
            Ok(std::path::PathBuf::from("/mock/socket"))
        }
        fn build_image_streaming(
            &self,
            _tag: &str,
            _dockerfile: &std::path::Path,
            _context: &std::path::Path,
            _no_cache: bool,
            _on_line: &mut dyn FnMut(&str),
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
        fn image_exists(&self, _tag: &str) -> bool {
            false
        }
        fn run_container(
            &self,
            _image: &str,
            _host_path: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
            _container_name: Option<&str>,
            _ssh_dir: Option<&std::path::Path>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn run_container_captured(
            &self,
            _image: &str,
            _host_path: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
            _container_name: Option<&str>,
            _ssh_dir: Option<&std::path::Path>,
        ) -> anyhow::Result<(String, String)> {
            Ok((String::new(), String::new()))
        }
        fn run_container_at_path(
            &self,
            _image: &str,
            _host_path: &str,
            _container_path: &str,
            _working_dir: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
            _container_name: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn run_container_captured_at_path(
            &self,
            _image: &str,
            _host_path: &str,
            _container_path: &str,
            _working_dir: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
        ) -> anyhow::Result<(String, String)> {
            Ok((String::new(), String::new()))
        }
        fn run_container_detached(
            &self,
            _image: &str,
            _host_path: &str,
            _container_path: &str,
            _working_dir: &str,
            _container_name: Option<&str>,
            _env_vars: Vec<(String, String)>,
            _allow_docker: bool,
            _host_settings: Option<&crate::runtime::HostSettings>,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
        fn start_container(&self, _container_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn stop_container(&self, _container_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn remove_container(&self, _container_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn is_container_running(&self, _container_id: &str) -> bool {
            false
        }
        fn find_stopped_container(
            &self,
            _name: &str,
            _image: &str,
        ) -> Option<crate::runtime::StoppedContainerInfo> {
            None
        }
        fn list_running_containers_by_prefix(&self, _prefix: &str) -> Vec<String> {
            vec![]
        }
        fn list_running_containers_with_ids_by_prefix(
            &self,
            _prefix: &str,
        ) -> Vec<(String, String)> {
            vec![]
        }
        fn get_container_workspace_mount(&self, _container_name: &str) -> Option<String> {
            None
        }
        fn query_container_stats(
            &self,
            _name: &str,
        ) -> Option<crate::runtime::ContainerStats> {
            None
        }
        fn build_run_args_pty(
            &self,
            _image: &str,
            _host_path: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
            _container_name: Option<&str>,
            _ssh_dir: Option<&std::path::Path>,
        ) -> Vec<String> {
            vec![]
        }
        fn build_run_args_pty_display(
            &self,
            _image: &str,
            _host_path: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
            _container_name: Option<&str>,
            _ssh_dir: Option<&std::path::Path>,
        ) -> Vec<String> {
            vec![]
        }
        fn build_run_args_pty_at_path(
            &self,
            _image: &str,
            _host_path: &str,
            _container_path: &str,
            _working_dir: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
            _container_name: Option<&str>,
        ) -> Vec<String> {
            vec![]
        }
        fn build_exec_args_pty(
            &self,
            _container_id: &str,
            _working_dir: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
        ) -> Vec<String> {
            vec![]
        }
        fn build_run_args_display(
            &self,
            _image: &str,
            _host_path: &str,
            _entrypoint: &[&str],
            _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool,
            _container_name: Option<&str>,
            _ssh_dir: Option<&std::path::Path>,
        ) -> Vec<String> {
            vec![]
        }
    }

    /// `InitQa` stub that returns preset answers and records which methods were called.
    struct MockInitQa {
        replace_aspec: bool,
        run_audit: bool,
        work_items: Option<WorkItemsConfig>,
        calls: std::sync::Mutex<Vec<&'static str>>,
    }

    impl MockInitQa {
        fn new(
            replace_aspec: bool,
            run_audit: bool,
            work_items: Option<WorkItemsConfig>,
        ) -> Self {
            Self {
                replace_aspec,
                run_audit,
                work_items,
                calls: std::sync::Mutex::new(vec![]),
            }
        }

        fn was_called(&self, method: &str) -> bool {
            self.calls.lock().unwrap().iter().any(|&s| s == method)
        }
    }

    impl InitQa for MockInitQa {
        fn ask_replace_aspec(&mut self) -> Result<bool> {
            self.calls.lock().unwrap().push("ask_replace_aspec");
            Ok(self.replace_aspec)
        }
        fn ask_run_audit(&mut self) -> Result<bool> {
            self.calls.lock().unwrap().push("ask_run_audit");
            Ok(self.run_audit)
        }
        fn ask_work_items_setup(&mut self) -> Result<Option<WorkItemsConfig>> {
            self.calls.lock().unwrap().push("ask_work_items_setup");
            Ok(self.work_items.take())
        }
    }

    /// `InitContainerLauncher` stub that records calls and returns `Ok(())` without
    /// touching Docker.
    struct MockContainerLauncher {
        build_tags: std::sync::Mutex<Vec<String>>,
        audit_agents: std::sync::Mutex<Vec<String>>,
    }

    impl MockContainerLauncher {
        fn new() -> Self {
            Self {
                build_tags: std::sync::Mutex::new(vec![]),
                audit_agents: std::sync::Mutex::new(vec![]),
            }
        }
        fn build_call_count(&self) -> usize {
            self.build_tags.lock().unwrap().len()
        }
        fn run_audit_call_count(&self) -> usize {
            self.audit_agents.lock().unwrap().len()
        }
    }

    impl InitContainerLauncher for MockContainerLauncher {
        fn build_image(
            &self,
            tag: &str,
            _dockerfile: &Path,
            _context: &Path,
            _sink: &OutputSink,
        ) -> Result<()> {
            self.build_tags.lock().unwrap().push(tag.to_string());
            Ok(())
        }
        fn run_audit(&self, agent: Agent, _cwd: &Path, _sink: &OutputSink) -> Result<()> {
            self.audit_agents
                .lock()
                .unwrap()
                .push(agent.as_str().to_string());
            Ok(())
        }
    }

    // ── Unit: execute() stages with mocks ─────────────────────────────────────

    #[tokio::test]
    async fn execute_mock_config_stage_sets_ok() {
        let tmp = setup_temp_repo();
        let root = tmp.path();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let runtime = std::sync::Arc::new(MockRuntime { available: false });
        let mut qa = MockInitQa::new(false, false, None);
        let launcher = MockContainerLauncher::new();
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };

        let summary = execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        assert!(
            matches!(summary.config, StepStatus::Ok(_)),
            "config stage should be Ok after write: {:?}",
            summary.config
        );
        assert!(
            root.join(".amux").join("config.json").exists(),
            "config.json must be written to disk"
        );
    }

    #[tokio::test]
    async fn execute_mock_aspec_folder_skipped_when_flag_false_and_dir_absent() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let runtime = std::sync::Arc::new(MockRuntime { available: false });
        let mut qa = MockInitQa::new(false, false, None);
        let launcher = MockContainerLauncher::new();
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };

        let summary = execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        assert!(
            matches!(summary.aspec_folder, StepStatus::Skipped(_)),
            "aspec_folder must be Skipped when --aspec is not passed: {:?}",
            summary.aspec_folder
        );
    }

    #[tokio::test]
    async fn execute_mock_audit_declined_runtime_unavailable_skips_build() {
        // Dockerfile.dev pre-exists (setup_temp_repo) so only agent dockerfile is new.
        // Stage 8 fires: runtime unavailable → both audit and image_build are Skipped.
        let tmp = setup_temp_repo();
        let root = tmp.path();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let runtime = std::sync::Arc::new(MockRuntime { available: false });
        let mut qa = MockInitQa::new(false, false, None);
        let launcher = MockContainerLauncher::new();
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };

        let summary = execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        assert!(
            matches!(summary.audit, StepStatus::Skipped(_)),
            "audit must be Skipped when declined: {:?}",
            summary.audit
        );
        assert_eq!(
            launcher.build_call_count(),
            0,
            "no build_image calls when runtime is unavailable"
        );
    }

    #[tokio::test]
    async fn execute_mock_audit_requested_runtime_unavailable_sets_failed() {
        let tmp = setup_temp_repo();
        let root = tmp.path();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let runtime = std::sync::Arc::new(MockRuntime { available: false });
        let mut qa = MockInitQa::new(false, true, None);
        let launcher = MockContainerLauncher::new();
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };

        let summary = execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        assert!(
            matches!(summary.audit, StepStatus::Failed(_)),
            "audit must be Failed when runtime is unavailable but audit was requested: {:?}",
            summary.audit
        );
        assert!(
            matches!(summary.image_build, StepStatus::Failed(_)),
            "image_build must be Failed when runtime is unavailable: {:?}",
            summary.image_build
        );
        assert_eq!(
            launcher.build_call_count(),
            0,
            "build_image must not be called when runtime unavailable"
        );
        assert_eq!(
            launcher.run_audit_call_count(),
            0,
            "run_audit must not be called when runtime unavailable"
        );
    }

    #[tokio::test]
    async fn execute_mock_audit_requested_runtime_available_calls_launcher_inline() {
        // Pre-create agent dockerfile so it counts as "existing" (not new).
        // This means audit is the only reason builds fire — cleaner call count.
        let tmp = setup_temp_repo();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".amux")).unwrap();
        std::fs::write(
            root.join(".amux").join("Dockerfile.claude"),
            "FROM ubuntu:22.04\n",
        )
        .unwrap();

        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let runtime = std::sync::Arc::new(MockRuntime { available: true });
        let mut qa = MockInitQa::new(false, true, None);
        let launcher = MockContainerLauncher::new();
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };

        let summary = execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        // Audit must run inline (not deferred): launcher.run_audit called within execute().
        assert_eq!(
            launcher.run_audit_call_count(),
            1,
            "run_audit must be called exactly once, inline (not deferred)"
        );
        // 4 build calls: pre-audit project + agent, post-audit project + agent.
        assert_eq!(
            launcher.build_call_count(),
            4,
            "expected 4 build_image calls for audit flow (pre×2 + post×2)"
        );
        assert!(
            matches!(summary.audit, StepStatus::Ok(_)),
            "audit must be Ok after successful run: {:?}",
            summary.audit
        );
        assert!(
            matches!(summary.image_build, StepStatus::Ok(_)),
            "image_build must be Ok after successful builds: {:?}",
            summary.image_build
        );
    }

    /// Verifies that an early-return from a Stage-7 build failure marks `work_items_setup`
    /// as `Skipped` rather than leaving it in the default `Pending` state.
    #[tokio::test]
    async fn execute_mock_stage7_build_failure_sets_work_items_skipped() {
        // A launcher that fails on the very first build_image call (Stage 7a).
        struct FailFirstBuildLauncher;
        impl InitContainerLauncher for FailFirstBuildLauncher {
            fn build_image(
                &self,
                _tag: &str,
                _dockerfile: &Path,
                _context: &Path,
                _sink: &OutputSink,
            ) -> Result<()> {
                Err(anyhow::anyhow!("simulated build failure"))
            }
            fn run_audit(&self, _agent: Agent, _cwd: &Path, _sink: &OutputSink) -> Result<()> {
                Ok(())
            }
        }

        let tmp = setup_temp_repo();
        let root = tmp.path();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        // Runtime must be available so Stage 7 is entered.
        let runtime = std::sync::Arc::new(MockRuntime { available: true });
        let mut qa = MockInitQa::new(false, true, None); // run_audit=true
        let launcher = FailFirstBuildLauncher;
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };

        let summary = execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        assert!(
            matches!(summary.audit, StepStatus::Failed(_)),
            "audit must be Failed after Stage-7a build failure: {:?}",
            summary.audit
        );
        assert!(
            matches!(summary.image_build, StepStatus::Failed(_)),
            "image_build must be Failed after Stage-7a build failure: {:?}",
            summary.image_build
        );
        assert!(
            matches!(summary.work_items_setup, StepStatus::Skipped(_)),
            "work_items_setup must be Skipped (not Pending) after early Stage-7 return: {:?}",
            summary.work_items_setup
        );
    }

    #[tokio::test]
    async fn execute_mock_work_items_qa_called_when_not_configured() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let runtime = std::sync::Arc::new(MockRuntime { available: false });
        let mut qa = MockInitQa::new(false, false, None);
        let launcher = MockContainerLauncher::new();
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };

        let _ = execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        assert!(
            qa.was_called("ask_work_items_setup"),
            "ask_work_items_setup must be called when work_items is not yet configured"
        );
    }

    #[tokio::test]
    async fn execute_mock_work_items_qa_not_called_when_already_configured() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        let pre = crate::config::RepoConfig {
            work_items: Some(WorkItemsConfig {
                dir: Some("existing-items".into()),
                template: None,
            }),
            ..Default::default()
        };
        crate::config::save_repo_config(root, &pre).unwrap();

        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let runtime = std::sync::Arc::new(MockRuntime { available: false });
        let mut qa = MockInitQa::new(false, false, None);
        let launcher = MockContainerLauncher::new();
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };

        let _ = execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        assert!(
            !qa.was_called("ask_work_items_setup"),
            "ask_work_items_setup must NOT be called when work_items is already configured"
        );
    }

    #[tokio::test]
    async fn execute_mock_work_items_accepted_sets_ok_status() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let runtime = std::sync::Arc::new(MockRuntime { available: false });
        let wi = WorkItemsConfig {
            dir: Some("items".into()),
            template: None,
        };
        let mut qa = MockInitQa::new(false, false, Some(wi));
        let launcher = MockContainerLauncher::new();
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };

        let summary = execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        assert!(
            matches!(summary.work_items_setup, StepStatus::Ok(_)),
            "work_items_setup must be Ok when a valid config is provided: {:?}",
            summary.work_items_setup
        );
    }

    #[tokio::test]
    async fn execute_mock_work_items_declined_sets_skipped_status() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let runtime = std::sync::Arc::new(MockRuntime { available: false });
        // work_items=None from MockInitQa means ask_work_items_setup returns None (declined).
        let mut qa = MockInitQa::new(false, false, None);
        let launcher = MockContainerLauncher::new();
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };

        let summary = execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        assert!(
            matches!(summary.work_items_setup, StepStatus::Skipped(_)),
            "work_items_setup must be Skipped when None is returned: {:?}",
            summary.work_items_setup
        );
    }

    #[tokio::test]
    async fn execute_mock_no_stage_remains_pending_after_complete_run() {
        let tmp = setup_temp_repo();
        let root = tmp.path();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let runtime = std::sync::Arc::new(MockRuntime { available: false });
        let mut qa = MockInitQa::new(false, false, None);
        let launcher = MockContainerLauncher::new();
        let params = InitParams {
            agent: Agent::Claude,
            aspec: false,
            git_root: root.to_path_buf(),
        };

        let summary = execute(params, &mut qa, &launcher, &sink, runtime)
            .await
            .unwrap();

        assert_ne!(
            summary.config,
            StepStatus::Pending,
            "config must not be Pending"
        );
        assert_ne!(
            summary.aspec_folder,
            StepStatus::Pending,
            "aspec_folder must not be Pending"
        );
        assert_ne!(
            summary.dockerfile,
            StepStatus::Pending,
            "dockerfile must not be Pending"
        );
        assert_ne!(
            summary.audit,
            StepStatus::Pending,
            "audit must not be Pending"
        );
        assert_ne!(
            summary.image_build,
            StepStatus::Pending,
            "image_build must not be Pending"
        );
        assert_ne!(
            summary.work_items_setup,
            StepStatus::Pending,
            "work_items_setup must not be Pending"
        );
    }

    // ── Unit: CliInitQa ───────────────────────────────────────────────────────

    #[test]
    fn cli_qa_ask_replace_aspec_yes_returns_true() {
        let tmp = TempDir::new().unwrap();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::mock_input(tx, vec!["y"]);
        let mut qa = CliInitQa::new(tmp.path(), sink);
        assert_eq!(
            qa.ask_replace_aspec().unwrap(),
            true,
            "\"y\" must return true"
        );
    }

    #[test]
    fn cli_qa_ask_replace_aspec_no_returns_false() {
        let tmp = TempDir::new().unwrap();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::mock_input(tx, vec!["n"]);
        let mut qa = CliInitQa::new(tmp.path(), sink);
        assert_eq!(
            qa.ask_replace_aspec().unwrap(),
            false,
            "\"n\" must return false"
        );
    }

    #[test]
    fn cli_qa_ask_replace_aspec_empty_input_returns_false() {
        let tmp = TempDir::new().unwrap();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::mock_input(tx, vec![""]);
        let mut qa = CliInitQa::new(tmp.path(), sink);
        assert_eq!(
            qa.ask_replace_aspec().unwrap(),
            false,
            "empty input must default to false"
        );
    }

    #[test]
    fn cli_qa_ask_replace_aspec_unexpected_char_returns_false() {
        let tmp = TempDir::new().unwrap();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::mock_input(tx, vec!["z"]);
        let mut qa = CliInitQa::new(tmp.path(), sink);
        assert_eq!(
            qa.ask_replace_aspec().unwrap(),
            false,
            "unrecognised char must default to false"
        );
    }

    #[test]
    fn cli_qa_ask_replace_aspec_eof_returns_false() {
        // Empty queue simulates EOF — read_line() returns "".
        let tmp = TempDir::new().unwrap();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::mock_input(tx, vec![] as Vec<String>);
        let mut qa = CliInitQa::new(tmp.path(), sink);
        assert_eq!(
            qa.ask_replace_aspec().unwrap(),
            false,
            "EOF (exhausted queue) must default to false"
        );
    }

    #[test]
    fn cli_qa_ask_run_audit_yes_when_dockerfile_exists_returns_true() {
        let tmp = setup_temp_repo();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::mock_input(tx, vec!["y"]);
        let mut qa = CliInitQa::new(tmp.path(), sink);
        assert_eq!(
            qa.ask_run_audit().unwrap(),
            true,
            "\"y\" must return true when Dockerfile.dev exists"
        );
    }

    #[test]
    fn cli_qa_ask_run_audit_no_when_dockerfile_exists_returns_false() {
        let tmp = setup_temp_repo();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::mock_input(tx, vec!["n"]);
        let mut qa = CliInitQa::new(tmp.path(), sink);
        assert_eq!(
            qa.ask_run_audit().unwrap(),
            false,
            "\"n\" must return false when Dockerfile.dev exists"
        );
    }

    #[test]
    fn cli_qa_ask_run_audit_yes_when_no_dockerfile_returns_true() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        // No Dockerfile.dev — different prompt branch.
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::mock_input(tx, vec!["y"]);
        let mut qa = CliInitQa::new(root, sink);
        assert_eq!(
            qa.ask_run_audit().unwrap(),
            true,
            "\"y\" must return true even when Dockerfile.dev does not yet exist"
        );
    }

    #[test]
    fn cli_qa_ask_run_audit_empty_input_returns_false() {
        let tmp = TempDir::new().unwrap();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::mock_input(tx, vec![""]);
        let mut qa = CliInitQa::new(tmp.path(), sink);
        assert_eq!(
            qa.ask_run_audit().unwrap(),
            false,
            "empty input must default to false"
        );
    }

    #[test]
    fn cli_qa_ask_work_items_setup_no_returns_none() {
        let tmp = TempDir::new().unwrap();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::mock_input(tx, vec!["n"]);
        let mut qa = CliInitQa::new(tmp.path(), sink);
        assert!(
            qa.ask_work_items_setup().unwrap().is_none(),
            "declining must return None"
        );
    }

    #[test]
    fn cli_qa_ask_work_items_setup_yes_with_dir_no_template() {
        let tmp = TempDir::new().unwrap();
        let (tx, _rx) = unbounded_channel();
        // "y" = accept offer, "my/items" = dir, "" = no template
        let sink = OutputSink::mock_input(tx, vec!["y", "my/items", ""]);
        let mut qa = CliInitQa::new(tmp.path(), sink);
        let result = qa.ask_work_items_setup().unwrap();
        assert!(result.is_some(), "accepting with a dir must return Some");
        let cfg = result.unwrap();
        assert_eq!(cfg.dir.as_deref(), Some("my/items"));
        assert!(cfg.template.is_none(), "blank template must be None");
    }

    #[test]
    fn cli_qa_ask_work_items_setup_yes_with_dir_and_template() {
        let tmp = TempDir::new().unwrap();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::mock_input(tx, vec!["y", "items", "template.md"]);
        let mut qa = CliInitQa::new(tmp.path(), sink);
        let result = qa.ask_work_items_setup().unwrap();
        assert!(result.is_some());
        let cfg = result.unwrap();
        assert_eq!(cfg.dir.as_deref(), Some("items"));
        assert_eq!(cfg.template.as_deref(), Some("template.md"));
    }

    #[test]
    fn cli_qa_ask_work_items_setup_yes_empty_dir_returns_none() {
        let tmp = TempDir::new().unwrap();
        let (tx, _rx) = unbounded_channel();
        // "y" = accept, "" = empty dir → treated as declined
        let sink = OutputSink::mock_input(tx, vec!["y", ""]);
        let mut qa = CliInitQa::new(tmp.path(), sink);
        assert!(
            qa.ask_work_items_setup().unwrap().is_none(),
            "empty dir must return None even after accepting the prompt"
        );
    }

    #[test]
    fn cli_qa_ask_work_items_setup_eof_returns_none() {
        // Empty queue: first read_line() returns "" → ask_yes_no parses as false → None.
        let tmp = TempDir::new().unwrap();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::mock_input(tx, vec![] as Vec<String>);
        let mut qa = CliInitQa::new(tmp.path(), sink);
        assert!(
            qa.ask_work_items_setup().unwrap().is_none(),
            "EOF (exhausted input queue) must return None"
        );
    }
}
