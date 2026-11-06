use super::*;
use crate::data::config::fields::{FileKind, MOVED_KEYS};
use serde_json::json;

fn file(kind: FileKind, value: Value) -> ConfigFile {
    ConfigFile::from_value(value, kind).expect("valid test file")
}

fn layers(global: Value, repo: Value, task: Option<Value>) -> ConfigLayers {
    ConfigLayers {
        global: file(FileKind::Global, global),
        repo: file(FileKind::Repo, repo),
        task: task.map(|t| file(FileKind::Task, t)),
        ..Default::default()
    }
}

fn resolve(layers: ConfigLayers) -> EffectiveConfig {
    EffectiveConfig::resolve(layers, FlagConfig::default(), EnvSnapshot::empty()).unwrap()
}

fn set(pairs: &[(&str, Value)]) -> ConfigOverrides {
    ConfigOverrides::new(
        pairs
            .iter()
            .map(|(p, v)| (p.to_string(), v.clone()))
            .collect(),
    )
}

#[test]
fn defaults_come_from_the_key_table() {
    let ec = EffectiveConfig::default();
    assert_eq!(ec.agent_stuck_timeout(), Duration::from_secs(30));
    assert_eq!(ec.scrollback_lines(), 10_000);
    assert_eq!(ec.launch_mode(), LaunchMode::Stdio);
    assert_eq!(ec.acp_fallback(), AcpFallback::Error);
    assert_eq!(ec.auth_mode(), AgentAuthMode::Keychain);
    assert_eq!(
        ec.auth_refresh(),
        AuthRefreshSettings {
            enabled: true,
            threshold: Duration::from_secs(1200),
            tick: Duration::from_secs(60)
        }
    );
    assert_eq!(ec.runtime_selection(), Ok(RuntimeSelection::Docker));
    assert_eq!(ec.max_concurrent_steps(), None);
    assert_eq!(ec.worktree_on_dirty_merge_target(), None);
    let daemon = ec.daemon();
    assert_eq!(daemon.max_concurrent_jobs, 2);
    assert_eq!(daemon.network.listen, "127.0.0.1:9876");
    assert_eq!(daemon.network.features, ["worker"]);
    assert_eq!(ec.worker_push_interaction(), (Interaction::Headless, true));
    assert_eq!(ec.builtin_runtime().memory_mib_or_default(), 4096);
    assert_eq!(
        ec.dockerfile_path(Path::new("/r")),
        PathBuf::from("/r/Dockerfile.dev")
    );
    assert_eq!(
        ec.work_items_template(Path::new("/r")),
        PathBuf::from("/r/aspec/work-items/0000-template.md")
    );
    // Every non-map row's default is placed at its path.
    for key in CONFIG_KEYS.iter().filter(|k| !k.is_map_member()) {
        let segments: Vec<&str> = key.path.split('.').collect();
        assert_eq!(
            get_path(ec.value(), &segments),
            Some(&key.default_value()),
            "{}",
            key.path
        );
        assert_eq!(ec.source_of(key.path), Layer::Default);
    }
}

#[test]
fn scalars_follow_default_global_repo_task_precedence() {
    let path = "workflows.prerequisites.unresolved";
    let pick = |g: Option<&str>, r: Option<&str>, t: Option<&str>| {
        let wrap = |v: Option<&str>| match v {
            Some(v) => json!({"workflows": {"prerequisites": {"unresolved": v}}}),
            None => json!({}),
        };
        let ec = resolve(layers(wrap(g), wrap(r), Some(wrap(t))));
        let value = ec.value()["workflows"]["prerequisites"]["unresolved"].clone();
        (value, ec.source_of(path))
    };
    assert_eq!(pick(None, None, None), (Value::Null, Layer::Default));
    assert_eq!(
        pick(Some("warn"), None, None),
        (json!("warn"), Layer::Global)
    );
    assert_eq!(
        pick(Some("warn"), Some("fail"), None),
        (json!("fail"), Layer::Repo)
    );
    assert_eq!(
        pick(Some("fail"), Some("fail"), Some("warn")),
        (json!("warn"), Layer::Task)
    );
    assert_eq!(pick(None, None, Some("fail")), (json!("fail"), Layer::Task));

    let ec = resolve(layers(
        json!({"agents": {"default": "codex", "stuckTimeout": "45s"}}),
        json!({"agents": {"default": "claude"}}),
        None,
    ));
    assert_eq!(ec.agent().as_deref(), Some("claude"));
    assert_eq!(ec.agent_stuck_timeout(), Duration::from_secs(45));
    assert_eq!(ec.source_of("agents.default"), Layer::Repo);
    assert_eq!(ec.source_of("agents.stuckTimeout"), Layer::Global);
    // The flag beats every layer.
    let ec = ec.with_flags(FlagConfig {
        agent: Some("gemini".into()),
        ..Default::default()
    });
    assert_eq!(ec.agent().as_deref(), Some("gemini"));
}

