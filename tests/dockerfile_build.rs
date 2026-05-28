/// Integration tests that build each agent's Dockerfile template using Docker.
///
/// These tests verify that the template Dockerfiles produce valid images
/// in their default/template states. They require a running Docker daemon
/// and network access, and are skipped if Docker is unavailable.
use std::io::Write;
use std::process::Command;

fn docker_available() -> bool {
    Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build the project base image and return its tag. The base image is used to
/// substitute the `{{AWMAN_BASE_IMAGE}}` placeholder in agent templates.
fn build_base_image() -> String {
    let tag = "amux-test-base:latest";
    let status = Command::new("docker")
        .args([
            "build",
            "-t",
            tag,
            "-f",
            "templates/Dockerfile.project",
            ".",
        ])
        .status()
        .expect("failed to invoke docker build for project base image");
    assert!(
        status.success(),
        "docker build failed for project base template"
    );
    tag.to_string()
}

fn build_template(template_path: &str, tag: &str) {
    if !docker_available() {
        eprintln!("Docker not available, skipping Dockerfile build test");
        return;
    }

    let base_tag = build_base_image();

    // Read the template and substitute the {{AWMAN_BASE_IMAGE}} placeholder so
    // Docker receives a valid image reference in the FROM directive.
    let template_content =
        std::fs::read_to_string(template_path).expect("failed to read template file");
    let dockerfile_content = template_content.replace("{{AWMAN_BASE_IMAGE}}", &base_tag);

    // Write the substituted content to a temporary file.
    let mut tmp = tempfile::NamedTempFile::new().expect("failed to create temp Dockerfile");
    tmp.write_all(dockerfile_content.as_bytes())
        .expect("failed to write temp Dockerfile");
    let tmp_path = tmp.path().to_path_buf();

    let status = Command::new("docker")
        .args([
            "build",
            "-t",
            tag,
            "-f",
            tmp_path.to_str().expect("temp path is not valid UTF-8"),
            ".",
        ])
        .status()
        .expect("failed to invoke docker build");

    assert!(
        status.success(),
        "docker build failed for template: {}",
        template_path
    );

    // Clean up the test images.
    for image in &[tag, &base_tag as &str] {
        let _ = Command::new("docker")
            .args(["rmi", image])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[test]
fn build_claude_template() {
    build_template("templates/Dockerfile.claude", "amux-test-claude:latest");
}

#[test]
fn build_codex_template() {
    build_template("templates/Dockerfile.codex", "amux-test-codex:latest");
}

#[test]
fn build_opencode_template() {
    build_template("templates/Dockerfile.opencode", "amux-test-opencode:latest");
}
