//! Integration and end-to-end tests for the modular Dockerfiles feature (work item 0049).
//!
//! Tests are organised into two sections:
//! - Library API tests: call `write_project_dockerfile` / `write_agent_dockerfile` directly;
//!   no Docker daemon required.
//! - End-to-end tests: invoke the compiled `amux` binary with a temporary git repo;
//!   require a running Docker daemon and are skipped when Docker is unavailable.
use awman::cli::Agent;
use awman::commands::init_flow::{
    dockerfile_for_agent_embedded, project_dockerfile_embedded, write_agent_dockerfile,
    write_project_dockerfile,
};
use awman::commands::output::OutputSink;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;
use tokio::sync::mpsc::unbounded_channel;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn amux_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_awman"))
}

fn docker_available() -> bool {
    Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create a minimal git repo inside a temporary directory, return both the
/// `TempDir` guard (keep it alive) and the path to the project subdirectory.
///
/// The project subdirectory is named `name` so that the derived Docker image
/// tag is predictable: `amux-{name}:latest`.
fn make_git_repo(name: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let project_dir = tmp.path().join(name);
    std::fs::create_dir_all(&project_dir).unwrap();
    // Minimal .git directory — enough for `find_git_root_from` to recognise it.
    std::fs::create_dir(project_dir.join(".git")).unwrap();
    (tmp, project_dir)
}

// ─── Library API tests (no Docker needed) ────────────────────────────────────

/// `write_project_dockerfile` followed by `write_agent_dockerfile` creates both
/// expected files under the project root.
#[tokio::test]
async fn write_both_dockerfiles_creates_correct_files() {
    let (_tmp, project_dir) = make_git_repo("myproject");

    let (tx, _rx) = unbounded_channel();
    let out = OutputSink::Channel(tx);
    let created_project = write_project_dockerfile(&project_dir, &out).await.unwrap();
    assert!(created_project, "project dockerfile should be created");
    assert!(project_dir.join("Dockerfile.dev").exists());

    let (tx2, _rx2) = unbounded_channel();
    let out2 = OutputSink::Channel(tx2);
    let created_agent = write_agent_dockerfile(&project_dir, &Agent::Claude, &out2)
        .await
        .unwrap();
    assert!(created_agent, "agent dockerfile should be created");
    assert!(project_dir.join(".amux").join("Dockerfile.claude").exists());
}

/// `write_agent_dockerfile` creates `.amux/Dockerfile.claude` at the expected path.
#[tokio::test]
async fn write_agent_dockerfile_creates_file_at_expected_path() {
    let (_tmp, project_dir) = make_git_repo("testproject");

    let (tx, _rx) = unbounded_channel();
    let out = OutputSink::Channel(tx);
    let created = write_agent_dockerfile(&project_dir, &Agent::Claude, &out)
        .await
        .unwrap();

    assert!(created, "write_agent_dockerfile should return true for a new file");
    assert!(
        project_dir.join(".amux").join("Dockerfile.claude").exists(),
        ".amux/Dockerfile.claude must be created"
    );
}

/// The embedded agent template for "testproject" produces `FROM amux-testproject:latest`
/// once `{{AWMAN_BASE_IMAGE}}` is substituted.  Tests the substitution logic directly
/// without a network call so the assertion is not affected by the remote template version.
#[test]
fn embedded_agent_template_substitution_produces_correct_from_line() {
    use awman::runtime::project_image_tag;
    use std::path::Path;

    let base_tag = project_image_tag(Path::new("/repos/testproject"));
    assert_eq!(base_tag, "amux-testproject:latest");

    let template = dockerfile_for_agent_embedded(&Agent::Claude);
    assert!(
        template.contains("{{AWMAN_BASE_IMAGE}}"),
        "embedded claude template must contain {{AWMAN_BASE_IMAGE}} placeholder"
    );

    let content = template.replace("{{AWMAN_BASE_IMAGE}}", &base_tag);
    assert!(
        content.contains("FROM amux-testproject:latest"),
        "substituted template must have FROM amux-testproject:latest; got:\n{}",
        content
    );
    assert!(
        !content.contains("{{AWMAN_BASE_IMAGE}}"),
        "placeholder must not appear after substitution"
    );
}

/// `write_project_dockerfile` does not overwrite an existing `Dockerfile.dev`.
#[tokio::test]
async fn ready_write_project_dockerfile_no_overwrite() {
    let tmp = TempDir::new().unwrap();
    let custom = "# custom project Dockerfile\nFROM ubuntu:22.04\n";
    std::fs::write(tmp.path().join("Dockerfile.dev"), custom).unwrap();

    let (tx, _rx) = unbounded_channel();
    let out = OutputSink::Channel(tx);
    let result = write_project_dockerfile(tmp.path(), &out).await.unwrap();

    assert!(!result, "should not overwrite existing Dockerfile.dev");
    let content = std::fs::read_to_string(tmp.path().join("Dockerfile.dev")).unwrap();
    assert_eq!(content, custom, "Dockerfile.dev must not be modified");
}

/// `write_agent_dockerfile` does not overwrite an existing `.amux/Dockerfile.{agent}`.
#[tokio::test]
async fn ready_write_agent_dockerfile_no_overwrite() {
    let tmp = TempDir::new().unwrap();
    let amux_dir = tmp.path().join(".amux");
    std::fs::create_dir_all(&amux_dir).unwrap();
    let custom = "# custom agent Dockerfile\nFROM mybase:latest\n";
    std::fs::write(amux_dir.join("Dockerfile.claude"), custom).unwrap();

    let (tx, _rx) = unbounded_channel();
    let out = OutputSink::Channel(tx);
    let result = write_agent_dockerfile(tmp.path(), &Agent::Claude, &out)
        .await
        .unwrap();

    assert!(!result, "should not overwrite existing Dockerfile.claude");
    let content = std::fs::read_to_string(amux_dir.join("Dockerfile.claude")).unwrap();
    assert_eq!(content, custom, ".amux/Dockerfile.claude must not be modified");
}

/// Multiple agent Dockerfiles can coexist in `.amux/` — one per agent name.
/// Verifies the file-system state expected before `ready --build` with multiple agents.
#[tokio::test]
async fn multiple_agent_dockerfiles_coexist_in_amux_dir() {
    let (_tmp, project_dir) = make_git_repo("multiagent");

    let (tx, _rx) = unbounded_channel();
    let out = OutputSink::Channel(tx);
    write_agent_dockerfile(&project_dir, &Agent::Claude, &out)
        .await
        .unwrap();

    let (tx2, _rx2) = unbounded_channel();
    let out2 = OutputSink::Channel(tx2);
    write_agent_dockerfile(&project_dir, &Agent::Codex, &out2)
        .await
        .unwrap();

    let amux_dir = project_dir.join(".amux");
    // Both files must exist at the correct paths.
    assert!(
        amux_dir.join("Dockerfile.claude").exists(),
        ".amux/Dockerfile.claude should be created"
    );
    assert!(
        amux_dir.join("Dockerfile.codex").exists(),
        ".amux/Dockerfile.codex should be created"
    );
}

// ─── End-to-end tests (require Docker daemon) ────────────────────────────────

/// `amux ready` on a repo that already has both Dockerfiles but no images builds
/// both `amux-{project}:latest` and `amux-{project}-claude:latest`.
#[test]
fn e2e_ready_creates_both_docker_images() {
    if !docker_available() {
        eprintln!("Docker not available — skipping e2e_ready_creates_both_docker_images");
        return;
    }

    let (_tmp, project_dir) = make_git_repo("amuxtest0049a");
    let project_tag = "amux-amuxtest0049a:latest";
    let agent_tag = "amux-amuxtest0049a-claude:latest";

    // Pre-create Dockerfiles so `amux ready` skips all prompts.
    let project_content = project_dockerfile_embedded();
    std::fs::write(project_dir.join("Dockerfile.dev"), &project_content).unwrap();
    std::fs::create_dir_all(project_dir.join(".amux")).unwrap();
    let agent_template = dockerfile_for_agent_embedded(&Agent::Claude);
    let agent_content = agent_template.replace("{{AWMAN_BASE_IMAGE}}", project_tag);
    std::fs::write(
        project_dir.join(".amux").join("Dockerfile.claude"),
        &agent_content,
    )
    .unwrap();

    // Remove any pre-existing images from previous runs.
    let _ = Command::new("docker")
        .args(["rmi", "-f", project_tag, agent_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let output = amux_bin()
        .current_dir(&project_dir)
        .args(["ready"])
        .output()
        .expect("failed to invoke amux ready");

    // Verify images exist before clean-up so assertion messages are useful.
    let project_exists = Command::new("docker")
        .args(["image", "inspect", project_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let agent_exists = Command::new("docker")
        .args(["image", "inspect", agent_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    // Always clean up, even on failure.
    let _ = Command::new("docker")
        .args(["rmi", "-f", project_tag, agent_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    assert!(
        output.status.success(),
        "amux ready should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        project_exists,
        "project image {} should exist after amux ready",
        project_tag
    );
    assert!(
        agent_exists,
        "agent image {} should exist after amux ready",
        agent_tag
    );
}

/// `amux ready --build` forces a rebuild of both images even when they already exist.
#[test]
fn e2e_ready_build_flag_rebuilds_both_images() {
    if !docker_available() {
        eprintln!("Docker not available — skipping e2e_ready_build_flag_rebuilds_both_images");
        return;
    }

    let (_tmp, project_dir) = make_git_repo("amuxtest0049b");
    let project_tag = "amux-amuxtest0049b:latest";
    let agent_tag = "amux-amuxtest0049b-claude:latest";

    // Pre-create Dockerfiles.
    let project_content = project_dockerfile_embedded();
    std::fs::write(project_dir.join("Dockerfile.dev"), &project_content).unwrap();
    std::fs::create_dir_all(project_dir.join(".amux")).unwrap();
    let agent_template = dockerfile_for_agent_embedded(&Agent::Claude);
    let agent_content = agent_template.replace("{{AWMAN_BASE_IMAGE}}", project_tag);
    std::fs::write(
        project_dir.join(".amux").join("Dockerfile.claude"),
        &agent_content,
    )
    .unwrap();

    // Remove any pre-existing images.
    let _ = Command::new("docker")
        .args(["rmi", "-f", project_tag, agent_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // First run: build images from scratch.
    let first = amux_bin()
        .current_dir(&project_dir)
        .args(["ready"])
        .output()
        .expect("failed to invoke amux ready (first run)");

    // Second run with --build: forces rebuild of both images.
    let second = amux_bin()
        .current_dir(&project_dir)
        .args(["ready", "--build"])
        .output()
        .expect("failed to invoke amux ready --build");

    // Always clean up.
    let _ = Command::new("docker")
        .args(["rmi", "-f", project_tag, agent_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    assert!(
        first.status.success(),
        "first amux ready should succeed; stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "amux ready --build should succeed; stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        second_stdout.contains("Rebuilding") || second_stdout.contains("rebuilt"),
        "ready --build output should mention rebuilding; got: {}",
        second_stdout
    );
}

/// `amux ready --build` with multiple `.amux/Dockerfile.*` files rebuilds ALL agent images,
/// not just the configured default.
#[test]
fn e2e_ready_build_rebuilds_all_agent_dockerfiles() {
    if !docker_available() {
        eprintln!("Docker not available — skipping e2e_ready_build_rebuilds_all_agent_dockerfiles");
        return;
    }

    let (_tmp, project_dir) = make_git_repo("amuxtest0049c");
    let project_tag = "amux-amuxtest0049c:latest";
    let claude_tag = "amux-amuxtest0049c-claude:latest";
    let codex_tag = "amux-amuxtest0049c-codex:latest";

    // Pre-create project Dockerfile and TWO agent Dockerfiles (claude + codex).
    let project_content = project_dockerfile_embedded();
    std::fs::write(project_dir.join("Dockerfile.dev"), &project_content).unwrap();
    std::fs::create_dir_all(project_dir.join(".amux")).unwrap();

    let claude_template = dockerfile_for_agent_embedded(&Agent::Claude);
    let claude_content = claude_template.replace("{{AWMAN_BASE_IMAGE}}", project_tag);
    std::fs::write(
        project_dir.join(".amux").join("Dockerfile.claude"),
        &claude_content,
    )
    .unwrap();

    let codex_template = dockerfile_for_agent_embedded(&Agent::Codex);
    let codex_content = codex_template.replace("{{AWMAN_BASE_IMAGE}}", project_tag);
    std::fs::write(
        project_dir.join(".amux").join("Dockerfile.codex"),
        &codex_content,
    )
    .unwrap();

    // Clean up any pre-existing images.
    let _ = Command::new("docker")
        .args(["rmi", "-f", project_tag, claude_tag, codex_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // First run: build images from scratch (only configured agent — claude — is auto-built).
    let first = amux_bin()
        .current_dir(&project_dir)
        .args(["ready"])
        .output()
        .expect("amux ready (first run)");

    // Second run with --build: must rebuild ALL agent dockerfiles in .amux/.
    let second = amux_bin()
        .current_dir(&project_dir)
        .args(["ready", "--build"])
        .output()
        .expect("amux ready --build");

    // Verify both agent images exist after --build.
    let claude_exists = Command::new("docker")
        .args(["image", "inspect", claude_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let codex_exists = Command::new("docker")
        .args(["image", "inspect", codex_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    // Always clean up.
    let _ = Command::new("docker")
        .args(["rmi", "-f", project_tag, claude_tag, codex_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    assert!(
        first.status.success(),
        "first amux ready should succeed; stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "amux ready --build should succeed; stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        claude_exists,
        "claude agent image {} should exist after --build",
        claude_tag
    );
    assert!(
        codex_exists,
        "codex agent image {} should exist after --build; \
         --build must rebuild all .amux/Dockerfile.* files, not just the configured agent",
        codex_tag
    );
}

/// `amux chat --agent codex` accepts "codex" as a valid agent name — no CLI
/// parsing error is produced.  The command is expected to fail for other reasons
/// (missing image / not in a real working repo), but not due to an unknown agent.
#[test]
fn e2e_chat_agent_codex_accepted_as_valid_flag() {
    let (_tmp, project_dir) = make_git_repo("amuxtest0049d");

    // Create minimal config so agent selection is deterministic.
    let config_dir = project_dir.join(".amux");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.json"), r#"{"agent":"codex"}"#).unwrap();

    let output = amux_bin()
        .current_dir(&project_dir)
        .args(["chat", "--agent", "codex", "--non-interactive"])
        .output()
        .expect("failed to invoke amux chat");

    let stderr = String::from_utf8_lossy(&output.stderr);
    // "codex" must be recognised — no parsing or validation error.
    assert!(
        !stderr.contains("unrecognized") && !stderr.contains("unknown agent"),
        "codex should be accepted as a valid --agent value; stderr: {}",
        stderr
    );
}

/// A repo with `Dockerfile.dev` but no `.amux/Dockerfile.*` triggers the legacy
/// layout detection message in `amux ready` output.
#[test]
fn e2e_ready_legacy_layout_detection() {
    let (_tmp, project_dir) = make_git_repo("amuxtest0049e");

    // Only create Dockerfile.dev — NOT .amux/Dockerfile.claude.
    let project_content = project_dockerfile_embedded();
    std::fs::write(project_dir.join("Dockerfile.dev"), &project_content).unwrap();

    // Run with stdin closed so the migration prompt gets EOF → decline.
    let output = amux_bin()
        .current_dir(&project_dir)
        .args(["ready"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to invoke amux ready");

    let all_output = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        all_output.to_lowercase().contains("legacy")
            || all_output.to_lowercase().contains("migrat"),
        "amux ready should detect and report the legacy layout; output:\n{}",
        all_output
    );
}

/// When the user declines migration, `amux ready` prints a deprecation warning.
/// Dockerfile.dev must not be modified.
#[test]
fn e2e_ready_legacy_decline_preserves_dockerfile() {
    let (_tmp, project_dir) = make_git_repo("amuxtest0049f");

    let original = "# custom single-file Dockerfile\nFROM ubuntu:22.04\n";
    std::fs::write(project_dir.join("Dockerfile.dev"), original).unwrap();

    // EOF on stdin → decline.
    let output = amux_bin()
        .current_dir(&project_dir)
        .args(["ready"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to invoke amux ready");

    let all_output = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        all_output.to_lowercase().contains("deprecation")
            || all_output.to_lowercase().contains("legacy"),
        "decline should produce a deprecation/legacy warning; output:\n{}",
        all_output
    );
    let preserved = std::fs::read_to_string(project_dir.join("Dockerfile.dev")).unwrap();
    assert_eq!(
        preserved, original,
        "declining migration must not modify Dockerfile.dev"
    );
}

/// When the user accepts migration, `amux ready` backs up `Dockerfile.dev` to
/// `Dockerfile.dev.bak` before overwriting it.
#[test]
fn e2e_ready_migration_accept_creates_backup() {
    if !docker_available() {
        eprintln!("Docker not available — skipping e2e_ready_migration_accept_creates_backup");
        return;
    }

    let (_tmp, project_dir) = make_git_repo("amuxtest0049h");
    let project_tag = "amux-amuxtest0049h:latest";
    let agent_tag = "amux-amuxtest0049h-claude:latest";

    // Write a legacy single-file Dockerfile.dev with a recognizable marker.
    let legacy_content = "FROM ubuntu:22.04\n# legacy single-file Dockerfile\n";
    std::fs::write(project_dir.join("Dockerfile.dev"), legacy_content).unwrap();

    // Remove any pre-existing images.
    let _ = Command::new("docker")
        .args(["rmi", "-f", project_tag, agent_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Answer 'y' to the migration prompt via stdin.
    let output = amux_bin()
        .current_dir(&project_dir)
        .args(["ready"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(b"y\n");
            }
            child.wait_with_output()
        })
        .expect("failed to invoke amux ready");

    // Always clean up images.
    let _ = Command::new("docker")
        .args(["rmi", "-f", project_tag, agent_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Regardless of audit success, the backup must be present.
    let bak_path = project_dir.join("Dockerfile.dev.bak");
    assert!(
        bak_path.exists(),
        "Dockerfile.dev.bak must be created during migration; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let bak_content = std::fs::read_to_string(&bak_path).unwrap();
    assert!(
        bak_content.contains("legacy single-file"),
        "backup must contain original Dockerfile.dev content"
    );
}

/// When `.amux/Dockerfile.{agent}` exists but the agent image is absent, `amux ready`
/// (or `amux chat`) should build the agent image on demand rather than failing opaquely.
/// This test uses `amux ready` to trigger the build path.
#[test]
fn e2e_agent_image_built_on_demand_when_dockerfile_exists() {
    if !docker_available() {
        eprintln!("Docker not available — skipping e2e_agent_image_built_on_demand_when_dockerfile_exists");
        return;
    }

    let (_tmp, project_dir) = make_git_repo("amuxtest0049i");
    let project_tag = "amux-amuxtest0049i:latest";
    let agent_tag = "amux-amuxtest0049i-claude:latest";

    // Pre-create both Dockerfiles (simulates a fresh clone that has never built images).
    let project_content = project_dockerfile_embedded();
    std::fs::write(project_dir.join("Dockerfile.dev"), &project_content).unwrap();
    std::fs::create_dir_all(project_dir.join(".amux")).unwrap();
    let agent_template = dockerfile_for_agent_embedded(&Agent::Claude);
    let agent_content = agent_template.replace("{{AWMAN_BASE_IMAGE}}", project_tag);
    std::fs::write(project_dir.join(".amux").join("Dockerfile.claude"), &agent_content).unwrap();

    // Ensure neither image exists before the test.
    let _ = Command::new("docker")
        .args(["rmi", "-f", project_tag, agent_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let output = amux_bin()
        .current_dir(&project_dir)
        .args(["ready"])
        .output()
        .expect("failed to invoke amux ready");

    let agent_exists = Command::new("docker")
        .args(["image", "inspect", agent_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    // Clean up.
    let _ = Command::new("docker")
        .args(["rmi", "-f", project_tag, agent_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    assert!(
        output.status.success(),
        "amux ready should succeed when both Dockerfiles exist but no images are built; \
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        agent_exists,
        "agent image {} must be built on demand when Dockerfile exists",
        agent_tag
    );
}

/// Legacy fallback: when no `.amux/Dockerfile.{agent}` and no project image exist,
/// `amux chat` should fail with a clear "run amux ready" message rather than an
/// opaque docker error.
#[test]
fn e2e_legacy_no_image_fails_with_clear_error() {
    if !docker_available() {
        eprintln!("Docker not available — skipping e2e_legacy_no_image_fails_with_clear_error");
        return;
    }

    let (_tmp, project_dir) = make_git_repo("amuxtest0049j");
    let project_tag = "amux-amuxtest0049j:latest";

    // Legacy layout: Dockerfile.dev present, no .amux/Dockerfile.*, no images built.
    let project_content = project_dockerfile_embedded();
    std::fs::write(project_dir.join("Dockerfile.dev"), &project_content).unwrap();

    // Ensure neither image exists.
    let _ = Command::new("docker")
        .args(["rmi", "-f", project_tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Decline migration (EOF), then attempt chat.
    // chat should fail before even reaching docker run.
    let output = amux_bin()
        .current_dir(&project_dir)
        .args(["chat", "--non-interactive"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to invoke amux chat");

    assert!(
        !output.status.success(),
        "amux chat should fail when no image is available"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("amux ready") || stderr.contains("not found") || stderr.contains("image"),
        "error message should guide user to run amux ready; got: {}",
        stderr
    );
}

