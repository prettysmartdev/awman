//! Boundary tests: every shipped Dockerfile template builds on the real
//! Docker Engine.
//!
//! Each agent template is built `FROM` the project base image, which is
//! itself built from `templates/Dockerfile.project`, exactly as `awman
//! ready` layers them. The templates download their agents (apt, npm,
//! vendor installers), so the engine needs outbound network access while
//! these tests run; the runner provides it for the build only. What is
//! built is what the binary ships (`awman::data::templates`), and a test
//! proves the shipped text equals the file under `templates/`.
//!
//! Adding `templates/Dockerfile.<name>` without a test here fails
//! `every_dockerfile_template_has_a_build_test`.

use crate::docker::engine::Engine;
use crate::support;
use awman::data::templates::{agent_dockerfile_for, pinned_cli_version, project_dockerfile_dev};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Every `templates/Dockerfile.<name>`, each with a build test below.
const DOCKERFILE_TEMPLATES: &[&str] = &[
    "antigravity",
    "claude",
    "cline",
    "codex",
    "copilot",
    "cursor",
    "custom",
    "opencode",
    "opencode-v2",
    "project",
];

fn templates_dir() -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates")).to_path_buf()
}

/// Build the project base from its shipped template, then `agent`'s
/// template on top of it, on the granted engine; both images are removed
/// afterwards, also when the build fails.
fn build_on_project_base(
    sandbox: &mut awman_test_support::TestSandbox,
    agent: &str,
    template: &str,
) {
    let engine = Engine::grant(sandbox);
    // Build/version scenarios may run concurrently for the same agent.
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    engine.sandbox.root().hash(&mut hash);
    let id = hash.finish();
    let base_tag = format!("awman-boundary-{agent}-{id:x}-base:latest");
    let tag = format!("awman-boundary-{agent}-{id:x}:latest");
    let cleanup = crate::docker::engine::Cleanup::new(&engine);
    cleanup.image(&tag).image(&base_tag);

    let (dockerfile, context) =
        engine.write_context(&format!("{agent}-base"), project_dockerfile_dev());
    engine.build(&base_tag, &dockerfile, &context);

    let rendered = template
        .replace("{{AWMAN_BASE_IMAGE}}", &base_tag)
        .replace("{{AWMAN_HEADER}}", "# awman boundary build");
    assert!(
        !rendered.contains("{{"),
        "the {agent} template still has an unresolved placeholder after substitution"
    );
    let (dockerfile, context) = engine.write_context(agent, &rendered);
    engine.build(&tag, &dockerfile, &context);
    assert!(
        engine.has_image(&tag),
        "{tag} is absent after a successful build"
    );
    if matches!(agent, "opencode" | "opencode-v2" | "cursor") {
        // Run this registered suite on native amd64 and arm64 engines. Record
        // which of those architectures the granted daemon proves in this run.
        let architecture =
            engine.run_ok(&["image", "inspect", "--format", "{{.Architecture}}", &tag]);
        assert!(matches!(architecture.as_str(), "amd64" | "arm64"));
        engine.sandbox.note("image_architecture", &architecture);
        let name = format!("awman-version-{agent}-{id:x}");
        cleanup.container(&name);
        let binary = if agent == "cursor" {
            "/home/awman/.local/bin/cursor-agent"
        } else {
            "/usr/local/bin/opencode"
        };
        let mut args = vec![
            "run",
            "--rm",
            "--name",
            &name,
            "--network",
            "none",
            "--entrypoint",
            binary,
            &tag,
        ];
        args.push("--version");
        let output = engine.run_ok(&args);
        engine.sandbox.note("in_image_cli_version", &output);
        if agent == "cursor" {
            assert!(
                !output.trim().is_empty(),
                "Cursor --version returned no output"
            );
        } else {
            let major = if agent == "opencode" { "1." } else { "2." };
            assert!(
                output
                    .split_whitespace()
                    .any(|token| token.trim_start_matches('v').starts_with(major)),
                "{agent} inside {architecture} image: expected major {major}x, got {output:?}"
            );
        }
        assert_eq!(
            engine.run_ok(&["image", "inspect", "--format", "{{.Config.User}}", &tag]),
            "awman",
            "the probe must execute as the runtime user"
        );
    }
    drop(cleanup);
}

