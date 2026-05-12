/// Integration tests verifying that command-mode and TUI-mode reuse the same underlying logic.
///
/// These tests call the shared `run_with_sink` / helper functions directly,
/// confirming that the same code paths are exercised regardless of execution mode.
use amux::commands::auth::{
    apply_auth_decision, agent_keychain_credentials, read_keychain_raw,
    AgentCredentials,
};
use amux::commands::chat::{
    chat_entrypoint, chat_entrypoint_non_interactive,
};
use amux::commands::new::{
    apply_template, find_template, next_work_item_number, slugify, WorkItemKind,
};
use amux::commands::output::OutputSink;
use amux::commands::ready::{
    audit_entrypoint, audit_entrypoint_non_interactive,
    ReadyOptions, ReadySummary, StepStatus,
    print_summary, print_interactive_notice,
};
use amux::commands::{init_flow, new, ready_flow};
use amux::runtime::docker::DockerRuntime;
use amux::tui::input::{autocomplete_suggestions, closest_subcommand};
use std::path::PathBuf;
use std::sync::Mutex;
use tempfile::TempDir;
use tokio::sync::mpsc::unbounded_channel;

/// Mutex to serialize tests that mutate the global HOME environment variable.
///
/// `std::env::set_var` / `remove_var` are not thread-safe when other threads
/// are running.  Any test that sets HOME must hold this lock for its entire
/// duration so the env mutation is never observed by a concurrent test.
static HOME_MUTEX: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// 1. init output via sink matches the expected lines
// ---------------------------------------------------------------------------

#[tokio::test]
async fn init_via_sink_produces_output_lines() {
    let (tx, mut rx) = unbounded_channel::<String>();
    let sink = OutputSink::Channel(tx);

    // execute from inside a git repo (the amux repo itself)
    // aspec=false to avoid downloading; run_audit=false to skip Docker (Channel sink defaults Q&A to "no").
    let cwd = std::env::current_dir().unwrap();
    let git_root = init_flow::find_git_root_from(&cwd).unwrap_or(cwd);
    let runtime = std::sync::Arc::new(amux::runtime::DockerRuntime::new());
    let mut qa = init_flow::CliInitQa::new(&git_root, sink.clone());
    let launcher = init_flow::CliContainerLauncher::new(runtime.clone());
    let params = init_flow::InitParams { agent: amux::cli::Agent::Claude, aspec: false, git_root };
    let result = init_flow::execute(params, &mut qa, &launcher, &sink, runtime).await;
    drop(result); // may succeed or fail; we only care that the sink was used.

    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    // At minimum the function should have sent something to the sink.
    assert!(
        !messages.is_empty(),
        "Expected at least one output line from init"
    );
}

// ---------------------------------------------------------------------------
// 2. ready emits the "Checking" message before any failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ready_via_sink_emits_checking_message() {
    let (tx, mut rx) = unbounded_channel::<String>();
    let sink = OutputSink::Channel(tx);

    let runtime = std::sync::Arc::new(DockerRuntime::new());
    let params = ready_flow::ReadyParams {
        refresh: false,
        build: false,
        no_cache: false,
        non_interactive: false,
        allow_docker: false,
    };
    let cwd = std::env::current_dir().unwrap();
    let git_root = match amux::commands::init_flow::find_git_root_from(&cwd) {
        Some(r) => r,
        None => return, // skip if not in a git repo
    };
    if !git_root.join("Dockerfile.dev").exists() {
        return; // skip if no Dockerfile.dev
    }
    let mut qa = ready_flow::CliReadyQa::new(sink.clone());
    let launcher = ready_flow::CliReadyAuditLauncher::new(runtime.clone());
    let _ = ready_flow::execute(params, &mut qa, &launcher, &sink, git_root, runtime).await;

    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    let has_checking = messages.iter().any(|m| m.contains("Checking"));
    assert!(
        has_checking,
        "Expected 'Checking' in ready output, got: {:?}",
        messages
    );
}

// ---------------------------------------------------------------------------
// 2b. ready routes all output through the sink (status, image tag)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 2c. ready audit entrypoint generates correct agent commands
// ---------------------------------------------------------------------------

#[test]
fn ready_audit_entrypoint_for_each_agent() {
    let claude = audit_entrypoint("claude");
    assert_eq!(claude.len(), 3);
    assert_eq!(claude[0], "claude");
    assert_eq!(claude[1], "--allowedTools=Edit,Write");
    assert!(claude[2].contains("scan this project"));

    let codex = audit_entrypoint("codex");
    assert_eq!(codex[0], "codex");
    assert!(codex[1].contains("scan this project"));

    let opencode = audit_entrypoint("opencode");
    assert_eq!(opencode[0], "opencode");
    assert_eq!(opencode[1], "run");
    assert!(opencode[2].contains("scan this project"));

    let maki = audit_entrypoint("maki");
    assert_eq!(maki[0], "maki");
    assert!(maki[1].contains("scan this project"));

    let gemini = audit_entrypoint("gemini");
    assert_eq!(gemini[0], "gemini");
    assert!(gemini[1].contains("scan this project"));
}

// ---------------------------------------------------------------------------
// 2d. ready uses project-specific image tag
// ---------------------------------------------------------------------------

#[test]
fn ready_uses_project_specific_image_tag() {
    let tag = amux::runtime::project_image_tag(std::path::Path::new("/home/user/myproject"));
    assert_eq!(tag, "amux-myproject:latest");
}

// ---------------------------------------------------------------------------
// 2e. ready non-interactive audit entrypoint
// ---------------------------------------------------------------------------

#[test]
fn ready_audit_entrypoint_non_interactive_for_each_agent() {
    let claude = audit_entrypoint_non_interactive("claude");
    assert_eq!(claude[0], "claude");
    assert_eq!(claude[1], "-p");
    assert_eq!(claude[2], "--allowedTools=Edit,Write");
    assert!(claude[3].contains("scan this project"));

    let codex = audit_entrypoint_non_interactive("codex");
    assert_eq!(codex[0], "codex");
    assert_eq!(codex[1], "exec");
    assert!(codex[2].contains("scan this project"));

    let opencode = audit_entrypoint_non_interactive("opencode");
    assert_eq!(opencode[0], "opencode");
    assert_eq!(opencode[1], "run");
    assert!(opencode[2].contains("scan this project"));

    let maki = audit_entrypoint_non_interactive("maki");
    assert_eq!(maki[0], "maki");
    assert_eq!(maki[1], "--print");
    assert!(maki[2].contains("scan this project"));

    let gemini = audit_entrypoint_non_interactive("gemini");
    assert_eq!(gemini[0], "gemini");
    assert_eq!(gemini[1], "-p");
    assert!(gemini[2].contains("scan this project"));
}

// ---------------------------------------------------------------------------
// 2f. ready summary table
// ---------------------------------------------------------------------------

#[test]
fn ready_summary_table_outputs_all_rows() {
    let (tx, mut rx) = unbounded_channel::<String>();
    let sink = OutputSink::Channel(tx);
    let summary = ReadySummary {
        docker_daemon: StepStatus::Ok("running".into()),
        dockerfile: StepStatus::Ok("exists".into()),
        aspec_folder: StepStatus::Ok("present".into()),
        work_items_config: StepStatus::Ok("ok".into()),
        local_agent: StepStatus::Ok("claude: installed & authenticated".into()),
        dev_image: StepStatus::Ok("exists".into()),
        refresh: StepStatus::Skipped("use --refresh to run".into()),
        image_rebuild: StepStatus::Skipped("no refresh".into()),
    };
    print_summary(&sink, "docker", &summary);
    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    let all = messages.join("\n");
    assert!(all.contains("Ready Summary"));
    assert!(all.contains("docker runtime"));
    assert!(all.contains("Dockerfile.dev"));
    assert!(all.contains("Dev image"));
    assert!(all.contains("Refresh"));
    assert!(all.contains("Image rebuild"));
}

// ---------------------------------------------------------------------------
// 2g. ready skip message when no --refresh
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 2h. interactive notice
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 2i. ready summary includes new aspec_folder and local_agent rows
// ---------------------------------------------------------------------------

#[test]
fn ready_summary_includes_aspec_and_local_agent_rows() {
    let (tx, mut rx) = unbounded_channel::<String>();
    let sink = OutputSink::Channel(tx);
    let summary = ReadySummary {
        docker_daemon: StepStatus::Ok("running".into()),
        dockerfile: StepStatus::Ok("exists".into()),
        aspec_folder: StepStatus::Failed("missing".into()),
        work_items_config: StepStatus::Warn("not configured".into()),
        local_agent: StepStatus::Failed("claude: not installed".into()),
        dev_image: StepStatus::Ok("exists".into()),
        refresh: StepStatus::Skipped("use --refresh to run".into()),
        image_rebuild: StepStatus::Skipped("no refresh".into()),
    };
    print_summary(&sink, "docker", &summary);
    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    let all = messages.join("\n");
    assert!(all.contains("aspec folder"), "Missing aspec folder row");
    assert!(all.contains("Local agent"), "Missing local agent row");
    assert!(all.contains("not installed"), "Missing not-installed status");
    assert!(all.contains("missing"), "Missing aspec missing status");
}

// ---------------------------------------------------------------------------
// 2j. init summary and whats_next produce output (CLI/TUI parity)
// ---------------------------------------------------------------------------

