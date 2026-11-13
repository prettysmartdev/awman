//! Context storage and safe-file regressions, confined to awman-test-support state.
use crate::support;
use awman::data::{
    context::staging::validate_staged_files,
    context::*,
    fs::{
        api_paths::ApiPaths,
        context_dirs::{validate_context_path, ContextDirResolver},
        safe_tree::{SafeTree, SafeTreeError, TreeBudget},
    },
};
use std::sync::atomic::AtomicBool;

#[test]
fn keyed_resolution_storage_preconditions_caps_and_diff() {
    let sandbox = support::sandbox(
        "data::context_store::keyed_resolution_storage_preconditions_caps_and_diff",
    );
    let resolver = ContextDirResolver::at_home(sandbox.awman_home());
    let key = ContextKey::parse("my-key").unwrap();
    // The resolver canonicalizes the trusted home, including Windows' verbatim
    // disk prefix. Compare paths in that same representation.
    let home = std::fs::canonicalize(sandbox.awman_home()).unwrap();
    assert_eq!(
        resolver.keyed_repo_dir(&key),
        home.join("context/repo/_keyed/my-key")
    );
    assert_eq!(
        resolver.repo_dir(Some("https://github.com/Owner/Repo.git"), sandbox.root()),
        home.join("context/repo/owner/repo")
    );
    assert_ne!(
        resolver.repo_dir(None, sandbox.root()),
        resolver.keyed_repo_dir(&key)
    );
    let store = RepoContextStore::open(
        &resolver,
        resolver.keyed_repo_dir(&key),
        ContextLimits {
            max_file_bytes: 4,
            max_total_bytes: 6,
            max_files: 2,
        },
    )
    .unwrap();
    let cancelled = AtomicBool::new(false);
    assert!(store.list(&cancelled).unwrap().is_empty());
    assert!(!store.dir().exists());
    let a = ContextRelPath::parse("nested/a.md").unwrap();
    let b = ContextRelPath::parse("b.md").unwrap();
    let c = ContextRelPath::parse("c.md").unwrap();
    if cfg!(windows) {
        assert_read_only(&store, &a);
    } else {
        let (first, outcome) = store.write(&a, b"old", &Precondition::None).unwrap();
        assert_eq!(outcome, WriteOutcome::Created);
        assert!(matches!(
            store.write(&a, b"new", &Precondition::Sha256("wrong".into())),
            Err(ContextStoreError::PreconditionFailed { .. })
        ));
        assert!(matches!(
            store.delete(&a, &Precondition::Sha256("wrong".into())),
            Err(ContextStoreError::PreconditionFailed { .. })
        ));
        assert_eq!(store.read(&a).unwrap().0, b"old");
        assert!(matches!(
            store.write(&b, b"12345", &Precondition::None),
            Err(ContextStoreError::TooLarge(_))
        ));
        assert!(matches!(
            store.write(&b, b"1234", &Precondition::None),
            Err(ContextStoreError::TooLarge(_))
        ));
        let start = store.snapshot(&cancelled).unwrap();
        let (replacement, outcome) = store
            .write(&a, b"new!", &Precondition::Sha256(first.sha256))
            .unwrap();
        assert_eq!(outcome, WriteOutcome::Replaced);
        assert_eq!(replacement, store.read(&a).unwrap().1);
        store.write(&b, b"ok", &Precondition::None).unwrap();
        assert!(matches!(
            store.write(&c, b"", &Precondition::None),
            Err(ContextStoreError::TooLarge(_))
        ));
        let changes = start.diff(&store.snapshot(&cancelled).unwrap());
        assert_eq!(
            changes
                .iter()
                .map(|c| (c.path.as_str(), c.change))
                .collect::<Vec<_>>(),
            vec![
                ("b.md", ChangeKind::Added),
                ("nested/a.md", ChangeKind::Modified)
            ]
        );
        let before_delete = store.snapshot(&cancelled).unwrap();
        store
            .delete(&a, &Precondition::Sha256(replacement.sha256))
            .unwrap();
        let changes = before_delete.diff(&store.snapshot(&cancelled).unwrap());
        assert_eq!(changes[0].change, ChangeKind::Deleted);
        assert_eq!(changes[0].size_bytes, None);
        assert_eq!(changes[0].sha256, None);
        let mut same = before_delete.clone();
        same.files.get_mut("nested/a.md").unwrap().modified_at = None;
        assert!(before_delete.diff(&same).is_empty());
    }
}