fn shipped(agent: &str) -> &'static str {
    agent_dockerfile_for(agent)
        .unwrap_or_else(|| panic!("no shipped Dockerfile template for {agent}"))
}

#[test]
fn every_dockerfile_template_has_a_build_test() {
    let mut on_disk: Vec<String> = std::fs::read_dir(templates_dir())
        .expect("templates directory")
        .map(|entry| entry.expect("a readable directory entry"))
        .filter_map(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .strip_prefix("Dockerfile.")
                .map(str::to_string)
        })
        .collect();
    on_disk.sort();
    assert_eq!(
        on_disk, DOCKERFILE_TEMPLATES,
        "templates/Dockerfile.* and DOCKERFILE_TEMPLATES differ: add a build test and list the template"
    );
}

#[test]
fn shipped_templates_equal_the_files_under_templates() {
    for agent in DOCKERFILE_TEMPLATES
        .iter()
        .filter(|a| !["project", "custom"].contains(a))
    {
        let file = std::fs::read_to_string(templates_dir().join(format!("Dockerfile.{agent}")))
            .unwrap_or_else(|e| panic!("Dockerfile.{agent}: {e}"));
        assert_eq!(
            shipped(agent),
            file,
            "the binary ships a different {agent} template"
        );
    }
    let project = std::fs::read_to_string(templates_dir().join("Dockerfile.project"))
        .expect("project template");
    assert_eq!(project_dockerfile_dev(), project);
}

#[test]
fn the_project_template_builds() {
    let mut sandbox = support::sandbox("templates::the_project_template_builds");
    let engine = Engine::grant(&mut sandbox);
    let tag = "awman-boundary-project:latest";
    let cleanup = crate::docker::engine::Cleanup::new(&engine);
    cleanup.image(tag);
    let (dockerfile, context) = engine.write_context("project", project_dockerfile_dev());
    engine.build(tag, &dockerfile, &context);
    assert!(engine.has_image(tag));
    drop(cleanup);
    sandbox.finish();
}

#[test]
fn the_antigravity_template_builds() {
    let mut sandbox = support::sandbox("templates::the_antigravity_template_builds");
    build_on_project_base(&mut sandbox, "antigravity", shipped("antigravity"));
    sandbox.finish();
}

#[test]
fn the_claude_template_builds() {
    let mut sandbox = support::sandbox("templates::the_claude_template_builds");
    build_on_project_base(&mut sandbox, "claude", shipped("claude"));
    sandbox.finish();
}

#[test]
fn the_cline_template_builds() {
    let mut sandbox = support::sandbox("templates::the_cline_template_builds");
    build_on_project_base(&mut sandbox, "cline", shipped("cline"));
    sandbox.finish();
}

#[test]
fn the_codex_template_builds() {
    let mut sandbox = support::sandbox("templates::the_codex_template_builds");
    build_on_project_base(&mut sandbox, "codex", shipped("codex"));
    sandbox.finish();
}

#[test]
fn the_copilot_template_builds() {
    let mut sandbox = support::sandbox("templates::the_copilot_template_builds");
    build_on_project_base(&mut sandbox, "copilot", shipped("copilot"));
    sandbox.finish();
}

#[test]
fn the_custom_agent_template_builds() {
    let mut sandbox = support::sandbox("templates::the_custom_agent_template_builds");
    let text = std::fs::read_to_string(templates_dir().join("Dockerfile.custom"))
        .expect("templates/Dockerfile.custom");
    // Render a minimal custom definition through every template slot. The
    // renderer's substitutions have separate unit coverage.
    let rendered = text
        .replace("{{AWMAN_NODE_BLOCK}}", "")
        .replace("{{AWMAN_APT_BLOCK}}", "")
        .replace("{{AWMAN_INSTALL_BLOCK}}", "")
        .replace("{{AWMAN_BINARY}}", "sh");
    build_on_project_base(&mut sandbox, "custom", &rendered);
    sandbox.finish();
}