#[test]
fn objects_merge_per_leaf() {
    let ec = resolve(layers(
        json!({"agents": {"auth": {"refresh": {"enabled": false, "tick": "5s"}}}}),
        json!({"agents": {"auth": {"refresh": {"threshold": "1m"}}}}),
        None,
    ));
    assert_eq!(
        ec.auth_refresh(),
        AuthRefreshSettings {
            enabled: false,
            threshold: Duration::from_secs(60),
            tick: Duration::from_secs(5)
        }
    );
    let ec = resolve(layers(
        json!({"runtime": {"builtin": {"vcpus": 4, "memory": "8GiB"}}}),
        json!({"runtime": {"builtin": {"vcpus": 6}}}),
        None,
    ));
    let builtin = ec.builtin_runtime();
    assert_eq!(builtin.vcpus_or_default(), 6);
    assert_eq!(builtin.memory_mib_or_default(), 8192);
    assert_eq!(ec.source_of("runtime.builtin.vcpus"), Layer::Repo);
    assert_eq!(ec.source_of("runtime.builtin.memory"), Layer::Global);
}

#[test]
fn arrays_and_maps_replace() {
    let ec = resolve(layers(
        json!({"agents": {
            "yolo": {"disallowedTools": ["rm", "curl"]},
            "flags": {"codex": ["--a"], "claude": ["--b"]}
        }}),
        json!({"agents": {
            "yolo": {"disallowedTools": ["git"]},
            "flags": {"codex": ["--c"]}
        }}),
        None,
    ));
    assert_eq!(ec.yolo_disallowed_tools(), ["git"]);
    assert_eq!(ec.agent_flags("codex"), ["--c"]);
    // The repo map replaced the global one as a whole.
    assert!(ec.agent_flags("claude").is_empty());
    assert_eq!(ec.source_of("agents.flags.codex"), Layer::Repo);

    let ec = resolve(layers(
        json!({"workflows": {"dynamic": {"agentsToModels": {"claude": ["a"], "codex": ["b"]}}}}),
        json!({}),
        Some(json!({"workflows": {"dynamic": {"agentsToModels": {"gemini": ["c"]}}}})),
    ));
    let map = ec.dynamic_workflows().unwrap().agents_to_models.unwrap();
    assert_eq!(map.value.keys().collect::<Vec<_>>(), ["gemini"]);
    assert_eq!(map.source, Layer::Task);
}

#[test]
fn overlays_add_across_layers_deduplicated() {
    let ec = resolve(layers(
        json!({"overlays": ["ssh()", "env(A)"]}),
        json!({"overlays": [" env(A) ", "skill(*)"]}),
        Some(json!({"overlays": ["env(B)"]})),
    ));
    assert_eq!(
        ec.config().overlays.as_deref().unwrap(),
        ["ssh()", "env(A)", "skill(*)", "env(B)"]
    );
    assert_eq!(ec.source_of("overlays"), Layer::Task);
    // Env and --set add to the file overlays as well.
    let mut l = layers(json!({"overlays": ["ssh()"]}), json!({}), None);
    l.env = set(&[("overlays", json!(["env(C)"]))]);
    l.set = set(&[("overlays", json!(["ssh()", "env(D)"]))]);
    let ec = resolve(l);
    assert_eq!(
        ec.config().overlays.as_deref().unwrap(),
        ["ssh()", "env(C)", "env(D)"]
    );
}

