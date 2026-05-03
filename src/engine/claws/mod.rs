//! `engine::claws` — `ClawsEngine`. Multi-phase state machine for `claws init`,
//! `claws ready`, and `claws chat`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::data::session::Session;
use crate::engine::container::ContainerRuntime;
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::message::{MessageLevel, UserMessage};
use crate::engine::overlay::OverlayEngine;
use crate::engine::step_status::StepStatus;

pub mod frontend;
pub mod phase;
pub mod summary;

pub use frontend::ClawsFrontend;
pub use phase::{ClawsFailure, ClawsPhase};
pub use summary::ClawsSummary;

// ── Constants ────────────────────────────────────────────────────────────────

/// Audit prompt seeded into the nanoclaw audit container.
const CLAWS_AUDIT_PROMPT: &str = "\
You have two jobs that can be performed in parallel: \n\
\n\
First, evaluate the nanoclaw source code to determine how containers communicate with each other \
over the network. Modify the codebase so that all network traffic gets configured to be \
container-to-container rather than container-to-host. The `host.docker.internal` route should \
never be used, since nanoclaw, onecli, and all workers will be running within containers, and so \
communication with the host machine will never be needed. The nanoclaw controller should \
communicate with onecli over a container-to-container connection. When worker containers are \
launched, they should be configured to communicate with onecli and the nanoclaw controller over \
a container-to-container connection. Nanoclaw should use the Docker CLI and container ports to determine how to \
perform all network communication. Modify the codebase comprehensively, looking for any place \
where network traffic is configured, including addresses and ports for the controller onecli, \
and workers. Think hard to ensure no container-to-host communication will happen after your \
changes are complete. Do not attempt to use Docker yourself, as you do not have access. \n\
\n\
Second, change the nanoclaw codebase to detect `/tmp/onecli-*` volume mounts that the OneCLI SDK adds, \
and automatically copy those cert files to `data/certs/` (which is on the actual host filesystem via mount) \
Also rewrite the container mount args to use those host-accessible paths. Ensure the agent container gets the \
CA cert correctly so Node.js can trust the OneCLI proxy's certificate.\
";

/// Issue 21: URL for the nanoclaw-specific Dockerfile template.
const NANOCLAW_DOCKERFILE_URL: &str =
    "https://raw.githubusercontent.com/prettysmartdev/amux/main/templates/Dockerfile.nanoclaw";

#[derive(Debug, Clone)]
pub enum ClawsMode {
    Init,
    Ready,
    Chat,
}

#[derive(Debug, Clone)]
pub struct ClawsEngineOptions {
    pub mode: ClawsMode,
    pub nanoclaw_url: Option<String>,
    pub refresh: bool,
    pub no_cache: bool,
    /// Resolved on-disk path for the local nanoclaw clone.
    pub clone_dir: PathBuf,
}

pub struct ClawsEngine {
    session: Arc<Session>,
    git_engine: Arc<GitEngine>,
    overlay_engine: Arc<OverlayEngine>,
    container_runtime: Arc<ContainerRuntime>,
    auth_engine: Arc<crate::engine::auth::AuthEngine>,
    options: ClawsEngineOptions,
    phase: ClawsPhase,
    summary: ClawsSummary,
}

impl ClawsEngine {
    pub fn new(
        session: Arc<Session>,
        git_engine: Arc<GitEngine>,
        overlay_engine: Arc<OverlayEngine>,
        container_runtime: Arc<ContainerRuntime>,
        auth_engine: Arc<crate::engine::auth::AuthEngine>,
        options: ClawsEngineOptions,
    ) -> Self {
        Self {
            session,
            git_engine,
            overlay_engine,
            container_runtime,
            auth_engine,
            options,
            phase: ClawsPhase::Preflight,
            summary: ClawsSummary::default(),
        }
    }

    pub fn phase(&self) -> &ClawsPhase {
        &self.phase
    }

    pub fn summary(&self) -> ClawsSummary {
        self.summary.clone()
    }

