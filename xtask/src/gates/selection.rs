//! Small path glob matcher: * stays in a segment; ** crosses directories.
use crate::manifest::{Manifest, Suite};
pub fn matches(pattern: &str, path: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        if p.is_empty() {
            return s.is_empty();
        }
        if p[0] == b'*' {
            let double = p.get(1) == Some(&b'*');
            let rest = &p[if double { 2 } else { 1 }..];
            if double && rest.first() == Some(&b'/') && go(&rest[1..], s) {
                return true;
            }
            return go(rest, s) || (!s.is_empty() && (double || s[0] != b'/') && go(p, &s[1..]));
        }
        !s.is_empty() && p[0] == s[0] && go(&p[1..], &s[1..])
    }
    go(pattern.as_bytes(), path.as_bytes())
}
pub fn mandatory(id: &str, s: &Suite) -> bool {
    s.always
        || s.tier == "unit"
        || s.kind == "doctest"
        || id.starts_with("lint-")
        || id.starts_with("component-parity-")
}
pub fn full_reason(m: &Manifest, gate: &str, event: &str, paths: &[String]) -> Option<String> {
    if gate != "pr" || event != "pull_request" {
        return Some(format!("full run: gate {gate}, event {event}"));
    }
    if paths.is_empty() {
        return Some("full-run fallback: empty changed-path set".into());
    }
    for path in paths {
        if m.selection.full_run.iter().any(|p| matches(p, path)) {
            return Some(format!("full-run fallback: shared path {path}"));
        }
        let gate_suites: std::collections::BTreeSet<_> = crate::manifest::validate::cells(m, gate)
            .unwrap_or_default()
            .into_iter()
            .map(|c| c.suite)
            .collect();
        if !m
            .suites
            .iter()
            .any(|(id, s)| gate_suites.contains(id) && s.paths.iter().any(|p| matches(p, path)))
        {
            return Some(format!("full-run fallback: unknown path {path}"));
        }
    }
    None
}