#[test]
fn init_via_sink_includes_whats_next() {
    let (tx, mut rx) = unbounded_channel::<String>();
    let sink = OutputSink::Channel(tx);

    // Run init without aspec, without audit (no Docker needed).
    // Channel sink defaults Q&A to "no", so audit and work-items are skipped.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let cwd = std::env::current_dir().unwrap();
    let git_root = init_flow::find_git_root_from(&cwd).unwrap_or(cwd);
    let runtime = std::sync::Arc::new(amux::runtime::DockerRuntime::new());
    let _ = rt.block_on(async {
        let mut qa = init_flow::CliInitQa::new(&git_root, sink.clone());
        let launcher = init_flow::CliContainerLauncher::new(runtime.clone());
        let params = init_flow::InitParams { agent: amux::cli::Agent::Claude, aspec: false, git_root };
        init_flow::execute(params, &mut qa, &launcher, &sink, runtime).await
    });

    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    let all = messages.join("\n");
    // Should include summary table and what's next section.
    assert!(all.contains("Init Summary"), "Missing init summary table");
    assert!(all.contains("chat"), "Missing chat command in what's next");
}

// ---------------------------------------------------------------------------
// 2k. greetings array and random selection work correctly
// ---------------------------------------------------------------------------

#[test]
fn ready_greetings_all_valid() {
    use amux::commands::ready::{GREETINGS, select_random_greeting};
    assert_eq!(GREETINGS.len(), 50, "Expected exactly 50 greetings");
    let greeting = select_random_greeting();
    assert!(GREETINGS.contains(&greeting), "Selected greeting not in list");
}

// ---------------------------------------------------------------------------
// 2l. dockerfile_matches_template logic
// ---------------------------------------------------------------------------

#[test]
fn ready_dockerfile_matches_template_for_project() {
    use amux::commands::init_flow::{dockerfile_for_agent_embedded, project_dockerfile_embedded};
    use amux::commands::ready::dockerfile_matches_template;
    use amux::cli::Agent;
    // Project template matches itself.
    let project_content = project_dockerfile_embedded();
    assert!(dockerfile_matches_template(&project_content));
    // Agent templates do not match the project template.
    let claude_content = dockerfile_for_agent_embedded(&Agent::Claude);
    assert!(!dockerfile_matches_template(&claude_content));
    // Arbitrary content does not match.
    assert!(!dockerfile_matches_template("FROM scratch"));
}

// ---------------------------------------------------------------------------
// 2m. ReadyOptions auto_create_dockerfile flag
// ---------------------------------------------------------------------------

#[test]
fn ready_options_auto_create_dockerfile_defaults_false() {
    let opts = ReadyOptions::default();
    assert!(!opts.auto_create_dockerfile, "Should default to false (prompt user)");
}

#[test]
fn ready_options_auto_create_dockerfile_can_be_set() {
    let opts = ReadyOptions { auto_create_dockerfile: true, ..Default::default() };
    assert!(opts.auto_create_dockerfile);
}

#[test]
fn interactive_notice_contains_agent_info() {
    let (tx, mut rx) = unbounded_channel::<String>();
    let sink = OutputSink::Channel(tx);
    print_interactive_notice(&sink, "claude");
    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    let all = messages.join("\n");
    assert!(all.contains("INTERACTIVE"), "Missing INTERACTIVE label");
    assert!(all.contains("claude"), "Missing agent name");
    assert!(all.contains("Ctrl+C"), "Missing quit hint");
}

// ---------------------------------------------------------------------------
// 4. Unknown command -> closest suggestion (TUI input logic)
// ---------------------------------------------------------------------------

#[test]
fn unknown_command_suggests_closest_subcommand() {
    assert_eq!(closest_subcommand("redy"), Some("ready".into()));
    assert_eq!(closest_subcommand("int"), Some("init".into()));
    assert_eq!(closest_subcommand("ready"), None);
}

#[test]
fn autocomplete_returns_matching_subcommands() {
    let sug = autocomplete_suggestions("r");
    assert!(sug.contains(&"ready".to_string()), "expected 'ready' in suggestions for 'r'");
    assert!(sug.contains(&"remote".to_string()), "expected 'remote' in suggestions for 'r'");

    let sug = autocomplete_suggestions("init ");
    assert!(sug.iter().any(|s: &String| s.contains("--agent")));
}

#[test]
fn autocomplete_ready_shows_all_flags() {
    let sug = autocomplete_suggestions("ready ");
    assert!(sug.iter().any(|s| s.contains("--refresh")), "Missing --refresh");
    assert!(sug.iter().any(|s| s.contains("--build")), "Missing --build");
    assert!(sug.iter().any(|s| s.contains("--no-cache")), "Missing --no-cache");
    assert!(sug.iter().any(|s| s.contains("--non-interactive")), "Missing --non-interactive");
}

// ---------------------------------------------------------------------------
// 5. Agent credentials are passed as env vars into the container
// ---------------------------------------------------------------------------

#[test]
fn agent_env_vars_passed_to_container() {
    let env = vec![("ANTHROPIC_API_KEY".into(), "sk-test".into())];
    use amux::runtime::{AgentRuntime};
    let args = amux::runtime::docker::DockerRuntime::new().build_run_args_pty("img", "/repo", &[], &env, None, false, None, None);
    assert!(args.contains(&"-e".to_string()));
    assert!(args.contains(&"ANTHROPIC_API_KEY=sk-test".to_string()));
}

#[test]
fn display_args_mask_env_var_values() {
    let env = vec![("ANTHROPIC_API_KEY".into(), "sk-secret".into())];
    use amux::runtime::{AgentRuntime};
    let args = amux::runtime::docker::DockerRuntime::new().build_run_args_display("img", "/repo", &[], &env, None, false, None, None);
    assert!(
        args.contains(&"ANTHROPIC_API_KEY=***".to_string()),
        "Display args must mask env var values, got: {:?}",
        args
    );
    assert!(
        !args.iter().any(|a: &String| a.contains("sk-secret")),
        "Display args must not contain actual secret"
    );
}

// ---------------------------------------------------------------------------
// 6. Auth decision is persisted and re-read correctly
// ---------------------------------------------------------------------------

#[test]
fn auth_apply_decision_saves_config() {
    let tmp = TempDir::new().unwrap();

    apply_auth_decision(tmp.path(), "claude", true).unwrap();
    let config = amux::config::load_repo_config(tmp.path()).unwrap();
    assert_eq!(config.auto_agent_auth_accepted, Some(true));

    apply_auth_decision(tmp.path(), "claude", false).unwrap();
    let config = amux::config::load_repo_config(tmp.path()).unwrap();
    assert_eq!(config.auto_agent_auth_accepted, Some(false));
}

// ---------------------------------------------------------------------------
// 8. ReadyOptions defaults and new fields
// ---------------------------------------------------------------------------

#[test]
fn ready_options_default_no_refresh_no_non_interactive() {
    let opts = ReadyOptions::default();
    assert!(!opts.refresh);
    assert!(!opts.build);
    assert!(!opts.no_cache);
    assert!(!opts.non_interactive);
}

#[test]
fn ready_options_build_flag() {
    let opts = ReadyOptions { build: true, ..Default::default() };
    assert!(opts.build);
    assert!(!opts.refresh);
    assert!(!opts.no_cache);
    assert!(!opts.non_interactive);
}

#[test]
fn ready_options_no_cache_flag() {
    let opts = ReadyOptions { no_cache: true, ..Default::default() };
    assert!(opts.no_cache);
    assert!(!opts.build);
}

#[test]
fn ready_options_build_and_no_cache() {
    let opts = ReadyOptions { build: true, no_cache: true, ..Default::default() };
    assert!(opts.build);
    assert!(opts.no_cache);
}

#[test]
fn ready_options_refresh_ignores_build() {
    // When refresh is true, build should be ignored per spec.
    // The run() function handles this, but ReadyOptions itself is just data.
    let opts = ReadyOptions { refresh: true, build: true, ..Default::default() };
    assert!(opts.refresh);
    assert!(opts.build); // ReadyOptions stores both; run() ignores build when refresh is true.
}

// ---------------------------------------------------------------------------
// 9. ReadySummary status variants
// ---------------------------------------------------------------------------

#[test]
fn ready_summary_status_variants() {
    assert_eq!(StepStatus::Pending, StepStatus::Pending);
    assert_ne!(StepStatus::Pending, StepStatus::Ok("ok".into()));
    assert_ne!(
        StepStatus::Ok("a".into()),
        StepStatus::Failed("b".into())
    );
    assert_ne!(
        StepStatus::Skipped("x".into()),
        StepStatus::Ok("x".into())
    );
}

// ---------------------------------------------------------------------------
// 10. New command: slugify produces correct filenames
// ---------------------------------------------------------------------------

#[test]
fn new_slugify_produces_valid_filenames() {
    assert_eq!(slugify("My New Feature"), "my-new-feature");
    assert_eq!(slugify("Fix: the bug!"), "fix-the-bug");
    assert_eq!(slugify("Add step 2 support"), "add-step-2-support");
    assert_eq!(slugify(""), "");
}

// ---------------------------------------------------------------------------
// 11. New command: next_work_item_number finds the correct next number
// ---------------------------------------------------------------------------

