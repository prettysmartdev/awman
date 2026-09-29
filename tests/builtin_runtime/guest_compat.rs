//! Production CLI -> AgentEngine -> builtin guest strategy coverage. These
//! probes execute synthetic shell agents inside the VM, never on the host.
//! Every shipped descriptor is exercised with named and all-skills overlays.
//! This is not the separate cross-product of artificial descriptor overrides,
//! nor evidence for native execution until the required hardware run completes.

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use awman::data::fs::auth_paths::AuthPathResolver;
use awman::engine::agent::agent_matrix::{
    matrix_for, SettingsMount, SystemPromptMode, SUPPORTED_AGENTS,
};

use crate::lifecycle::{awman, gate, install_ready_ping_stub, run_bounded};

fn probe(agent: &str) -> String {
    let matrix = matrix_for(agent).unwrap();
    let mut script = String::from(
        r#"#!/bin/sh
set -eu
test "$(id -u)" = 1234
test "$HOME" != /root
IFS= read -r prompt
test "$prompt" = 'matrix task with spaces'
test "$AWMAN_MATRIX_ENV" = 'literal value with spaces'
test "$(cat /matrix/ro/sentinel)" = read-only
if echo forbidden > /matrix/ro/forbidden 2>/dev/null; then exit 71; fi
printf writeback > "$HOME/matrix-rw/writeback"
test -d /awman/context/global
test -d /awman/context/repo
test -d /awman/context/workflow
printf workflow-write > /awman/context/workflow/writeback
"#,
    );
    match matrix.settings_mount {
        SettingsMount::None => script.push_str("test ! -e \"$HOME/.matrix-no-settings\"\n"),
        SettingsMount::Direct(relative) => script.push_str(&format!(
            "grep -q settings-sentinel \"$HOME/{relative}/settings.json\"\nprintf settings-writeback > \"$HOME/{relative}/writeback\"\n"
        )),
        SettingsMount::Claude => script.push_str(
            "test -f \"$HOME/.claude.json\"\ngrep -q settings-sentinel \"$HOME/.claude/settings.json\"\nif grep -R -q forbidden-refresh-token \"$HOME/.claude\"; then exit 72; fi\n"
        ),
        SettingsMount::Antigravity => script.push_str(
            "grep -q settings-sentinel \"$HOME/.gemini/settings.json\"\n"
        ),
    }
    if let Some(relative) = matrix.skills_mount {
        script.push_str(&format!(
            "grep -q skill-sentinel \"$HOME/{relative}/matrix/SKILL.md\"\n"
        ));
    }
    match matrix.system_prompt_delivery {
        SystemPromptMode::Append => script.push_str(
            "found=0\nwhile [ $# -gt 0 ]; do if [ \"$1\" = --append-system-prompt-file ]; then shift; grep -q 'Global Developer Context' \"$1\"; found=1; fi; shift; done\ntest $found = 1\n"
        ),
        SystemPromptMode::AppendInline { .. } => script.push_str(
            "printf '%s\\n' \"$@\" | grep -q 'developer_instructions=.*Global Developer Context'\n"
        ),
        SystemPromptMode::Replace => script.push_str(
            "found=0\nwhile [ $# -gt 0 ]; do if [ \"$1\" = --system ]; then shift; printf '%s' \"$1\" | grep -q 'Global Developer Context'; found=1; fi; shift; done\ntest $found = 1\n"
        ),
        SystemPromptMode::AgentsMd => script.push_str(
            "grep -q 'Global Developer Context' /awman/context/global/AGENTS.md\n"
        ),
        SystemPromptMode::EnvFile { var } => script.push_str(&format!(
            "prompt_file=$(printenv {var})\ngrep -q 'Global Developer Context' \"$prompt_file\"\n"
        )),
        SystemPromptMode::AddDir { .. } => script.push_str(
            "found=0\nwhile [ $# -gt 0 ]; do if [ \"$1\" = --add-dir ]; then shift; if [ \"$1\" = /awman/context/global ]; then found=1; fi; fi; shift; done\ntest $found = 1\ngrep -q 'Global Developer Context' /awman/context/global/AGENTS.md\n"
        ),
        SystemPromptMode::Unsupported => script.push_str(
            "if printf '%s\\n' \"$@\" | grep -q 'Global Developer Context'; then exit 73; fi\n"
        ),
    }
    script.push_str("printf 'PASS guest-strategies\\n'\nprintf 'matrix-stderr\\n' >&2\n");
    script
}

