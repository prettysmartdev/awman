/// Integration tests for downloading templates and the aspec folder from GitHub.
///
/// These tests verify that the download module correctly fetches files from GitHub,
/// extracts tarball contents, and integrates with the init and ready commands.
/// Tests that require network access are skipped when offline.
use awman::cli::Agent;
use awman::commands::download;
use awman::commands::init_flow::{self, CliContainerLauncher, CliInitQa, InitParams};
use awman::commands::output::OutputSink;
use tempfile::TempDir;
use tokio::sync::mpsc::unbounded_channel;

/// Check whether we have network connectivity to GitHub.
fn has_network() -> bool {
    std::process::Command::new("curl")
        .args(["-sf", "--max-time", "5", "https://api.github.com/"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Dockerfile template download tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn download_dockerfile_template_claude() {
    if !has_network() {
        eprintln!("No network, skipping download test");
        return;
    }
    let (tx, mut rx) = unbounded_channel();
    let out = OutputSink::Channel(tx);

    let result = download::download_dockerfile_template(&Agent::Claude, &out).await;
    assert!(result.is_ok(), "Download failed: {:?}", result.err());

    let content = result.unwrap();
    assert!(content.contains("{{AMUX_BASE_IMAGE}}"), "Template should use AMUX_BASE_IMAGE placeholder");
    assert!(content.contains("claude"), "Template should reference claude");

    // Verify log messages were emitted.
    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        messages.iter().any(|m| m.contains("Downloading")),
        "Expected download log message, got: {:?}",
        messages
    );
    assert!(
        messages.iter().any(|m| m.contains("bytes")),
        "Expected size log message, got: {:?}",
        messages
    );
}

#[tokio::test]
async fn download_dockerfile_template_codex() {
    if !has_network() {
        eprintln!("No network, skipping download test");
        return;
    }
    let (tx, _rx) = unbounded_channel();
    let out = OutputSink::Channel(tx);

    let result = download::download_dockerfile_template(&Agent::Codex, &out).await;
    assert!(result.is_ok(), "Download failed: {:?}", result.err());

    let content = result.unwrap();
    assert!(content.contains("{{AMUX_BASE_IMAGE}}"));
    assert!(content.contains("codex") || content.contains("Codex"));
}

#[tokio::test]
async fn download_dockerfile_template_opencode() {
    if !has_network() {
        eprintln!("No network, skipping download test");
        return;
    }
    let (tx, _rx) = unbounded_channel();
    let out = OutputSink::Channel(tx);

    let result = download::download_dockerfile_template(&Agent::Opencode, &out).await;
    assert!(result.is_ok(), "Download failed: {:?}", result.err());

    let content = result.unwrap();
    assert!(content.contains("{{AMUX_BASE_IMAGE}}"));
    assert!(content.contains("opencode"));
}

// ---------------------------------------------------------------------------
// aspec folder download tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn download_aspec_folder_creates_files() {
    if !has_network() {
        eprintln!("No network, skipping download test");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let (tx, mut rx) = unbounded_channel();
    let out = OutputSink::Channel(tx);

    let result = download::download_aspec_folder(tmp.path(), &out).await;
    assert!(result.is_ok(), "Download failed: {:?}", result.err());

    // The aspec folder should exist and contain key files.
    let aspec_dir = tmp.path().join("aspec");
    assert!(aspec_dir.exists(), "aspec/ directory should exist");
    assert!(
        aspec_dir.join("foundation.md").exists(),
        "aspec/foundation.md should exist"
    );

    // Verify log messages were emitted.
    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        messages.iter().any(|m| m.contains("Downloading")),
        "Expected download log message, got: {:?}",
        messages
    );
    assert!(
        messages.iter().any(|m| m.contains("Extracted")),
        "Expected extraction log message, got: {:?}",
        messages
    );
    assert!(
        messages.iter().any(|m| m.contains("files")),
        "Expected file count log message, got: {:?}",
        messages
    );
}

#[tokio::test]
async fn download_aspec_folder_contains_work_items_template() {
    if !has_network() {
        eprintln!("No network, skipping download test");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let (tx, _rx) = unbounded_channel();
    let out = OutputSink::Channel(tx);

    let result = download::download_aspec_folder(tmp.path(), &out).await;
    assert!(result.is_ok(), "Download failed: {:?}", result.err());

    let template = tmp.path().join("aspec/work-items/0000-template.md");
    assert!(
        template.exists(),
        "aspec/work-items/0000-template.md should exist after download"
    );

    let content = std::fs::read_to_string(&template).unwrap();
    assert!(
        content.contains("Work Item"),
        "Template should contain 'Work Item' header"
    );
}

// ---------------------------------------------------------------------------
// Init command integration with downloads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn init_downloads_aspec_folder_when_missing() {
    if !has_network() {
        eprintln!("No network, skipping download test");
        return;
    }

    let tmp = TempDir::new().unwrap();
    // Create a fake git root.
    std::fs::create_dir(tmp.path().join(".git")).unwrap();

    let (tx, mut rx) = unbounded_channel();
    let out = OutputSink::Channel(tx);

    // Pass aspec=true so the aspec folder is downloaded.
    let runtime = std::sync::Arc::new(awman::runtime::DockerRuntime::new());
    let git_root = tmp.path().to_path_buf();
    let mut qa = CliInitQa::new(&git_root, out.clone());
    let launcher = CliContainerLauncher::new(runtime.clone());
    let params = InitParams { agent: Agent::Claude, aspec: true, git_root };
    let result = init_flow::execute(params, &mut qa, &launcher, &out, runtime).await;

    assert!(result.is_ok(), "Init failed: {:?}", result.err());

    // aspec folder should have been downloaded.
    let aspec_dir = tmp.path().join("aspec");
    assert!(aspec_dir.exists(), "aspec folder should be downloaded");

    // Dockerfile.dev should exist.
    assert!(
        tmp.path().join("Dockerfile.dev").exists(),
        "Dockerfile.dev should be created"
    );

    // Config should be written to .awman/config.json.
    assert!(
        tmp.path().join(".awman/config.json").exists(),
        "Config should be written"
    );

    // Verify download log messages.
    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        messages.iter().any(|m| m.contains("Downloading")),
        "Expected download messages, got: {:?}",
        messages
    );
}