#[test]
fn guidance_adds_across_layers_with_attribution_and_a_limit() {
    let ec = resolve(layers(
        json!({"workflows": {"dynamic": {"guidance": ["one", "two"]}}}),
        json!({"workflows": {"dynamic": {"guidance": ["two ", "three"]}}}),
        Some(json!({"workflows": {"dynamic": {"guidance": ["four"]}}})),
    ));
    let dynamic = ec.dynamic_workflows().unwrap();
    assert_eq!(
        dynamic.guidance_texts().unwrap(),
        ["one", "two", "three", "four"]
    );
    let sources: Vec<Layer> = dynamic.guidance.iter().map(|g| g.source).collect();
    assert_eq!(
        sources,
        [Layer::Global, Layer::Global, Layer::Repo, Layer::Task]
    );
    assert_eq!(dynamic.guidance_counts.global, 2);
    assert_eq!(dynamic.guidance_counts.repo, 1);
    assert_eq!(dynamic.guidance_counts.task, 1);

    let many = |prefix: &str| -> Vec<String> { (0..30).map(|i| format!("{prefix}{i}")).collect() };
    let err = EffectiveConfig::resolve(
        layers(
            json!({"workflows": {"dynamic": {"guidance": many("g")}}}),
            json!({"workflows": {"dynamic": {"guidance": many("r")}}}),
            None,
        ),
        FlagConfig::default(),
        EnvSnapshot::empty(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("60 entries") && err.contains("limit is 50"),
        "{err}"
    );
}

#[test]
fn env_and_set_layers_apply_last_and_null_resets() {
    let mut l = layers(
        json!({"workflows": {"maxConcurrentSteps": 3}}),
        json!({"workflows": {"maxConcurrentSteps": 4}, "tui": {"scrollbackLines": 9}}),
        None,
    );
    l.env = set(&[("workflows.maxConcurrentSteps", json!(5))]);
    l.set = set(&[("tui.scrollbackLines", Value::Null)]);
    let ec = resolve(l);
    assert_eq!(ec.max_concurrent_steps(), Some(5));
    assert_eq!(ec.source_of("workflows.maxConcurrentSteps"), Layer::Env);
    // `null` resets the leaf to its built-in default.
    assert_eq!(ec.scrollback_lines(), 10_000);
    assert_eq!(ec.source_of("tui.scrollbackLines"), Layer::Default);

    // A map entry set by an override changes that leaf only.
    let mut l = layers(
        json!({"remote": {"default": "prod", "targets": {"prod": {"url": "https://a:1"}}}}),
        json!({}),
        None,
    );
    l.set = set(&[("remote.targets.prod.apiKey", json!("secret-key-9876"))]);
    let ec = resolve(l);
    let target = ec.remote_target(None).unwrap();
    assert_eq!(target.url, "https://a:1");
    assert_eq!(target.api_key.as_deref(), Some("secret-key-9876"));
    assert_eq!(
        ec.masked_value()["remote"]["targets"]["prod"]["apiKey"],
        json!("••••9876")
    );

    // An override of the wrong type is refused.
    let l = ConfigLayers {
        set: set(&[("daemon.maxConcurrentJobs", json!("many"))]),
        ..Default::default()
    };
    assert!(EffectiveConfig::resolve(l, FlagConfig::default(), EnvSnapshot::empty()).is_err());
}

#[test]
fn env_layer_hook_reads_derived_names() {
    let env = EnvSnapshot::with_overrides([
        ("AWMAN_OVERLAYS", "ssh(), env(A)"),
        ("AWMAN_AGENTS_STUCK_TIMEOUT", "2m"),
        ("AWMAN_REMOTE_TARGETS_PROD_API_KEY", "k"),
        ("AWMAN_HOME", "/ignored-here"),
    ]);
    let global = file(
        FileKind::Global,
        json!({"remote": {"targets": {"prod": {"url": "https://h:1"}}}}),
    );
    let overrides = env_overrides(&env, &[&global]).unwrap();
    let paths: Vec<&str> = overrides.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        [
            "agents.stuckTimeout",
            "overlays",
            "remote.targets.prod.apiKey"
        ]
    );
    let bad = EnvSnapshot::with_overrides([("AWMAN_DAEMON_MAX_CONCURRENT_JOBS", "0")]);
    let err = env_overrides(&bad, &[]).unwrap_err().to_string();
    assert!(err.contains("AWMAN_DAEMON_MAX_CONCURRENT_JOBS"), "{err}");
}

