//! Safe work item draft reads: symlinks, directories, oversize and bad UTF-8.

use crate::support;
use awman::data::fs::context_dirs::ContextDirResolver;
use awman::data::fs::work_item_draft::{
    read_work_item_draft, DraftReadError, PrivateWorkItemFile, WorkItemDraft, MAX_WORK_ITEM_BYTES,
};
use std::path::Path;

fn new_draft(sandbox: &awman_test_support::TestSandbox, seed: &str) -> WorkItemDraft {
    let resolver = ContextDirResolver::at_home(sandbox.tmp().join("home"));
    let paths = resolver.work_item_draft_paths(
        Some("https://github.com/Acme/Widgets.git"),
        Path::new("/unused/checkout"),
        uuid::Uuid::new_v4(),
    );
    WorkItemDraft::create(paths, seed).unwrap()
}

#[test]
fn draft_paths_live_in_the_repo_context_overlay() {
    let resolver = ContextDirResolver::at_home("/home/u/.awman");
    let id = uuid::Uuid::new_v4();
    let paths = resolver.work_item_draft_paths(
        Some("https://github.com/Acme/Widgets.git"),
        Path::new("/checkout"),
        id,
    );
    assert!(paths
        .host_path
        .ends_with(format!("acme/widgets/scratch/work-items/{id}.md")));
    assert!(paths.host_path.starts_with("/home/u/.awman/context/repo"));
    assert_eq!(
        paths.container_path,
        Path::new(&format!("/awman/context/repo/scratch/work-items/{id}.md"))
    );
}

#[test]
fn a_normal_draft_is_seeded_read_and_removed() {
    let sandbox =
        support::sandbox("data::work_item_drafts::a_normal_draft_is_seeded_read_and_removed");
    let draft = new_draft(&sandbox, "seed body\n");
    let first = read_work_item_draft(&draft).unwrap();
    assert_eq!(first.content, "seed body\n");
    assert_eq!(first.sha256.len(), 64);
    assert_eq!(draft.stamp().unwrap().len, 10);

    std::fs::write(&draft.paths().host_path, "edited\n").unwrap();
    let second = read_work_item_draft(&draft).unwrap();
    assert_eq!(second.content, "edited\n");
    assert_ne!(first.sha256, second.sha256);

    let host = draft.paths().host_path.clone();
    draft.remove().unwrap();
    assert!(!host.exists());
}

