//! Component contracts for session-scoped workflow inputs.

#[test]
fn work_item_lookup_uses_session_scope_without_executing_content() {
    use crate::command::work_item::find_work_item_file;
    use crate::data::session::{Session, SessionOpenOptions};
    use awman_test_support::{TestCase, TestSandbox};
    let s = TestSandbox::for_test(TestCase::component(
        "command::commands::exec_workflow::session_contracts::work_item_lookup_uses_session_scope_without_executing_content",
    ));
    let dirs = s.work().join("aspec/work-items");
    std::fs::create_dir_all(&dirs).unwrap();
    let item = dirs.join("0144-hostile.md");
    std::fs::write(&item, "# Task\n$(codex) `claude` ; gemini\n").unwrap();
    let env = crate::data::config::env::EnvSnapshot::from_full_env(
        s.env()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect(),
    );
    let session = Session::open_at_git_root(
        s.work(),
        s.work(),
        SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        find_work_item_file(&session, &s.work(), 144).unwrap(),
        item.clone()
    );
    assert!(find_work_item_file(&session, &s.work(), 145).is_err());
    assert_eq!(
        std::fs::read_to_string(item).unwrap(),
        "# Task\n$(codex) `claude` ; gemini\n"
    );
    s.finish();
}