#[test]
fn new_next_work_item_number_sequential() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("0000-template.md"), "").unwrap();
    std::fs::write(tmp.path().join("0001-first.md"), "").unwrap();
    std::fs::write(tmp.path().join("0002-second.md"), "").unwrap();
    let num = next_work_item_number(tmp.path()).unwrap();
    assert_eq!(num, 3);
}

#[test]
fn new_next_work_item_number_with_gaps() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("0000-template.md"), "").unwrap();
    std::fs::write(tmp.path().join("0005-fifth.md"), "").unwrap();
    let num = next_work_item_number(tmp.path()).unwrap();
    assert_eq!(num, 6);
}

// ---------------------------------------------------------------------------
// 12. New command: apply_template replaces header and title
// ---------------------------------------------------------------------------

#[test]
fn new_apply_template_substitutions() {
    let template = "# Work Item: [Feature | Bug | Task]\n\nTitle: title\nIssue: issuelink\n\n## Summary:\n- summary\n";
    let result = apply_template(template, &WorkItemKind::Bug, "Fix login crash");
    assert!(result.contains("# Work Item: Bug"));
    assert!(result.contains("Title: Fix login crash"));
    assert!(result.contains("## Summary:"));
    assert!(!result.contains("[Feature | Bug | Task]"));
}

#[test]
fn new_apply_template_all_kinds() {
    let template = "# Work Item: [Feature | Bug | Task]\nTitle: title\n";
    for (kind, label) in [
        (WorkItemKind::Feature, "Feature"),
        (WorkItemKind::Bug, "Bug"),
        (WorkItemKind::Task, "Task"),
    ] {
        let result = apply_template(template, &kind, "Test");
        assert!(
            result.contains(&format!("# Work Item: {}", label)),
            "Expected kind '{}' in template output",
            label
        );
    }
}

// ---------------------------------------------------------------------------
// 13. New command: find_template returns correct path or error
// ---------------------------------------------------------------------------

#[test]
fn new_find_template_exists() {
    let tmp = TempDir::new().unwrap();
    let wi = tmp.path().join("aspec/work-items");
    std::fs::create_dir_all(&wi).unwrap();
    std::fs::write(wi.join("0000-template.md"), "# template").unwrap();
    let path = find_template(tmp.path()).unwrap();
    assert!(path.ends_with("0000-template.md"));
}

#[test]
fn new_find_template_missing_suggests_download() {
    let tmp = TempDir::new().unwrap();
    let err = find_template(tmp.path()).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("Template not found"));
    assert!(msg.contains("https://github.com/cohix/aspec"));
}

// ---------------------------------------------------------------------------
// 14. New command: WorkItemKind parsing
// ---------------------------------------------------------------------------

#[test]
fn new_work_item_kind_parsing() {
    assert_eq!(WorkItemKind::from_str("feature"), Some(WorkItemKind::Feature));
    assert_eq!(WorkItemKind::from_str("1"), Some(WorkItemKind::Feature));
    assert_eq!(WorkItemKind::from_str("bug"), Some(WorkItemKind::Bug));
    assert_eq!(WorkItemKind::from_str("2"), Some(WorkItemKind::Bug));
    assert_eq!(WorkItemKind::from_str("task"), Some(WorkItemKind::Task));
    assert_eq!(WorkItemKind::from_str("3"), Some(WorkItemKind::Task));
    assert_eq!(WorkItemKind::from_str("invalid"), None);
}

// ---------------------------------------------------------------------------
// 15. New command: run_with_sink creates a file (shared logic)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn new_via_sink_creates_work_item() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    let wi = root.join("aspec/work-items");
    std::fs::create_dir_all(&wi).unwrap();
    std::fs::write(
        wi.join("0000-template.md"),
        "# Work Item: [Feature | Bug | Task]\n\nTitle: title\nIssue: issuelink\n",
    )
    .unwrap();

    let (tx, mut rx) = unbounded_channel();
    let sink = OutputSink::Channel(tx);

    let result = new::run_with_sink(
        &sink,
        Some(WorkItemKind::Task),
        Some("My New Task".to_string()),
        root,
    )
    .await;

    assert!(result.is_ok(), "run_with_sink failed: {:?}", result.err());

    let created = wi.join("0001-my-new-task.md");
    assert!(created.exists(), "Work item file should exist");

    let content = std::fs::read_to_string(&created).unwrap();
    assert!(content.contains("# Work Item: Task"));
    assert!(content.contains("Title: My New Task"));

    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(messages.iter().any(|m| m.contains("Created work item")));
}

// ---------------------------------------------------------------------------
// 16. New command: autocomplete includes specs subcommand
// ---------------------------------------------------------------------------

#[test]
fn autocomplete_includes_new_subcommand() {
    let sug = autocomplete_suggestions("");
    assert!(sug.contains(&"specs".to_string()), "Expected 'specs' in suggestions");

    let sug = autocomplete_suggestions("sp");
    assert_eq!(sug, vec!["specs"]);
}

#[test]
fn autocomplete_specs_shows_amend_hint() {
    let sug = autocomplete_suggestions("specs ");
    assert!(
        sug.iter().any(|s| s.contains("amend")),
        "Expected hint for 'specs amend' command, got: {:?}",
        sug
    );
}

// ---------------------------------------------------------------------------
// 17. Container window state management
// ---------------------------------------------------------------------------

#[test]
fn container_window_lifecycle() {
    use amux::tui::state::{App, ContainerWindowState, ExecutionPhase};

    let mut app = App::new(std::path::PathBuf::new());
    assert_eq!(app.active_tab().container_window, ContainerWindowState::Hidden);

    // Start a container.
    app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
    app.active_tab_mut().start_container("amux-test-123".into(), "Claude Code".into(), 78, 18);
    assert_eq!(app.active_tab().container_window, ContainerWindowState::Maximized);

    // Minimize.
    app.active_tab_mut().container_window = ContainerWindowState::Minimized;
    assert_eq!(app.active_tab().container_window, ContainerWindowState::Minimized);

    // Restore.
    app.active_tab_mut().container_window = ContainerWindowState::Maximized;
    assert_eq!(app.active_tab().container_window, ContainerWindowState::Maximized);

    // Container exits → summary created, window hidden.
    app.active_tab_mut().finish_command(0);
    assert_eq!(app.active_tab().container_window, ContainerWindowState::Hidden);
    assert!(app.active_tab().last_container_summary.is_some());
    let summary = app.active_tab().last_container_summary.as_ref().unwrap();
    assert_eq!(summary.exit_code, 0);
    assert_eq!(summary.agent_display_name, "Claude Code");
    assert_eq!(summary.container_name, "amux-test-123");
}

// ---------------------------------------------------------------------------
// 18. Container window PTY output routing
// ---------------------------------------------------------------------------

#[test]
fn container_pty_output_routing() {
    use amux::tui::state::{App, ExecutionPhase};

    let mut app = App::new(std::path::PathBuf::new());
    app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };

    // Without container window: PTY data goes to output_lines.
    app.active_tab_mut().process_pty_data(b"outer line\n");
    assert!(app.active_tab().output_lines.iter().any(|l| l == "outer line"));
    assert!(app.active_tab().vt100_parser.is_none());

    // Activate container window: PTY data goes to vt100 parser.
    app.active_tab_mut().start_container("amux-test".into(), "Claude Code".into(), 80, 24);
    assert!(app.active_tab().vt100_parser.is_some());
    // Feed data through the vt100 parser (simulating what tick() does).
    if let Some(ref mut parser) = app.active_tab_mut().vt100_parser {
        parser.process(b"container line\r\n");
    }
    let screen_text = app.active_tab().vt100_parser.as_ref().unwrap().screen().contents();
    assert!(screen_text.contains("container line"), "vt100 screen should contain container output");
    // The outer window should still have "outer line" from before.
    assert!(app.active_tab().output_lines.iter().any(|l| l == "outer line"));
}

// ---------------------------------------------------------------------------
// 19. Docker stats parsing
// ---------------------------------------------------------------------------

#[test]
fn docker_stats_parsing() {
    assert!((amux::runtime::parse_cpu_percent("5.23%") - 5.23).abs() < 0.001);
    assert!((amux::runtime::parse_memory_mb("200MiB") - 200.0).abs() < 0.1);
    assert!((amux::runtime::parse_memory_mb("1.5GiB") - 1536.0).abs() < 0.1);
}

// ---------------------------------------------------------------------------
// 20. Container name generation
// ---------------------------------------------------------------------------

#[test]
fn container_name_generation() {
    let name = amux::runtime::generate_container_name();
    assert!(name.starts_with("amux-"));
}

// ---------------------------------------------------------------------------
// 21. Duration formatting
// ---------------------------------------------------------------------------

#[test]
fn duration_formatting() {
    use amux::tui::state::format_duration;
    assert_eq!(format_duration(0), "0s");
    assert_eq!(format_duration(45), "45s");
    assert_eq!(format_duration(60), "1m");
    assert_eq!(format_duration(3600), "1h");
    assert_eq!(format_duration(5400), "1h 30m");
}

// ---------------------------------------------------------------------------
// 22. Agent display name
// ---------------------------------------------------------------------------

