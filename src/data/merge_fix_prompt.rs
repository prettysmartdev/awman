//! Pure detection of git conflict markers in merge output.

/// `true` when any line starts with a git conflict marker: `<<<<<<<`,
/// `=======` or `>>>>>>>`.
pub fn has_conflict_markers(contents: &str) -> bool {
    contents.lines().any(|line| {
        line.starts_with("<<<<<<<") || line.starts_with("=======") || line.starts_with(">>>>>>>")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_markers_only_count_at_line_start() {
        assert!(has_conflict_markers("a\n<<<<<<< HEAD\nb\n"));
        assert!(has_conflict_markers("=======\n"));
        assert!(has_conflict_markers("x\n>>>>>>> branch"));
        assert!(!has_conflict_markers("  <<<<<<< indented\n"));
        assert!(!has_conflict_markers("resolved text\n"));
    }
}
