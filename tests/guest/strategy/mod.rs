use crate::support;
use awman::engine::agent::definition::{SettingsMount, SystemPromptMode};
use awman::engine::agent::first_party::first_party;
fn probe(agent: &str) -> String {
    let matrix = first_party(agent).unwrap();
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
    match &matrix.settings_mount {
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
    if let Some(relative) = &matrix.skills_mount {
        script.push_str(&format!(
            "grep -q skill-sentinel \"$HOME/{relative}/matrix/SKILL.md\"\n"
        ));
    }
    match &matrix.system_prompt_delivery {
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
        SystemPromptMode::EnvDir { var } => script.push_str(&format!(
            "prompt_dir=$(printenv {var})\ngrep -q 'Global Developer Context' \"$prompt_dir/AGENTS.md\"\n"
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
fn run(id: &'static str, agent: &str, skill: &str) {
    use awman::data::fs::auth_paths::AuthPathResolver;
    let s = support::sandbox(id);
    let archive = support::fixture(&s);
    support::watch(&s);
    let descriptor = first_party(agent).unwrap();
    let repo = support::session::prepare(&s, &archive, agent, &probe(agent));
    let ro = s.root().join("ro");
    let rw = s.root().join("rw");
    for dir in [&ro, &rw, &s.awman_home().join("skills/matrix")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(ro.join("sentinel"), "read-only").unwrap();
    std::fs::write(
        s.home().join(".matrix-no-settings"),
        "host-home-must-not-leak",
    )
    .unwrap();
    std::fs::write(
        s.awman_home().join("skills/matrix/SKILL.md"),
        "skill-sentinel",
    )
    .unwrap();
    let paths = AuthPathResolver::at_home(s.home()).resolve(agent);
    let settings = match &descriptor.settings_mount {
        SettingsMount::Direct(relative) => Some(s.home().join(relative)),
        _ => paths.settings_dir,
    };
    if let Some(dir) = settings {
        std::fs::create_dir_all(&dir).unwrap();
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
    if agent == "claude" {
        std::fs::write(
            s.home().join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"refreshToken":"forbidden-refresh-token"}}"#,
        )
        .unwrap();
    }
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(repo.join(".awman/config.json")).unwrap()).unwrap();
    // Use the descriptor's actual executable name, including renamed frontends.
    config["overlays"][0] = format!(
        "dir({}:/usr/local/bin/{}:ro)",
        s.root().join("synthetic-agent").display(),
        descriptor.interactive_entrypoint[0]
    )
    .into();
    for overlay in [
        format!("dir({}:/matrix/ro:ro)", ro.display()),
        format!("dir({}:~/matrix-rw:rw)", rw.display()),
        "env(AWMAN_MATRIX_ENV)".into(),
        "context(global:ro)".into(),
        "context(repo:ro)".into(),
        "context(workflow:rw)".into(),
        skill.into(),
    ] {
        config["overlays"]
            .as_array_mut()
            .unwrap()
            .push(overlay.into());
    }
    std::fs::write(
        repo.join(".awman/config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    support::session::ready(&s, support::awman_exe(), &repo);
    let out = support::awman(&s)
        .current_dir(repo)
        .env("AWMAN_TEST_BUILTIN", "1")
        .env("AWMAN_MATRIX_ENV", "literal value with spaces")
        .args([
            "exec",
            "prompt",
            "-n",
            "--agent",
            agent,
            "matrix task with spaces",
        ])
        .run();
    out.assert_success();
    assert!(
        out.stdout_text().contains("PASS guest-strategies"),
        "{out:?}"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("matrix-stderr"));
    assert_eq!(
        std::fs::read_to_string(rw.join("writeback")).unwrap(),
        "writeback"
    );
}
#[test]
fn claude_named_skills() {
    run("strategy::claude_named_skills", "claude", "skill(matrix)");
}
#[test]
fn claude_all_skills() {
    run("strategy::claude_all_skills", "claude", "skill(*)");
}
#[test]
fn codex_named_skills() {
    run("strategy::codex_named_skills", "codex", "skill(matrix)");
}
#[test]
fn codex_all_skills() {
    run("strategy::codex_all_skills", "codex", "skill(*)");
}
#[test]
fn opencode_named_skills() {
    run(
        "strategy::opencode_named_skills",
        "opencode",
        "skill(matrix)",
    );
}
#[test]
fn opencode_all_skills() {
    run("strategy::opencode_all_skills", "opencode", "skill(*)");
}
#[test]
fn gemini_named_skills() {
    run("strategy::gemini_named_skills", "gemini", "skill(matrix)");
}
#[test]
fn gemini_all_skills() {
    run("strategy::gemini_all_skills", "gemini", "skill(*)");
}
#[test]
fn copilot_named_skills() {
    run("strategy::copilot_named_skills", "copilot", "skill(matrix)");
}
#[test]
fn copilot_all_skills() {
    run("strategy::copilot_all_skills", "copilot", "skill(*)");
}
#[test]
fn cline_named_skills() {
    run("strategy::cline_named_skills", "cline", "skill(matrix)");
}
#[test]
fn cline_all_skills() {
    run("strategy::cline_all_skills", "cline", "skill(*)");
}
#[test]
fn antigravity_named_skills() {
    run(
        "strategy::antigravity_named_skills",
        "antigravity",
        "skill(matrix)",
    );
}
#[test]
fn antigravity_all_skills() {
    run(
        "strategy::antigravity_all_skills",
        "antigravity",
        "skill(*)",
    );
}
