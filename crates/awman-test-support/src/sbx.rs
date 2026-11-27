//! Strict SBX cleanup evidence. Unknown rows can never establish absence.
/// Whether successful removal and complete listing prove an owned name absent.
pub fn removal_verified(removed: bool, listed: bool, bytes: &[u8], name: &str) -> bool {
    removed && listed && listed_names(bytes).is_some_and(|names| !names.iter().any(|n| n == name))
}

/// Parse every row, rejecting invalid names, duplicate names and ambiguous keys.
pub fn listed_names(stdout: &[u8]) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let rows = match &value {
        serde_json::Value::Array(rows) => rows,
        other => match (other.get("sandboxes"), other.get("Sandboxes")) {
            (Some(v), None) | (None, Some(v)) => v.as_array()?,
            _ => return None,
        },
    };
    let mut names = Vec::new();
    for row in rows {
        let name = match (row.get("name"), row.get("Name")) {
            (Some(v), None) | (None, Some(v)) => v.as_str()?,
            _ => return None,
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            || names.iter().any(|n| n == name)
        {
            return None;
        }
        names.push(name.to_owned());
    }
    Some(names)
}

#[cfg(test)]
mod tests {
    #[test]
    fn cleanup_never_proves_absence_from_unknown_or_partial_listings() {
        for raw in [
            "null",
            "{}",
            "not json",
            r#"[{"sandbox_name":"awman-owned"}]"#,
            r#"[{"name":"valid"},{"state":"running"}]"#,
            r#"[{"name":"a","Name":"b"}]"#,
            r#"[{"name":""}]"#,
            r#"[{"name":"awman/invalid"}]"#,
            r#"[{"name":"awman-\u0000"}]"#,
            r#"[{"name":"a"},{"name":"a"}]"#,
        ] {
            assert!(super::listed_names(raw.as_bytes()).is_none(), "{raw}");
        }
        assert_eq!(super::listed_names(b"[]"), Some(vec![]));
        assert!(!super::removal_verified(false, true, b"[]", "awman-owned"));
        assert!(!super::removal_verified(
            false,
            true,
            br#"[{"unknown":"awman-owned"}]"#,
            "awman-owned"
        ));
        assert!(!super::removal_verified(
            true,
            true,
            br#"[{"unknown":"awman-owned"}]"#,
            "awman-owned"
        ));
        assert!(!super::removal_verified(true, false, b"[]", "awman-owned"));
        assert!(super::removal_verified(true, true, b"[]", "awman-owned"));
        assert_eq!(
            super::listed_names(br#"{"Sandboxes":[{"Name":"awman-owned"}]}"#),
            Some(vec!["awman-owned".into()])
        );
    }
}
