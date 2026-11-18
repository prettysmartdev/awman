//! Layer 0 template inclusions — embedded Dockerfile templates and audit
//! prompts. These exist as a single Layer-0 module so callers everywhere
//! can resolve the templates without having to redefine paths.

pub mod audit_prompts;

pub use audit_prompts::{init_audit_prompt, ready_audit_prompt};

/// The project base Dockerfile. Written by `awman init` and `awman ready`
/// when no `Dockerfile.dev` exists at the git root.
pub fn project_dockerfile_dev() -> &'static str {
    include_str!("../../../templates/Dockerfile.project")
}

/// Per-agent Dockerfile template (fallback when network download fails).
pub fn agent_dockerfile_for(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => include_str!("../../../templates/Dockerfile.claude"),
        "codex" => include_str!("../../../templates/Dockerfile.codex"),
        "opencode" => include_str!("../../../templates/Dockerfile.opencode"),
        "opencode-v2" => include_str!("../../../templates/Dockerfile.opencode-v2"),
        "cursor" => include_str!("../../../templates/Dockerfile.cursor"),
        "copilot" => include_str!("../../../templates/Dockerfile.copilot"),
        "cline" => include_str!("../../../templates/Dockerfile.cline"),
        "antigravity" => include_str!("../../../templates/Dockerfile.antigravity"),
        _ => return None,
    })
}

/// Returns `true` when the given content matches the bundled project base
/// template (ignoring leading/trailing whitespace). Used by the ready engine
/// to decide whether an audit should be offered.
pub fn dockerfile_matches_template(content: &str) -> bool {
    content.trim() == project_dockerfile_dev().trim()
}

/// Per-agent Docker Sandbox kit YAML template (`spec.yaml` source).
///
/// Additive sibling of [`agent_dockerfile_for`] — the sbx kit templates live
/// alongside the Dockerfile templates in `templates/` and are embedded with
/// the same `include_str!` mechanism. A caller that wants a Dockerfile calls
/// `agent_dockerfile_for`; a caller that wants a kit calls this. The two
/// families never collide because of the `sbx-kit.` filename prefix.
pub fn sbx_kit_template_for(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => include_str!("../../../templates/sbx-kit.claude.yaml"),
        "codex" => include_str!("../../../templates/sbx-kit.codex.yaml"),
        "opencode" => include_str!("../../../templates/sbx-kit.opencode.yaml"),
        "opencode-v2" => include_str!("../../../templates/sbx-kit.opencode-v2.yaml"),
        "cursor" => include_str!("../../../templates/sbx-kit.cursor.yaml"),
        "copilot" => include_str!("../../../templates/sbx-kit.copilot.yaml"),
        "cline" => include_str!("../../../templates/sbx-kit.cline.yaml"),
        "antigravity" => include_str!("../../../templates/sbx-kit.antigravity.yaml"),
        _ => return None,
    })
}

/// Per-agent Docker Sandbox startup script (`apply-session-config.sh` source).
///
/// Written into the emitted kit's `files/home/.awman/` directory by the kit
/// emitter at `awman ready` time and re-run by the kit's `commands.startup`
/// on every sandbox restart.
pub fn sbx_apply_script_for(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => include_str!("../../../templates/sbx-apply.claude.sh"),
        "codex" => include_str!("../../../templates/sbx-apply.codex.sh"),
        "opencode" => include_str!("../../../templates/sbx-apply.opencode.sh"),
        "opencode-v2" => include_str!("../../../templates/sbx-apply.opencode-v2.sh"),
        "cursor" => include_str!("../../../templates/sbx-apply.cursor.sh"),
        "copilot" => include_str!("../../../templates/sbx-apply.copilot.sh"),
        "cline" => include_str!("../../../templates/sbx-apply.cline.sh"),
        "antigravity" => include_str!("../../../templates/sbx-apply.antigravity.sh"),
        _ => return None,
    })
}

