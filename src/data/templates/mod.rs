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

/// Reviewed v3 descriptor. Refused agents have no kit sources.
pub fn sbx_kit_template_for(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => include_str!("../../../templates/sbx-v3/awman-claude-mixin.yaml"),
        "codex" => include_str!("../../../templates/sbx-v3/awman-codex-mixin.yaml"),
        "copilot" => include_str!("../../../templates/sbx-v3/awman-copilot-mixin.yaml"),
        "opencode" => include_str!("../../../templates/sbx-v3/awman-opencode.yaml"),
        "cline" => include_str!("../../../templates/sbx-v3/awman-cline.yaml"),
        "antigravity" => include_str!("../../../templates/sbx-v3/awman-antigravity.yaml"),
        _ => return None,
    })
}

/// Dockerfile paired with the v3 descriptor, using the same stem.
pub fn sbx_dockerfile_for(agent: &str) -> Option<&'static str> {
    Some(match agent {
        "claude" => include_str!("../../../templates/sbx-v3/awman-claude-mixin.dockerfile"),
        "codex" => include_str!("../../../templates/sbx-v3/awman-codex-mixin.dockerfile"),
        "copilot" => include_str!("../../../templates/sbx-v3/awman-copilot-mixin.dockerfile"),
        "opencode" => include_str!("../../../templates/sbx-v3/awman-opencode.dockerfile"),
        "cline" => include_str!("../../../templates/sbx-v3/awman-cline.dockerfile"),
        "antigravity" => include_str!("../../../templates/sbx-v3/awman-antigravity.dockerfile"),
        _ => return None,
    })
}

/// Shared synchronous apply protocol source; rendered with a matrix allowlist.
pub fn sbx_apply_script() -> &'static str {
    include_str!("../../../templates/sbx-v3/awman-apply.sh")
}

/// Owned workloads' receipt-checking entrypoint.
pub fn sbx_launch_script() -> &'static str {
    include_str!("../../../templates/sbx-v3/awman-launch.sh")
}