#[cfg(unix)]
#[test]
fn unsafe_files_and_parents_are_never_followed() {
    use std::os::unix::fs::symlink;
    let sandbox =
        support::sandbox("data::context_store::unsafe_files_and_parents_are_never_followed");
    let resolver = ContextDirResolver::at_home(sandbox.awman_home());
    let store = RepoContextStore::for_key(&resolver, &ContextKey::parse("safe").unwrap()).unwrap();
    let regular = ContextRelPath::parse("note").unwrap();
    store.write(&regular, b"safe", &Precondition::None).unwrap();
    let outside = sandbox.tmp().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret"), b"secret").unwrap();
    symlink(outside.join("secret"), store.dir().join("link")).unwrap();
    symlink(&outside, store.dir().join("parent")).unwrap();
    std::fs::create_dir(store.dir().join("directory")).unwrap();
    nix::unistd::mkfifo(&store.dir().join("fifo"), nix::sys::stat::Mode::S_IRUSR).unwrap();
    let tree = SafeTree::new(store.dir());
    for raw in ["link", "parent/secret", "directory", "fifo"] {
        let path = ContextRelPath::parse(raw).unwrap();
        assert!(
            matches!(tree.read(&path, 100), Err(SafeTreeError::NotFound)),
            "{raw}"
        );
        assert!(
            store.write(&path, b"x", &Precondition::None).is_err(),
            "{raw}"
        );
        assert!(store.delete(&path, &Precondition::None).is_err(), "{raw}");
    }
    assert_eq!(
        tree.list(TreeBudget::default(), &AtomicBool::new(false))
            .unwrap()
            .len(),
        1
    );
    // A listed file exchanged for a link is rejected on the next open.
    std::fs::remove_file(store.dir().join("note")).unwrap();
    symlink(outside.join("secret"), store.dir().join("note")).unwrap();
    assert!(tree.read(&regular, 100).is_err());
    assert_eq!(std::fs::read(outside.join("secret")).unwrap(), b"secret");
    let bad_root = resolver.keyed_repo_dir(&ContextKey::parse("bad").unwrap());
    symlink(&outside, &bad_root).unwrap();
    assert!(RepoContextStore::open(&resolver, bad_root, ContextLimits::REPO).is_err());
    assert!(validate_context_path(
        resolver.awman_home(),
        &resolver.global_dir().join("../escape")
    )
    .is_err());
    assert!(validate_context_path(resolver.awman_home(), &outside).is_err());
}

