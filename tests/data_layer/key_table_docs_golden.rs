//! The generated key table in `docs/07-configuration.md` matches
//! `fields::render_docs_table()`.
//!
//! The block lives between `<!-- key-table:start -->` and
//! `<!-- key-table:end -->`. Run with `AWMAN_WRITE_GOLDEN=1` to rewrite it.

use awman::data::config::fields::render_docs_table;

const START: &str = "<!-- key-table:start -->";
const END: &str = "<!-- key-table:end -->";

#[test]
fn key_table_docs_golden() {
    let path = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .join("docs/07-configuration.md");
    let text = std::fs::read_to_string(&path).expect("read docs/07-configuration.md");
    let table = render_docs_table();
    let (Some(start), Some(end)) = (text.find(START), text.find(END)) else {
        if std::env::var("AWMAN_WRITE_GOLDEN").as_deref() == Ok("1") {
            panic!("add {START} and {END} markers to {} first", path.display());
        }
        // The markers arrive with the docs rewrite; until then there is
        // nothing to compare.
        eprintln!("{}: no key-table markers yet; skipping", path.display());
        return;
    };
    let current = &text[start + START.len()..end];
    let expected = format!("\n{table}");
    if std::env::var("AWMAN_WRITE_GOLDEN").as_deref() == Ok("1") {
        let updated = format!("{}{expected}{}", &text[..start + START.len()], &text[end..]);
        std::fs::write(&path, updated).expect("write docs/07-configuration.md");
        return;
    }
    assert_eq!(
        current, expected,
        "docs/07-configuration.md key table is stale; rerun with AWMAN_WRITE_GOLDEN=1"
    );
}