/// Default OpenCode artifact selected by the bundled Dockerfile. Users may
/// choose another build; runtime compatibility checks only the selected major.
pub fn pinned_cli_version(agent: &str) -> Option<&'static str> {
    match agent {
        "opencode" => Some("1.19.6"),
        "opencode-v2" => Some("2.3.2"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENTS: &[&str] = &[
        "claude",
        "codex",
        "copilot",
        "opencode",
        "cline",
        "antigravity",
        "cursor",
        "opencode-v2",
    ];

    fn sha256_pins(text: &str) -> Vec<&str> {
        text.split(|c: char| !c.is_ascii_hexdigit())
            .filter(|t| t.len() == 64)
            .collect()
    }

    #[test]
    fn v3_sources_cover_six_routes_and_never_advertise_refused_agents() {
        for agent in AGENTS {
            assert!(agent_dockerfile_for(agent).is_some());
            let admitted = !matches!(*agent, "cursor" | "opencode-v2");
            assert_eq!(sbx_kit_template_for(agent).is_some(), admitted, "{agent}");
            assert_eq!(sbx_dockerfile_for(agent).is_some(), admitted, "{agent}");
        }
        for agent in ["gemini", "custom", "project", "", "unknown", "OpenCode"] {
            assert!(agent_dockerfile_for(agent).is_none());
            assert!(sbx_kit_template_for(agent).is_none());
            assert!(sbx_dockerfile_for(agent).is_none());
            assert!(pinned_cli_version(agent).is_none());
        }
    }

    #[test]
    fn descriptors_and_recipes_keep_the_reviewed_authority_ceiling() {
        // These are source-shape assertions, not evidence of live SBX support.
        for agent in AGENTS {
            let Some(descriptor) = sbx_kit_template_for(agent) else {
                continue;
            };
            let yaml: serde_yaml::Value = serde_yaml::from_str(descriptor).unwrap();
            assert_eq!(yaml["schemaVersion"].as_str(), Some("3"));
            assert!(!descriptor.contains("lifecycle"));
            assert!(!descriptor.contains("agent-context"));
            assert!(!descriptor.contains("registry.npmjs.org"));
            let (display, service, domain, key, header) = match *agent {
                "opencode" => (
                    "OpenCode 1",
                    "anthropic",
                    "api.anthropic.com",
                    "ANTHROPIC_API_KEY",
                    "x-api-key",
                ),
                "cline" => (
                    "Cline",
                    "anthropic",
                    "api.anthropic.com",
                    "ANTHROPIC_API_KEY",
                    "x-api-key",
                ),
                "antigravity" => (
                    "Antigravity",
                    "google",
                    "generativelanguage.googleapis.com",
                    "GEMINI_API_KEY",
                    "x-goog-api-key",
                ),
                "claude" => ("Claude Code", "", "", "", ""),
                "codex" => ("Codex", "", "", "", ""),
                "copilot" => ("GitHub Copilot", "", "", "", ""),
                _ => unreachable!(),
            };
            let expected = if service.is_empty() {
                serde_json::json!({"schemaVersion":"3","kind":"mixin","displayName":format!("awman policy for {display}"),"description":"Files awman uses to apply and verify per-launch policy"})
            } else {
                serde_json::json!({"schemaVersion":"3","kind":"workload","displayName":format!("awman {display}"),"description":format!("{display} launched and policed by awman"),"capabilities":[
                    {"type":"com.docker.sandbox/sbx@1"},
                    {"type":"com.docker.sandbox/network-policy@1","config":{"runtime":{"allow":[format!("{domain}:443")]}}},
                    {"type":"com.docker.sandbox/credential@1","description":format!("{service} API access for {display}"),"config":{"service":service,"phase":"runtime","apiKey":{"name":key,"proxyManaged":true,"inject":[{"domain":domain,"header":header,"format":"%s"}]}}}
                ]})
            };
            assert_eq!(yaml, serde_yaml::to_value(expected).unwrap(), "{agent}");
            let recipe = sbx_dockerfile_for(agent).unwrap();
            for source in [descriptor, recipe, sbx_apply_script(), sbx_launch_script()] {
                assert!(!source.contains("docker/sandbox-templates"));
                assert!(!source.contains("session.json"));
                assert!(!source.contains("AWMAN_VERSION"));
            }
            if ["claude", "codex", "copilot"].contains(agent) {
                assert_eq!(yaml["kind"].as_str(), Some("mixin"));
                assert!(yaml.get("capabilities").is_none());
                assert_eq!(recipe, "FROM scratch\nCOPY --chmod=0755 rootfs/usr/local/lib/awman/awman-apply /usr/local/lib/awman/awman-apply\nCOPY --chmod=0644 rootfs/usr/local/lib/awman/kit-id /usr/local/lib/awman/kit-id\n");
                assert!(!recipe.contains("USER "));
                assert!(!recipe.contains("ENTRYPOINT"));
            } else {
                assert_eq!(yaml["kind"].as_str(), Some("workload"));
                let caps = yaml["capabilities"].as_sequence().unwrap();
                assert_eq!(caps.len(), 3);
                assert_eq!(caps[0]["type"].as_str(), Some("com.docker.sandbox/sbx@1"));
                assert_eq!(
                    caps[1]["type"].as_str(),
                    Some("com.docker.sandbox/network-policy@1")
                );
                assert_eq!(
                    caps[2]["type"].as_str(),
                    Some("com.docker.sandbox/credential@1")
                );
                let domain = if *agent == "antigravity" {
                    "generativelanguage.googleapis.com"
                } else {
                    "api.anthropic.com"
                };
                assert_eq!(
                    caps[1]["config"]["runtime"]["allow"]
                        .as_sequence()
                        .unwrap()
                        .len(),
                    1
                );
                assert_eq!(
                    caps[1]["config"]["runtime"]["allow"][0].as_str(),
                    Some(format!("{domain}:443").as_str())
                );
                let cred = &caps[2]["config"];
                assert_eq!(cred["phase"].as_str(), Some("runtime"));
                assert_eq!(cred["apiKey"]["proxyManaged"].as_bool(), Some(true));
                assert_eq!(cred["apiKey"]["inject"].as_sequence().unwrap().len(), 1);
                assert_eq!(cred["apiKey"]["inject"][0]["domain"].as_str(), Some(domain));
                assert!(recipe.starts_with("FROM docker.io/sbx/shell-image:latest\n"));
                assert!(recipe.contains("USER agent\n"));
                assert!(recipe.contains("ENTRYPOINT [\"/usr/local/lib/awman/awman-launch\""));
                assert!(recipe.contains("--version"));
            }
        }
    }

    #[test]
    fn owned_installers_use_reviewed_sources_and_exact_version_checks() {
        let cline = sbx_dockerfile_for("cline").unwrap();
        assert!(agent_dockerfile_for("cline")
            .unwrap()
            .contains("RUN npm install -g --prefix=/usr/local cline\n"));
        assert!(cline.contains("--ignore-scripts cline@3.0.76"));
        assert!(!cline.contains("@cline/cli"));
        let agy = sbx_dockerfile_for("antigravity").unwrap();
        let installer = "https://antigravity.google/cli/install.sh";
        assert!(agy.contains(installer));
        assert!(agent_dockerfile_for("antigravity")
            .unwrap()
            .contains(installer));
        assert!(agy.contains("sha256sum -c -"));
        assert!(agy.contains("1\\.5\\.18"));
        assert!(!agy.contains("@google/antigravity-cli"));
    }

    #[test]
    fn opencode_workload_keeps_the_reviewed_tarball_and_major_gate() {
        let image = agent_dockerfile_for("opencode").unwrap();
        let recipe = sbx_dockerfile_for("opencode").unwrap();
        assert_eq!(sha256_pins(image), sha256_pins(recipe));
        assert_eq!(sha256_pins(recipe).len(), 4);
        assert!(recipe.contains(&format!(
            "OC_VERSION={}",
            pinned_cli_version("opencode").unwrap()
        )));
        assert!(recipe.contains("sha256sum -c -"));
        assert!(recipe.contains("runuser -u agent -- env -i"));
        assert!(recipe.contains("expected opencode 1.x"));
        for agent in AGENTS {
            for source in std::iter::once(agent_dockerfile_for(agent).unwrap())
                .chain(sbx_dockerfile_for(agent))
            {
                assert!(!source.contains("releases/latest"));
                assert!(!source.contains("/latest/download"));
            }
        }
        let v2 = agent_dockerfile_for("opencode-v2").unwrap();
        assert!(v2.contains("expected opencode 2.x"));
        assert_ne!(image, v2);
        let cursor = agent_dockerfile_for("cursor").unwrap();
        assert!(cursor.contains("https://cursor.com/install"));
        assert!(cursor.contains("cursor-agent --help"));
        assert!(pinned_cli_version("cursor").is_none());
    }
}