    pub async fn step(
        &mut self,
        frontend: &mut dyn ClawsFrontend,
    ) -> Result<ClawsPhase, EngineError> {
        frontend.report_phase(&self.phase);
        let next = match (&self.phase, &self.options.mode) {
            (ClawsPhase::Preflight, ClawsMode::Init) => {
                if self.options.clone_dir.exists() {
                    ClawsPhase::AwaitingCloneDecision
                } else {
                    ClawsPhase::CloningRepo
                }
            }
            (ClawsPhase::Preflight, ClawsMode::Ready) => {
                self.summary.clone = StepStatus::Skipped;
                self.summary.permissions_check = StepStatus::Skipped;
                self.summary.image_build = StepStatus::Skipped;
                self.summary.audit = StepStatus::Skipped;
                self.summary.configure = StepStatus::Skipped;

                // Ask docker which claws controllers exist and what state
                // they're in. The query is best-effort — if docker isn't
                // installed we treat that as "absent" and let the
                // confirm_offer_init path drive the decision.
                match query_claws_controller_state() {
                    ControllerState::Running => {
                        self.summary.controller = StepStatus::Done;
                        ClawsPhase::Complete
                    }
                    ControllerState::Stopped => {
                        if frontend.confirm_restart_stopped()? {
                            ClawsPhase::LaunchingController
                        } else {
                            self.summary.controller = StepStatus::Skipped;
                            ClawsPhase::Complete
                        }
                    }
                    ControllerState::Absent => {
                        if frontend.confirm_offer_init()? {
                            // Switch into Init mode and start over.
                            self.options.mode = ClawsMode::Init;
                            // Reset transient state we just marked Skipped.
                            self.summary.clone = StepStatus::Pending;
                            self.summary.permissions_check = StepStatus::Pending;
                            self.summary.image_build = StepStatus::Pending;
                            self.summary.audit = StepStatus::Pending;
                            self.summary.configure = StepStatus::Pending;
                            ClawsPhase::Preflight
                        } else {
                            self.summary.controller = StepStatus::Skipped;
                            ClawsPhase::Complete
                        }
                    }
                }
            }
            (ClawsPhase::Preflight, ClawsMode::Chat) => {
                self.summary.clone = StepStatus::Skipped;
                self.summary.permissions_check = StepStatus::Skipped;
                self.summary.image_build = StepStatus::Skipped;
                self.summary.audit = StepStatus::Skipped;
                self.summary.configure = StepStatus::Skipped;

                // Chat requires a running controller — if there isn't one,
                // surface a structured failure pointing at `amux claws ready`.
                if matches!(query_claws_controller_state(), ControllerState::Running) {
                    ClawsPhase::AttachingChat
                } else {
                    ClawsPhase::Failed(ClawsFailure::ControllerNotRunning {
                        hint: "no running claws controller; run `amux claws ready` first"
                            .to_string(),
                    })
                }
            }
            (ClawsPhase::AwaitingCloneDecision, _) => {
                if frontend.ask_replace_existing_clone(&self.options.clone_dir)? {
                    ClawsPhase::CloningRepo
                } else {
                    self.summary.clone = StepStatus::Skipped;
                    ClawsPhase::CheckingPermissions
                }
            }
            (ClawsPhase::CloningRepo, _) => {
                // Clone the nanoclaw repo into the resolved clone_dir. We try
                // SSH first, then fall back to HTTPS (matching old-amux
                // behaviour). GIT_SSH_COMMAND auto-accepts new fingerprints so
                // the clone can proceed non-interactively.
                //
                // TODO(issue-17): The full fork-and-clone flow (gh repo fork)
                // is not yet implemented in the new engine. This is a known
                // simplification — the SSH/HTTPS fallback covers the basic
                // clone case.
                let parent = self
                    .options
                    .clone_dir
                    .parent()
                    .unwrap_or(std::path::Path::new("/"));
                let _ = std::fs::create_dir_all(parent);

                let clone_dir_str = self.options.clone_dir.to_str().unwrap_or("");

                // If the user supplied an explicit URL, use it directly
                // (no SSH/HTTPS fallback).
                let clone_ok = if let Some(explicit_url) = self.options.nanoclaw_url.as_deref() {
                    let output = std::process::Command::new("git")
                        .args(["clone", explicit_url, clone_dir_str])
                        .env("GIT_SSH_COMMAND", "ssh -o StrictHostKeyChecking=accept-new")
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::piped())
                        .output();
                    match output {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            let msg = "git binary not found on PATH".to_string();
                            self.summary.clone = StepStatus::Failed(msg.clone());
                            return Ok({
                                self.phase =
                                    ClawsPhase::Failed(ClawsFailure::Cloning { message: msg });
                                self.phase.clone()
                            });
                        }
                        Err(e) => {
                            let msg = format!("git clone: {e}");
                            self.summary.clone = StepStatus::Failed(msg.clone());
                            return Ok({
                                self.phase =
                                    ClawsPhase::Failed(ClawsFailure::Cloning { message: msg });
                                self.phase.clone()
                            });
                        }
                        Ok(out) => out.status.success(),
                    }
                } else {
                    // Try SSH clone first.
                    let ssh_url = "git@github.com:prettysmartdev/nanoclaw.git";
                    let ssh_result = std::process::Command::new("git")
                        .args(["clone", ssh_url, clone_dir_str])
                        .env("GIT_SSH_COMMAND", "ssh -o StrictHostKeyChecking=accept-new")
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::piped())
                        .status();

                    match ssh_result {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            let msg = "git binary not found on PATH".to_string();
                            self.summary.clone = StepStatus::Failed(msg.clone());
                            return Ok({
                                self.phase =
                                    ClawsPhase::Failed(ClawsFailure::Cloning { message: msg });
                                self.phase.clone()
                            });
                        }
                        Err(e) => {
                            let msg = format!("git clone: {e}");
                            self.summary.clone = StepStatus::Failed(msg.clone());
                            return Ok({
                                self.phase =
                                    ClawsPhase::Failed(ClawsFailure::Cloning { message: msg });
                                self.phase.clone()
                            });
                        }
                        Ok(status) if status.success() => true,
                        _ => {
                            // SSH failed — fall back to HTTPS.
                            let https_url = "https://github.com/prettysmartdev/nanoclaw.git";
                            std::process::Command::new("git")
                                .args(["clone", https_url, clone_dir_str])
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::piped())
                                .status()
                                .map(|s| s.success())
                                .unwrap_or(false)
                        }
                    }
                };

                if clone_ok {
                    self.summary.clone = StepStatus::Done;
                    // Issue 20: set permissive permissions after successful clone.
                    let _ = std::process::Command::new("chmod")
                        .args(["-R", "u+rwX", clone_dir_str])
                        .status();
                    // Issue 21: download nanoclaw-specific Dockerfile and write
                    // as Dockerfile.dev in the clone directory.
                    download_nanoclaw_dockerfile(&self.options.clone_dir);
                } else {
                    let msg = "git clone failed via both SSH and HTTPS".to_string();
                    self.summary.clone = StepStatus::Failed(msg);
                }
                ClawsPhase::CheckingPermissions
            }
            (ClawsPhase::CheckingPermissions, _) => {
                // Inspect whether the resolved clone_dir is writable by the
                // current user. If yes, the step is Done with no prompts. If
                // not, surface the sudo commands we'd need to chown/chmod and
                // ask the frontend to confirm — non-TTY frontends decline, the
                // step is Skipped, and the build phase will likely fail with a
                // clearer permission error.
                let writable = check_clone_dir_writable(&self.options.clone_dir);
                if writable {
                    self.summary.permissions_check = StepStatus::Done;
                } else {
                    let user = std::env::var("USER").unwrap_or_else(|_| "$USER".into());
                    let needed = vec![
                        format!(
                            "sudo chown -R {user} {}",
                            self.options.clone_dir.display()
                        ),
                        format!(
                            "sudo chmod -R u+rwX {}",
                            self.options.clone_dir.display()
                        ),
                    ];
                    match frontend.confirm_sudo_actions(&needed)? {
                        true => {
                            // Issue 19: actually execute sudo chown + chmod
                            // to fix permissions on the clone directory.
                            let clone_path_str =
                                self.options.clone_dir.to_str().unwrap_or("");
                            // Resolve uid:gid via `id` commands (avoids
                            // `unsafe` libc calls forbidden by the crate).
                            let uid_str = std::process::Command::new("id")
                                .arg("-u")
                                .output()
                                .map(|o| {
                                    String::from_utf8_lossy(&o.stdout)
                                        .trim()
                                        .to_string()
                                })
                                .unwrap_or_else(|_| user.clone());
                            let gid_str = std::process::Command::new("id")
                                .arg("-g")
                                .output()
                                .map(|o| {
                                    String::from_utf8_lossy(&o.stdout)
                                        .trim()
                                        .to_string()
                                })
                                .unwrap_or_else(|_| uid_str.clone());
                            let chown_status = std::process::Command::new("sudo")
                                .args([
                                    "chown",
                                    "-R",
                                    &format!("{}:{}", uid_str, gid_str),
                                    clone_path_str,
                                ])
                                .status();
                            if let Ok(s) = chown_status {
                                if !s.success() {
                                    return Err(EngineError::Other(
                                        "sudo chown failed".into(),
                                    ));
                                }
                            }
                            let chmod_status = std::process::Command::new("sudo")
                                .args(["chmod", "-R", "u+rwX", clone_path_str])
                                .status();
                            if let Ok(s) = chmod_status {
                                if !s.success() {
                                    return Err(EngineError::Other(
                                        "sudo chmod failed".into(),
                                    ));
                                }
                            }
                            self.summary.permissions_check = StepStatus::Done;
                        }
                        false => {
                            self.summary.permissions_check = StepStatus::Skipped;
                        }
                    }
                }
                ClawsPhase::BuildingImage
            }
            (ClawsPhase::BuildingImage, _) => {
                use crate::data::claws_paths::claws_image_tag;
                let dockerfile_dev = self.options.clone_dir.join("Dockerfile.dev");
                let dockerfile_plain = self.options.clone_dir.join("Dockerfile");
                let dockerfile = if dockerfile_dev.exists() {
                    dockerfile_dev
                } else {
                    dockerfile_plain
                };
                let tag = claws_image_tag(self.session.git_root());
                if dockerfile.exists() {
                    let mut sink = |line: &str| {
                        frontend.report_step_status(line, StepStatus::Running);
                    };
                    match self.container_runtime.build_image(
                        &tag,
                        &dockerfile,
                        &self.options.clone_dir,
                        self.options.no_cache,
                        &mut sink,
                    ) {
                        Ok(()) => self.summary.image_build = StepStatus::Done,
                        Err(e) => {
                            self.summary.image_build = StepStatus::Failed(e.to_string())
                        }
                    }
                } else {
                    self.summary.image_build =
                        StepStatus::Failed("nanoclaw Dockerfile missing".into());
                }
                ClawsPhase::AwaitingAuditDecision
            }
            (ClawsPhase::AwaitingAuditDecision, _) => {
                if frontend.ask_run_audit()? {
                    ClawsPhase::RunningAudit
                } else {
                    self.summary.audit = StepStatus::Skipped;
                    ClawsPhase::Configuring
                }
            }
            (ClawsPhase::RunningAudit, _) => {
                use crate::data::claws_paths::claws_image_tag;
                let tag = claws_image_tag(self.session.git_root());
                // Issue 22: Run the audit container with a seeded prompt
                // (matching old-amux behaviour). The prompt instructs the
                // agent to audit the nanoclaw environment. Failure is
                // non-fatal — a failed audit doesn't block the rest of
                // the init flow but is surfaced in the summary.
                let cf = frontend.container_frontend();
                let agent_name = self
                    .session
                    .effective_config()
                    .agent()
                    .unwrap_or_else(|| "claude".to_string());
                let entrypoint = chat_entrypoint_for(&agent_name);
                let mut args = vec![
                    "run".to_string(),
                    "--rm".to_string(),
                    "-i".to_string(),
                    tag.clone(),
                ];
                args.extend(entrypoint);
                args.push(CLAWS_AUDIT_PROMPT.to_string());
                let status = std::process::Command::new("docker")
                    .args(&args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .status();
                drop(cf);
                match status {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        self.summary.audit = StepStatus::Failed(
                            EngineError::ContainerRuntimeUnavailable {
                                binary: "docker".into(),
                            }
                            .to_string(),
                        );
                    }
                    Err(e) => {
                        self.summary.audit =
                            StepStatus::Failed(format!("docker run audit: {e}"));
                    }
                    Ok(s) if s.success() => self.summary.audit = StepStatus::Done,
                    Ok(s) => {
                        self.summary.audit = StepStatus::Failed(format!(
                            "audit exited with code {}",
                            s.code().unwrap_or(-1)
                        ))
                    }
                }
                ClawsPhase::Configuring
            }
            (ClawsPhase::Configuring, _) => {
                use crate::data::claws_paths::{
                    claws_clone_path, claws_config_path, claws_controller_name,
                };
                if let Some(home) = dirs::home_dir() {
                    let _ = std::fs::create_dir_all(claws_clone_path(
                        &home,
                        self.session.git_root(),
                    ));
                    let cfg_path = claws_config_path(&home, self.session.git_root());
                    // Issue 23: persist container_name alongside git_root.
                    let controller_name = claws_controller_name(self.session.git_root());
                    let body = serde_json::json!({
                        "git_root": self.session.git_root(),
                        "version": 1,
                        "container_name": controller_name,
                    });
                    let _ = std::fs::write(
                        &cfg_path,
                        serde_json::to_string_pretty(&body).unwrap_or_default(),
                    );
                }
                self.summary.configure = StepStatus::Done;
                ClawsPhase::LaunchingController
            }
            (ClawsPhase::LaunchingController, _) => {
                use crate::data::claws_paths::{claws_controller_name, claws_image_tag};
                let tag = claws_image_tag(self.session.git_root());
                let controller_name = claws_controller_name(self.session.git_root());

                // Issue 26: warn the user about Docker socket access.
                frontend.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: "The nanoclaw controller will have access to the host Docker \
                           socket. This grants the container ability to manage other \
                           containers on this host."
                        .to_string(),
                });

                // If a stopped container of this name already exists, prefer
                // `docker start` over `run`. `--rm` would otherwise auto-remove
                // it; without `--rm`, a `docker run --name X` collides with
                // any existing-but-stopped container.
                let already_exists = std::process::Command::new("docker")
                    .args([
                        "ps",
                        "-a",
                        "--format",
                        "{{.Names}}",
                        "--filter",
                        &format!("name=^{controller_name}$"),
                    ])
                    .output()
                    .map(|o| {
                        o.status.success()
                            && String::from_utf8_lossy(&o.stdout)
                                .lines()
                                .any(|l| l.trim() == controller_name)
                    })
                    .unwrap_or(false);

                let spawn_result = if already_exists {
                    std::process::Command::new("docker")
                        .args(["start", &controller_name])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                } else {
                    // Forward host env vars the controller needs.
                    let mut cmd = std::process::Command::new("docker");
                    cmd.args([
                        "run",
                        "-d",
                        // No `--rm`: we want stopped controllers to be
                        // restartable via `docker start` per the Ready flow.
                        "--name",
                        &controller_name,
                        "--label",
                        "amux-claws=true",
                        "--label",
                        "amux=true",
                        // Mount the Docker socket so the controller can
                        // orchestrate child agent containers (matches legacy
                        // `oldsrc/commands/claws.rs::launch_controller`).
                        "-v",
                        "/var/run/docker.sock:/var/run/docker.sock",
                    ]);

                    // Issue 25: Forward credential env vars from multiple
                    // sources — hardcoded well-known keys, envPassthrough
                    // from the effective config, and keychain credentials.

                    // 1. Well-known credential env vars (superset of old list).
                    for name in [
                        "OPENAI_API_KEY",
                        "ANTHROPIC_API_KEY",
                        "GEMINI_API_KEY",
                        "GH_TOKEN",
                        "GITHUB_TOKEN",
                        "CLAUDE_CODE_OAUTH_TOKEN",
                        "CODEX_API_KEY",
                    ] {
                        if let Ok(v) = std::env::var(name) {
                            cmd.arg("-e").arg(format!("{name}={v}"));
                        }
                    }

                    // 2. envPassthrough from EffectiveConfig — forward any
                    //    user-configured env vars that are set on the host.
                    let passthrough_vars =
                        self.session.effective_config().env_passthrough();
                    for name in &passthrough_vars {
                        if let Ok(v) = std::env::var(name) {
                            cmd.arg("-e").arg(format!("{name}={v}"));
                        }
                    }

                    // 3. Keychain credentials (macOS: Claude OAuth token, etc.)
                    let eff_agent_name = self
                        .session
                        .effective_config()
                        .agent()
                        .unwrap_or_else(|| "claude".to_string());
                    if let Ok(agent) =
                        crate::data::session::AgentName::new(&eff_agent_name)
                    {
                        let keychain_creds =
                            crate::engine::auth::keychain::agent_keychain_credentials(
                                &agent,
                            );
                        for (key, val) in &keychain_creds {
                            cmd.arg("-e").arg(format!("{key}={val}"));
                        }
                    }

                    cmd.arg(&tag);
                    cmd.stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                };

                match spawn_result {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        self.summary.controller = StepStatus::Failed(
                            EngineError::ContainerRuntimeUnavailable {
                                binary: "docker".into(),
                            }
                            .to_string(),
                        );
                    }
                    Err(e) => {
                        let msg = format!("launch controller: {e}");
                        self.summary.controller = StepStatus::Failed(msg.clone());
                        return Ok({
                            self.phase = ClawsPhase::Failed(ClawsFailure::ImageBuild {
                                tag: tag.clone(),
                                message: msg,
                            });
                            self.phase.clone()
                        });
                    }
                    Ok(_child) => {
                        // Issue 23: also persist the container name in the
                        // config now that it has been launched.
                        if let Some(home) = dirs::home_dir() {
                            use crate::data::claws_paths::claws_config_path;
                            let cfg_path =
                                claws_config_path(&home, self.session.git_root());
                            let body = serde_json::json!({
                                "git_root": self.session.git_root(),
                                "version": 1,
                                "container_name": controller_name,
                            });
                            let _ = std::fs::write(
                                &cfg_path,
                                serde_json::to_string_pretty(&body)
                                    .unwrap_or_default(),
                            );
                        }
                        self.summary.controller = StepStatus::Done;
                    }
                }
                ClawsPhase::Complete
            }
            (ClawsPhase::AttachingChat, ClawsMode::Chat) => {
                use crate::data::claws_paths::claws_controller_name;
                use crate::data::session::AgentName;
                let controller_name = claws_controller_name(self.session.git_root());
                let agent_name = self
                    .session
                    .effective_config()
                    .agent()
                    .unwrap_or_else(|| "claude".to_string());
                let entrypoint = chat_entrypoint_for(&agent_name);
                let mut exec_args = vec![
                    "exec".to_string(),
                    "-it".to_string(),
                ];
                // Forward agent credentials into the exec session.
                if let Ok(agent) = AgentName::new(&agent_name) {
                    if let Ok(creds) = self.auth_engine.agent_keychain_credentials(&agent) {
                        for (k, v) in &creds.env_vars {
                            exec_args.push("-e".to_string());
                            exec_args.push(format!("{k}={v}"));
                        }
                    }
                }
                exec_args.push(controller_name.clone());
                exec_args.extend(entrypoint);
                let status = std::process::Command::new("docker")
                    .args(&exec_args)
                    .stdin(std::process::Stdio::inherit())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .status();
                match status {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        let msg = EngineError::ContainerRuntimeUnavailable {
                            binary: "docker".into(),
                        }
                        .to_string();
                        return Ok({
                            self.phase = ClawsPhase::Failed(ClawsFailure::ChatAttach {
                                controller: controller_name,
                                message: msg,
                            });
                            self.phase.clone()
                        });
                    }
                    Err(e) => {
                        let msg = format!("docker exec: {e}");
                        return Ok({
                            self.phase = ClawsPhase::Failed(ClawsFailure::ChatAttach {
                                controller: controller_name,
                                message: msg,
                            });
                            self.phase.clone()
                        });
                    }
                    Ok(s) if s.success() => {
                        // Successful chat session — controller stays running.
                        self.summary.controller = StepStatus::Done;
                    }
                    Ok(s) => {
                        let msg = format!(
                            "claws-chat exited with code {}",
                            s.code().unwrap_or(-1)
                        );
                        return Ok({
                            self.phase = ClawsPhase::Failed(ClawsFailure::ChatAttach {
                                controller: controller_name,
                                message: msg,
                            });
                            self.phase.clone()
                        });
                    }
                }
                ClawsPhase::Complete
            }
            (ClawsPhase::AttachingChat, _) => {
                // Only valid in Chat mode; for other modes this is a no-op.
                ClawsPhase::Complete
            }
            (ClawsPhase::Complete | ClawsPhase::Failed(_), _) => self.phase.clone(),
        };
        self.phase = next.clone();
        if matches!(self.phase, ClawsPhase::Complete | ClawsPhase::Failed(_)) {
            frontend.report_summary(&self.summary);
        }
        Ok(next)
    }

    pub async fn run_to_completion(
        &mut self,
        frontend: &mut dyn ClawsFrontend,
    ) -> Result<ClawsSummary, EngineError> {
        loop {
            let next = self.step(frontend).await?;
            if matches!(next, ClawsPhase::Complete | ClawsPhase::Failed(_)) {
                break;
            }
        }
        Ok(self.summary.clone())
    }
}

