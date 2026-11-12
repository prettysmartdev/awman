use crate::support;
/// Guards: f23e75a4 fixture is required and digest-verified before import.
#[test]
fn pinned_image_defaults_and_cached_import_are_preserved() {
    let s = support::sandbox("import::pinned_image_defaults_and_cached_import_are_preserved");
    let out = support::scenario(&s, "image-defaults");
    assert_eq!(
        out.lines().filter(|l| l.starts_with("IMPORT\t")).count(),
        2,
        "{out}"
    );
    assert!(out.contains("EXIT\t0"), "{out}");
    let guest = std::fs::read_to_string(s.work().join("image-defaults.stdout")).unwrap();
    support::transcript(&guest, "image-checks.sh", "image-contract-pass");
}
