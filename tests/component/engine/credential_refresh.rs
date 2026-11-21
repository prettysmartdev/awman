//! engine::credential_refresh component tests (filled in by slice-layer1-engine).

#[cfg(unix)]
#[test]
fn helper_atomic_generations_are_single_open_and_refuse_links() {
    use awman::data::secret::SecretString;
    use awman::engine::credential_refresh::delivery::HelperDirectory;
    let sandbox = crate::support::sandbox(
        "engine::credential_refresh::helper_atomic_generations_are_single_open_and_refuse_links",
    );
    let root = sandbox.tmp().join("helper");
    awman::data::fs::private_files::create_private_dir(&root).unwrap();
    let directory = HelperDirectory::open(&root).unwrap();
    directory
        .stage(1, &SecretString::new("test-secret-old"))
        .unwrap();
    let old = std::fs::File::open(root.join("payload")).unwrap();
    directory
        .stage(2, &SecretString::new("test-secret-new"))
        .unwrap();
    use std::io::Read;
    let mut content = String::new();
    let mut old = old;
    old.read_to_string(&mut content).unwrap();
    assert_eq!(content, "1\ntest-secret-old");
    assert_eq!(
        std::fs::read_to_string(root.join("payload")).unwrap(),
        "2\ntest-secret-new"
    );
    std::fs::hard_link(root.join("payload"), root.join("alias")).unwrap();
    assert!(directory
        .stage(3, &SecretString::new("test-secret-third"))
        .is_err());
    std::fs::remove_file(root.join("alias")).unwrap();
    std::fs::remove_file(root.join("payload")).unwrap();
    std::os::unix::fs::symlink(root.join("elsewhere"), root.join("payload")).unwrap();
    assert!(directory
        .stage(4, &SecretString::new("test-secret-fourth"))
        .is_err());
    assert!(directory
        .stage(0, &SecretString::new("test-secret-zero"))
        .is_err());
    assert!(directory
        .stage(5, &SecretString::new("test-secret-newline\n"))
        .is_err());
    sandbox.finish();
}
#[cfg(unix)]
#[tokio::test]
async fn same_agent_authorities_notify_matching_leases_and_never_rewrite_managed_files() {
    use awman::data::{fs::AuthPathResolver, session::AgentName};
    use awman::engine::{
        auth::{
            credential::{CredentialBinding, CredentialFingerprint},
            RefreshableCredentialDelivery,
        },
        credential_refresh::{lease::SourceSink, CredentialRefreshMonitor, MonitorConfig},
    };
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };
    let sandbox=crate::support::sandbox("engine::credential_refresh::same_agent_authorities_notify_matching_leases_and_never_rewrite_managed_files");
    let resolver = AuthPathResolver::at_home(sandbox.home().to_path_buf());
    let a = sandbox.tmp().join("source-a");
    let b = sandbox.tmp().join("source-b");
    std::fs::write(
        &a,
        r#"{"claudeAiOauth":{"accessToken":"test-secret-a","expiresAt":4102444800000}}"#,
    )
    .unwrap();
    std::fs::write(
        &b,
        r#"{"claudeAiOauth":{"accessToken":"test-secret-b","expiresAt":4102444800000}}"#,
    )
    .unwrap();
    let binding_a = CredentialBinding::to_file(resolver.clone(), &a);
    let binding_b = CredentialBinding::to_file(resolver.clone(), &b);
    let monitor = CredentialRefreshMonitor::with_binding_in(
        MonitorConfig {
            tick_interval: Duration::from_secs(60),
            ..Default::default()
        },
        binding_a.clone(),
        &crate::support::env_snapshot(&sandbox),
    );
    let received = Arc::new(Mutex::new(Vec::<String>::new()));
    let delivery = RefreshableCredentialDelivery {
        agent: AgentName::new("claude").unwrap(),
        spec_agent: "claude",
        credential_env_key: "CLAUDE_CODE_OAUTH_TOKEN",
        staged_path: sandbox.tmp().join("not-written"),
        staged_root: sandbox.tmp().to_path_buf(),
        initial_fingerprint: CredentialFingerprint::zeroed(),
    };
    let db = std::sync::Arc::new(
        awman::data::fs::api_db::SqliteSessionStore::open(&sandbox.tmp().join("authorities.db"))
            .unwrap(),
    );
    let store = awman::data::automation_store::AutomationStore::open(db).unwrap();
    let authority_a = store
        .credential_authority(
            "fixture-owner",
            &awman::engine::credential_refresh::authority::source_key(
                &awman::engine::auth::credential::HostCredentialSource::HostFile {
                    path: a.clone(),
                },
                "claude",
            ),
            "source-a",
        )
        .unwrap();
    let authority_b = store
        .credential_authority(
            "fixture-owner",
            &awman::engine::credential_refresh::authority::source_key(
                &awman::engine::auth::credential::HostCredentialSource::HostFile {
                    path: b.clone(),
                },
                "claude",
            ),
            "source-b",
        )
        .unwrap();
    let out = received.clone();
    let lease_a = monitor
        .register_managed(
            &delivery,
            "a",
            binding_a.clone(),
            authority_a,
            SourceSink(Arc::new(move |s| {
                out.lock().unwrap().push(s.secret.expose().to_owned());
                true
            })),
        )
        .unwrap();
    let out = received.clone();
    let lease_b = monitor
        .register_managed(
            &delivery,
            "b",
            binding_b.clone(),
            authority_b,
            SourceSink(Arc::new(move |s| {
                out.lock().unwrap().push(s.secret.expose().to_owned());
                true
            })),
        )
        .unwrap();
    monitor
        .refresh_bound(&delivery.agent, binding_a, Duration::from_secs(2))
        .await;
    monitor
        .refresh_bound(&delivery.agent, binding_b, Duration::from_secs(2))
        .await;
    let ids: Vec<_> = monitor
        .status()
        .into_iter()
        .map(|s| s.authority_id)
        .collect();
    assert!(ids.contains(&authority_a) && ids.contains(&authority_b));
    assert!(monitor
        .register_managed(
            &delivery,
            "conflict",
            awman::engine::auth::credential::CredentialBinding::to_file(resolver.clone(), &a),
            authority_b,
            SourceSink(Arc::new(|_| true))
        )
        .is_err());
    let values = received.lock().unwrap().clone();
    assert!(values.contains(&"test-secret-a".into()));
    assert!(values.contains(&"test-secret-b".into()));
    assert_eq!(values.len(), 2);
    assert!(!delivery.staged_path.exists());
    let status = format!("{:?}", monitor.status());
    assert!(!status.contains("test-secret"));
    assert!(!status.contains("fingerprint"));
    drop((lease_a, lease_b, monitor));
    sandbox.finish();
}
#[test]
fn antigravity_access_only_payload_discards_operator_refresh_grant_and_debug_bytes() {
    use awman::engine::auth::keychain::{access_only_antigravity, AgentSecretFile};
    let raw=br#"{"token":{"access_token":"test-secret-access","refresh_token":"test-secret-refresh","token_type":"Bearer","expiry":"2030-01-01T00:00:00Z"},"auth_method":"consumer","private":"test-secret-extra"}"#;
    let contents = access_only_antigravity(raw).unwrap();
    let text = String::from_utf8(contents.clone()).unwrap();
    assert!(text.contains("test-secret-access"));
    assert!(!text.contains("refresh"));
    assert!(!text.contains("test-secret-extra"));
    let file = AgentSecretFile {
        relative_path: "payload".into(),
        contents,
        mode: 0o600,
    };
    assert!(!format!("{file:?}").contains("test-secret"));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn authority_lock_serializes_owned_processes_across_data_homes() {
    use awman::engine::{
        auth::credential::HostCredentialSource,
        credential_refresh::authority::{lock_source_at, source_key},
    };
    use std::time::Duration;
    let sandbox = crate::support::sandbox(
        "engine::credential_refresh::authority_lock_serializes_owned_processes_across_data_homes",
    );
    let source = HostCredentialSource::HostFile {
        path: sandbox.tmp().join("private-source"),
    };
    let root = sandbox.tmp().join("locks");
    let lock = lock_source_at(&root, &source, "claude", Duration::from_secs(1))
        .await
        .unwrap();
    drop(lock);
    let key = source_key(&source, "claude");
    let path = root.join(key);
    let mut process=sandbox.command("/bin/sh").args(["-c","set -eu; exec 9<>\"$1\"; /usr/bin/flock -x 9; printf 'locked\\n'; IFS= read -r release","authority-holder"]).arg(&path).stdin(awman_test_support::Stdin::Piped).ready(awman_test_support::Ready::StdoutLine("locked".into())).spawn();
    assert!(lock_source_at(&root, &source, "claude", Duration::ZERO)
        .await
        .is_err());
    process.stdin().write_all(b"release\n").unwrap();
    assert_eq!(
        process.wait(Duration::from_secs(1)),
        awman_test_support::Exit::Code(0)
    );
    let root = sandbox.tmp().join("locks");
    let lock = lock_source_at(&root, &source, "claude", Duration::from_secs(1))
        .await
        .unwrap();
    drop(lock);
    std::fs::remove_file(path).unwrap();
    sandbox.finish();
}

#[tokio::test]
async fn legacy_file_refresh_registers_new_tokens_with_the_launch_redactor() {
    use awman::data::{fs::AuthPathResolver, session::AgentName};
    use awman::engine::{
        auth::{
            credential::{CredentialBinding, CredentialFingerprint},
            redaction::RedactionSet,
            RefreshableCredentialDelivery,
        },
        credential_refresh::{CredentialLeaseFactory, CredentialRefreshMonitor, MonitorConfig},
    };
    use std::time::Duration;
    let sandbox=crate::support::sandbox("engine::credential_refresh::legacy_file_refresh_registers_new_tokens_with_the_launch_redactor");
    let source = sandbox.tmp().join("source");
    let root = sandbox.tmp().join("staged");
    awman::data::fs::private_files::create_private_dir(&root).unwrap();
    std::fs::write(
        &source,
        r#"{"claudeAiOauth":{"accessToken":"test-secret-new-legacy","expiresAt":4102444800000}}"#,
    )
    .unwrap();
    let path = root.join(".credentials.json");
    std::fs::write(&path, b"old credential").unwrap();
    let binding = CredentialBinding::to_file(AuthPathResolver::at_home(sandbox.home()), &source);
    let monitor = CredentialRefreshMonitor::with_binding_in(
        MonitorConfig {
            tick_interval: Duration::from_secs(60),
            ..Default::default()
        },
        binding.clone(),
        &crate::support::env_snapshot(&sandbox),
    );
    let delivery = RefreshableCredentialDelivery {
        agent: AgentName::new("claude").unwrap(),
        spec_agent: "claude",
        credential_env_key: "CLAUDE_CODE_OAUTH_TOKEN",
        staged_path: path.clone(),
        staged_root: root,
        initial_fingerprint: CredentialFingerprint::zeroed(),
    };
    let redaction = RedactionSet::new();
    let lease = monitor.register_redacted_lease(&delivery, "legacy-private", redaction.clone());
    monitor
        .refresh_bound(&delivery.agent, binding, Duration::from_secs(2))
        .await;
    assert!(std::fs::read_to_string(path)
        .unwrap()
        .contains("test-secret-new-legacy"));
    assert_eq!(redaction.redact("test-secret-new-legacy"), "***");
    drop((lease, monitor));
    sandbox.finish();
}

#[test]
fn refused_redaction_bundle_preserves_confirmed_output() {
    use awman::engine::auth::{credential::SecretString, redaction::RedactionSet};
    let sandbox = crate::support::sandbox(
        "engine::credential_refresh::refused_redaction_bundle_preserves_confirmed_output",
    );
    let redaction = RedactionSet::new();
    redaction.add(SecretString::new("test-secret-kept"));
    assert!(redaction
        .try_add_many([SecretString::new("x".repeat(RedactionSet::MAX_BYTES))])
        .is_err());
    assert!(!redaction.is_retired());
    assert_eq!(
        redaction.redact("test-secret-kept normal output"),
        "*** normal output"
    );
    sandbox.finish();
}

#[cfg(unix)]
#[test]
fn helper_retirement_cleans_payload_temporary_files_and_blocks_late_writes() {
    use awman::{
        data::{fs::private_files::create_private_dir, secret::SecretString},
        engine::credential_refresh::delivery::HelperDirectory,
    };
    let sandbox = crate::support::sandbox("engine::credential_refresh::helper_retirement_cleans_payload_temporary_files_and_blocks_late_writes");
    let root = sandbox.tmp().join("helper");
    create_private_dir(&root).unwrap();
    std::fs::write(root.join("payload"), "test-secret-crash-left").unwrap();
    std::fs::write(root.join(".generation-crash-left"), "test-secret-partial").unwrap();
    let directory = HelperDirectory::open(&root).unwrap();
    assert!(!root.join("payload").exists());
    assert!(!root.join(".generation-crash-left").exists());
    directory
        .stage(2, &SecretString::new("test-secret-retire"))
        .unwrap();
    std::fs::write(root.join(".generation-orphan"), "test-secret-partial").unwrap();
    directory.retire().unwrap();
    assert!(!root.join("payload").exists());
    assert!(!root.join(".generation-orphan").exists());
    assert!(directory
        .stage(3, &SecretString::new("test-secret-late"))
        .is_err());
    directory.retire().unwrap();
    drop(directory);
    let directory = HelperDirectory::open(&root).unwrap();
    directory
        .stage(4, &SecretString::new("test-secret-drop"))
        .unwrap();
    drop(directory);
    assert!(!root.join("payload").exists());
    sandbox.finish();
}

#[cfg(unix)]
#[test]
fn authority_identity_survives_missing_files_and_keychain_accounts_stay_distinct() {
    use awman::engine::{
        auth::credential::HostCredentialSource, credential_refresh::authority::source_key,
    };
    let sandbox = crate::support::sandbox("engine::credential_refresh::authority_identity_survives_missing_files_and_keychain_accounts_stay_distinct");
    let real = sandbox.tmp().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = sandbox.tmp().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let source = HostCredentialSource::HostFile {
        path: alias.join("credential"),
    };
    let absent = source_key(&source, "claude");
    std::fs::write(real.join("credential"), "test-secret").unwrap();
    assert_eq!(source_key(&source, "claude"), absent);
    std::fs::remove_file(real.join("credential")).unwrap();
    assert_eq!(source_key(&source, "claude"), absent);
    assert_ne!(
        source_key(
            &HostCredentialSource::MacosKeychain {
                service: "service",
                account: Some("a")
            },
            "claude"
        ),
        source_key(
            &HostCredentialSource::MacosKeychain {
                service: "service",
                account: Some("b")
            },
            "claude"
        )
    );
    sandbox.finish();
}

#[cfg(unix)]
#[tokio::test]
async fn authority_lock_uses_private_runtime_root_and_reports_insecure_root_remediation() {
    use awman::{
        data::{config::env::EnvSnapshot, fs::private_files::create_private_dir},
        engine::{
            auth::credential::HostCredentialSource,
            credential_refresh::authority::{lock_root, lock_source_at},
        },
    };
    let sandbox = crate::support::sandbox("engine::credential_refresh::authority_lock_uses_private_runtime_root_and_reports_insecure_root_remediation");
    let runtime = sandbox.tmp().join("runtime");
    create_private_dir(&runtime).unwrap();
    let env = EnvSnapshot::from_full_env(std::collections::BTreeMap::from([(
        "XDG_RUNTIME_DIR".into(),
        runtime.to_str().unwrap().to_owned(),
    )]));
    assert_eq!(
        lock_root(&env, &sandbox.home()),
        runtime.join("awman-credential-authorities")
    );
    let insecure = runtime.join("awman-credential-authorities");
    std::fs::create_dir(&insecure).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&insecure, std::fs::Permissions::from_mode(0o777)).unwrap();
    let source = HostCredentialSource::HostFile {
        path: sandbox.tmp().join("source"),
    };
    assert_eq!(
        lock_source_at(&insecure, &source, "claude", std::time::Duration::ZERO)
            .await
            .unwrap_err(),
        "authority_lock_root_refused_remove_foreign_or_insecure_directory"
    );
    use awman::engine::{
        auth::credential::{CredentialBinding, CredentialReadError},
        credential_refresh::{CredentialRefreshMonitor, MonitorConfig, RefreshOutcome},
    };
    let binding = CredentialBinding::to_file(
        awman::data::fs::AuthPathResolver::at_home(sandbox.home()),
        sandbox.tmp().join("source"),
    );
    let monitor =
        CredentialRefreshMonitor::with_binding_in(MonitorConfig::default(), binding.clone(), &env);
    let outcome = monitor
        .refresh_bound(
            &awman::data::session::AgentName::new("claude").unwrap(),
            binding,
            std::time::Duration::from_secs(1),
        )
        .await;
    assert!(
        matches!(&outcome, RefreshOutcome::Unavailable { reason: CredentialReadError::Unavailable(reason) } if reason.contains("remove_foreign_or_insecure_directory"))
    );
    assert_eq!(monitor.status()[0].last_outcome, Some(outcome));
    assert_eq!(monitor.status()[0].consecutive_failures, 1);
    sandbox.finish();
}