// ── Helper functions ─────────────────────────────────────────────────────────

/// Issue 24: Build the entrypoint command for a given agent name.
fn chat_entrypoint_for(agent: &str) -> Vec<String> {
    match agent {
        "claude" => vec!["claude".to_string()],
        "codex" => vec!["codex".to_string()],
        _ => vec![agent.to_string()],
    }
}

/// Issue 21: Download the nanoclaw Dockerfile template and write it as
/// `Dockerfile.dev` in the clone directory. If the download fails, check
/// if a `Dockerfile` or `Dockerfile.dev` already exists and use that.
fn download_nanoclaw_dockerfile(clone_dir: &std::path::Path) {
    let dockerfile_dev = clone_dir.join("Dockerfile.dev");

    // Attempt download via curl (available on most systems).
    let result = std::process::Command::new("curl")
        .args(["-fsSL", NANOCLAW_DOCKERFILE_URL, "-o"])
        .arg(&dockerfile_dev)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let download_ok = result.map(|s| s.success()).unwrap_or(false);

    if !download_ok {
        // Download failed — check if a usable Dockerfile already exists.
        if dockerfile_dev.exists() {
            // Already have Dockerfile.dev, nothing to do.
            return;
        }
        let dockerfile = clone_dir.join("Dockerfile");
        if dockerfile.exists() {
            // Copy Dockerfile to Dockerfile.dev as fallback.
            let _ = std::fs::copy(&dockerfile, &dockerfile_dev);
        }
    }
}