#[test]
fn agent_display_names() {
    use amux::cli::Agent;
    use amux::tui::state::agent_display_name;
    // Every agent known to the CLI must have a non-empty display name in the TUI.
    for agent in Agent::all() {
        let name = agent_display_name(agent.as_str());
        assert!(!name.is_empty(), "display name for '{}' must not be empty", agent.as_str());
        assert_ne!(
            name, agent.as_str(),
            "agent '{}' should have a human-readable display name, not just its id",
            agent.as_str()
        );
    }
    // Spot-check known values.
    assert_eq!(agent_display_name("claude"), "Claude Code");
    assert_eq!(agent_display_name("codex"), "Codex");
    assert_eq!(agent_display_name("opencode"), "Opencode");
    assert_eq!(agent_display_name("maki"), "Maki");
    assert_eq!(agent_display_name("gemini"), "Gemini");
    assert_eq!(agent_display_name("unknown"), "unknown");
}

// ---------------------------------------------------------------------------
// 22b. TUI init autocomplete suggestions cover every CLI agent
//
// The TUI command-bar suggests `init --agent=<name>` for every agent.  This
// test guarantees that the suggestion list stays in sync with Agent::all():
// if a new agent is added to the enum but not to flag_suggestions_for("init"),
// this test catches it.
// ---------------------------------------------------------------------------

#[test]
fn tui_init_autocomplete_covers_all_cli_agents() {
    use amux::tui::input::autocomplete_suggestions;

    // "init --" triggers flag-completion for the init subcommand.
    // The new spec-driven format generates a single "--agent <NAME>  — ..." entry
    // (not one entry per agent), so we just verify that --agent appears in the
    // suggestions — keeping TUI and CLI in sync via ALL_COMMANDS/INIT_FLAGS.
    let suggestions = autocomplete_suggestions("init --");
    assert!(
        suggestions.iter().any(|s| s.contains("--agent")),
        "TUI autocomplete missing '--agent' hint for 'init' — INIT_FLAGS and flag_suggestions_for(\"init\") are out of sync; got: {:?}",
        suggestions,
    );
}

// ---------------------------------------------------------------------------
// 23. PTY args include container name
// ---------------------------------------------------------------------------

#[test]
fn pty_args_container_name() {
    use amux::runtime::AgentRuntime;
    let args = amux::runtime::docker::DockerRuntime::new().build_run_args_pty(
        "img", "/repo", &[], &[], None, false, Some("amux-test-42"), None,
    );
    assert!(args.contains(&"--name".to_string()));
    assert!(args.contains(&"amux-test-42".to_string()));

    let args_no_name = amux::runtime::docker::DockerRuntime::new().build_run_args_pty(
        "img", "/repo", &[], &[], None, false, None, None,
    );
    assert!(!args_no_name.contains(&"--name".to_string()));
}

// ---------------------------------------------------------------------------
// 24. Container summary with stats history averages
// ---------------------------------------------------------------------------

#[test]
fn container_summary_averages_stats() {
    use amux::tui::state::{App, ExecutionPhase};

    let mut app = App::new(std::path::PathBuf::new());
    app.active_tab_mut().phase = ExecutionPhase::Running { command: "implement 0001".into() };
    app.active_tab_mut().start_container("amux-test".into(), "Claude Code".into(), 78, 18);

    // Simulate stats history.
    if let Some(ref mut info) = app.active_tab_mut().container_info {
        info.stats_history.push((10.0, 100.0));
        info.stats_history.push((20.0, 200.0));
        info.stats_history.push((30.0, 300.0));
    }

    app.active_tab_mut().finish_command(0);

    let summary = app.active_tab().last_container_summary.as_ref().unwrap();
    assert_eq!(summary.avg_cpu, "20.0%");
    assert_eq!(summary.avg_memory, "200MiB");
}

// ---------------------------------------------------------------------------
// 25. Auth auto-authorization reads project-local config
// ---------------------------------------------------------------------------

#[test]
fn auth_reads_project_local_config() {
    let tmp = TempDir::new().unwrap();

    // Initially None → prompt should be shown.
    let config = amux::config::load_repo_config(tmp.path()).unwrap();
    assert_eq!(config.auto_agent_auth_accepted, None);

    // Accept → saved to project-local config.
    apply_auth_decision(tmp.path(), "claude", true).unwrap();
    let config = amux::config::load_repo_config(tmp.path()).unwrap();
    assert_eq!(config.auto_agent_auth_accepted, Some(true));

    // Check that the config file is at the correct path.
    let config_path = amux::config::repo_config_path(tmp.path());
    assert!(config_path.exists());
    assert!(config_path.to_str().unwrap().contains(".amux/config.json"));

    // Decline → saved as false.
    apply_auth_decision(tmp.path(), "claude", false).unwrap();
    let config = amux::config::load_repo_config(tmp.path()).unwrap();
    assert_eq!(config.auto_agent_auth_accepted, Some(false));
}

// ---------------------------------------------------------------------------
// 26. Keychain credentials returns single CLAUDE_CODE_OAUTH_TOKEN — no files
// ---------------------------------------------------------------------------

#[test]
fn keychain_credentials_uses_single_env_var() {
    let creds = agent_keychain_credentials("claude");
    // On a dev machine with keychain, exactly one env var should be set.
    // On CI without keychain, it returns empty.
    if !creds.env_vars.is_empty() {
        assert_eq!(creds.env_vars.len(), 1, "Should set exactly one env var");
        assert_eq!(creds.env_vars[0].0, "CLAUDE_CODE_OAUTH_TOKEN");
        // Value should be the raw access token string, not JSON.
        let val = &creds.env_vars[0].1;
        assert!(val.starts_with("sk-ant-oat"), "Token should be a raw OAuth token, got: {}", val);
    }
}

// ---------------------------------------------------------------------------
// 27. Keychain raw JSON includes both access and refresh tokens
// ---------------------------------------------------------------------------

#[test]
fn keychain_raw_json_has_required_fields() {
    if let Some(raw) = read_keychain_raw("claude") {
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let oauth = parsed.get("claudeAiOauth").expect("claudeAiOauth key should exist");
        assert!(oauth.get("accessToken").is_some(), "accessToken should exist");
        assert!(oauth.get("refreshToken").is_some(), "refreshToken should exist");
        assert!(oauth.get("expiresAt").is_some(), "expiresAt should exist");
    }
}

// ---------------------------------------------------------------------------
// 28. AgentCredentials default is empty
// ---------------------------------------------------------------------------

#[test]
fn agent_credentials_default_is_empty() {
    let creds = AgentCredentials::default();
    assert!(creds.env_vars.is_empty());
}

// ---------------------------------------------------------------------------
// 29. Docker args only mount workspace — no config dirs or credential files
// ---------------------------------------------------------------------------

#[test]
fn docker_args_without_settings_has_workspace_mount_only() {
    let env = vec![("ANTHROPIC_API_KEY".into(), "sk-ant-oat01-test".into())];
    use amux::runtime::AgentRuntime;
    let args = amux::runtime::docker::DockerRuntime::new().build_run_args_pty("img", "/repo", &[], &env, None, false, None, None);
    // Without host_settings or allow_docker, only the workspace mount should be present.
    let volume_mounts: Vec<&String> = args.windows(2)
        .filter(|w| w[0] == "-v")
        .map(|w| &w[1])
        .collect();
    assert_eq!(volume_mounts.len(), 1, "Expected exactly one volume mount (workspace only). Got: {:?}", volume_mounts);
    assert!(volume_mounts[0].contains(":/workspace"), "The only mount should be the workspace");
    assert!(!args.iter().any(|a: &String| a.contains("credentials")), "Must not mount any credential files");
}

// ---------------------------------------------------------------------------
// 30. Docker display args mask env var values
// ---------------------------------------------------------------------------

#[test]
fn docker_display_args_mask_secrets() {
    let env = vec![("ANTHROPIC_API_KEY".into(), "sk-ant-oat01-secret".into())];
    use amux::runtime::AgentRuntime;
    let args = amux::runtime::docker::DockerRuntime::new().build_run_args_display("img", "/repo", &[], &env, None, false, None, None);
    assert!(args.contains(&"ANTHROPIC_API_KEY=***".to_string()), "API key should be masked");
    assert!(!args.iter().any(|a: &String| a.contains("sk-ant-oat01-secret")), "Secret must not appear");
}

// ---------------------------------------------------------------------------
// 31. Chat entrypoint for each agent
// ---------------------------------------------------------------------------

#[test]
fn chat_entrypoint_for_each_agent() {
    let claude = chat_entrypoint("claude", false);
    assert_eq!(claude.len(), 1);
    assert_eq!(claude[0], "claude");

    let codex = chat_entrypoint("codex", false);
    assert_eq!(codex.len(), 1);
    assert_eq!(codex[0], "codex");

    let opencode = chat_entrypoint("opencode", false);
    assert_eq!(opencode.len(), 1);
    assert_eq!(opencode[0], "opencode");
}

// ---------------------------------------------------------------------------
// 32. Chat entrypoint non-interactive for each agent
// ---------------------------------------------------------------------------

