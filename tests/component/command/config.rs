//! Command projections derive coverage from every config-key registration.
use awman::command::commands::config::{collect_config_rows, ConfigDocs};
use awman::data::config::{fields::CONFIG_KEYS, Layer};
use std::collections::BTreeSet;

fn json_examples(markdown: &str) -> Vec<(usize, String)> {
    let mut examples = Vec::new();
    let mut in_json = false;
    let mut start = 0;
    let mut body = String::new();
    for (index, line) in markdown.lines().enumerate() {
        if !in_json && line.trim() == "```json" {
            in_json = true;
            start = index + 1;
            body.clear();
        } else if in_json && line.trim() == "```" {
            examples.push((start, body.clone()));
            in_json = false;
        } else if in_json {
            body.push_str(line);
            body.push('\n');
        }
    }
    examples
}

#[test]
fn active_runtime_config_examples_parse_and_retired_names_hint_current_syntax() {
    use awman::data::config::{file::ConfigFile, runtime::RuntimeSelection, FileKind};
    use std::path::Path;

    let cli = include_str!("../../../aspec/uxui/cli.md");
    let runtime_section = cli
        .split_once("## Runtime and image-source configuration")
        .unwrap()
        .1;
    let cli_examples = json_examples(runtime_section);
    assert_eq!(
        cli_examples.len(),
        2,
        "the authoritative runtime examples changed"
    );
    for (line, example) in cli_examples {
        ConfigFile::parse(
            &example,
            FileKind::Global,
            Path::new("cli-spec-example.json"),
        )
        .unwrap_or_else(|error| {
            panic!("CLI spec JSON example at runtime section line {line}: {error}")
        });
    }

    let obsolete_scalar = ConfigFile::parse(
        r#"{"runtime":"builtin"}"#,
        FileKind::Global,
        Path::new("old-config.json"),
    )
    .unwrap_err()
    .to_string();
    assert!(
        obsolete_scalar.contains("`runtime` moved to `runtime.kind`"),
        "{obsolete_scalar}"
    );

    let runtimes = include_str!("../../../docs/11-runtimes.md");
    let docs_example = json_examples(runtimes)
        .into_iter()
        .find(|(_, example)| example.contains("\"kind\": \"builtin\""))
        .expect("builtin config example in runtimes guide");
    ConfigFile::parse(
        &docs_example.1,
        FileKind::Global,
        Path::new("runtimes-guide.json"),
    )
    .expect("the runtimes guide builtin config example should use current syntax");

    // Parse every active builtin config sample in the configuration guide
    // through the same production decoder used for config files.
    let configuration = include_str!("../../../docs/07-configuration.md");
    let runtime_settings = configuration
        .split_once("### Builtin runtime settings")
        .unwrap()
        .1
        .split_once("## Reference: all keys")
        .unwrap()
        .0;
    let docs_examples = json_examples(runtime_settings);
    assert!(
        docs_examples.len() >= 3,
        "runtime config examples disappeared"
    );
    for (line, example) in docs_examples {
        ConfigFile::parse(
            &example,
            FileKind::Global,
            Path::new("configuration-guide.json"),
        )
        .unwrap_or_else(|error| panic!("configuration guide example at line {line}: {error}"));
    }

    for (obsolete, current) in [
        ("apple-containers", "apple"),
        ("docker-sbx-experimental", "sbx"),
        ("builtin-experimental", "builtin"),
    ] {
        let input = format!(r#"{{"runtime":{{"kind":"{obsolete}"}}}}"#);
        let error = ConfigFile::parse(&input, FileKind::Global, Path::new("old-config.json"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!("runtime.kind is now \"{current}\"")),
            "{error}"
        );
        assert!(RuntimeSelection::hint_for(obsolete).is_some());
    }
}

#[test]
fn builtin_init_config_journey_uses_the_nested_runtime_contract() {
    use awman::data::config::{file::ConfigFile, runtime::RuntimeSelection, FileKind};
    use std::path::Path;

    let config = ConfigFile::parse(
        r#"{"runtime":{"kind":"builtin","builtin":{"builder":{"enabled":true},"imageSource":{"type":"docker-local"}}}}"#,
        FileKind::Global,
        Path::new("init-config.json"),
    )
    .expect("builtin runtime config used before init");
    let runtime = config.runtime.expect("runtime config");
    assert_eq!(runtime.kind, Some(RuntimeSelection::BuiltinExperimental));
    assert!(runtime.builtin.unwrap().builder.unwrap().enabled.unwrap());
}

#[test]
fn every_config_key_has_a_command_row_or_a_declared_map_container() {
    let mut empty = serde_json::json!({});
    for key in CONFIG_KEYS {
        awman::data::config::config_json::set_config_field(
            &mut empty,
            &key.path.replace('*', "example"),
            key.default_value(),
        );
    }
    let rows = collect_config_rows(&ConfigDocs {
        global: &empty,
        repo: &empty,
        effective: &empty,
        source: &|_| Layer::Default,
    });
    let names: BTreeSet<_> = rows.iter().map(|r| r.field.as_str()).collect();
    for key in CONFIG_KEYS {
        let concrete = key.path.replace('*', "example");
        let expected = if key.path == "workServers.*.labels.*" {
            "workServers.example.labels"
        } else {
            &concrete
        };
        assert!(
            names.contains(expected)
                || rows
                    .iter()
                    .any(|r| r.field.starts_with(&format!("{expected}."))),
            "missing command row for {}",
            key.path
        );
    }
    assert_eq!(names.len(), rows.len(), "duplicate config rows");
    assert!(rows.iter().all(|r| !r.type_name.is_empty()));
}

#[tokio::test]
async fn api_process_overrides_are_refused_before_session_and_queue_allocation() {
    use super::support::{prompt, AdmissionFixture};
    use crate::support;
    use awman::command::commands::api_server::admission::AdmissionError;
    let s = support::sandbox(
        "command::config::api_process_overrides_are_refused_before_session_and_queue_allocation",
    );
    let _repo = awman_test_support::fixtures::RepoBuilder::at(&s, s.work())
        .file(
            ".awman/config.json",
            r#"{"agents":{"automation":{"cancelTimeout":"60s"}}}"#,
        )
        .init();
    let f = AdmissionFixture::new(&s);
    std::fs::create_dir_all(f.paths.sessions_dir()).unwrap();
    for set in [
        "runtime.kind=builtin",
        "agents.auth.refresh.enabled=false",
        "agents.auth.refresh.tick=1s",
    ] {
        let mut command = prompt("never launch");
        command.args.splice(0..0, ["--set".into(), set.into()]);
        let result = f
            .admission
            .admit_session(f.local(&s), Some(command), false, None)
            .await;
        let Err(AdmissionError::CommandValidation(error)) = result else {
            panic!("process conflict must refuse admission")
        };
        assert_eq!(error.code(), "process_scoped_setting", "{error}");
        assert_eq!(error.http_status(), 409);
        assert!(f.store.list_sessions().unwrap().is_empty());
        assert_eq!(
            std::fs::read_dir(f.paths.sessions_dir()).unwrap().count(),
            0
        );
    }
    let mut matching = prompt("matching runtime");
    matching.args.splice(
        0..0,
        [
            "--set".into(),
            "runtime.kind=docker".into(),
            "--set".into(),
            "agents.automation.outputDrainTimeout=30s".into(),
        ],
    );
    let admitted = f
        .admission
        .admit_session(f.local(&s), Some(matching), false, None)
        .await
        .unwrap();
    let mut conflict = prompt("must not queue");
    conflict
        .args
        .splice(0..0, ["--set".into(), "runtime.kind=builtin".into()]);
    assert!(matches!(
        f.admission.admit_command(&admitted.session_id, conflict, None).await,
        Err(AdmissionError::CommandValidation(error)) if error.code() == "process_scoped_setting"
    ));
    assert_eq!(f.store.list_sessions().unwrap().len(), 1);
    assert_eq!(
        f.store
            .list_commands_for_session(&admitted.session_id, 100)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn one_shot_startup_binds_repository_refresh_settings_before_engines() {
    use awman_test_support::fixtures::{ConfigTree, RepoBuilder};
    let s = crate::support::sandbox(
        "command::config::one_shot_startup_binds_repository_refresh_settings_before_engines",
    );
    let repo = RepoBuilder::new(&s, "project").init();
    ConfigTree::new()
        .repo(serde_json::json!({"agents":{"auth":{"refresh":{"threshold":"10m"}}}}))
        .write(&s, Some(repo.path()));
    let env = crate::support::env_snapshot(&s).with_invocation(repo.path().to_path_buf(), vec![]);
    for path in [
        vec!["config", "show"],
        vec!["config", "set"],
        vec!["exec", "prompt"],
    ] {
        let outcome =
            awman::command::startup::Startup::new(path.iter().map(|s| s.to_string()).collect())
                .run(repo.path().to_path_buf(), env.clone())
                .unwrap();
        assert_eq!(
            outcome.engines.process_binding().auth_refresh.threshold,
            std::time::Duration::from_secs(600)
        );
    }
    s.finish();
}