/// CLI version each bundled first-party template pins, for agents with a pin.
/// Layer 1 definitions cross-check their tested version against this value.
pub fn pinned_cli_version(agent: &str) -> Option<&'static str> {
    match agent {
        "opencode" => Some("1.19.6"),
        "opencode-v2" => Some("2.3.2"),
        "cursor" => Some("2026.11.17-f532c45"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENTS: &[&str] = &[
        "claude",
        "codex",
        "opencode",
        "opencode-v2",
        "cursor",
        "copilot",
        "cline",
        "antigravity",
    ];
    const PINNED: &[&str] = &["opencode", "opencode-v2", "cursor"];

    fn sha256_pins(text: &str) -> Vec<&str> {
        text.split(|c: char| !c.is_ascii_hexdigit())
            .filter(|token| token.len() == 64)
            .collect()
    }

    #[test]
    fn lookups_supply_the_eight_builtins_and_refuse_retired_or_unknown_names() {
        for agent in AGENTS {
            assert!(agent_dockerfile_for(agent).is_some(), "{agent}");
            assert!(sbx_kit_template_for(agent).is_some(), "{agent}");
            assert!(sbx_apply_script_for(agent).is_some(), "{agent}");
        }
        for agent in ["gemini", "custom", "project", "", "unknown", "OpenCode"] {
            assert!(agent_dockerfile_for(agent).is_none(), "{agent}");
            assert!(sbx_kit_template_for(agent).is_none(), "{agent}");
            assert!(sbx_apply_script_for(agent).is_none(), "{agent}");
            assert!(pinned_cli_version(agent).is_none(), "{agent}");
        }
    }

    #[test]
    fn bundled_downloads_never_select_a_latest_release() {
        for agent in AGENTS {
            for text in [
                agent_dockerfile_for(agent).unwrap(),
                sbx_kit_template_for(agent).unwrap(),
            ] {
                assert!(!text.contains("releases/latest"), "{agent}");
                assert!(!text.contains("/latest/download"), "{agent}");
            }
        }
    }

    #[test]
    fn pinned_images_and_kits_verify_the_same_versioned_artifacts() {
        for agent in PINNED {
            let version = pinned_cli_version(agent).unwrap();
            let dockerfile = agent_dockerfile_for(agent).unwrap();
            let kit = sbx_kit_template_for(agent).unwrap();
            if agent.starts_with("opencode") {
                assert!(dockerfile.contains("*) echo \"awman: unsupported OpenCode artifact ${OC_ARCH}${OC_LIBC}\" >&2; exit 1"));
            }
            let hashes = sha256_pins(dockerfile);
            assert_eq!(hashes.len(), if *agent == "cursor" { 2 } else { 4 });
            assert_eq!(hashes, sha256_pins(kit), "{agent} image/kit hash drift");
            assert!(dockerfile.contains(&format!("awman.agent=\"{agent}\"")));
            assert!(dockerfile.contains(&format!("awman.agent.cli.version=\"{version}\"")));
            assert!(!dockerfile.lines().any(|l| l.starts_with("ARG ")));
            for text in [dockerfile, kit] {
                assert!(
                    text.contains(&format!("[ \"$v\" = \"{version}\" ]")),
                    "{agent}"
                );
                assert!(text.contains("sha256sum -c -"), "{agent}");
                assert!(
                    text.contains("has no Linux package for architecture"),
                    "{agent}"
                );
                assert!(text.contains("(supported: x86_64, aarch64)"), "{agent}");
                let commands = text
                    .lines()
                    .filter(|l| !l.trim_start().starts_with('#'))
                    .collect::<Vec<_>>()
                    .join("\n");
                // Match the command token: sha256sum is a required pipeline
                // consumer and must not be mistaken for the shell `sh`.
                for consumer in commands.split('|').skip(1) {
                    let command = consumer.split_whitespace().next().unwrap_or("");
                    assert!(!matches!(command, "bash" | "sh"), "{agent}");
                }
                assert!(!text.contains("CURSOR_API_KEY"), "{agent}");
                assert!(!text.contains("ANTHROPIC_API_KEY="), "{agent}");
                assert!(!text.contains("auth.json"), "{agent}");
            }
            // The build assertion runs after USER awman, just as a launch does.
            let smoke = dockerfile.split("USER awman").nth(1).unwrap();
            assert!(smoke.contains("--version"), "{agent}");
            assert!(kit.contains("runuser -u agent -- env -i"), "{agent}");
        }
    }

    #[test]
    fn opencode_templates_keep_the_two_majors_separate() {
        let v1 = agent_dockerfile_for("opencode").unwrap();
        let v2 = agent_dockerfile_for("opencode-v2").unwrap();
        assert_ne!(v1, v2);
        for (agent, major) in [("opencode", 1), ("opencode-v2", 2)] {
            let version = pinned_cli_version(agent).unwrap();
            for text in [
                agent_dockerfile_for(agent).unwrap(),
                sbx_kit_template_for(agent).unwrap(),
            ] {
                assert!(text.contains(&format!("expected opencode {version} ({major}.x)")));
                assert!(text.contains("https://github.com/anomalyco/opencode/releases/download/v${OC_VERSION}/${OC_ASSET}"));
                assert!(text.contains("x64-musl)"));
                assert!(text.contains("arm64-musl)"));
            }
        }
    }

    #[test]
    fn cursor_pin_is_root_owned_and_checks_the_update_disable_flag() {
        for text in [
            agent_dockerfile_for("cursor").unwrap(),
            sbx_kit_template_for("cursor").unwrap(),
        ] {
            assert!(text.contains("https://downloads.cursor.com/lab/${CA_VERSION}/linux/${CA_ARCH}/agent-cli-package.tar.gz"));
            assert!(text.contains("chown -R root:root /opt/cursor-agent"));
            assert!(text.contains("chmod -R go-w /opt/cursor-agent"));
            assert!(text.contains("cursor-agent --disable-auto-update --help"));
            assert!(text.contains("cursor-agent --disable-auto-update --version"));
            assert!(text.contains("requires glibc; this base image uses musl"));
            assert!(!text.contains("/home/awman/.local/bin"));
            assert!(!text.contains("/home/agent/.local/bin"));
            assert!(!text.contains("/usr/local/bin/agent\n"));
        }
    }

    #[test]
    fn controlled_kits_declare_their_entrypoints_and_do_not_rewrite_agent_config() {
        for (agent, entrypoint) in [
            ("opencode", vec!["opencode"]),
            ("opencode-v2", vec!["opencode", "--standalone"]),
            ("cursor", vec!["cursor-agent", "--disable-auto-update"]),
        ] {
            let rendered = sbx_kit_template_for(agent)
                .unwrap()
                .replace("{{AWMAN_VERSION}}", "test");
            let kit: serde_yaml::Value = serde_yaml::from_str(&rendered).unwrap();
            assert_eq!(kit["kind"].as_str(), Some("agent"));
            assert_eq!(
                kit["name"].as_str(),
                Some(format!("awman-{agent}").as_str())
            );
            assert!(kit.get("extends").is_none());
            let actual: Vec<_> = kit["agent"]["entrypoint"]["run"]
                .as_sequence()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect();
            assert_eq!(actual, entrypoint);
            let script = sbx_apply_script_for(agent).unwrap();
            assert!(script.contains("ENV_SH=\"$HOME/.awman/env.sh\""));
            assert!(!script.contains(".config/opencode"));
            assert!(!script.contains("cli-config.json"));
            assert!(!script.contains("auth.json"));
        }
    }
}