#[test]
fn staged_snapshots_are_validated_before_writes_and_budgeted_by_decoded_size() {
    let sandbox = support::sandbox("data::context_store::staged_snapshots_are_validated_before_writes_and_budgeted_by_decoded_size");
    let limits = ContextLimits {
        max_file_bytes: 3,
        max_total_bytes: 3,
        max_files: 2,
    };
    assert!(matches!(
        validate_staged_files([("../escape", "YQ==")], limits),
        Err(StagingError::Path { .. })
    ));
    assert!(matches!(
        validate_staged_files([("a", "!")], limits),
        Err(StagingError::Encoding { .. })
    ));
    assert!(matches!(
        validate_staged_files([("A", ""), ("a", "")], limits),
        Err(StagingError::Duplicate { .. })
    ));
    for entries in [[("a", ""), ("a/b", "")], [("a/b", ""), ("a", "")]] {
        assert!(matches!(
            validate_staged_files(entries, limits),
            Err(StagingError::FileDirClash { .. })
        ));
    }
    assert!(matches!(
        validate_staged_files([("a", "YWJjZA==")], limits),
        Err(StagingError::TooLarge { .. })
    ));
    assert!(matches!(
        validate_staged_files([("a", "YWI="), ("b", "YWI=")], limits),
        Err(StagingError::TooLarge { .. })
    ));
    assert!(matches!(
        validate_staged_files([("a", ""), ("b", ""), ("c", "")], limits),
        Err(StagingError::TooManyFiles { .. })
    ));
    let files = validate_staged_files([("a/b", "YWJj")], limits).unwrap();
    let resolver = ContextDirResolver::at_home(sandbox.awman_home());
    let dir = resolver.lease_repo_dir(&awman::data::ids::LeaseId::new());
    let store = RepoContextStore::open(&resolver, dir, limits).unwrap();
    if cfg!(unix) {
        store.stage_all(&files).unwrap();
    } else {
        assert!(matches!(
            store.stage_all(&files),
            Err(ContextStoreError::Data(_))
        ));
        assert!(
            !store.dir().exists(),
            "refused staging must leave no directory"
        );
        std::fs::create_dir_all(store.dir().join("a")).unwrap();
        std::fs::write(store.dir().join("a/b"), b"abc").unwrap();
    }
    assert_eq!(store.read(&files[0].path).unwrap().0, b"abc");
    assert!(store.stage_all(&files).is_err());
    assert!(matches!(
        SafeTree::new(store.dir()).read(&files[0].path, 2),
        Err(SafeTreeError::TooLarge { .. })
    ));
    assert!(matches!(
        SafeTree::new(store.dir()).list(
            TreeBudget {
                max_entries: 0,
                ..Default::default()
            },
            &AtomicBool::new(false)
        ),
        Err(SafeTreeError::Incomplete)
    ));
    let dir = store.dir().to_path_buf();
    store.remove_all().unwrap();
    assert!(!dir.exists());
}

#[test]
fn invocation_and_change_records_survive_reload_and_fence_stale_attempts() {
    let sandbox = support::sandbox("data::context_store::invocation_and_change_records_survive_reload_and_fence_stale_attempts");
    let paths = ApiPaths::from_root(sandbox.tmp().join("api"));
    let first = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    let pins = CommandInvocationPins::new(paths.clone());
    assert!(pins.current("session", "command", 1).unwrap().is_none());
    pins.pin("session", "command", 1, first).unwrap();
    drop(pins);
    let pins = CommandInvocationPins::new(paths.clone());
    assert_eq!(
        pins.current("session", "command", 1)
            .unwrap()
            .unwrap()
            .invocation_id,
        first
    );
    assert!(pins.current("session", "command", 2).unwrap().is_none());
    pins.pin("session", "command", 2, second).unwrap();
    pins.pin("session", "command", 1, first).unwrap();
    assert_eq!(
        pins.current("session", "command", 2)
            .unwrap()
            .unwrap()
            .invocation_id,
        second
    );
    assert_eq!(pins.all_for_session("session").unwrap().len(), 1);
    assert!(pins.pinned_invocations().unwrap().contains(&second));
    let start = CommandContextRecord::Start {
        attempt: 2,
        snapshot: ContextSnapshot::default(),
    };
    let end = CommandContextRecord::Final {
        attempt: 2,
        changes: vec![],
    };
    CommandContextRecord::write_start(&paths, "session", "command", &start).unwrap();
    CommandContextRecord::write_final(&paths, "session", "command", &end).unwrap();
    assert_eq!(
        CommandContextRecord::read_start(&paths, "session", "command").unwrap(),
        Some(start)
    );
    assert_eq!(
        CommandContextRecord::read_final(&paths, "session", "command").unwrap(),
        Some(end)
    );
    let stale = CommandContextRecord::Final {
        attempt: 1,
        changes: vec![],
    };
    CommandContextRecord::write_final(&paths, "session", "command", &stale).unwrap();
    CommandContextRecord::write_start(&paths, "session", "command", &stale).unwrap();
    assert!(matches!(
        CommandContextRecord::read_final(&paths, "session", "command").unwrap(),
        Some(CommandContextRecord::Final { attempt: 2, .. })
    ));
    assert!(pins.pinned_invocations().unwrap().contains(&first));
    pins.remove("session", "command").unwrap();
    assert!(pins.current("session", "command", 2).unwrap().is_none());
}