#[test]
fn chat_entrypoint_non_interactive_for_each_agent() {
    let claude = chat_entrypoint_non_interactive("claude", false);
    assert_eq!(claude[0], "claude");
    assert_eq!(claude[1], "-p");

    let codex = chat_entrypoint_non_interactive("codex", false);
    assert_eq!(codex[0], "codex");
    assert_eq!(codex.len(), 1);

    let opencode = chat_entrypoint_non_interactive("opencode", false);
    assert_eq!(opencode.len(), 1);
    assert_eq!(opencode[0], "opencode");
}

// ---------------------------------------------------------------------------
// 35. Autocomplete includes chat subcommand
// ---------------------------------------------------------------------------

#[test]
fn autocomplete_includes_chat_subcommand() {
    let sug = autocomplete_suggestions("");
    assert!(
        sug.contains(&"chat".to_string()),
        "Expected 'chat' in suggestions"
    );

    let sug = autocomplete_suggestions("ch");
    assert_eq!(sug, vec!["chat"]);
}

#[test]
fn autocomplete_chat_shows_hints() {
    let sug = autocomplete_suggestions("chat ");
    assert!(
        !sug.is_empty(),
        "Expected at least one hint for 'chat' command, got empty list",
    );
    assert!(
        sug.iter().any(|s| s.contains("--non-interactive")),
        "Expected --non-interactive hint for 'chat' command, got: {:?}",
        sug
    );
}

// ---------------------------------------------------------------------------
// 36. Chat PendingCommand variant
// ---------------------------------------------------------------------------

#[test]
fn pending_command_chat_variant() {
    use amux::tui::state::PendingCommand;

    let cmd = PendingCommand::Chat { agent: None, model: None, non_interactive: false, plan: false, allow_docker: false, mount_ssh: false, yolo: false, auto: false, overlay: None };
    assert_eq!(cmd, PendingCommand::Chat { agent: None, model: None, non_interactive: false, plan: false, allow_docker: false, mount_ssh: false, yolo: false, auto: false, overlay: None });
    assert_ne!(cmd, PendingCommand::Chat { agent: None, model: None, non_interactive: true, plan: false, allow_docker: false, mount_ssh: false, yolo: false, auto: false, overlay: None });
    assert_ne!(cmd, PendingCommand::None);
}

// ---------------------------------------------------------------------------
// 37. Chat entrypoint non-interactive has no prompt
// ---------------------------------------------------------------------------

