//! Host pings use owned native fakes and private instance environments.
use crate::support;
use awman::data::{config::env::EnvSnapshot, session::AgentName};
use awman::engine::{
    agent::AgentRegistry,
    ready::{HostAgentPinger, LocalAgentPingResult, GREETINGS},
};
use awman_test_support::{fakes::FakeCli, TestSandbox};
use std::{path::Path, time::Duration};

fn environment(sandbox: &TestSandbox, paths: &[&Path], cwd: &Path) -> EnvSnapshot {
    let mut env = sandbox.env().clone();
    env.set("PATH", std::env::join_paths(paths).unwrap());
    EnvSnapshot::from_instance_env(
        env.iter()
            .map(|(k, v)| (k.to_string_lossy().into(), v.to_string_lossy().into()))
            .collect(),
        cwd.to_path_buf(),
        vec![],
    )
}

fn definition(env: &EnvSnapshot) -> awman::engine::agent::ResolvedAgent {
    AgentRegistry::resolve(env, &AgentName::new("claude").unwrap(), None).unwrap()
}

fn install(sandbox: &TestSandbox) -> FakeCli {
    let fake = sandbox.install_fake(support::fake_cli_exe(), "claude");
    fake.on(&[]).stdout("owned installed response\n");
    fake
}

#[tokio::test]
async fn installed_ping_skips_relative_empty_and_absolute_checkout_entries() {
    let sandbox = support::sandbox(
        "engine::ready::installed_ping_skips_relative_empty_and_absolute_checkout_entries",
    );
    let fake = install(&sandbox);
    let bin = sandbox.work().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    awman_test_support::exe::install_as(fake.exe(), &bin, "claude");
    awman_test_support::exe::install_as(fake.exe(), &sandbox.work(), "claude");
    let subdir = sandbox.work().join("nested");
    std::fs::create_dir_all(&subdir).unwrap();
    std::fs::create_dir(sandbox.work().join(".git")).unwrap();
    let paths: [&Path; 4] = [Path::new(""), Path::new("../bin"), &bin, &sandbox.bin()];
    let env = environment(&sandbox, &paths, &subdir);
    let result = HostAgentPinger::with_env(env.clone())
        .ping(&definition(&env))
        .await;
    let LocalAgentPingResult::Ok { greeting, response } = result else {
        panic!("{result:?}");
    };
    assert_eq!(response, "owned installed response");
    assert!(GREETINGS.contains(&greeting.as_str()));
    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].argv, ["--model", "haiku", "--print", &greeting]);
    assert_eq!(calls[0].env["HOME"], sandbox.home().to_string_lossy());
    assert!(!calls[0].cwd.starts_with(sandbox.work()));
    assert!(!calls[0].cwd.exists(), "scratch directory must be removed");
    sandbox.finish();
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_alias_is_refused_and_lookup_continues_to_installed_program() {
    let sandbox = support::sandbox(
        "engine::ready::symlink_alias_is_refused_and_lookup_continues_to_installed_program",
    );
    let fake = install(&sandbox);
    let local = sandbox.work().join("local");
    let alias = sandbox.tmp().join("alias");
    std::fs::create_dir_all(&local).unwrap();
    awman_test_support::exe::install_as(fake.exe(), &local, "claude");
    std::os::unix::fs::symlink(&local, &alias).unwrap();
    let env = environment(&sandbox, &[&alias, &sandbox.bin()], &sandbox.work());
    assert!(matches!(
        HostAgentPinger::with_env(env.clone())
            .ping(&definition(&env))
            .await,
        LocalAgentPingResult::Ok { .. }
    ));
    assert_eq!(fake.calls().len(), 1);
    let env = environment(&sandbox, &[&alias], &sandbox.work());
    assert_eq!(
        HostAgentPinger::with_env(env.clone())
            .ping(&definition(&env))
            .await,
        LocalAgentPingResult::RefusedInsideCheckout
    );
    assert_eq!(fake.calls().len(), 1);
    sandbox.finish();
}

