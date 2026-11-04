//! Docs link integrity: every relative link under `docs/` points at a file
//! that exists.

/// Walk every `.md` file under `docs/` (recursively, including subdirectories
/// like `docs/blog/` and `docs/releases/`) and verify that each relative file
/// link `[text](target)` or `[text](target#anchor)` resolves to a file that
/// exists, so a renamed or deleted doc cannot leave dangling links behind.
///
/// Content inside `<!-- … -->` is skipped: it never reaches a reader, so a
/// link there cannot be broken for anyone. Screenshot placeholders left for a
/// human to fill in later live in such comments.
#[test]
fn docs_internal_links_all_resolve() {
    let docs_dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../..")).join("docs");
    assert!(
        docs_dir.is_dir(),
        "docs/ directory not found at {docs_dir:?}"
    );

    let mut broken: Vec<String> = Vec::new();
    let md_files = collect_md_files_recursively(&docs_dir);
    assert!(
        !md_files.is_empty(),
        "expected at least one .md file under {docs_dir:?}"
    );

    for file_path in md_files {
        let content = std::fs::read_to_string(&file_path)
            .unwrap_or_else(|e| panic!("read {file_path:?}: {e}"));

        // Links resolve relative to the directory of the current file,
        // not the root of docs/.
        let base_dir = file_path
            .parent()
            .expect("md file must have a parent dir")
            .to_path_buf();

        for link_target in extract_md_links(&strip_html_comments(&content)) {
            if link_target.starts_with("http://")
                || link_target.starts_with("https://")
                || link_target.starts_with("mailto:")
                || link_target.starts_with('#')
            {
                continue;
            }

            let target_file = link_target
                .split_once('#')
                .map(|(base, _)| base)
                .unwrap_or(&link_target);

            if target_file.is_empty() {
                continue;
            }

            let resolved = base_dir.join(target_file);
            if !resolved.exists() {
                let rel = file_path
                    .strip_prefix(&docs_dir)
                    .unwrap_or(&file_path)
                    .display();
                broken.push(format!("{rel}: broken link [{link_target}] → {resolved:?}",));
            }
        }
    }

    assert!(
        broken.is_empty(),
        "Found broken internal links under docs/:\n{}",
        broken.join("\n")
    );
}

#[test]
fn commented_out_links_are_not_checked() {
    let content = "\
live [one](./a.md) here
<!-- SCREENSHOT PLACEHOLDER: save it as images/squad-tab.png, then use:
     ![squad tab](./images/squad-tab.png) -->
live [two](./b.md) here
";
    let targets = extract_md_links(&strip_html_comments(content));
    assert_eq!(
        targets,
        vec!["./a.md".to_string(), "./b.md".to_string()],
        "links inside <!-- --> must not be treated as live links"
    );
}

#[test]
fn strip_html_comments_handles_multiple_and_unterminated_spans() {
    assert_eq!(strip_html_comments("a<!--x-->b<!--y-->c"), "abc");
    assert_eq!(strip_html_comments("no comments here"), "no comments here");
    // An unterminated comment swallows the rest of the input.
    assert_eq!(strip_html_comments("keep<!--dropped [x](./y.md)"), "keep");
}

fn collect_md_files_recursively(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(collect_md_files_recursively(&path));
        } else if path.extension().and_then(|x| x.to_str()) == Some("md") {
            out.push(path);
        }
    }
    out
}

/// Remove every `<!-- … -->` span from a Markdown string so that commented-out
/// content is not mistaken for a live link. An unterminated `<!--` swallows the
/// rest of the input, which is how Markdown renderers treat it.
fn strip_html_comments(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + "-->".len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Extract all link targets from `[text](target)` patterns in a Markdown string.
/// Does not parse full CommonMark; only handles inline `[…](…)` links.
fn extract_md_links(content: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Look for `](` which marks the start of a link target.
        if bytes[i] == b']' && i + 1 < bytes.len() && bytes[i + 1] == b'(' {
            i += 2; // skip `](`
            let start = i;
            // Collect until `)` or end of line.
            while i < bytes.len() && bytes[i] != b')' && bytes[i] != b'\n' {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b')' {
                let target = &content[start..i];
                // Skip image alt-text that ended up here (already have `![…]`
                // syntax, so real link detection is good enough for our purpose).
                targets.push(target.to_string());
            }
        }
        i += 1;
    }

    targets
}
