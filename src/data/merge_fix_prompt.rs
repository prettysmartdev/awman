//! Pure detection of git conflict markers in merge output.

/// `true` when any line starts with a git conflict marker: `<<<<<<<`,
/// `=======` or `>>>>>>>`.
pub fn has_conflict_markers(contents: &str) -> bool {
    contents.lines().any(|line| {
        line.starts_with("<<<<<<<") || line.starts_with("=======") || line.starts_with(">>>>>>>")
    })
}