#[tokio::test]
async fn checkout_only_and_missing_programs_never_authenticate() {
    let sandbox =
        support::sandbox("engine::ready::checkout_only_and_missing_programs_never_authenticate");
    let fake = install(&sandbox);
    awman_test_support::exe::install_as(fake.exe(), &sandbox.work(), "claude");
    let env = environment(&sandbox, &[&sandbox.work()], &sandbox.work());
    assert_eq!(
        HostAgentPinger::with_env(env.clone())
            .ping(&definition(&env))
            .await,
        LocalAgentPingResult::RefusedInsideCheckout
    );
    let missing = sandbox.tmp().join("no-programs");
    for paths in [vec![Path::new("")], vec![Path::new(".")], vec![&missing]] {
        let env = environment(&sandbox, &paths, &sandbox.work());
        assert_eq!(
            HostAgentPinger::with_env(env.clone())
                .ping(&definition(&env))
                .await,
            LocalAgentPingResult::NotInstalled
        );
    }
    assert!(fake.calls().is_empty());
    sandbox.finish();
}

#[tokio::test]
async fn linked_worktrees_and_main_checkout_are_excluded_from_either_checkout() {
    let sandbox = support::sandbox(
        "engine::ready::linked_worktrees_and_main_checkout_are_excluded_from_either_checkout",
    );
    let fake = install(&sandbox);
    let main = sandbox.work();
    let linked = sandbox.tmp().join("linked");
    let admin = main.join(".git/worktrees/linked");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::create_dir_all(&linked).unwrap();
    std::fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", admin.display()),
    )
    .unwrap();
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    std::fs::write(
        admin.join("gitdir"),
        format!("{}\n", linked.join(".git").display()),
    )
    .unwrap();
    awman_test_support::exe::install_as(fake.exe(), &main, "claude");
    awman_test_support::exe::install_as(fake.exe(), &linked, "claude");
    for (cwd, candidate) in [(&main, &linked), (&linked, &main)] {
        let env = environment(&sandbox, &[candidate], cwd);
        assert_eq!(
            HostAgentPinger::with_env(env.clone())
                .ping(&definition(&env))
                .await,
            LocalAgentPingResult::RefusedInsideCheckout
        );
    }
    assert!(fake.calls().is_empty());
    sandbox.finish();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelling_ping_kills_owned_child_and_removes_empty_private_cwd() {
    let sandbox = support::sandbox(
        "engine::ready::cancelling_ping_kills_owned_child_and_removes_empty_private_cwd",
    );
    let fake = sandbox.install_fake(support::fake_cli_exe(), "claude");
    fake.on(&[]).hold_until_released();
    let env = environment(&sandbox, &[&sandbox.bin()], &sandbox.work());
    let def = definition(&env);
    let ping = tokio::spawn(async move { HostAgentPinger::with_env(env).ping(&def).await });
    let call = awman_test_support::wait::wait_for_async(
        "owned ping to hold",
        Duration::from_secs(3),
        || async { fake.calls().into_iter().find(|call| call.held) },
    )
    .await;
    assert_eq!(std::fs::read_dir(&call.cwd).unwrap().count(), 0);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&call.cwd).unwrap().permissions().mode() & 0o777,
        0o700
    );
    // Observe the production-owned process rather than starting a second
    // supervisor. The unique scratch cwd identifies only this test's child.
    let pid = std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(Result::ok)
        .find_map(|entry| {
            let pid = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
            (std::fs::read_link(entry.path().join("cwd")).ok().as_ref() == Some(&call.cwd))
                .then_some(pid)
        })
        .expect("held child has the private cwd");
    ping.abort();
    assert!(ping.await.unwrap_err().is_cancelled());
    awman_test_support::wait::wait_for_async(
        "ping child reaped",
        Duration::from_secs(3),
        || async { (!Path::new("/proc").join(pid.to_string()).exists()).then_some(()) },
    )
    .await;
    assert!(!call.cwd.exists());
    sandbox.finish();
}