#[cfg(unix)]
#[test]
fn seeded_files_are_owner_only_and_dropping_keeps_the_draft() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = support::sandbox(
        "data::work_item_drafts::seeded_files_are_owner_only_and_dropping_keeps_the_draft",
    );
    let draft = new_draft(&sandbox, "x");
    let host = draft.paths().host_path.clone();
    assert_eq!(
        std::fs::metadata(&host).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(host.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    drop(draft);
    assert!(
        host.exists(),
        "a draft must survive Drop so a failed upload can be recovered"
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_replacing_the_draft_is_refused_and_never_read() {
    let sandbox = support::sandbox(
        "data::work_item_drafts::a_symlink_replacing_the_draft_is_refused_and_never_read",
    );
    let secret = sandbox.tmp().join("host-secret.txt");
    std::fs::write(&secret, "TOP SECRET").unwrap();
    let draft = new_draft(&sandbox, "seed");
    let host = draft.paths().host_path.clone();
    std::fs::remove_file(&host).unwrap();
    std::os::unix::fs::symlink(&secret, &host).unwrap();

    assert!(matches!(
        read_work_item_draft(&draft),
        Err(DraftReadError::Symlink { .. })
    ));
    assert!(matches!(draft.stamp(), Err(DraftReadError::Symlink { .. })));
    // Removing the replacement unlinks the link itself, never its target.
    draft.remove().unwrap();
    assert!(!host.exists());
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "TOP SECRET");
}

#[test]
fn a_directory_replacing_the_draft_is_refused() {
    let sandbox =
        support::sandbox("data::work_item_drafts::a_directory_replacing_the_draft_is_refused");
    let draft = new_draft(&sandbox, "seed");
    let host = draft.paths().host_path.clone();
    std::fs::remove_file(&host).unwrap();
    std::fs::create_dir(&host).unwrap();
    assert!(matches!(
        read_work_item_draft(&draft),
        Err(DraftReadError::NotRegular { .. })
    ));
}

#[test]
fn an_oversized_draft_is_refused() {
    let sandbox = support::sandbox("data::work_item_drafts::an_oversized_draft_is_refused");
    let draft = new_draft(&sandbox, "seed");
    std::fs::write(
        &draft.paths().host_path,
        vec![b'a'; MAX_WORK_ITEM_BYTES as usize + 1],
    )
    .unwrap();
    assert!(matches!(
        read_work_item_draft(&draft),
        Err(DraftReadError::TooLarge { .. })
    ));
    // Exactly at the cap is accepted.
    std::fs::write(
        &draft.paths().host_path,
        vec![b'a'; MAX_WORK_ITEM_BYTES as usize],
    )
    .unwrap();
    assert_eq!(
        read_work_item_draft(&draft).unwrap().content.len() as u64,
        MAX_WORK_ITEM_BYTES
    );
}

#[test]
fn invalid_utf8_is_a_typed_rejection() {
    let sandbox = support::sandbox("data::work_item_drafts::invalid_utf8_is_a_typed_rejection");
    let draft = new_draft(&sandbox, "seed");
    std::fs::write(&draft.paths().host_path, [0x66, 0xff, 0xfe, 0x6f]).unwrap();
    assert!(matches!(
        read_work_item_draft(&draft),
        Err(DraftReadError::InvalidUtf8)
    ));
}

#[test]
fn a_missing_draft_is_reported_as_missing() {
    let sandbox =
        support::sandbox("data::work_item_drafts::a_missing_draft_is_reported_as_missing");
    let draft = new_draft(&sandbox, "seed");
    std::fs::remove_file(&draft.paths().host_path).unwrap();
    assert!(matches!(
        read_work_item_draft(&draft),
        Err(DraftReadError::Missing)
    ));
    // Removing an already missing draft is not an error.
    draft.remove().unwrap();
}

#[cfg(unix)]
#[test]
fn a_hard_linked_draft_is_refused() {
    let sandbox = support::sandbox("data::work_item_drafts::a_hard_linked_draft_is_refused");
    let draft = new_draft(&sandbox, "seed");
    let other = sandbox.tmp().join("alias");
    std::fs::hard_link(&draft.paths().host_path, &other).unwrap();
    assert!(matches!(
        read_work_item_draft(&draft),
        Err(DraftReadError::MultipleLinks { .. })
    ));
}

#[cfg(unix)]
#[test]
fn a_replaced_scratch_directory_is_detected() {
    let sandbox =
        support::sandbox("data::work_item_drafts::a_replaced_scratch_directory_is_detected");
    let draft = new_draft(&sandbox, "seed");
    let work_items = draft.paths().host_path.parent().unwrap().to_path_buf();
    let elsewhere = sandbox.tmp().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(
        elsewhere.join(draft.paths().host_path.file_name().unwrap()),
        "OTHER TREE",
    )
    .unwrap();
    std::fs::rename(&work_items, sandbox.tmp().join("moved-away")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &work_items).unwrap();
    assert!(matches!(
        read_work_item_draft(&draft),
        Err(DraftReadError::UnsafeAncestor { .. })
    ));
}

#[cfg(unix)]
#[test]
fn creation_refuses_a_symlinked_scratch_directory() {
    let sandbox =
        support::sandbox("data::work_item_drafts::creation_refuses_a_symlinked_scratch_directory");
    let resolver = ContextDirResolver::at_home(sandbox.tmp().join("home"));
    let paths = resolver.work_item_draft_paths(None, Path::new("/x/proj"), uuid::Uuid::new_v4());
    let scratch = paths
        .host_path
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let target = sandbox.tmp().join("outside");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &scratch).unwrap();
    assert!(WorkItemDraft::create(paths, "seed").is_err());
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
}

#[test]
fn private_work_item_files_are_removed_on_drop() {
    let sandbox =
        support::sandbox("data::work_item_drafts::private_work_item_files_are_removed_on_drop");
    let parent = sandbox.tmp().join("private");
    let file = PrivateWorkItemFile::create(&parent, "body").unwrap();
    let path = file.path().to_path_buf();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "body");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    let second = PrivateWorkItemFile::create(&parent, "body").unwrap();
    assert_ne!(second.path(), path, "staging names must be unpredictable");
    drop(file);
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn an_existing_wide_drafts_directory_is_made_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = support::sandbox(
        "data::work_item_drafts::an_existing_wide_drafts_directory_is_made_owner_only",
    );
    let resolver = ContextDirResolver::at_home(sandbox.tmp().join("home"));
    let paths = resolver.work_item_draft_paths(
        Some("https://github.com/Acme/Widgets.git"),
        Path::new("/unused/checkout"),
        uuid::Uuid::new_v4(),
    );
    let dir = paths.host_path.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let draft = WorkItemDraft::create(paths, "private issue body").unwrap();
    // An editor's rename-save may leave the draft world-readable; the
    // directory above it must still keep other users out.
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    draft.remove().unwrap();
}