#[test]
fn session_binding_precedence_and_database_keys_share_one_model() {
    use awman::{
        command::context::scope::{RepoContextScope, RepoContextSource},
        data::{
            fs::api_db::SqliteSessionStore,
            session::{RepoContextBinding, Session, SessionOpenOptions},
        },
    };
    let sandbox = support::sandbox(
        "data::context_store::session_binding_precedence_and_database_keys_share_one_model",
    );
    let resolver = ContextDirResolver::at_home(sandbox.awman_home());
    let mut session = Session::open_at_directory_root(
        sandbox.work().to_path_buf(),
        SessionOpenOptions {
            env: Some(support::env_snapshot(&sandbox)),
            ..Default::default()
        },
    )
    .unwrap();
    let session_key = ContextKey::parse("session-key").unwrap();
    let overlay_key = ContextKey::parse("overlay-key").unwrap();
    let scope = RepoContextScope::new(&resolver, &session);
    let inferred = scope.resolve(None, Some("https://github.com/org/repo.git"));
    assert_eq!(
        inferred.host_dir,
        resolver.repo_dir(Some("https://github.com/org/repo.git"), session.git_root())
    );
    assert_eq!(
        scope.resolve(Some(&overlay_key), None).host_dir,
        resolver.keyed_repo_dir(&overlay_key)
    );
    session.set_repo_context(RepoContextBinding::Keyed(session_key.clone()));
    let selected = RepoContextScope::new(&resolver, &session).resolve(Some(&overlay_key), None);
    assert_eq!(
        selected.source,
        RepoContextSource::SessionKey(session_key.clone())
    );
    assert_eq!(selected.ignored_overlay_key, Some(overlay_key.clone()));
    let staged = resolver.lease_repo_dir(&awman::data::ids::LeaseId::new());
    session.set_repo_context(RepoContextBinding::Staged(staged.clone()));
    assert_eq!(
        RepoContextScope::new(&resolver, &session)
            .resolve(Some(&overlay_key), None)
            .host_dir,
        staged
    );
    let child = Session::open_worktree_for(sandbox.work(), sandbox.work(), &session).unwrap();
    assert_eq!(child.repo_context(), session.repo_context());
    let paths = ApiPaths::from_root(sandbox.tmp().join("api"));
    std::fs::create_dir_all(paths.root()).unwrap();
    let store = SqliteSessionStore::open_at(&paths.db_path()).unwrap();
    store
        .insert_session("session", "repo", "2026-01-01T00:00:00Z")
        .unwrap();
    assert_eq!(
        store.get_session("session").unwrap().unwrap().context_key,
        None
    );
    store
        .set_context_key("session", Some(&session_key))
        .unwrap();
    drop(store);
    let store = SqliteSessionStore::open_at(&paths.db_path()).unwrap();
    assert_eq!(
        store
            .get_session("session")
            .unwrap()
            .unwrap()
            .context_key
            .as_deref(),
        Some("session-key")
    );
    assert_eq!(
        store.list_sessions().unwrap()[0].context_key.as_deref(),
        Some("session-key")
    );
}

#[test]
fn oversized_agent_file_can_be_repaired_or_deleted_without_hashing_it() {
    let sandbox = support::sandbox(
        "data::context_store::oversized_agent_file_can_be_repaired_or_deleted_without_hashing_it",
    );
    let resolver = ContextDirResolver::at_home(sandbox.awman_home());
    let key = ContextKey::parse("repair").unwrap();
    let store = RepoContextStore::for_key(&resolver, &key).unwrap();
    std::fs::create_dir_all(store.dir()).unwrap();
    let path = ContextRelPath::parse("note").unwrap();
    let service = awman::command::context::repo_api::RepoContextService::new(resolver);
    for condition in [None, Some("*")] {
        // Larger than both the download and listing hash budgets. A sparse
        // agent file must remain repairable without reading 257 MiB.
        std::fs::File::create(store.dir().join("note"))
            .unwrap()
            .set_len(257 << 20)
            .unwrap();
        assert!(matches!(
            store.read(&path),
            Err(ContextStoreError::TooLarge(_))
        ));
        if cfg!(windows) {
            assert!(matches!(
                service.put("repair", "note", b"fixed", condition),
                Err(awman::command::context::ContextApiError::Data(_))
            ));
            assert!(matches!(
                service.delete("repair", "note", condition),
                Err(awman::command::context::ContextApiError::Data(_))
            ));
            assert_eq!(
                std::fs::metadata(store.dir().join("note")).unwrap().len(),
                257 << 20
            );
            continue;
        }
        service.put("repair", "note", b"fixed", condition).unwrap();
        assert_eq!(store.read(&path).unwrap().0, b"fixed");
        std::fs::File::create(store.dir().join("note"))
            .unwrap()
            .set_len(257 << 20)
            .unwrap();
        service.delete("repair", "note", condition).unwrap();
        assert!(!store.dir().join("note").exists());
    }
    assert!(service.put("repair", "note", b"x", Some("*")).is_err());
}