#[test]
fn task_scope_keys_are_allowed_in_repo_files_and_daemon_keys_rejected_in_task_files() {
    let path = Path::new("/r/.awman/config.json");
    assert!(ConfigFile::parse(
        r#"{"workflows":{"dynamic":{"defaultLeader":"claude::opus"}}}"#,
        FileKind::Repo,
        path
    )
    .is_ok());
    let task = Path::new("/h/squad/tasks/t/config.json");
    let err = ConfigFile::parse(r#"{"daemon":{"verboseSetup":true}}"#, FileKind::Task, task)
        .unwrap_err()
        .to_string();
    assert!(err.contains("daemon.verboseSetup"), "{err}");
    assert!(err.contains("global config file"), "{err}");
    assert!(err.contains("squad task config file"), "{err}");
    let err = ConfigFile::parse(r#"{"remote":{"default":"x"}}"#, FileKind::Repo, path)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("remote.default") && err.contains("a repo config file"),
        "{err}"
    );
}

/// A JSON document carrying `old` (a dotted path) with a plausible value.
fn document_with(old: &str) -> Value {
    let mut doc = json!({});
    let segments: Vec<&str> = old.split('.').collect();
    let value = match *segments.last().unwrap() {
        "agentStuckTimeout"
        | "workers"
        | "maxConcurrentAgents"
        | "terminal_scrollback_lines"
        | "maxConcurrentEvaluations"
        | "port"
        | "thresholdMinutes"
        | "tickSeconds"
        | "memoryMib"
        | "rootDiskMib"
        | "mountQuotaMib"
        | "timeoutSecs"
        | "maxConcurrentSteps" => json!(7),
        "alwaysNonInteractive" | "auto_agent_auth_accepted" => json!(true),
        "workDirs" | "workDirRoots" | "savedDirs" | "guidance" | "envPassthrough" => {
            json!(["x"])
        }
        "agentFlags" | "agentsToModels" | "authRefresh" | "builtin" | "work_items"
        | "dynamicWorkflows" | "workflows" => json!({}),
        _ => json!("codex"),
    };
    set_path(&mut doc, &segments, value);
    doc
}

#[test]
fn every_moved_key_fails_with_its_hint_and_is_never_applied() {
    let path = Path::new("/g/config.json");
    for (old, new, _) in MOVED_KEYS {
        let doc = document_with(old);
        let err = ConfigFile::parse(&doc.to_string(), FileKind::Global, path)
            .expect_err(old)
            .to_string();
        // An old key is unknown (and lists its valid siblings), except one
        // whose name is now a section (`runtime`): that is a type error.
        assert!(
            err.contains("unknown config key") && err.contains("valid keys here")
                || err.contains("expected an object"),
            "{old}: {err}"
        );
        assert!(err.contains(&format!("`{old}`")), "{old}: {err}");
        assert!(err.contains(new), "{old}: hint must name {new}: {err}");
        // Never applied: loading the file from disk fails, so nothing of it
        // reaches the merge, and the would-be target stays at its default.
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("config.json");
        std::fs::write(&file_path, doc.to_string()).unwrap();
        assert!(
            ConfigFile::load(&file_path, FileKind::Global).is_err(),
            "{old}"
        );
        let env = EnvSnapshot::with_overrides([(
            crate::data::fs::data_paths::AWMAN_HOME,
            tmp.path().to_str().unwrap(),
        )]);
        assert!(
            ConfigLayers::load(&env, None, ConfigOverrides::default()).is_err(),
            "{old}"
        );
    }
    // The example from the work item, verbatim.
    let err = ConfigFile::parse(r#"{"agentStuckTimeout":30}"#, FileKind::Repo, path)
        .unwrap_err()
        .to_string();
    assert!(
        err.ends_with("`agentStuckTimeout` moved to `agents.stuckTimeout` (\"30s\")"),
        "{err}"
    );
}

#[test]
fn old_runtime_spellings_are_rejected_with_the_valid_set() {
    let err = ConfigFile::parse(
        r#"{"runtime":{"kind":"docker-sbx-experimental"}}"#,
        FileKind::Global,
        Path::new("/g/config.json"),
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("runtime.kind") && err.contains("docker, apple, sbx, builtin"),
        "{err}"
    );
}

#[test]
fn builtin_network_ceiling_includes_trusted_overrides_and_repo_may_only_narrow() {
    let l = layers(
        json!({"runtime": {"builtin": {"network": {"mode": "none"}}}}),
        json!({"runtime": {"builtin": {"network": {"mode": "public"}}}}),
        None,
    );
    assert!(resolve(l).builtin_network().is_err());
    let mut l = layers(
        json!({}),
        json!({"runtime": {"builtin": {"network": {"mode": "public"}}}}),
        None,
    );
    l.set = set(&[("runtime.builtin.network.mode", json!("none"))]);
    assert!(resolve(l).builtin_network().is_err());
}

#[test]
fn repo_and_task_files_narrow_prerequisites_never_grant() {
    use crate::data::config::workflows::{CredentialUse, PrerequisiteResolveContext};
    let ec = resolve(layers(
        json!({"workflows": {"prerequisites": {"ssh": {"use": "allow"}}}}),
        json!({"workflows": {"prerequisites": {"githubToken": {"use": "allow"}}}}),
        Some(json!({"workflows": {"prerequisites": {"ssh": {"use": "deny"}}}})),
    ));
    let policy = ec.prerequisite_policy(PrerequisiteResolveContext::default());
    assert_eq!(policy.ssh.use_, CredentialUse::Deny);
    assert_eq!(policy.github_token.use_, CredentialUse::Ask);
}

#[test]
fn interaction_spellings_round_trip() {
    let ec = resolve(layers(
        json!({"daemon": {"features": {"squad": {"interaction": "headless"},
            "worker": {"push": {"interaction": "interactive", "allowOverride": false}}}}}),
        json!({}),
        None,
    ));
    assert_eq!(ec.daemon().squad.interaction, Interaction::Headless);
    assert_eq!(
        ec.worker_push_interaction(),
        (Interaction::Interactive, false)
    );
    assert!(ConfigFile::parse(
        r#"{"daemon":{"features":{"squad":{"interaction":"non-interactive"}}}}"#,
        FileKind::Global,
        Path::new("/g")
    )
    .is_err());
}

#[test]
fn remote_target_resolution_names_the_keys() {
    let ec = EffectiveConfig::default();
    let err = ec.remote_target(None).unwrap_err().to_string();
    assert!(
        err.contains("--remote") && err.contains("remote.default"),
        "{err}"
    );
    let ec = resolve(layers(
        json!({"remote": {"targets": {"a": {"url": "https://a:1"}}}}),
        json!({}),
        None,
    ));
    assert_eq!(ec.remote_target(Some("a")).unwrap().url, "https://a:1");
    assert!(ec
        .remote_target(Some("b"))
        .unwrap_err()
        .to_string()
        .contains("remote.targets.b"));
}

#[test]
fn set_override_cannot_create_a_dash_underscore_duplicate_map_name() {
    let l = ConfigLayers {
        set: set(&[("agents.flags.in_fact", json!(["override"]))]),
        ..layers(
            json!({"agents": {"flags": {"in-fact": ["global"]}}}),
            json!({}),
            None,
        )
    };
    let err = EffectiveConfig::resolve(l, FlagConfig::default(), EnvSnapshot::empty())
        .unwrap_err()
        .to_string();
    assert!(err.contains("in-fact") && err.contains("in_fact"), "{err}");
}

#[test]
fn cross_layer_dash_underscore_map_names_are_rejected_not_rebound_by_env() {
    let env = EnvSnapshot::with_overrides([("AWMAN_AGENTS_FLAGS_IN_FACT", "env")]);
    let global = file(FileKind::Global, json!({}));
    let repo = file(
        FileKind::Repo,
        json!({"agents": {"flags": {"in_fact": ["repo"]}}}),
    );
    let global_dash = file(
        FileKind::Global,
        json!({"agents": {"flags": {"in-fact": ["global"]}}}),
    );
    let _ = global;
    let env_layer = env_overrides(&env, &[&global_dash, &repo]).unwrap();
    let l = ConfigLayers {
        global: global_dash,
        repo,
        env: env_layer,
        ..Default::default()
    };
    let err = EffectiveConfig::resolve(l, FlagConfig::default(), env)
        .unwrap_err()
        .to_string();
    assert!(err.contains("in-fact") && err.contains("in_fact"), "{err}");
}

#[test]
fn existing_map_entries_get_leaf_defaults_with_default_provenance() {
    let cfg = resolve(layers(
        json!({"workServers": {"prod": {"url": "https://example.com"}}}),
        json!({}),
        None,
    ));
    let v = cfg.value();
    assert_eq!(v["workServers"]["prod"]["enabled"], json!(true));
    assert_eq!(v["workServers"]["prod"]["claim"], json!("auto"));
    assert_eq!(v["workServers"]["prod"]["maxConcurrentJobs"], json!(1));
    assert_eq!(cfg.source_of("workServers.prod.enabled"), Layer::Default);
    assert_eq!(cfg.source_of("workServers.prod.url"), Layer::Global);
    // A wildcard row never invents an entry.
    let empty = resolve(layers(json!({}), json!({}), None));
    assert_eq!(empty.value()["workServers"], json!({}));
}