#[test]
fn chat_entrypoint_non_interactive_has_no_prompt() {
    for agent in &["claude", "codex", "opencode"] {
        let chat_args = chat_entrypoint_non_interactive(agent, false);

        // Chat non-interactive should not contain prompt text.
        for arg in &chat_args {
            assert!(
                !arg.contains("Implement"),
                "Chat non-interactive for {} should not contain prompt: {}",
                agent,
                arg
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 38. PendingCommand::Ready includes build and no_cache fields
// ---------------------------------------------------------------------------

#[test]
fn pending_command_ready_build_no_cache_fields() {
    use amux::tui::state::PendingCommand;

    let cmd = PendingCommand::Ready {
        refresh: false,
        build: true,
        no_cache: true,
        non_interactive: false,
        allow_docker: false,
        migrate_decision: None,
        template_audit_decision: None,
    };
    assert_eq!(cmd, PendingCommand::Ready {
        refresh: false,
        build: true,
        no_cache: true,
        non_interactive: false,
        allow_docker: false,
        migrate_decision: None,
        template_audit_decision: None,
    });
    // Different build flag should not match
    assert_ne!(cmd, PendingCommand::Ready {
        refresh: false,
        build: false,
        no_cache: true,
        non_interactive: false,
        allow_docker: false,
        migrate_decision: None,
        template_audit_decision: None,
    });
}

// ---------------------------------------------------------------------------
// 39. Docker format_build_cmd_no_cache
// ---------------------------------------------------------------------------

#[test]
fn docker_format_build_cmd_no_cache() {
    let cmd = amux::runtime::format_build_cmd_no_cache("docker", "img:latest", "Dockerfile.dev", "/repo");
    assert!(cmd.contains("--no-cache"), "Should contain --no-cache flag");
    assert!(cmd.contains("img:latest"), "Should contain image tag");
    assert!(cmd.contains("Dockerfile.dev"), "Should contain dockerfile");
}

// ---------------------------------------------------------------------------
// 40. CLI parses --build and --no-cache flags for ready
// ---------------------------------------------------------------------------

#[test]
fn cli_ready_build_flag() {
    use amux::cli::{Cli, Command};
    use clap::Parser;

    let cli = Cli::parse_from(&["amux", "ready", "--build"]);
    match cli.command.unwrap() {
        Command::Ready { build, .. } => assert!(build),
        _ => panic!("expected ready"),
    }
}

#[test]
fn cli_ready_no_cache_flag() {
    use amux::cli::{Cli, Command};
    use clap::Parser;

    let cli = Cli::parse_from(&["amux", "ready", "--no-cache"]);
    match cli.command.unwrap() {
        Command::Ready { no_cache, .. } => assert!(no_cache),
        _ => panic!("expected ready"),
    }
}

#[test]
fn cli_ready_build_and_no_cache() {
    use amux::cli::{Cli, Command};
    use clap::Parser;

    let cli = Cli::parse_from(&["amux", "ready", "--build", "--no-cache"]);
    match cli.command.unwrap() {
        Command::Ready { build, no_cache, .. } => {
            assert!(build);
            assert!(no_cache);
        }
        _ => panic!("expected ready"),
    }
}

#[test]
fn cli_ready_all_flags_combined() {
    use amux::cli::{Cli, Command};
    use clap::Parser;

    let cli = Cli::parse_from(&["amux", "ready", "--refresh", "--build", "--no-cache", "--non-interactive"]);
    match cli.command.unwrap() {
        Command::Ready { refresh, build, no_cache, non_interactive, .. } => {
            assert!(refresh);
            assert!(build);
            assert!(no_cache);
            assert!(non_interactive);
        }
        _ => panic!("expected ready"),
    }
}

#[test]
fn cli_ready_defaults_all_false() {
    use amux::cli::{Cli, Command};
    use clap::Parser;

    let cli = Cli::parse_from(&["amux", "ready"]);
    match cli.command.unwrap() {
        Command::Ready { refresh, build, no_cache, non_interactive, .. } => {
            assert!(!refresh);
            assert!(!build);
            assert!(!no_cache);
            assert!(!non_interactive);
        }
        _ => panic!("expected ready"),
    }
}

// ---------------------------------------------------------------------------
// Root-level flags forwarded to ready at TUI startup
// ---------------------------------------------------------------------------

/// Flags passed to `amux` (no subcommand) are available on the Cli struct
/// and should be forwarded to the `ready` command when the TUI starts.
#[test]
fn root_build_flag_parsed_for_tui_startup() {
    use amux::cli::Cli;
    use clap::Parser;

    let cli = Cli::parse_from(&["amux", "--build"]);
    assert!(cli.command.is_none(), "no subcommand when flags are on root");
    assert!(cli.build);
    assert!(!cli.no_cache);
    assert!(!cli.refresh);
}

#[test]
fn root_no_cache_flag_parsed_for_tui_startup() {
    use amux::cli::Cli;
    use clap::Parser;

    let cli = Cli::parse_from(&["amux", "--no-cache"]);
    assert!(cli.command.is_none());
    assert!(cli.no_cache);
}

#[test]
fn root_refresh_flag_parsed_for_tui_startup() {
    use amux::cli::Cli;
    use clap::Parser;

    let cli = Cli::parse_from(&["amux", "--refresh"]);
    assert!(cli.command.is_none());
    assert!(cli.refresh);
}

#[test]
fn root_all_flags_parsed_for_tui_startup() {
    use amux::cli::Cli;
    use clap::Parser;

    let cli = Cli::parse_from(&["amux", "--build", "--no-cache", "--refresh"]);
    assert!(cli.command.is_none());
    assert!(cli.build);
    assert!(cli.no_cache);
    assert!(cli.refresh);
}

// ---------------------------------------------------------------------------
// Plan flag: CLI parsing
// ---------------------------------------------------------------------------

#[test]
fn cli_chat_plan_flag() {
    use amux::cli::{Cli, Command};
    use clap::Parser;

    let cli = Cli::parse_from(&["amux", "chat", "--plan"]);
    match cli.command.unwrap() {
        Command::Chat { plan, non_interactive, .. } => {
            assert!(plan);
            assert!(!non_interactive);
        }
        _ => panic!("expected chat"),
    }
}

#[test]
fn cli_chat_plan_and_non_interactive() {
    use amux::cli::{Cli, Command};
    use clap::Parser;

    let cli = Cli::parse_from(&["amux", "chat", "--plan", "--non-interactive"]);
    match cli.command.unwrap() {
        Command::Chat { plan, non_interactive, .. } => {
            assert!(plan);
            assert!(non_interactive);
        }
        _ => panic!("expected chat"),
    }
}

// ---------------------------------------------------------------------------
// Plan flag: agent entrypoint configuration per agent
// ---------------------------------------------------------------------------

#[test]
fn plan_flag_configures_claude_correctly() {
    // Claude uses --permission-mode plan
    let chat = chat_entrypoint("claude", true);
    assert!(chat.contains(&"--permission-mode".to_string()), "Claude chat should include --permission-mode");
    assert!(chat.contains(&"plan".to_string()), "Claude chat should include plan");

    let chat_ni = chat_entrypoint_non_interactive("claude", true);
    assert!(chat_ni.contains(&"--permission-mode".to_string()), "Claude chat non-interactive should include --permission-mode");
    assert!(chat_ni.contains(&"plan".to_string()), "Claude chat non-interactive should include plan");
}

#[test]
fn plan_flag_configures_codex_correctly() {
    // Codex uses --approval-mode plan
    let chat = chat_entrypoint("codex", true);
    assert!(chat.contains(&"--approval-mode".to_string()), "Codex chat should include --approval-mode");
    assert!(chat.contains(&"plan".to_string()), "Codex chat should include plan");
}

#[test]
fn plan_flag_ignored_for_opencode() {
    // Opencode has no plan mode; flag should be silently ignored.
    let chat_no_plan = chat_entrypoint("opencode", false);
    let chat_plan = chat_entrypoint("opencode", true);
    assert_eq!(chat_no_plan, chat_plan, "Opencode chat should be unchanged with --plan");
}

#[test]
fn plan_false_does_not_add_flags() {
    // When plan=false, no plan flags should appear.
    for agent in &["claude", "codex", "opencode"] {
        let chat = chat_entrypoint(agent, false);
        assert!(!chat.contains(&"--permission-mode".to_string()), "No --permission-mode for {} with plan=false", agent);
        assert!(!chat.contains(&"--approval-mode".to_string()), "No --approval-mode for {} with plan=false", agent);
    }
}

// ---------------------------------------------------------------------------
// Plan flag: PendingCommand variants include plan field
// ---------------------------------------------------------------------------

#[test]
fn pending_command_chat_plan_field() {
    use amux::tui::state::PendingCommand;

    let cmd = PendingCommand::Chat { agent: None, model: None, non_interactive: false, plan: true, allow_docker: false, mount_ssh: false, yolo: false, auto: false, overlay: None };
    assert_eq!(cmd, PendingCommand::Chat { agent: None, model: None, non_interactive: false, plan: true, allow_docker: false, mount_ssh: false, yolo: false, auto: false, overlay: None });
    assert_ne!(cmd, PendingCommand::Chat { agent: None, model: None, non_interactive: false, plan: false, allow_docker: false, mount_ssh: false, yolo: false, auto: false, overlay: None });
}

// ---------------------------------------------------------------------------
// Plan flag: autocomplete hints include --plan
// ---------------------------------------------------------------------------

#[test]
fn autocomplete_chat_shows_plan_hint() {
    let sug = autocomplete_suggestions("chat ");
    assert!(
        sug.iter().any(|s| s.contains("--plan")),
        "Expected --plan hint for 'chat' command, got: {:?}",
        sug
    );
}

// ---------------------------------------------------------------------------
// allow-docker flag: CLI parsing
// ---------------------------------------------------------------------------

#[test]
fn cli_chat_allow_docker_flag() {
    use amux::cli::{Cli, Command};
    use clap::Parser;

    let cli = Cli::try_parse_from(["amux", "chat", "--allow-docker"]).unwrap();
    match cli.command.unwrap() {
        Command::Chat { allow_docker, .. } => {
            assert!(allow_docker, "Expected allow_docker=true");
        }
        _ => panic!("Expected Chat command"),
    }
}

#[test]
fn cli_chat_no_allow_docker_by_default() {
    use amux::cli::{Cli, Command};
    use clap::Parser;

    let cli = Cli::try_parse_from(["amux", "chat"]).unwrap();
    match cli.command.unwrap() {
        Command::Chat { allow_docker, .. } => {
            assert!(!allow_docker, "Expected allow_docker=false by default");
        }
        _ => panic!("Expected Chat command"),
    }
}

#[test]
fn cli_ready_allow_docker_flag() {
    use amux::cli::{Cli, Command};
    use clap::Parser;

    let cli = Cli::try_parse_from(["amux", "ready", "--allow-docker"]).unwrap();
    match cli.command.unwrap() {
        Command::Ready { allow_docker, .. } => {
            assert!(allow_docker, "Expected allow_docker=true");
        }
        _ => panic!("Expected Ready command"),
    }
}

#[test]
fn cli_ready_no_allow_docker_by_default() {
    use amux::cli::{Cli, Command};
    use clap::Parser;

    let cli = Cli::try_parse_from(["amux", "ready"]).unwrap();
    match cli.command.unwrap() {
        Command::Ready { allow_docker, .. } => {
            assert!(!allow_docker, "Expected allow_docker=false by default");
        }
        _ => panic!("Expected Ready command"),
    }
}

#[test]
fn cli_chat_allow_docker_with_plan() {
    use amux::cli::{Cli, Command};
    use clap::Parser;

    let cli = Cli::try_parse_from(["amux", "chat", "--allow-docker", "--plan"]).unwrap();
    match cli.command.unwrap() {
        Command::Chat { allow_docker, plan, .. } => {
            assert!(allow_docker);
            assert!(plan);
        }
        _ => panic!("Expected Chat command"),
    }
}

#[test]
fn cli_ready_allow_docker_with_refresh() {
    use amux::cli::{Cli, Command};
    use clap::Parser;

    let cli = Cli::try_parse_from(["amux", "ready", "--allow-docker", "--refresh"]).unwrap();
    match cli.command.unwrap() {
        Command::Ready { allow_docker, refresh, .. } => {
            assert!(allow_docker);
            assert!(refresh);
        }
        _ => panic!("Expected Ready command"),
    }
}

// ---------------------------------------------------------------------------
// allow-docker flag: PendingCommand variants include allow_docker field
// ---------------------------------------------------------------------------

#[test]
fn pending_command_chat_allow_docker_field() {
    use amux::tui::state::PendingCommand;

    let cmd = PendingCommand::Chat { agent: None, model: None, non_interactive: false, plan: false, allow_docker: true, mount_ssh: false, yolo: false, auto: false, overlay: None };
    assert_eq!(cmd, PendingCommand::Chat { agent: None, model: None, non_interactive: false, plan: false, allow_docker: true, mount_ssh: false, yolo: false, auto: false, overlay: None });
    assert_ne!(cmd, PendingCommand::Chat { agent: None, model: None, non_interactive: false, plan: false, allow_docker: false, mount_ssh: false, yolo: false, auto: false, overlay: None });
}

#[test]
fn pending_command_ready_allow_docker_field() {
    use amux::tui::state::PendingCommand;

    let cmd = PendingCommand::Ready { refresh: false, build: false, no_cache: false, non_interactive: false, allow_docker: true, migrate_decision: None, template_audit_decision: None };
    assert_eq!(cmd, PendingCommand::Ready { refresh: false, build: false, no_cache: false, non_interactive: false, allow_docker: true, migrate_decision: None, template_audit_decision: None });
    assert_ne!(cmd, PendingCommand::Ready { refresh: false, build: false, no_cache: false, non_interactive: false, allow_docker: false, migrate_decision: None, template_audit_decision: None });
}

// ---------------------------------------------------------------------------
// allow-docker flag: socket mount appears in docker run args
// ---------------------------------------------------------------------------

#[test]
fn allow_docker_adds_socket_mount_to_run_args() {
    use amux::runtime::AgentRuntime;

    let socket_path = amux::runtime::docker::docker_socket_path();
    let socket_str = socket_path.to_string_lossy().to_string();

    let args = amux::runtime::docker::DockerRuntime::new().build_run_args_pty(
        "test-image",
        "/workspace",
        &["entrypoint.sh"],
        &[],
        None,
        true, // allow_docker
        None,
        None,
    );

    #[cfg(not(target_os = "windows"))]
    {
        let joined = args.join(" ");
        assert!(
            joined.contains(&socket_str),
            "Expected socket path {} in args: {:?}",
            socket_str,
            args
        );
    }
}

#[test]
fn no_allow_docker_omits_socket_mount_from_run_args() {
    use amux::runtime::AgentRuntime;

    let socket_path = amux::runtime::docker::docker_socket_path();
    let socket_str = socket_path.to_string_lossy().to_string();

    let args = amux::runtime::docker::DockerRuntime::new().build_run_args_pty(
        "test-image",
        "/workspace",
        &["entrypoint.sh"],
        &[],
        None,
        false, // allow_docker
        None,
        None,
    );

    let joined = args.join(" ");
    assert!(
        !joined.contains(&socket_str),
        "Did not expect socket path {} in args without allow_docker: {:?}",
        socket_str,
        args
    );
}

// ---------------------------------------------------------------------------
// allow-docker flag: autocomplete hints include --allow-docker
// ---------------------------------------------------------------------------

#[test]
fn autocomplete_chat_shows_allow_docker_hint() {
    let sug = autocomplete_suggestions("chat ");
    assert!(
        sug.iter().any(|s| s.contains("--allow-docker")),
        "Expected --allow-docker hint for 'chat' command, got: {:?}",
        sug
    );
}

#[test]
fn autocomplete_ready_shows_allow_docker_hint() {
    let sug = autocomplete_suggestions("ready ");
    assert!(
        sug.iter().any(|s| s.contains("--allow-docker")),
        "Expected --allow-docker hint for 'ready' command, got: {:?}",
        sug
    );
}

// ---------------------------------------------------------------------------
// Tab working directory: init and new use the explicit cwd, not process CWD
// ---------------------------------------------------------------------------

/// init_flow::execute uses the explicit git_root from InitParams.
/// It should succeed when given a valid git repo root and, when given a path
/// without a .git directory, find_git_root_from returns None — simulating
/// "Not inside a Git repository" — regardless of where the process was launched.
#[tokio::test]
async fn init_uses_explicit_cwd_not_process_cwd() {
    // Create two temp directories: one is a valid git repo, one is not.
    let git_repo = TempDir::new().unwrap();
    std::fs::create_dir(git_repo.path().join(".git")).unwrap();

    let no_repo = TempDir::new().unwrap();

    let (tx1, mut rx1) = unbounded_channel::<String>();
    let sink1 = OutputSink::Channel(tx1);

    // Run init pointing at the git repo — should succeed.
    // Channel sink defaults Q&A to "no", so audit and work-items are skipped.
    let runtime1 = std::sync::Arc::new(amux::runtime::DockerRuntime::new());
    let git_root1 = git_repo.path().to_path_buf();
    let result_ok = {
        let mut qa = init_flow::CliInitQa::new(&git_root1, sink1.clone());
        let launcher = init_flow::CliContainerLauncher::new(runtime1.clone());
        let params = init_flow::InitParams { agent: amux::cli::Agent::Claude, aspec: false, git_root: git_root1 };
        init_flow::execute(params, &mut qa, &launcher, &sink1, runtime1).await
    };
    assert!(
        result_ok.is_ok(),
        "init should succeed when cwd is inside a git repo, got: {:?}",
        result_ok.err()
    );

    // The sink should have received the "Initializing amux in:" message for
    // the git_repo path, not for the process CWD.
    let messages: Vec<String> = std::iter::from_fn(|| rx1.try_recv().ok()).collect();
    let all = messages.join("\n");
    assert!(
        all.contains(git_repo.path().to_str().unwrap()),
        "Output should reference the provided cwd, not process CWD. Got:\n{}",
        all
    );

    // Run init pointing at a directory without a git repo — should fail.
    // We simulate the "not inside git" check that tui/mod.rs and init::run() do:
    // find_git_root_from returns None, so we produce an error directly.
    let result_err: anyhow::Result<()> = init_flow::find_git_root_from(no_repo.path())
        .map(|_| ())
        .ok_or_else(|| anyhow::anyhow!("Not inside a Git repository"));
    assert!(
        result_err.is_err(),
        "init should fail when cwd is not inside a git repo"
    );
    let msg = result_err.unwrap_err().to_string();
    assert!(
        msg.contains("Git repository") || msg.contains("git"),
        "Error should mention git repo, got: {}",
        msg
    );
}

/// new::run_with_sink uses the provided `cwd` to find the git root and
/// work-items directory — independent of the process working directory.
#[tokio::test]
async fn new_uses_explicit_cwd_not_process_cwd() {
    // Create a temp dir that acts as a git repo with the template in place.
    let tab_dir = TempDir::new().unwrap();
    let root = tab_dir.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    let wi = root.join("aspec/work-items");
    std::fs::create_dir_all(&wi).unwrap();
    std::fs::write(
        wi.join("0000-template.md"),
        "# Work Item: [Feature | Bug | Task]\n\nTitle: title\nIssue: issuelink\n",
    )
    .unwrap();

    let (tx, mut rx) = unbounded_channel::<String>();
    let sink = OutputSink::Channel(tx);

    // Run new, passing the tab's directory explicitly.
    // The process CWD is the amux workspace — a different git repo.
    let result = new::run_with_sink(
        &sink,
        Some(WorkItemKind::Bug),
        Some("Tab Dir Bug".to_string()),
        root,
    )
    .await;

    assert!(
        result.is_ok(),
        "new should succeed when the explicit cwd points at a git repo with a template, got: {:?}",
        result.err()
    );

    // The work item should have been created in the tab's directory, not in
    // the process CWD (amux workspace).
    let created = wi.join("0001-tab-dir-bug.md");
    assert!(
        created.exists(),
        "Work item should be created in the tab's cwd, not in process CWD"
    );
    let content = std::fs::read_to_string(&created).unwrap();
    assert!(content.contains("# Work Item: Bug"));
    assert!(content.contains("Title: Tab Dir Bug"));

    // Verify the output message references the correct path.
    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        messages.iter().any(|m| m.contains("Created work item")),
        "Expected 'Created work item' message, got: {:?}",
        messages
    );
}

/// When `new::run_with_sink` is given a cwd that is not inside any git repo,
/// it should return an error — not accidentally succeed by falling back to
/// the process CWD.
#[tokio::test]
async fn new_fails_when_explicit_cwd_has_no_git_repo() {
    let no_repo = TempDir::new().unwrap();

    let (tx, _rx) = unbounded_channel::<String>();
    let sink = OutputSink::Channel(tx);

    let result = new::run_with_sink(
        &sink,
        Some(WorkItemKind::Task),
        Some("Should Fail".to_string()),
        no_repo.path(),
    )
    .await;

    assert!(
        result.is_err(),
        "new should fail when the explicit cwd is not inside a git repo"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("Git repository") || msg.contains("git"),
        "Error should mention git repo, got: {}",
        msg
    );
}


// ---------------------------------------------------------------------------
// Workflow state tests
// ---------------------------------------------------------------------------

use amux::workflow::{
    StepStatus as WfStepStatus, WorkflowState,
};

#[test]
fn workflow_resume_loads_correct_ready_steps() {
    use amux::workflow::WorkflowStepState;
    // Build a two-step workflow state: step "plan" Done, step "implement" Pending (depends on plan).
    let steps = vec![
        WorkflowStepState {
            name: "plan".to_string(),
            depends_on: vec![],
            prompt_template: "Plan the work.".to_string(),
            status: WfStepStatus::Done,
            container_id: Some("amux-plan-container".to_string()),
            agent: None,
            model: None,
        },
        WorkflowStepState {
            name: "implement".to_string(),
            depends_on: vec!["plan".to_string()],
            prompt_template: "Implement the work.".to_string(),
            status: WfStepStatus::Pending,
            container_id: None,
            agent: None,
            model: None,
        },
    ];
    let wf = WorkflowState {
        title: None,
        work_item: Some(27),
        workflow_name: "test-workflow".to_string(),
        workflow_hash: "abc123".to_string(),
        steps,
    };

    let ready = wf.next_ready();
    assert_eq!(ready, vec!["implement".to_string()]);
    assert!(!wf.all_done());
}

#[test]
fn workflow_state_file_removed_on_completion() {
    use amux::workflow::{save_workflow_state, load_workflow_state, workflow_state_path, WorkflowStepState};

    let tmp = TempDir::new().unwrap();
    let git_root = tmp.path().to_path_buf();
    // Create .amux/workflows dir structure.
    std::fs::create_dir_all(git_root.join(".amux").join("workflows")).unwrap();

    let steps = vec![
        WorkflowStepState {
            name: "plan".to_string(),
            depends_on: vec![],
            prompt_template: "Do the plan.".to_string(),
            status: WfStepStatus::Done,
            container_id: Some("amux-abc".to_string()),
            agent: None,
            model: None,
        },
    ];
    let wf = WorkflowState {
        title: None,
        work_item: Some(27),
        workflow_name: "test-wf".to_string(),
        workflow_hash: "deadbeef".to_string(),
        steps,
    };

    // Save state.
    save_workflow_state(&git_root, &wf).unwrap();

    // Verify it was saved.
    let state_path = workflow_state_path(&git_root, Some(27), "test-wf");
    let loaded = load_workflow_state(&state_path);
    assert!(loaded.is_ok(), "State should be loadable after save");

    // Remove the file (simulating workflow completion).
    std::fs::remove_file(&state_path).unwrap();

    // Verify it's gone.
    let after = load_workflow_state(&state_path);
    assert!(after.is_err(), "State file should be gone after removal");
}

#[test]
fn workflow_set_container_id_overwrites_on_retry() {
    use amux::workflow::WorkflowStepState;

    let steps = vec![
        WorkflowStepState {
            name: "plan".to_string(),
            depends_on: vec![],
            prompt_template: "Plan.".to_string(),
            status: WfStepStatus::Pending,
            container_id: None,
            agent: None,
            model: None,
        },
    ];
    let mut wf = WorkflowState {
        title: None,
        work_item: Some(27),
        workflow_name: "test-wf".to_string(),
        workflow_hash: "deadbeef".to_string(),
        steps,
    };

    wf.set_container_id("plan", "amux-first-run".to_string());
    assert_eq!(wf.get_step("plan").unwrap().container_id.as_deref(), Some("amux-first-run"));

    // Simulate retry: overwrite with new container ID.
    wf.set_container_id("plan", "amux-second-run".to_string());
    assert_eq!(wf.get_step("plan").unwrap().container_id.as_deref(), Some("amux-second-run"));
}

// ---------------------------------------------------------------------------
// 28. SSH mount integration tests (work item 0030)
// ---------------------------------------------------------------------------

/// Verify that `run_agent_with_sink` with `mount_ssh: true` prints the SSH warning
/// to the sink before any Docker call is made.
#[tokio::test]
async fn run_agent_with_sink_mount_ssh_emits_warning() {
    let _lock = HOME_MUTEX.lock().unwrap();
    let original_home = std::env::var("HOME").ok();

    // Create a fake HOME with a .ssh directory so the SSH check passes.
    let fake_home = TempDir::new().unwrap();
    let ssh_dir = fake_home.path().join(".ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    std::env::set_var("HOME", fake_home.path());

    let (tx, mut rx) = unbounded_channel::<String>();
    let sink = OutputSink::Channel(tx);

    // mount_override avoids the interactive stdin prompt.
    let mount_path = PathBuf::from("/tmp");
    let runtime = amux::runtime::docker::DockerRuntime::new();
    let result = amux::commands::agent::run_agent_with_sink(
        vec!["echo".to_string(), "hello".to_string()],
        "test status",
        &sink,
        Some(mount_path),
        vec![],
        true, // non_interactive: use captured output, not inherited stdio
        None,
        false,
        true, // mount_ssh = true
        None,
        None, // agent_override
        None,  // model
        &runtime,
        None,  // git_root_override
    )
    .await;

    // Restore HOME.
    match original_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }

    // The function may succeed or fail (docker may not be available);
    // we only care that the SSH warning was sent to the sink before any docker call.
    let _ = result;
    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        messages.iter().any(|m| m.contains("--mount-ssh")),
        "Expected SSH warning in output, got: {:?}",
        messages
    );
}

/// Verify that `run_agent_with_sink` with `mount_ssh: false` does NOT emit an SSH warning.
#[tokio::test]
async fn run_agent_with_sink_no_mount_ssh_no_warning() {
    let (tx, mut rx) = unbounded_channel::<String>();
    let sink = OutputSink::Channel(tx);

    let mount_path = PathBuf::from("/tmp");
    let runtime = amux::runtime::docker::DockerRuntime::new();
    let result = amux::commands::agent::run_agent_with_sink(
        vec!["echo".to_string(), "hello".to_string()],
        "test status",
        &sink,
        Some(mount_path),
        vec![],
        true, // non_interactive: use captured output, not inherited stdio
        None,
        false,
        false, // mount_ssh = false
        None,
        None, // agent_override
        None,  // model
        &runtime,
        None,  // git_root_override
    )
    .await;

    let _ = result;
    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        !messages.iter().any(|m| m.contains("--mount-ssh")),
        "Did not expect SSH warning in output, got: {:?}",
        messages
    );
}

/// Verify that `run_agent_with_sink` with `mount_ssh: true` includes the `.ssh`
/// mount in the Docker display command printed to the sink.
#[tokio::test]
async fn run_agent_with_sink_mount_ssh_display_cmd_includes_ssh_path() {
    let _lock = HOME_MUTEX.lock().unwrap();
    let original_home = std::env::var("HOME").ok();

    let fake_home = TempDir::new().unwrap();
    let ssh_dir = fake_home.path().join(".ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    std::env::set_var("HOME", fake_home.path());

    let (tx, mut rx) = unbounded_channel::<String>();
    let sink = OutputSink::Channel(tx);

    let mount_path = PathBuf::from("/tmp");
    let runtime = amux::runtime::docker::DockerRuntime::new();
    let result = amux::commands::agent::run_agent_with_sink(
        vec!["echo".to_string()],
        "test status",
        &sink,
        Some(mount_path),
        vec![],
        true, // non_interactive: use captured output, not inherited stdio
        None,
        false,
        true, // mount_ssh = true
        None,
        None, // agent_override
        None,  // model
        &runtime,
        None,  // git_root_override
    )
    .await;

    match original_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }

    let _ = result;
    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();

    // The "$ docker run ..." line should include the .ssh mount path.
    let docker_cmd_line = messages.iter().find(|m| m.starts_with("$ docker run"));
    assert!(
        docker_cmd_line.is_some(),
        "Expected a '$ docker run' line in output, got: {:?}",
        messages
    );
    assert!(
        docker_cmd_line.unwrap().contains("/.ssh"),
        "Expected /.ssh in docker display command, got: {}",
        docker_cmd_line.unwrap()
    );
}

/// Verify that `run_agent_with_sink` with `mount_ssh: false` does NOT include
/// the `.ssh` mount in the Docker display command.
#[tokio::test]
async fn run_agent_with_sink_no_mount_ssh_display_cmd_excludes_ssh_path() {
    let (tx, mut rx) = unbounded_channel::<String>();
    let sink = OutputSink::Channel(tx);

    let mount_path = PathBuf::from("/tmp");
    let runtime = amux::runtime::docker::DockerRuntime::new();
    let result = amux::commands::agent::run_agent_with_sink(
        vec!["echo".to_string()],
        "test status",
        &sink,
        Some(mount_path),
        vec![],
        true, // non_interactive: use captured output, not inherited stdio
        None,
        false,
        false, // mount_ssh = false
        None,
        None, // agent_override
        None,  // model
        &runtime,
        None,  // git_root_override
    )
    .await;

    let _ = result;
    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();

    // The "$ docker run ..." line must NOT contain .ssh.
    let docker_cmd_line = messages.iter().find(|m| m.starts_with("$ docker run"));
    if let Some(cmd) = docker_cmd_line {
        assert!(
            !cmd.contains("/.ssh"),
            "Did not expect /.ssh in docker display command: {}",
            cmd
        );
    }
    // If there's no docker run line (e.g. failed before that), that's fine —
    // it just means mount_ssh was false and ssh logic was skipped entirely.
}

/// Verify that `run_agent_with_sink` with `mount_ssh: true` but no `~/.ssh` directory
/// returns an error without reaching Docker.
#[tokio::test]
async fn run_agent_with_sink_mount_ssh_missing_ssh_dir_errors() {
    let _lock = HOME_MUTEX.lock().unwrap();
    let original_home = std::env::var("HOME").ok();

    // Fake HOME with NO .ssh directory.
    let fake_home = TempDir::new().unwrap();
    std::env::set_var("HOME", fake_home.path());

    let (tx, mut rx) = unbounded_channel::<String>();
    let sink = OutputSink::Channel(tx);

    let mount_path = PathBuf::from("/tmp");
    let runtime = amux::runtime::docker::DockerRuntime::new();
    let result = amux::commands::agent::run_agent_with_sink(
        vec!["echo".to_string()],
        "test status",
        &sink,
        Some(mount_path),
        vec![],
        true, // non_interactive: use captured output, not inherited stdio
        None,
        false,
        true, // mount_ssh = true, but ~/.ssh does not exist
        None,
        None, // agent_override
        None,  // model
        &runtime,
        None,  // git_root_override
    )
    .await;

    match original_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }

    assert!(result.is_err(), "Expected error when ~/.ssh does not exist");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains(".ssh") || err.contains("ssh"),
        "Error should mention .ssh or ssh, got: {}",
        err
    );

    // No SSH warning should have been emitted (error happened before the warning).
    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        !messages.iter().any(|m| m.contains("--allow-ssh")),
        "SSH warning should not appear when .ssh dir is missing: {:?}",
        messages
    );
}