#[cfg(unix)]
#[tokio::test]
async fn custom_ping_approval_binds_definition_and_canonical_program_separately() {
    let sandbox = support::sandbox(
        "engine::ready::custom_ping_approval_binds_definition_and_canonical_program_separately",
    );
    let fake = install(&sandbox);
    let local = sandbox.work().join("bin");
    std::fs::create_dir_all(&local).unwrap();
    awman_test_support::exe::install_as(fake.exe(), &local, "claude");
    std::os::unix::fs::symlink(sandbox.bin().join(".fake"), local.join(".fake")).unwrap();
    let agents = sandbox.work().join(".awman/agents");
    std::fs::create_dir_all(&agents).unwrap();
    let file = agents.join("owned.toml");
    let contents = "schema = 1\ndisplay_name = \"Owned\"\n[run]\ncommand = [\"owned\"]\n[image]\ninstall = [\"echo owned\"]\n[ping]\ncommand = [\"claude\", \"--custom\"]\n";
    std::fs::write(&file, contents).unwrap();
    let name = AgentName::new("owned").unwrap();
    let env = environment(&sandbox, &[&local], &sandbox.work());
    assert!(AgentRegistry::resolve(&env, &name, Some(&sandbox.work())).is_err());
    let untrusted = AgentRegistry::load_untrusted(&env, &name, Some(&sandbox.work())).unwrap();
    AgentRegistry::record_approval(&env, untrusted.definition(), Some(&sandbox.work())).unwrap();
    let approved = AgentRegistry::resolve(&env, &name, Some(&sandbox.work())).unwrap();
    // An approved custom program in a checkout never grants first-party trust.
    assert_eq!(
        HostAgentPinger::with_env(env.clone())
            .ping(&definition(&env))
            .await,
        LocalAgentPingResult::RefusedInsideCheckout
    );
    let changed = environment(&sandbox, &[&sandbox.bin()], &sandbox.work());
    assert!(AgentRegistry::resolve(&changed, &name, Some(&sandbox.work())).is_err());
    // Already resolved definitions pin the approved path, even if the ping's
    // snapshot has a different PATH by the time it executes.
    assert!(matches!(
        HostAgentPinger::with_env(changed).ping(&approved).await,
        LocalAgentPingResult::Ok { .. }
    ));
    assert_eq!(fake.calls().len(), 1);
    std::fs::write(&file, contents.replace("--custom", "--changed")).unwrap();
    assert!(AgentRegistry::resolve(&env, &name, Some(&sandbox.work())).is_err());
    sandbox.finish();
}

#[tokio::test]
async fn installed_refresh_reports_success_only_after_credential_expiry_advances() {
    use awman::engine::{
        auth::{credential::CredentialBinding, keychain::refreshable_spec_for},
        ready::HostRefreshOutcome,
    };
    let s = support::sandbox(
        "engine::ready::installed_refresh_reports_success_only_after_credential_expiry_advances",
    );
    let fake = s.install_fake(support::fake_cli_exe(), "claude");
    fake.on(&[]).hold_until_released();
    let source = s.tmp().join("host-credential.json");
    std::fs::write(
        &source,
        r#"{"claudeAiOauth":{"accessToken":"owned-before","expiresAt":1}}"#,
    )
    .unwrap();
    let binding = CredentialBinding::to_file(
        awman::data::fs::AuthPathResolver::at_home(s.home()),
        &source,
    );
    let env = environment(&s, &[&s.bin()], &s.work());
    let spec = refreshable_spec_for(&AgentName::new("claude").unwrap()).unwrap();
    let ping = tokio::spawn(async move {
        HostAgentPinger::with_env(env)
            .refresh_credential(spec, &binding)
            .await
    });
    awman_test_support::wait::wait_for_async(
        "installed refresh ping to hold",
        Duration::from_secs(3),
        || async { fake.calls().iter().any(|c| c.held).then_some(()) },
    )
    .await;
    // Simulate the host provider publishing a rotated credential while the
    // owned fake is running. No host provider or login state is involved.
    std::fs::write(
        &source,
        r#"{"claudeAiOauth":{"accessToken":"owned-after","expiresAt":4102444800000}}"#,
    )
    .unwrap();
    fake.release();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), ping)
            .await
            .unwrap()
            .unwrap(),
        HostRefreshOutcome::Advanced {
            expires_at: std::time::UNIX_EPOCH + Duration::from_millis(4102444800000)
        }
    );
    assert_eq!(fake.calls().len(), 1);
    s.finish();
}

