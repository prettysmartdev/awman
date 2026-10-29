//! Layer 3 names no config field (WI 0150).
//!
//! The map, list and scalar rows of the config views, and the editors behind
//! them, are driven by each row's `ConfigRowShape` and the hints Layer 2
//! supplies. A frontend source file that spells out the dynamic-workflow
//! setting names would be back to matching on field names, so none may.
//! Tests use synthetic names (`demo.map`, `demo.list.0`) for the same reason.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read frontend dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_frontend_source_names_the_dynamic_workflow_settings() {
    // Built from pieces so that this file does not contain them itself.
    let forbidden = [
        ["dynamic", "Workflows"].concat(),
        ["workflows", ".", "dynamic"].concat(),
        // The member settings, which a frontend could still match on after
        // stripping a prefix.
        ["agents", "ToModels"].concat(),
        ["default", "Leader"].concat(),
        ["maxConcurrent", "Steps"].concat(),
    ];
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/frontend");
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    assert!(
        files.len() > 50,
        "expected to scan the whole frontend tree, found {} files",
        files.len()
    );

    let mut hits = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).expect("read source");
        for (n, line) in text.lines().enumerate() {
            for needle in &forbidden {
                if line.contains(needle.as_str()) {
                    hits.push(format!("{}:{}: {needle}", file.display(), n + 1));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "frontend sources must not name config fields:\n{}",
        hits.join("\n")
    );
}