/// Check whether the current process can write to `dir` (or its parent if
/// `dir` doesn't yet exist). We test by attempting to create + remove a
/// dotfile rather than parsing mode bits, which is portable across Unix
/// permission models (POSIX, ACLs) and Windows.
fn check_clone_dir_writable(dir: &std::path::Path) -> bool {
    let probe_dir = if dir.exists() {
        dir.to_path_buf()
    } else {
        match dir.parent() {
            Some(p) => p.to_path_buf(),
            None => return false,
        }
    };
    if !probe_dir.exists() {
        // Try to create it; if that fails, treat as unwritable.
        if std::fs::create_dir_all(&probe_dir).is_err() {
            return false;
        }
    }
    let probe = probe_dir.join(format!(".amux-claws-perm-{}.tmp", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Result of querying docker for the state of an `amux-claws` controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControllerState {
    /// A controller is running (visible in `docker ps`).
    Running,
    /// A controller exists but is stopped (visible in `docker ps -a`).
    Stopped,
    /// No controller is registered with docker, OR docker isn't installed.
    Absent,
}

/// Best-effort `docker ps` query for an `amux-claws=true` labeled container.
/// Failures (missing docker, network errors) collapse to `Absent` so the
/// caller can prompt the user to initialize one.
fn query_claws_controller_state() -> ControllerState {
    use std::process::Command;
    // Running controllers first.
    let running = Command::new("docker")
        .args([
            "ps",
            "--filter",
            "label=amux-claws=true",
            "--format",
            "{{.Names}}",
        ])
        .output();
    if let Ok(out) = &running {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if !s.trim().is_empty() {
                return ControllerState::Running;
            }
        }
    } else {
        // Docker binary missing — treat as absent.
        return ControllerState::Absent;
    }
    // Then any (running or stopped) controllers.
    let any = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "label=amux-claws=true",
            "--format",
            "{{.Names}}",
        ])
        .output();
    if let Ok(out) = any {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if !s.trim().is_empty() {
                return ControllerState::Stopped;
            }
        }
    }
    ControllerState::Absent
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
    use crate::engine::container::frontend::{
        ContainerFrontend, ContainerProgress, ContainerStatus,
    };
    use crate::engine::message::{UserMessage, UserMessageSink};
    use crate::engine::overlay::OverlayEngine;
    use crate::engine::step_status::StepStatus;

    // ── Fake frontend ────────────────────────────────────────────────────────

    struct FakeClawsFrontend {
        replace_existing_clone: bool,
        run_audit: bool,
        container_frontend_call_count: usize,
    }

    impl FakeClawsFrontend {
        fn new(replace_existing_clone: bool, run_audit: bool) -> Self {
            Self {
                replace_existing_clone,
                run_audit,
                container_frontend_call_count: 0,
            }
        }
    }

    struct FakeContainerFrontend;
    impl UserMessageSink for FakeContainerFrontend {
        fn write_message(&mut self, _: UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    #[async_trait::async_trait]
    impl ContainerFrontend for FakeContainerFrontend {
        fn write_stdout(&mut self, _: &[u8]) -> Result<(), EngineError> {
            Ok(())
        }
        fn write_stderr(&mut self, _: &[u8]) -> Result<(), EngineError> {
            Ok(())
        }
        async fn read_stdin(&mut self, _: &mut [u8]) -> Result<usize, EngineError> {
            Ok(0)
        }
        fn report_status(&mut self, _: ContainerStatus) {}
        fn report_progress(&mut self, _: ContainerProgress) {}
        fn resize_pty(&mut self, _: u16, _: u16) {}
    }

    impl UserMessageSink for FakeClawsFrontend {
        fn write_message(&mut self, _: UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl ClawsFrontend for FakeClawsFrontend {
        fn ask_replace_existing_clone(&mut self, _path: &Path) -> Result<bool, EngineError> {
            Ok(self.replace_existing_clone)
        }

        fn ask_run_audit(&mut self) -> Result<bool, EngineError> {
            Ok(self.run_audit)
        }

        fn report_phase(&mut self, _phase: &ClawsPhase) {}

        fn report_step_status(&mut self, _step: &str, _status: StepStatus) {}

        fn container_frontend(&mut self) -> Box<dyn ContainerFrontend> {
            self.container_frontend_call_count += 1;
            Box::new(FakeContainerFrontend)
        }

        fn report_summary(&mut self, _: &ClawsSummary) {}

        fn confirm_sudo_actions(&mut self, _commands: &[String]) -> Result<bool, EngineError> {
            // Test default: approve so the permission step doesn't get
            // skipped for tests that don't care about that decision.
            Ok(true)
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_engine(mode: ClawsMode, clone_dir: std::path::PathBuf) -> ClawsEngine {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = StaticGitRootResolver::new(tmp.path());
        let session = Arc::new(
            crate::data::session::Session::open(
                tmp.path().to_path_buf(),
                &resolver,
                SessionOpenOptions::default(),
            )
            .unwrap(),
        );
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
        ));
        let runtime = Arc::new(crate::engine::container::ContainerRuntime::docker());
        let auth_paths = crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path());
        let headless_paths = crate::data::fs::headless_paths::HeadlessPaths::at_home(tmp.path());
        let auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(auth_paths, headless_paths));
        ClawsEngine::new(
            session,
            Arc::new(GitEngine::new()),
            overlay,
            runtime,
            auth_engine,
            ClawsEngineOptions {
                mode,
                nanoclaw_url: None,
                refresh: false,
                no_cache: false,
                clone_dir,
            },
        )
    }

    // ── Tests ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn init_mode_fresh_clone_runs_all_phases() {
        // clone_dir does not exist -> no AwaitingCloneDecision, goes straight to CloningRepo.
        let clone_dir = tempfile::tempdir().unwrap();
        let clone_path = clone_dir.path().join("nanoclaw"); // nonexistent subdir
        let mut engine = make_engine(ClawsMode::Init, clone_path);
        let mut frontend = FakeClawsFrontend::new(true, true);
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::Complete);
        // clone / image_build depend on git+docker availability; accept Done or Failed.
        assert!(matches!(
            summary.clone,
            StepStatus::Done | StepStatus::Failed(_)
        ));
        assert!(matches!(summary.permissions_check, StepStatus::Done));
        assert!(matches!(
            summary.image_build,
            StepStatus::Done | StepStatus::Failed(_)
        ));
        // Audit now shells docker; Done in environments with docker, Failed
        // (containing the runtime-unavailable message) when docker is missing.
        assert!(matches!(
            summary.audit,
            StepStatus::Done | StepStatus::Failed(_)
        ));
        assert!(matches!(summary.configure, StepStatus::Done));
        // controller depends on docker availability in the test environment.
        assert!(matches!(
            summary.controller,
            StepStatus::Done | StepStatus::Failed(_)
        ));
    }

    #[tokio::test]
    async fn awaiting_clone_decision_false_skips_clone() {
        // clone_dir exists -> triggers AwaitingCloneDecision.
        let clone_dir = tempfile::tempdir().unwrap();
        let mut engine = make_engine(ClawsMode::Init, clone_dir.path().to_path_buf());
        // Decline the clone replacement.
        let mut frontend = FakeClawsFrontend::new(false, true);
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::Complete);
        assert!(
            matches!(summary.clone, StepStatus::Skipped),
            "clone must be Skipped when user declines"
        );
        // Continues to permissions and beyond.
        assert!(matches!(summary.permissions_check, StepStatus::Done));
    }

    #[tokio::test]
    async fn awaiting_audit_decision_false_skips_audit() {
        let clone_dir = tempfile::tempdir().unwrap();
        let clone_path = clone_dir.path().join("nanoclaw");
        let mut engine = make_engine(ClawsMode::Init, clone_path);
        let mut frontend = FakeClawsFrontend::new(true, false); // decline audit
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::Complete);
        assert!(
            matches!(summary.audit, StepStatus::Skipped),
            "audit must be Skipped when declined"
        );
        assert!(matches!(summary.configure, StepStatus::Done));
    }

    #[tokio::test]
    async fn ready_mode_with_no_controller_and_decline_offer_init_skips_controller() {
        // No docker / no controller -> `query_claws_controller_state` returns
        // `Absent`. With the default `confirm_offer_init = false`, Ready
        // marks `controller = Skipped` and completes without launching.
        let clone_dir = tempfile::tempdir().unwrap();
        let mut engine = make_engine(ClawsMode::Ready, clone_dir.path().to_path_buf());
        let mut frontend = FakeClawsFrontend::new(true, true);
        let summary = engine.run_to_completion(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::Complete);
        assert!(matches!(summary.clone, StepStatus::Skipped));
        assert!(matches!(summary.permissions_check, StepStatus::Skipped));
        assert!(matches!(summary.image_build, StepStatus::Skipped));
        assert!(matches!(summary.audit, StepStatus::Skipped));
        assert!(matches!(summary.configure, StepStatus::Skipped));
        // With no docker / no controller and the offer-init prompt declined,
        // controller remains Skipped (not Done).
        assert!(matches!(summary.controller, StepStatus::Skipped));
    }

    #[tokio::test]
    async fn chat_mode_without_running_controller_fails_with_structured_error() {
        // No docker / no controller -> preflight transitions to
        // `Failed(ControllerNotRunning)`, never reaching AttachingChat.
        let clone_dir = tempfile::tempdir().unwrap();
        let mut engine = make_engine(ClawsMode::Chat, clone_dir.path().to_path_buf());
        let mut frontend = FakeClawsFrontend::new(true, true);
        let _ = engine.run_to_completion(&mut frontend).await.unwrap();
        match engine.phase() {
            ClawsPhase::Failed(ClawsFailure::ControllerNotRunning { hint }) => {
                assert!(
                    hint.contains("amux claws ready"),
                    "hint must point at `amux claws ready`: {hint}"
                );
            }
            other => panic!("expected Failed(ControllerNotRunning), got {other:?}"),
        }
        // Chat mode does NOT call container_frontend on the failure path.
        assert_eq!(
            frontend.container_frontend_call_count, 0,
            "Chat mode must not call container_frontend on failure"
        );
    }

    #[tokio::test]
    async fn each_phase_reachable_via_step_in_init_mode() {
        let clone_dir = tempfile::tempdir().unwrap();
        let clone_path = clone_dir.path().join("nanoclaw"); // doesn't exist -> no AwaitingCloneDecision
        let mut engine = make_engine(ClawsMode::Init, clone_path);
        let mut frontend = FakeClawsFrontend::new(true, true);
        assert_eq!(engine.phase(), &ClawsPhase::Preflight);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::CloningRepo);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::CheckingPermissions);
        engine.step(&mut frontend).await.unwrap();
        assert_eq!(engine.phase(), &ClawsPhase::BuildingImage);
    }
}