#[cfg(windows)]
#[test]
fn managed_windows_enrollment_refuses_the_legacy_locking_exception() {
    use awman::{
        data::{fs::AuthPathResolver, session::AgentName},
        engine::{
            auth::{
                credential::{CredentialBinding, CredentialFingerprint},
                RefreshableCredentialDelivery,
            },
            credential_refresh::{lease::SourceSink, CredentialRefreshMonitor, MonitorConfig},
        },
    };
    let sandbox = crate::support::sandbox("engine::credential_refresh::managed_windows_enrollment_refuses_the_legacy_locking_exception");
    let binding = CredentialBinding::to_file(
        AuthPathResolver::at_home(sandbox.home()),
        sandbox.tmp().join("source"),
    );
    let monitor = CredentialRefreshMonitor::with_binding_in(
        MonitorConfig::default(),
        binding.clone(),
        &crate::support::env_snapshot(&sandbox),
    );
    let delivery = RefreshableCredentialDelivery {
        agent: AgentName::new("claude").unwrap(),
        spec_agent: "claude",
        credential_env_key: "CLAUDE_CODE_OAUTH_TOKEN",
        staged_path: sandbox.tmp().join("staged"),
        staged_root: sandbox.tmp(),
        initial_fingerprint: CredentialFingerprint::zeroed(),
    };
    let result = monitor.register_managed(
        &delivery,
        "fixture",
        binding,
        uuid::Uuid::new_v4(),
        SourceSink(std::sync::Arc::new(|_| panic!("unlocked managed callback"))),
    );
    assert!(matches!(result, Err("safe_authority_lock_unavailable")));
    sandbox.finish();
}