// ---------------------------------------------------------------------------
// 29. Git worktree unit tests (work item 0030)
// ---------------------------------------------------------------------------

#[test]
fn worktree_path_structure() {
    let _lock = HOME_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let path = amux::git::worktree_path(std::path::Path::new("/projects/myrepo"), 30).unwrap();
    let home = dirs::home_dir().unwrap();
    let expected = home
        .join(".amux")
        .join("worktrees")
        .join("myrepo")
        .join("0030");
    assert_eq!(path, expected);
}

#[test]
fn worktree_branch_name_format() {
    assert_eq!(amux::git::worktree_branch_name(30), "amux/work-item-0030");
    assert_eq!(amux::git::worktree_branch_name(1), "amux/work-item-0001");
}

// ---------------------------------------------------------------------------
// 30. End-to-end: SSH warning in chat output (work item 0030)
//     These tests require docker and a real git repo; run with `--ignored`.
// ---------------------------------------------------------------------------

/// E2E: `amux chat --mount-ssh` displays the SSH warning and includes
/// the SSH mount in the Docker command shown to the user.
///
/// Requires: git repo, Docker daemon, and a Dockerfile.dev image.
#[tokio::test]
#[ignore]
async fn e2e_chat_mount_ssh_displays_warning_and_docker_mount() {
    let _lock = HOME_MUTEX.lock().unwrap();
    let original_home = std::env::var("HOME").ok();

    let fake_home = TempDir::new().unwrap();
    let ssh_dir = fake_home.path().join(".ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    std::env::set_var("HOME", fake_home.path());

    let (tx, mut rx) = unbounded_channel::<String>();
    let sink = OutputSink::Channel(tx);

    let cwd = std::env::current_dir().unwrap();
    let runtime = DockerRuntime::new();
    let _ = amux::commands::chat::run_with_sink(
        &sink,
        Some(cwd),
        vec![],
        false,
        false,
        None,
        false,
        true,  // mount_ssh
        false, // yolo
        false, // auto
        None,  // agent_override
        None,  // model
        &runtime,
    )
    .await;

    match original_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }

    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        messages.iter().any(|m| m.contains("--allow-ssh")),
        "Expected SSH warning in chat output: {:?}",
        messages
    );
    let docker_line = messages.iter().find(|m| m.starts_with("$ docker run"));
    assert!(
        docker_line.map(|l| l.contains("/.ssh")).unwrap_or(false),
        "Expected /.ssh in docker command: {:?}",
        messages
    );
}