// Exercise the documented Windows contract with real reads and snapshots,
// including proof that neither overwrite nor delete changes agent-owned bytes.
fn assert_read_only(store: &RepoContextStore, path: &ContextRelPath) {
    assert!(matches!(
        store.write(path, b"new", &Precondition::None),
        Err(ContextStoreError::Data(_))
    ));
    assert!(!store.dir().exists());
    let file = store.dir().join(path.as_str());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"old").unwrap();
    let (bytes, meta) = store.read(path).unwrap();
    assert_eq!(bytes, b"old");
    let cancelled = AtomicBool::new(false);
    assert_eq!(store.list(&cancelled).unwrap(), vec![meta.clone()]);
    let start = store.snapshot(&cancelled).unwrap();
    for condition in [
        Precondition::None,
        Precondition::Exists,
        Precondition::Sha256(meta.sha256),
    ] {
        assert!(matches!(
            store.write(path, b"new", &condition),
            Err(ContextStoreError::Data(_))
        ));
        assert!(matches!(
            store.delete(path, &condition),
            Err(ContextStoreError::Data(_))
        ));
        assert_eq!(std::fs::read(&file).unwrap(), b"old");
    }
    std::fs::write(&file, b"new").unwrap();
    let changes = start.diff(&store.snapshot(&cancelled).unwrap());
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, path.as_str());
    assert_eq!(changes[0].change, ChangeKind::Modified);
    std::fs::remove_file(file).unwrap();
    assert_eq!(
        start.diff(&store.snapshot(&cancelled).unwrap())[0].change,
        ChangeKind::Deleted
    );
}

#[cfg(unix)]
#[test]
fn trusted_home_links_and_parent_components_work_but_context_links_are_refused() {
    let sandbox = support::sandbox("data::context_store::trusted_home_links_and_parent_components_work_but_context_links_are_refused");
    let real = sandbox.work().join("real");
    std::fs::create_dir_all(&real).unwrap();
    let linked = sandbox.work().join("linked");
    std::os::unix::fs::symlink(&real, &linked).unwrap();
    std::fs::create_dir_all(real.join("missing")).unwrap();
    let resolver = ContextDirResolver::at_home(linked.join("missing/../home"));
    let key = ContextKey::parse("home").unwrap();
    let store = RepoContextStore::for_key(&resolver, &key).unwrap();
    let path = ContextRelPath::parse("note").unwrap();
    store.write(&path, b"works", &Precondition::None).unwrap();
    assert_eq!(store.list(&AtomicBool::new(false)).unwrap().len(), 1);
    assert_eq!(store.read(&path).unwrap().0, b"works");
    let invocation = uuid::Uuid::new_v4();
    let dir = resolver.workflow_dir(invocation);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note"), b"works").unwrap();
    let reader = awman::engine::workflow::context_files::WorkflowContextReader::for_invocation(
        &resolver, invocation,
    )
    .unwrap();
    assert_eq!(reader.list(&AtomicBool::new(false)).unwrap().len(), 1);
    assert_eq!(reader.read("note").unwrap().0, b"works");
    std::fs::rename(
        resolver.awman_home().join("context"),
        resolver.awman_home().join("moved"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        resolver.awman_home().join("moved"),
        resolver.awman_home().join("context"),
    )
    .unwrap();
    assert!(store.read(&path).is_err());
}
