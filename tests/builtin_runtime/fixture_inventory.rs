//! The spike fixture's assertions, promoted to tests.
//!
//! `tools/oci-runtime-spike/{image,guest}-checks.sh` are the 9 image and 32
//! mount/config assertions that passed on Apple Silicon. Here the names are
//! *derived from the scripts* (so a removed or renamed check fails this test),
//! classified against the work item's compatibility dimensions, and a strict
//! transcript parser decides whether a guest run really passed every one.
//! The `builtin_hw_*` tests feed real guest output to the same parser.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const IMAGE_COMPLETE: &str = "image-contract-pass";
pub const GUEST_COMPLETE: &str = "guest-suite-complete";

fn script(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tools/oci-runtime-spike")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Names of every `check <name> …` line in a spike script, in file order.
pub fn check_names(script_text: &str) -> Vec<String> {
    script_text
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("check ")?;
            let name = rest.split_whitespace().next()?;
            (!name.starts_with('$')).then(|| name.to_owned())
        })
        .collect()
}

pub fn image_checks() -> Vec<String> {
    check_names(&script("image-checks.sh"))
}

pub fn guest_checks() -> Vec<String> {
    check_names(&script("guest-checks.sh"))
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Transcript {
    pub passed: BTreeSet<String>,
    pub failed: Vec<String>,
    pub unsupported: Vec<String>,
    pub duplicates: Vec<String>,
    pub markers: BTreeSet<String>,
}

pub fn parse_transcript(stdout: &str) -> Transcript {
    let mut t = Transcript::default();
    for line in stdout.lines() {
        match line.split_once('\t') {
            Some(("PASS", name)) => {
                if !t.passed.insert(name.to_owned()) {
                    t.duplicates.push(name.to_owned());
                }
            }
            Some(("FAIL", name)) => t.failed.push(name.to_owned()),
            Some(("UNSUPPORTED", what)) => t.unsupported.push(what.to_owned()),
            _ => {
                t.markers.insert(line.trim().to_owned());
            }
        }
    }
    t
}

/// Every problem that stops `transcript` from being a genuine full pass.
pub fn problems(expected: &[String], transcript: &Transcript, completion: &str) -> Vec<String> {
    let mut found = Vec::new();
    for name in expected {
        if !transcript.passed.contains(name) {
            found.push(format!("check {name} did not PASS"));
        }
    }
    for name in &transcript.failed {
        found.push(format!("check {name} FAILED"));
    }
    for what in &transcript.unsupported {
        found.push(format!("UNSUPPORTED is not a pass: {what}"));
    }
    for name in &transcript.duplicates {
        found.push(format!("check {name} reported twice"));
    }
    for name in transcript.passed.iter().filter(|n| !expected.contains(n)) {
        found.push(format!("unexpected check {name}"));
    }
    if !transcript.markers.contains(completion) {
        found.push(format!("completion marker {completion:?} is missing"));
    }
    found
}

/// Which compatibility dimension each guest check evidences.
pub fn guest_classification() -> BTreeMap<&'static str, &'static [&'static str]> {
    BTreeMap::from([
        ("platform", &["linux"][..]),
        (
            "overlay-directory",
            &[
                "directory-read",
                "directory-write",
                "directory-readonly",
                "nested-mount",
                "nested-readonly",
                "rename",
                "hardlink",
                "executable-bit",
                "spaces",
            ][..],
        ),
        ("overlay-file", &["file-readonly", "file-readwrite"][..]),
        ("overlay-skill", &["skill-named"][..]),
        (
            "overlay-context",
            &[
                "context-global",
                "context-repo",
                "context-workflow",
                "context-readwrite",
            ][..],
        ),
        ("overlay-env", &["environment"][..]),
        ("settings-direct", &["settings-direct"][..]),
        (
            "settings-claude",
            &["settings-claude-file", "settings-claude-directory"][..],
        ),
        ("settings-antigravity", &["settings-antigravity"][..]),
        // Append, AppendInline and Replace prompt modes are argv (checked in the
        // fake-driver matrix); the file/env/dir modes are guest-visible:
        (
            "prompt-file-and-env",
            &["file-prompt", "environment-prompt"][..],
        ),
        ("prompt-agents-md", &["agents-md"][..]),
        ("prompt-add-dir", &["add-directory"][..]),
        (
            "credential-refresh",
            &[
                "credential-refresh-token-absent",
                "live-atomic-host-refresh",
            ][..],
        ),
        (
            "cwd-and-isolation",
            &[
                "working-directory",
                "symlink-escape",
                "outside-host-secret",
                "readonly-remount",
            ][..],
        ),
    ])
}

pub const IMAGE_CLASSIFICATION: &[(&str, &[&str])] = &[
    (
        "image-defaults",
        &[
            "image-user",
            "image-group",
            "image-workdir",
            "image-home",
            "image-env",
        ],
    ),
    (
        "image-layers",
        &[
            "image-file-whiteout",
            "image-opaque-whiteout",
            "image-upper-layer",
            "image-executable-mode",
        ],
    ),
];

// ─── tests ───────────────────────────────────────────────────────────────────

#[test]
fn builtin_fixture_has_exactly_the_nine_image_assertions() {
    let names = image_checks();
    assert_eq!(names.len(), 9, "{names:?}");
    let classified: BTreeSet<&str> = IMAGE_CLASSIFICATION
        .iter()
        .flat_map(|(_, n)| n.iter().copied())
        .collect();
    assert_eq!(
        names.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        classified
    );
}

#[test]
fn builtin_fixture_has_exactly_the_thirty_two_mount_and_config_assertions() {
    let names = guest_checks();
    assert_eq!(names.len(), 32, "{names:?}");
    assert_eq!(
        names.iter().collect::<BTreeSet<_>>().len(),
        32,
        "names are unique"
    );
}

#[test]
fn builtin_every_guest_assertion_is_classified_and_every_dimension_is_covered() {
    let names: BTreeSet<String> = guest_checks().into_iter().collect();
    let classification = guest_classification();
    let mut classified = BTreeSet::new();
    for (dimension, members) in &classification {
        assert!(!members.is_empty(), "{dimension} has no assertion");
        for member in *members {
            assert!(
                names.contains(*member),
                "{dimension}: {member} is not in guest-checks.sh"
            );
            assert!(
                classified.insert(member.to_string()),
                "{member} classified twice"
            );
        }
    }
    assert_eq!(
        names, classified,
        "a new or removed guest check must be classified against a compatibility dimension"
    );
}

#[test]
fn builtin_fixture_pins_a_non_root_image_user_with_explicit_overrides_available() {
    // The promoted fixture image runs as uid/gid 1234 by default (image
    // defaults); the hardware tests separately override the user explicitly.
    let checker = script("image-checks.sh");
    assert!(checker.contains(r#"[ "$(id -u)" = 1234 ]"#));
    assert!(checker.contains(r#"[ "$(id -g)" = 1234 ]"#));
    assert!(checker.contains("printf 'image-contract-pass\\n'"));
    assert!(script("guest-checks.sh").contains("printf 'guest-suite-complete\\n'"));
}

fn full_pass(names: &[String], completion: &str) -> String {
    let mut out: String = names.iter().map(|n| format!("PASS\t{n}\n")).collect();
    out.push_str(completion);
    out.push('\n');
    out
}

#[test]
fn builtin_transcript_parser_accepts_only_a_complete_clean_pass() {
    let expected = guest_checks();
    let good = full_pass(&expected, GUEST_COMPLETE);
    assert_eq!(
        problems(&expected, &parse_transcript(&good), GUEST_COMPLETE),
        Vec::<String>::new()
    );
}

#[test]
fn builtin_transcript_parser_rejects_every_kind_of_non_pass() {
    let expected = guest_checks();
    let good = full_pass(&expected, GUEST_COMPLETE);

    let failing = good.replacen("PASS\tspaces", "FAIL\tspaces", 1);
    assert!(problems(&expected, &parse_transcript(&failing), GUEST_COMPLETE).len() >= 2);

    let unsupported =
        format!("UNSUPPORTED\tfile mounts, Claude config file, file/env prompts\n{good}");
    assert!(
        problems(&expected, &parse_transcript(&unsupported), GUEST_COMPLETE)
            .iter()
            .any(|p| p.contains("UNSUPPORTED"))
    );

    let truncated: String = good.lines().take(10).map(|l| format!("{l}\n")).collect();
    let found = problems(&expected, &parse_transcript(&truncated), GUEST_COMPLETE);
    assert!(found.iter().any(|p| p.contains("completion marker")));
    assert!(
        found.len() > 20,
        "a truncated run is many missing checks, not a pass"
    );

    let unfinished = good.replace(GUEST_COMPLETE, "");
    assert!(!problems(&expected, &parse_transcript(&unfinished), GUEST_COMPLETE).is_empty());

    let dup = format!("PASS\tlinux\n{good}");
    assert!(problems(&expected, &parse_transcript(&dup), GUEST_COMPLETE)
        .iter()
        .any(|p| p.contains("twice")));

    let extra = format!("PASS\tsurprise\n{good}");
    assert!(
        problems(&expected, &parse_transcript(&extra), GUEST_COMPLETE)
            .iter()
            .any(|p| p.contains("unexpected"))
    );

    assert!(!problems(&expected, &parse_transcript(""), GUEST_COMPLETE).is_empty());
}

#[test]
fn builtin_image_transcript_requires_the_completion_marker_too() {
    let expected = image_checks();
    let good = full_pass(&expected, IMAGE_COMPLETE);
    assert!(problems(&expected, &parse_transcript(&good), IMAGE_COMPLETE).is_empty());
    let without = good.replace(IMAGE_COMPLETE, "");
    assert!(!problems(&expected, &parse_transcript(&without), IMAGE_COMPLETE).is_empty());
}