#[tokio::test]
async fn init_skips_aspec_download_when_folder_exists() {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    std::fs::create_dir_all(tmp.path().join("aspec")).unwrap();

    let (tx, mut rx) = unbounded_channel();
    let out = OutputSink::Channel(tx);

    // Pass aspec=true so init tries to download, but the folder already exists.
    let runtime = std::sync::Arc::new(awman::runtime::DockerRuntime::new());
    let git_root = tmp.path().to_path_buf();
    let mut qa = CliInitQa::new(&git_root, out.clone());
    let launcher = CliContainerLauncher::new(runtime.clone());
    let params = InitParams { agent: Agent::Claude, aspec: true, git_root };
    let result = init_flow::execute(params, &mut qa, &launcher, &out, runtime).await;

    assert!(result.is_ok(), "Init failed: {:?}", result.err());

    let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        messages.iter().any(|m| m.contains("already exists")),
        "Expected 'already exists' message for aspec folder, got: {:?}",
        messages
    );
}

// ---------------------------------------------------------------------------
// write_project_dockerfile integration with download fallback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_project_dockerfile_creates_with_embedded_template() {
    // write_project_dockerfile uses the embedded project template (no network needed).
    let tmp = TempDir::new().unwrap();
    let (tx, _rx) = unbounded_channel();
    let out = OutputSink::Channel(tx);

    let result = init_flow::write_project_dockerfile(tmp.path(), &out).await;
    assert!(result.is_ok(), "write_project_dockerfile failed: {:?}", result.err());
    assert!(result.unwrap(), "Should return true when creating new file");

    let content = std::fs::read_to_string(tmp.path().join("Dockerfile.dev")).unwrap();
    assert!(
        content.contains("debian:bookworm-slim"),
        "Should contain valid Dockerfile content"
    );
}

#[tokio::test]
async fn write_project_dockerfile_preserves_existing_file() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("Dockerfile.dev"), "CUSTOM").unwrap();

    let (tx, _rx) = unbounded_channel();
    let out = OutputSink::Channel(tx);

    let result = init_flow::write_project_dockerfile(tmp.path(), &out).await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "Should return false when file exists");

    let content = std::fs::read_to_string(tmp.path().join("Dockerfile.dev")).unwrap();
    assert_eq!(content, "CUSTOM", "Existing file must not be overwritten");
}