#[tokio::test]
async fn separate_git_directory_does_not_hide_the_main_checkout_from_a_linked_ping() {
    let s = support::sandbox(
        "engine::ready::separate_git_directory_does_not_hide_the_main_checkout_from_a_linked_ping",
    );
    let fake = install(&s);
    let main = s.work();
    let linked = s.tmp().join("linked");
    let common = s.tmp().join("git-storage");
    let admin = common.join("worktrees/linked");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::create_dir_all(&linked).unwrap();
    std::fs::write(main.join(".git"), format!("gitdir: {}\n", common.display())).unwrap();
    std::fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", admin.display()),
    )
    .unwrap();
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    std::fs::write(
        admin.join("gitdir"),
        format!("{}\n", linked.join(".git").display()),
    )
    .unwrap();
    awman_test_support::exe::install_as(fake.exe(), &main, "claude");
    let env = environment(&s, &[&main], &linked);
    assert_eq!(
        HostAgentPinger::with_env(env.clone())
            .ping(&definition(&env))
            .await,
        LocalAgentPingResult::RefusedInsideCheckout
    );
    assert!(fake.calls().is_empty());
    let env = environment(&s, &[&main, &s.bin()], &linked);
    assert!(matches!(
        HostAgentPinger::with_env(env.clone())
            .ping(&definition(&env))
            .await,
        LocalAgentPingResult::Ok { .. }
    ));
    assert_eq!(fake.calls().len(), 1);
    s.finish();
}

#[cfg(unix)]
#[tokio::test]
async fn installed_script_cannot_run_checkout_interpreter() {
    use std::os::unix::fs::PermissionsExt;
    let s = support::sandbox("engine::ready::installed_script_cannot_run_checkout_interpreter");
    let local = s.work().join("node_modules/.bin");
    std::fs::create_dir_all(&local).unwrap();
    let installed = s.tmp().join("installed");
    std::fs::create_dir_all(&installed).unwrap();
    let marker = s.tmp().join("checkout-code-ran");
    for (path, body) in [
        (
            installed.join("claude"),
            "#!/usr/bin/env a02-private-interpreter\n".to_string(),
        ),
        (
            local.join("a02-private-interpreter"),
            format!("#!/bin/sh\necho ran > '{}'\n", marker.display()),
        ),
    ] {
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let env = environment(&s, &[&local, &installed], &s.work());
    assert!(!matches!(
        HostAgentPinger::with_env(env.clone())
            .ping(&definition(&env))
            .await,
        LocalAgentPingResult::Ok { .. }
    ));
    assert!(!marker.exists());
    s.finish();
}

#[cfg(unix)]
#[tokio::test]
async fn non_repository_home_does_not_exclude_installed_programs() {
    let s =
        support::sandbox("engine::ready::non_repository_home_does_not_exclude_installed_programs");
    let fake = install(&s);
    let local = s.home().join(".local/bin");
    std::fs::create_dir_all(&local).unwrap();
    awman_test_support::exe::install_as(fake.exe(), &local, "claude");
    std::os::unix::fs::symlink(s.bin().join(".fake"), local.join(".fake")).unwrap();
    let env = environment(&s, &[&local], &s.home());
    assert!(matches!(
        HostAgentPinger::with_env(env.clone())
            .ping(&definition(&env))
            .await,
        LocalAgentPingResult::Ok { .. }
    ));
    assert_eq!(fake.calls().len(), 1);
    s.finish();
}

#[tokio::test]
async fn relative_linked_worktree_backlinks_remain_excluded() {
    let s = support::sandbox("engine::ready::relative_linked_worktree_backlinks_remain_excluded");
    let fake = install(&s);
    let main = s.work();
    let linked = main.join("linked");
    let admin = main.join(".git/worktrees/linked");
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::create_dir_all(&linked).unwrap();
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    std::fs::write(admin.join("gitdir"), "../../../linked/.git\n").unwrap();
    std::fs::write(linked.join(".git"), "gitdir: ../.git/worktrees/linked\n").unwrap();
    awman_test_support::exe::install_as(fake.exe(), &linked, "claude");
    let env = environment(&s, &[&linked], &main);
    assert_eq!(
        HostAgentPinger::with_env(env.clone())
            .ping(&definition(&env))
            .await,
        LocalAgentPingResult::RefusedInsideCheckout
    );
    assert!(fake.calls().is_empty());
    s.finish();
}