#[test]
fn the_cursor_template_builds() {
    let mut sandbox = support::sandbox("templates::the_cursor_template_builds");
    build_on_project_base(&mut sandbox, "cursor", shipped("cursor"));
    sandbox.finish();
}

#[test]
fn the_opencode_template_builds() {
    let mut sandbox = support::sandbox("templates::the_opencode_template_builds");
    build_on_project_base(&mut sandbox, "opencode", shipped("opencode"));
    sandbox.finish();
}

#[test]
fn the_opencode_v2_template_builds() {
    let mut sandbox = support::sandbox("templates::the_opencode_v2_template_builds");
    build_on_project_base(&mut sandbox, "opencode-v2", shipped("opencode-v2"));
    sandbox.finish();
}

#[test]
fn the_opencode_image_runs_1x() {
    let mut sandbox = support::sandbox("templates::the_opencode_image_runs_1x");
    build_on_project_base(&mut sandbox, "opencode", shipped("opencode"));
    sandbox.finish();
}

#[test]
fn the_opencode_v2_image_runs_2x() {
    let mut sandbox = support::sandbox("templates::the_opencode_v2_image_runs_2x");
    build_on_project_base(&mut sandbox, "opencode-v2", shipped("opencode-v2"));
    sandbox.finish();
}

#[test]
fn the_cursor_image_runs_its_installed_binary() {
    let mut sandbox = support::sandbox("templates::the_cursor_image_runs_its_installed_binary");
    build_on_project_base(&mut sandbox, "cursor", shipped("cursor"));
    sandbox.finish();
}

/// Exercise an installer refusal in the real build shell without downloading
/// or executing an agent. These commands replace uname/ldd only inside RUN.
fn assert_install_refused(engine: &Engine<'_>, agent: &str, functions: &str, expected: &str) {
    let template = shipped(agent);
    let install = template
        .split("\nRUN ")
        .nth(1)
        .expect("pinned install RUN")
        .split("\n\n")
        .next()
        .unwrap();
    let base = engine.base_image();
    // The granted shell base need not provide bash. This installer prefix uses
    // POSIX shell grammar; no agent or runtime-user smoke is reached here.
    let dockerfile = format!("FROM {base}\nUSER root\nRUN {functions}; {install}\n");
    let (file, context) = engine.write_context(&format!("refused-{agent}"), &dockerfile);
    let output = engine
        .command()
        .args(["build", "--file"])
        .arg(file)
        .arg(context)
        .deadline(crate::docker::engine::BUILD_DEADLINE)
        .run();
    assert!(
        !output.exit.success(),
        "{agent} installer accepted {expected}"
    );
    let diagnostics = format!("{}\n{}", output.stdout_text(), output.stderr_text());
    assert!(diagnostics.contains(expected), "{agent}: {diagnostics}");
}

#[test]
fn pinned_installers_refuse_an_unsupported_architecture_precisely() {
    let mut sandbox = support::sandbox(
        "templates::pinned_installers_refuse_an_unsupported_architecture_precisely",
    );
    let engine = Engine::grant(&mut sandbox);
    for (agent, binary) in [
        ("opencode", "opencode"),
        ("opencode-v2", "opencode-v2"),
        ("cursor", "cursor-agent"),
    ] {
        let version = pinned_cli_version(agent)
            .map(|v| format!(" {v}"))
            .unwrap_or_default();
        assert_install_refused(
            &engine, agent, "uname() { echo riscv64; }",
            &format!("awman: {binary}{version} has no Linux package for architecture 'riscv64' (supported: x86_64, aarch64)"),
        );
    }
    sandbox.finish();
}

#[test]
fn cursor_refuses_a_musl_base_even_when_ldd_exits_nonzero() {
    let mut sandbox =
        support::sandbox("templates::cursor_refuses_a_musl_base_even_when_ldd_exits_nonzero");
    let engine = Engine::grant(&mut sandbox);
    assert_install_refused(
        &engine,
        "cursor",
        "uname() { echo aarch64; }; ldd() { echo musl; return 1; }",
        "awman: cursor-agent requires glibc; this base image uses musl",
    );
    sandbox.finish();
}
