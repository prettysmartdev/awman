//! Golden files: compare text with a committed file.
//!
//! The file is rewritten only when `INSTA_UPDATE=always` (the switch insta
//! uses, set by `cargo xtask test update-golden`) and never on CI.

use std::path::Path;

/// Panic with a line diff unless `actual` equals the file at `path`. A
/// missing file fails too (tell the developer how to create it).
pub fn assert_golden(path: &Path, actual: &str) {
    let env = crate::runtime_env::get();
    let update = env.insta_update.as_deref() == Some("always");
    match check(path, actual, update, env.ci) {
        Ok(()) => {}
        Err(message) => panic!("{message}"),
    }
}

fn check(path: &Path, actual: &str, update: bool, ci: bool) -> Result<(), String> {
    if update && ci {
        return Err(
            "golden files are never updated on CI (INSTA_UPDATE=always with CI set)".into(),
        );
    }
    let expected = std::fs::read_to_string(path).ok();
    if expected.as_deref() == Some(actual) {
        return Ok(());
    }
    if update {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, actual).map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(());
    }
    match expected {
        None => Err(format!(
            "golden file {} does not exist; run `cargo xtask test update-golden` to create it",
            path.display()
        )),
        Some(expected) => Err(format!(
            "output differs from golden file {} (run `cargo xtask test update-golden` if intended):\n{}",
            path.display(),
            diff(&expected, actual)
        )),
    }
}

/// A simple line diff: `-` expected, `+` actual, for the lines that differ.
fn diff(expected: &str, actual: &str) -> String {
    let a: Vec<&str> = expected.lines().collect();
    let b: Vec<&str> = actual.lines().collect();
    let mut out = String::new();
    for i in 0..a.len().max(b.len()) {
        match (a.get(i), b.get(i)) {
            (Some(x), Some(y)) if x == y => {}
            (x, y) => {
                if let Some(x) = x {
                    out.push_str(&format!("{:>4} - {x}\n", i + 1));
                }
                if let Some(y) = y {
                    out.push_str(&format!("{:>4} + {y}\n", i + 1));
                }
            }
        }
    }
    if out.is_empty() {
        out.push_str("(only trailing newline differs)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_passes_and_difference_shows_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        assert!(check(&path, "a\nb\n", false, false).is_ok());
        let err = check(&path, "a\nc\n", false, false).unwrap_err();
        assert!(err.contains("2 - b") && err.contains("2 + c"), "{err}");
    }

    #[test]
    fn missing_file_fails_unless_updating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new/g.txt");
        assert!(check(&path, "x", false, false)
            .unwrap_err()
            .contains("does not exist"));
        assert!(check(&path, "x", true, false).is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");
    }

    #[test]
    fn never_updates_on_ci() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.txt");
        assert!(check(&path, "x", true, true)
            .unwrap_err()
            .contains("never updated on CI"));
        assert!(!path.exists());
    }
}