#[test]
fn builtin_hw_actual_awman_all_agent_settings_prompt_overlay_strategies() {
    const TEST: &str = "builtin_hw_actual_awman_all_agent_settings_prompt_overlay_strategies";
    let Some((binary, archive)) = gate(TEST) else {
        return;
    };
    for agent in SUPPORTED_AGENTS {
        let matrix = matrix_for(agent).unwrap();
        let temp = tempfile::Builder::new()
            .prefix("aw-matrix.")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path();
        let repo = root.join("repo");
        let home = root.join("home");
        let private_bin = root.join("bin");
        let ro = root.join("ro");
        let rw = root.join("rw");
        for dir in [
            &repo.join(".awman"),
            &home.join(".awman/skills/matrix"),
            &private_bin,
            &ro,
            &rw,
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success());
        std::fs::write(repo.join("Dockerfile.dev"), "FROM scratch\n").unwrap();
        std::fs::write(
            repo.join(format!(".awman/Dockerfile.{agent}")),
            "FROM awman-repo:latest\n",
        )
        .unwrap();
        std::fs::write(ro.join("sentinel"), "read-only").unwrap();
        std::fs::write(home.join(".matrix-no-settings"), "host-home-must-not-leak").unwrap();
        std::fs::write(home.join(".awman/skills/matrix/SKILL.md"), "skill-sentinel").unwrap();
        let paths = AuthPathResolver::at_home(&home).resolve(agent);
        if let SettingsMount::Direct(relative) = matrix.settings_mount {
            let dir = home.join(relative);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("settings.json"),
                r#"{"fixture":"settings-sentinel"}"#,
            )
            .unwrap();
        }
        if let Some(dir) = &paths.settings_dir {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(
                dir.join("settings.json"),
                r#"{"fixture":"settings-sentinel"}"#,
            )
            .unwrap();
        }
        if let Some(path) = paths.config_file {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "{}").unwrap();
        }
        if *agent == "claude" {
            std::fs::write(
                home.join(".claude/.credentials.json"),
                r#"{"claudeAiOauth":{"refreshToken":"forbidden-refresh-token"}}"#,
            )
            .unwrap();
        }
        let executable = root.join("synthetic-agent");
        std::fs::write(&executable, probe(agent)).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(home.join(".awman/config.json"), serde_json::to_vec(&serde_json::json!({
            "runtime": "builtin", "default_agent": agent,
            "builtin": {"stateDir": root.join("s"), "imageSource": {"type":"archive", "path":archive}, "network":{"mode":"none"}}
        })).unwrap()).unwrap();
        // Trap both container helpers and accidental host synthetic-agent use.
        let marker = root.join("host-execution");
        for name in [
            "docker",
            "container",
            "sbx",
            matrix.interactive_entrypoint[0],
        ] {
            let helper = private_bin.join(name);
            std::fs::write(
                &helper,
                format!("#!/bin/sh\ntouch '{}'\nexit 91\n", marker.display()),
            )
            .unwrap();
            std::fs::set_permissions(helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let ping_log = root.join("ready-pings");
        install_ready_ping_stub(&private_bin, agent, &ping_log);
        for (index, skill) in ["skill(matrix)", "skill(*)"].into_iter().enumerate() {
            std::fs::write(repo.join(".awman/config.json"), serde_json::to_vec(&serde_json::json!({
                "agent":agent, "auth":"none", "autoAgentAuthAccepted":true,
                "overlays":[format!("dir({}:/usr/local/bin/{}:ro)", executable.display(), matrix.interactive_entrypoint[0]),
                    format!("dir({}:/matrix/ro:ro)", ro.display()), format!("dir({}:~/matrix-rw:rw)", rw.display()),
                    "env(AWMAN_MATRIX_ENV)", "context(global:ro)", "context(repo:ro)", "context(workflow:rw)", skill]
            })).unwrap()).unwrap();
            let mut ready = awman(&binary, &repo, &home, &private_bin);
            ready.args(["ready", "--json"]);
            let out = run_bounded(ready, Duration::from_secs(180));
            assert!(
                out.status.success(),
                "{agent}/{skill} ready: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let expected_pings = "ping\n".repeat(index + 1);
            assert_eq!(std::fs::read_to_string(&ping_log).unwrap(), expected_pings);
            let mut exec = awman(&binary, &repo, &home, &private_bin);
            exec.env("AWMAN_MATRIX_ENV", "literal value with spaces")
                .args([
                    "exec",
                    "prompt",
                    "--non-interactive",
                    "--agent",
                    agent,
                    "matrix task with spaces",
                ]);
            let out = run_bounded(exec, Duration::from_secs(180));
            assert!(
                out.status.success(),
                "{agent}/{skill} guest: stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(String::from_utf8_lossy(&out.stdout).contains("PASS guest-strategies"));
            assert!(String::from_utf8_lossy(&out.stderr).contains("matrix-stderr"));
            assert_eq!(
                std::fs::read_to_string(rw.join("writeback")).unwrap(),
                "writeback"
            );
            if let SettingsMount::Direct(relative) = matrix.settings_mount {
                assert_eq!(
                    std::fs::read_to_string(home.join(relative).join("writeback")).unwrap(),
                    "settings-writeback"
                );
            }
            assert!(
                !marker.exists(),
                "{agent}/{skill}: host helper or agent executed"
            );
            assert_eq!(
                std::fs::read_to_string(&ping_log).unwrap(),
                expected_pings,
                "guest task must not invoke the host ping stand-in"
            );
        }
    }
}

#[test]
fn synthetic_strategy_probes_parse_as_shell_without_running_on_host() {
    for agent in SUPPORTED_AGENTS {
        let output = std::process::Command::new("sh")
            .args(["-n", "-c", &probe(agent)])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{agent}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