/// The workflow's pre-step and post-exit paths both call refresh_now; the
/// background loop reaches the same pinger only while a lease is live.
#[cfg(unix)]
#[tokio::test]
async fn synchronous_refresh_and_live_tick_use_only_installed_snapshot_programs() {
    use awman::data::{fs::AuthPathResolver, session::AgentName};
    use awman::engine::{
        auth::{
            credential::{CredentialBinding, CredentialFingerprint},
            RefreshableCredentialDelivery,
        },
        credential_refresh::{CredentialRefreshMonitor, MonitorConfig, RefreshOutcome},
    };
    use std::time::Duration;
    let sandbox = crate::support::sandbox(
        "engine::credential_refresh::synchronous_refresh_and_live_tick_use_only_installed_snapshot_programs",
    );
    let fake = sandbox.install_fake(crate::support::fake_cli_exe(), "claude");
    fake.on(&[]).stdout("hello\n");
    let local = sandbox.work().join("bin");
    std::fs::create_dir_all(&local).unwrap();
    awman_test_support::exe::install_as(fake.exe(), &local, "claude");
    let source = sandbox.tmp().join("host-credential.json");
    let original = r#"{"claudeAiOauth":{"accessToken":"owned-test-secret","expiresAt":1}}"#;
    std::fs::write(&source, original).unwrap();
    let binding = CredentialBinding::to_file(AuthPathResolver::at_home(sandbox.home()), &source);
    let mut entries = sandbox.env().clone();
    entries.set(
        "PATH",
        std::env::join_paths([&local, &sandbox.bin()]).unwrap(),
    );
    let elsewhere = sandbox.tmp().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let env = awman::data::config::env::EnvSnapshot::from_instance_env(
        entries
            .iter()
            .map(|(k, v)| (k.to_string_lossy().into(), v.to_string_lossy().into()))
            .collect(),
        elsewhere,
        vec![],
    );
    let monitor = CredentialRefreshMonitor::with_binding_in(
        MonitorConfig {
            tick_interval: Duration::from_secs(60),
            ..Default::default()
        },
        binding,
        &env,
    );
    let agent = AgentName::new("claude").unwrap();
    assert!(matches!(
        monitor.refresh_now(&agent, Duration::from_secs(3)).await,
        RefreshOutcome::Stale { .. }
    ));
    assert!(
        fake.calls().is_empty(),
        "no host ping without a registered checkout"
    );
    monitor.add_checkout_roots([sandbox.work(), sandbox.tmp().join("retired-checkout")]);
    assert_eq!(monitor.live_lease_count(), 0);
    assert!(fake.calls().is_empty());
    // The two existing synchronous workflow callers use this exact boundary.
    // A successful executable exit without rotation is truthfully stale.
    for expected_calls in 1..=2 {
        let outcome = monitor.refresh_now(&agent, Duration::from_secs(3)).await;
        assert!(
            matches!(outcome, RefreshOutcome::Stale { .. }),
            "{outcome:?}"
        );
        assert_eq!(fake.calls().len(), expected_calls);
    }
    // A new monitor has no previous backoff and starts a real tick when its
    // first lease is registered. It uses the same explicit snapshot.
    drop(monitor);
    let binding = CredentialBinding::to_file(AuthPathResolver::at_home(sandbox.home()), &source);
    let monitor = CredentialRefreshMonitor::with_binding_in(
        MonitorConfig {
            tick_interval: Duration::from_secs(60),
            ..Default::default()
        },
        binding,
        &env,
    );
    monitor.add_checkout_roots([sandbox.work(), sandbox.tmp().join("retired-checkout")]);
    let staged = sandbox.tmp().join("staged.json");
    std::fs::write(&staged, original).unwrap();
    let delivery = RefreshableCredentialDelivery {
        agent,
        spec_agent: "claude",
        credential_env_key: "CLAUDE_CODE_OAUTH_TOKEN",
        staged_path: staged,
        staged_root: sandbox.tmp(),
        initial_fingerprint: CredentialFingerprint::zeroed(),
    };
    let lease = monitor.register(&delivery, "owned-fake-container");
    awman_test_support::wait::wait_for_async(
        "periodic host ping and status",
        Duration::from_secs(3),
        || async {
            (fake.calls().len() == 3
                && monitor
                    .status()
                    .iter()
                    .any(|s| matches!(s.last_outcome, Some(RefreshOutcome::Stale { .. }))))
            .then_some(())
        },
    )
    .await;
    drop(lease);
    assert_eq!(monitor.live_lease_count(), 0);
    drop(monitor);
    assert_eq!(std::fs::read_to_string(source).unwrap(), original);
    for call in fake.calls() {
        assert_eq!(call.argv.len(), 4);
        assert!(awman::engine::ready::GREETINGS.contains(&call.argv[3].as_str()));
        assert_eq!(call.env["HOME"], sandbox.home().to_string_lossy());
        assert!(!call.cwd.starts_with(sandbox.work()));
    }
    sandbox.finish();
}
