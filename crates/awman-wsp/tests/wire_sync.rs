//! Repository builds enforce source identity; packaged crates remain portable.
#[test]
fn vendored_wire_helpers_match_awman() {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src/data/wire.rs");
    // The upstream copy does not exist in a standalone/published crate.
    if source.exists() {
        assert_eq!(
            std::fs::read(source).unwrap(),
            include_bytes!("../src/wire.rs")
        );
    }
}
