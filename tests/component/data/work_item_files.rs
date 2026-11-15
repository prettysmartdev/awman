//! Local work item files: lookup, containment, reads and creation.

use crate::support;
use awman::data::fs::work_item_files::{
    matching_work_item_files, next_work_item_number, read_local_work_item, read_work_item_template,
    write_new_local_work_item, LocalWorkItemFileError,
};

fn repo(sandbox: &awman_test_support::TestSandbox) -> std::path::PathBuf {
    let root = sandbox.tmp().join("repo");
    std::fs::create_dir_all(root.join("aspec/work-items")).unwrap();
    root
}

#[test]
fn matches_are_all_returned_sorted_and_nonrecursive() {
    let sandbox =
        support::sandbox("data::work_item_files::matches_are_all_returned_sorted_and_nonrecursive");
    let root = repo(&sandbox);
    let dir = root.join("aspec/work-items");
    for name in ["0042-b.md", "0042-a.md", "0043-c.md", "0042-note.txt"] {
        std::fs::write(dir.join(name), "x").unwrap();
    }
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/0042-nested.md"), "x").unwrap();
    let found = matching_work_item_files(&root, &dir, 42).unwrap();
    let names: Vec<_> = found
        .iter()
        .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["0042-a.md", "0042-b.md"]);
    assert!(matching_work_item_files(&root, &dir, 7).unwrap().is_empty());
    assert!(matching_work_item_files(&root, &root.join("missing"), 42)
        .unwrap()
        .is_empty());
}

#[test]
fn a_directory_outside_the_checkout_is_refused() {
    let sandbox =
        support::sandbox("data::work_item_files::a_directory_outside_the_checkout_is_refused");
    let root = repo(&sandbox);
    let outside = sandbox.tmp().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("0001-x.md"), "x").unwrap();
    assert!(matches!(
        matching_work_item_files(&root, &outside, 1),
        Err(LocalWorkItemFileError::OutsideCheckout { .. })
    ));
}

#[test]
fn reads_relative_paths_from_the_working_dir_and_refuses_escapes() {
    let sandbox = support::sandbox(
        "data::work_item_files::reads_relative_paths_from_the_working_dir_and_refuses_escapes",
    );
    let root = repo(&sandbox);
    std::fs::write(root.join("aspec/work-items/0001-a.md"), "hello").unwrap();
    let file = read_local_work_item(&root, &root, "aspec/work-items/0001-a.md".as_ref()).unwrap();
    assert_eq!(file.content, "hello");
    let sub = root.join("aspec");
    assert_eq!(
        read_local_work_item(&root, &sub, "work-items/0001-a.md".as_ref())
            .unwrap()
            .content,
        "hello"
    );
    std::fs::write(sandbox.tmp().join("outside.md"), "no").unwrap();
    assert!(matches!(
        read_local_work_item(&root, &root, "../outside.md".as_ref()),
        Err(LocalWorkItemFileError::OutsideCheckout { .. })
    ));
    assert!(matches!(
        read_local_work_item(&root, &root, "nope.md".as_ref()),
        Err(LocalWorkItemFileError::NotFound { .. })
    ));
}

#[cfg(unix)]
#[test]
fn symlinks_are_allowed_only_when_they_stay_inside_the_checkout() {
    let sandbox = support::sandbox(
        "data::work_item_files::symlinks_are_allowed_only_when_they_stay_inside_the_checkout",
    );
    let root = repo(&sandbox);
    let inside = root.join("aspec/work-items/0001-a.md");
    std::fs::write(&inside, "inside").unwrap();
    let outside = sandbox.tmp().join("secret.md");
    std::fs::write(&outside, "secret").unwrap();
    std::os::unix::fs::symlink(&inside, root.join("ok.md")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("bad.md")).unwrap();
    assert_eq!(
        read_local_work_item(&root, &root, "ok.md".as_ref())
            .unwrap()
            .content,
        "inside"
    );
    assert!(matches!(
        read_local_work_item(&root, &root, "bad.md".as_ref()),
        Err(LocalWorkItemFileError::OutsideCheckout { .. })
    ));
}

#[test]
fn oversized_directory_and_invalid_utf8_inputs_are_refused() {
    let sandbox = support::sandbox(
        "data::work_item_files::oversized_directory_and_invalid_utf8_inputs_are_refused",
    );
    let root = repo(&sandbox);
    std::fs::write(root.join("big.md"), vec![b'a'; 1_048_577]).unwrap();
    std::fs::write(root.join("bin.md"), [0xff, 0xfe]).unwrap();
    std::fs::create_dir(root.join("dir.md")).unwrap();
    assert!(matches!(
        read_local_work_item(&root, &root, "big.md".as_ref()),
        Err(LocalWorkItemFileError::TooLarge { .. })
    ));
    assert!(matches!(
        read_local_work_item(&root, &root, "bin.md".as_ref()),
        Err(LocalWorkItemFileError::InvalidUtf8 { .. })
    ));
    assert!(matches!(
        read_local_work_item(&root, &root, "dir.md".as_ref()),
        Err(LocalWorkItemFileError::NotRegular { .. })
    ));
}

#[test]
fn numbering_and_creation_never_overwrite() {
    let sandbox = support::sandbox("data::work_item_files::numbering_and_creation_never_overwrite");
    let root = repo(&sandbox);
    let dir = root.join("created");
    assert_eq!(next_work_item_number(&dir).unwrap(), 1);
    let first = write_new_local_work_item(&root, &dir, 1, "first", "one").unwrap();
    assert_eq!(first.file_name().unwrap(), "0001-first.md");
    std::fs::write(dir.join("0009-x.md"), "x").unwrap();
    std::fs::write(dir.join("README.md"), "x").unwrap();
    assert_eq!(next_work_item_number(&dir).unwrap(), 10);
    assert!(write_new_local_work_item(&root, &dir, 1, "first", "two").is_err());
    assert_eq!(std::fs::read_to_string(first).unwrap(), "one");
    assert!(write_new_local_work_item(&root, &dir, 2, "../escape", "x").is_err());
}

#[cfg(unix)]
#[test]
fn templates_outside_the_checkout_are_never_read() {
    let sandbox =
        support::sandbox("data::work_item_files::templates_outside_the_checkout_are_never_read");
    let root = repo(&sandbox);
    let secret = sandbox.tmp().join("host-secret");
    std::fs::write(&secret, "aws_secret_access_key = x").unwrap();
    // `new spec` can publish the template to a tracker, so a repo config
    // naming a host file, an escaping relative path, or a committed symlink
    // must not read it.
    let link = root.join("aspec/work-items/0000-template.md");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    for path in [secret.clone(), root.join("../host-secret"), link] {
        let error = read_work_item_template(&root, &path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside the repository checkout"),
            "{}: {error}",
            path.display()
        );
    }
    // A FIFO is refused instead of blocking the command.
    let fifo = root.join("aspec/work-items/fifo.md");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
    assert!(read_work_item_template(&root, &fifo).is_err());
    let template = root.join("aspec/work-items/0001-template.md");
    std::fs::write(&template, "# {{title}}").unwrap();
    assert_eq!(
        read_work_item_template(&root, &template).unwrap(),
        "# {{title}}"
    );
}
