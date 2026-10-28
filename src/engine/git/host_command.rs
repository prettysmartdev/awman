//! Host Git must not run repository-supplied hooks, monitors or content drivers.
use std::path::Path;
use std::process::Command;

pub(super) fn command(cwd: &Path) -> Command {
    let mut command = discovery_command(cwd);
    if cwd.join(".git").exists() {
        // The session/lifecycle owns the mount root. Writable core.worktree
        // configuration must never redirect host staging or checkout.
        command
            .arg("-c")
            .arg(format!("core.worktree={}", cwd.display()));
    }
    command
}

pub(super) fn discovery_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .env("GIT_NO_LAZY_FETCH", "1");
    for setting in [
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "commit.gpgSign=false",
        "tag.gpgSign=false",
        "push.gpgSign=false",
        "maintenance.auto=false",
        "gc.auto=0",
        "diff.external=",
        "protocol.ext.allow=never",
        "submodule.recurse=false",
        "core.alternateRefsCommand=",
    ] {
        command.args(["-c", setting]);
    }
    // Attribute names are arbitrary. Disable every configured executable
    // content driver, including those selected by .git/info/attributes.
    // `git config` only reads settings; it does not execute their values.
    if let Ok(output) = Command::new("git")
        .args(["config", "--null", "--name-only", "--get-regexp", "^(filter\\..*\\.(clean|smudge|process)|diff\\..*\\.(command|textconv)|merge\\..*\\.driver)$"])
        .env("GIT_CONFIG_NOSYSTEM", "1").current_dir(cwd).output()
    {
        for key in output.stdout.split(|byte| *byte == 0).filter(|key| !key.is_empty()) {
            let key = String::from_utf8_lossy(key);
            let value = if key.starts_with("merge.") { "false" } else { "" };
            command.arg("-c").arg(format!("{key}={value}"));
            if let Some(name) = key.strip_prefix("filter.").and_then(|s| s.rsplit_once('.').map(|p| p.0)) {
                command.arg("-c").arg(format!("filter.{name}.required=false"));
            }
        }
    }
    command
}
