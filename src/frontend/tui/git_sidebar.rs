//! Git sidebar — shared data types + the background diff-polling task.
//!
//! The sidebar shows a per-file `+/-` change summary of the tab's working
//! directory, refreshed every ~2 seconds by a background tokio task. The
//! parsing helpers ([`parse_porcelain_status`], [`parse_numstat`],
//! [`build_summary`]) are pure functions so they can be unit-tested without
//! spawning git or touching the disk; the task glue ([`start_git_diff_poll_task`])
//! wires them to `git` subprocesses and the shared mutex the renderer reads.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// How a file changed relative to `HEAD`. Drives the sidebar accent color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitFileChangeType {
    Added,
    Modified,
    Deleted,
}

/// A single changed file with its per-file line counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFileEntry {
    pub path: String,
    pub change_type: GitFileChangeType,
    pub additions: u32,
    pub deletions: u32,
    /// `git diff --numstat` reports `-\t-\tpath` for binary files. We surface
    /// these as `+0 -0` with a `(binary)` suffix rather than dropping them.
    pub binary: bool,
}

/// The full diff snapshot rendered by the sidebar / status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDiffSummary {
    pub files: Vec<GitFileEntry>,
    pub total_additions: u32,
    pub total_deletions: u32,
    /// Current branch name. `None` on detached HEAD or when git can't tell.
    pub branch: Option<String>,
}

/// Cross-thread shared diff summary. The poll task writes here; the TUI
/// renderer reads it. `None` means "no git data" (not a repo, no commits and
/// no status, or git failed). Mirrors the `SharedActiveWorktreePath` pattern.
pub type SharedGitDiffSummary = Arc<Mutex<Option<GitDiffSummary>>>;

/// Sidebar open/close state, stored on the `Tab`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitSidebarState {
    Open,
    Closed,
}

/// Minimum usable sidebar width in columns. When 1/4 of the terminal is
/// narrower than this the sidebar is treated as closed (only the status-bar
/// summary shows).
pub const MIN_SIDEBAR_WIDTH: u16 = 20;

/// The sidebar's column width for the given terminal width and state. Returns
/// `0` when the sidebar is closed or would be narrower than
/// [`MIN_SIDEBAR_WIDTH`]. Shared by the renderer (layout split) and the event
/// loop (PTY resize) so both agree on when the sidebar is effectively present.
pub fn sidebar_width(term_cols: u16, state: GitSidebarState) -> u16 {
    match state {
        GitSidebarState::Open => {
            let w = term_cols / 4;
            if w >= MIN_SIDEBAR_WIDTH {
                w
            } else {
                0
            }
        }
        GitSidebarState::Closed => 0,
    }
}

/// One parsed `git diff --numstat` row. `additions`/`deletions` are `None`
/// for binary files (git prints `-` in those columns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumstatEntry {
    pub path: String,
    pub additions: Option<u32>,
    pub deletions: Option<u32>,
}

/// Parse `git status --porcelain` output into `(path, change_type)` pairs.
///
/// Change-type mapping (per the work item spec): `??` → Added, any status
/// column containing `D` → Deleted, everything else → Modified. Rename lines
/// (`R  old -> new`) resolve to the new path.
pub fn parse_porcelain_status(stdout: &str) -> Vec<(String, GitFileChangeType)> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        // Porcelain v1 lines are `XY<space>PATH`: two status columns, a
        // separator space, then the path. Anything shorter is malformed.
        if line.len() < 4 {
            continue;
        }
        let code = &line[..2];
        let rest = &line[3..];
        let path = rename_target(rest);
        let change_type = if code == "??" {
            GitFileChangeType::Added
        } else if code.contains('D') {
            GitFileChangeType::Deleted
        } else {
            GitFileChangeType::Modified
        };
        out.push((path, change_type));
    }
    out
}

/// Resolve a porcelain rename entry (`old -> new`) to its destination path.
fn rename_target(rest: &str) -> String {
    match rest.rfind(" -> ") {
        Some(idx) => rest[idx + 4..].to_string(),
        None => rest.to_string(),
    }
}

/// Parse `git diff --numstat HEAD` output into per-file entries.
///
/// Each line is `additions\tdeletions\tpath`. Binary files report `-` for both
/// counts (surfaced as `None`). Rename paths (`{old => new}` or `old => new`)
/// resolve to the destination.
pub fn parse_numstat(stdout: &str) -> Vec<NumstatEntry> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(a), Some(d), Some(p)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let additions = if a == "-" { None } else { a.parse().ok() };
        let deletions = if d == "-" { None } else { d.parse().ok() };
        out.push(NumstatEntry {
            path: resolve_numstat_path(p),
            additions,
            deletions,
        });
    }
    out
}

/// Resolve a numstat rename path. Handles the two git forms:
/// `prefix{old => new}suffix` and the bare `old => new`.
fn resolve_numstat_path(raw: &str) -> String {
    if let (Some(open), Some(close)) = (raw.find('{'), raw.find('}')) {
        if open < close {
            let prefix = &raw[..open];
            let inner = &raw[open + 1..close];
            let suffix = &raw[close + 1..];
            let new_part = inner
                .split("=>")
                .nth(1)
                .map(str::trim)
                .unwrap_or_else(|| inner.trim());
            return format!("{prefix}{new_part}{suffix}");
        }
    }
    if raw.contains("=>") {
        if let Some(new_part) = raw.split("=>").nth(1) {
            return new_part.trim().to_string();
        }
    }
    raw.to_string()
}

/// Combine porcelain change-types, numstat line counts, and pre-counted
/// untracked-file line totals into a [`GitDiffSummary`].
///
/// The porcelain list drives the file set (it captures both staged and
/// unstaged changes). Line counts come from numstat for tracked files, from
/// `untracked_lines` for `??` files, and default to `0` when neither is
/// available (e.g. the no-commits fallback). Binary files become `+0 -0`
/// with `binary = true`.
pub fn build_summary(
    porcelain: &[(String, GitFileChangeType)],
    numstat: &[NumstatEntry],
    untracked_lines: &HashMap<String, u32>,
) -> GitDiffSummary {
    let mut files = Vec::new();
    let mut total_additions = 0u32;
    let mut total_deletions = 0u32;

    for (path, change_type) in porcelain {
        let (additions, deletions, binary) =
            if let Some(entry) = numstat.iter().find(|n| &n.path == path) {
                match (entry.additions, entry.deletions) {
                    (Some(a), Some(d)) => (a, d, false),
                    // A `-` in either column means git treated it as binary.
                    _ => (0, 0, true),
                }
            } else if let Some(&count) = untracked_lines.get(path) {
                (count, 0, false)
            } else {
                (0, 0, false)
            };

        total_additions = total_additions.saturating_add(additions);
        total_deletions = total_deletions.saturating_add(deletions);
        files.push(GitFileEntry {
            path: path.clone(),
            change_type: *change_type,
            additions,
            deletions,
            binary,
        });
    }

    GitDiffSummary {
        files,
        total_additions,
        total_deletions,
        // The branch comes from a separate git call; the poll task fills it in.
        branch: None,
    }
}

/// The sidebar's border title — a condensed `git status` line. Always
/// non-empty so the open sidebar is labeled even with no data or no changes:
/// `main: 3 changed`, `main: clean`, `HEAD: 1 changed` (detached), or
/// `git status` when there is no git data at all.
pub fn sidebar_title(summary: &Option<GitDiffSummary>) -> String {
    let Some(summary) = summary else {
        return "git status".to_string();
    };
    let branch = summary.branch.as_deref().unwrap_or("HEAD");
    match summary.files.len() {
        0 => format!("{branch}: clean"),
        n => format!("{branch}: {n} changed"),
    }
}

/// Spawn the background diff-polling task.
///
/// Every ~2 seconds the task runs `git status --porcelain` and
/// `git diff --numstat HEAD` against `root`, parses them into a
/// [`GitDiffSummary`], and replaces the shared value. On failure (not a repo,
/// git missing) it stores `None`. When `HEAD` doesn't exist yet (no commits)
/// numstat fails, so every file is reported as Added with `0` line counts. The
/// loop exits promptly when `cancel` is triggered.
pub fn start_git_diff_poll_task(
    root: PathBuf,
    summary: SharedGitDiffSummary,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if cancel.is_cancelled() {
                break;
            }

            let next = poll_once(&root).await;
            if let Ok(mut guard) = summary.lock() {
                *guard = next;
            }

            // Sleep ~2s, but wake immediately on cancellation.
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
    })
}

/// Run one poll cycle: gather porcelain + numstat, count untracked lines, and
/// assemble a summary. Returns `None` when the directory isn't a usable repo.
async fn poll_once(root: &Path) -> Option<GitDiffSummary> {
    // Porcelain is the source of truth for the file set. If it fails we can't
    // show anything meaningful, so bail to `None`.
    let porcelain_out = run_git(&["status", "--porcelain"], root).await?;
    let porcelain = parse_porcelain_status(&porcelain_out);

    // Branch name for the sidebar title. Empty output means detached HEAD;
    // surfaced as `None` so the title falls back to "HEAD".
    let branch = run_git(&["branch", "--show-current"], root)
        .await
        .map(|out| out.trim().to_string())
        .filter(|name| !name.is_empty());

    // numstat fails when there are no commits (no HEAD). Fall back to an empty
    // list + "no commits" flag so we still render the file set.
    let (numstat, has_commits) = match run_git(&["diff", "--numstat", "HEAD"], root).await {
        Some(out) => (parse_numstat(&out), true),
        None => {
            if run_git(&["rev-parse", "--verify", "HEAD"], root)
                .await
                .is_some()
            {
                return None;
            }
            (Vec::new(), false)
        }
    };

    // No-commits fallback: treat every file as Added with 0 line counts.
    if !has_commits {
        let porcelain: Vec<(String, GitFileChangeType)> = porcelain
            .into_iter()
            .map(|(path, _)| (path, GitFileChangeType::Added))
            .collect();
        let mut summary = build_summary(&porcelain, &[], &HashMap::new());
        summary.branch = branch;
        return Some(summary);
    }

    // Count lines for untracked files (`??`) not covered by numstat.
    let mut untracked_lines = HashMap::new();
    for (path, change_type) in &porcelain {
        if *change_type == GitFileChangeType::Added
            && !numstat.iter().any(|n| &n.path == path)
        {
            let count = match repo_relative_file_path(root, path) {
                Some(file_path) => count_file_lines(&file_path).await,
                None => 0,
            };
            untracked_lines.insert(path.clone(), count);
        }
    }

    let mut summary = build_summary(&porcelain, &numstat, &untracked_lines);
    summary.branch = branch;

    Some(summary)
}

/// Run `git` with explicit args in `root`. Returns stdout on success, `None`
/// on spawn failure or non-zero exit. Never shells out through a string.
async fn run_git(args: &[&str], root: &Path) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn repo_relative_file_path(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel);
    if rel.is_absolute() {
        return None;
    }
    if rel.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return None;
    }
    Some(root.join(rel))
}

/// Count the lines in an untracked file. Missing/unreadable files count as 0.
async fn count_file_lines(path: &Path) -> u32 {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if meta.file_type().is_file() => {}
        _ => return 0,
    }
    match tokio::fs::read(path).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).lines().count() as u32,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_porcelain_status ─────────────────────────────────────────────

    #[test]
    fn porcelain_untracked_is_added() {
        let got = parse_porcelain_status("?? newfile.rs\n");
        assert_eq!(
            got,
            vec![("newfile.rs".to_string(), GitFileChangeType::Added)]
        );
    }

    #[test]
    fn porcelain_staged_delete_is_deleted() {
        // `D ` (deleted in index) → Deleted.
        let got = parse_porcelain_status("D  deleted.rs\n");
        assert_eq!(
            got,
            vec![("deleted.rs".to_string(), GitFileChangeType::Deleted)]
        );
    }

    #[test]
    fn porcelain_unstaged_delete_is_deleted() {
        // ` D` (deleted in worktree) → Deleted (status column contains 'D').
        let got = parse_porcelain_status(" D deleted.rs\n");
        assert_eq!(
            got,
            vec![("deleted.rs".to_string(), GitFileChangeType::Deleted)]
        );
    }

    #[test]
    fn porcelain_staged_modify_is_modified() {
        // `M ` (modified in index) → Modified.
        let got = parse_porcelain_status("M  mod.rs\n");
        assert_eq!(got, vec![("mod.rs".to_string(), GitFileChangeType::Modified)]);
    }

    #[test]
    fn porcelain_unstaged_modify_is_modified() {
        // ` M` (modified in worktree) → Modified.
        let got = parse_porcelain_status(" M mod.rs\n");
        assert_eq!(got, vec![("mod.rs".to_string(), GitFileChangeType::Modified)]);
    }

    #[test]
    fn porcelain_rename_resolves_to_destination() {
        // `R ` rename lines carry `old -> new`; we track the destination path.
        let got = parse_porcelain_status("R  old.rs -> new.rs\n");
        assert_eq!(got, vec![("new.rs".to_string(), GitFileChangeType::Modified)]);
    }

    #[test]
    fn porcelain_skips_malformed_short_lines() {
        // Lines shorter than `XY<space>PATH` are ignored.
        let got = parse_porcelain_status("M\n?\n\n");
        assert!(got.is_empty());
    }

    #[test]
    fn porcelain_parses_multiple_lines() {
        let out = "?? a.rs\n M b.rs\nD  c.rs\n";
        let got = parse_porcelain_status(out);
        assert_eq!(
            got,
            vec![
                ("a.rs".to_string(), GitFileChangeType::Added),
                ("b.rs".to_string(), GitFileChangeType::Modified),
                ("c.rs".to_string(), GitFileChangeType::Deleted),
            ]
        );
    }

    // ── parse_numstat ──────────────────────────────────────────────────────

    #[test]
    fn numstat_plain_counts() {
        let got = parse_numstat("5\t2\tsrc/foo.rs\n");
        assert_eq!(
            got,
            vec![NumstatEntry {
                path: "src/foo.rs".to_string(),
                additions: Some(5),
                deletions: Some(2),
            }]
        );
    }

    #[test]
    fn numstat_binary_is_none_columns() {
        let got = parse_numstat("-\t-\timg.png\n");
        assert_eq!(
            got,
            vec![NumstatEntry {
                path: "img.png".to_string(),
                additions: None,
                deletions: None,
            }]
        );
    }

    #[test]
    fn numstat_rename_braced_resolves_to_destination() {
        let got = parse_numstat("3\t1\t{old.rs => new.rs}\n");
        assert_eq!(
            got,
            vec![NumstatEntry {
                path: "new.rs".to_string(),
                additions: Some(3),
                deletions: Some(1),
            }]
        );
    }

    #[test]
    fn numstat_rename_braced_with_prefix_and_suffix() {
        // git emits `dir/{old => new}/file` for renames within a subtree.
        let got = parse_numstat("1\t0\tsrc/{old => new}/file.rs\n");
        assert_eq!(got[0].path, "src/new/file.rs");
    }

    #[test]
    fn numstat_rename_bare_arrow_resolves_to_destination() {
        let got = parse_numstat("2\t2\told.rs => new.rs\n");
        assert_eq!(got[0].path, "new.rs");
    }

    #[test]
    fn numstat_skips_malformed_lines() {
        // No tab separators → skipped rather than mis-parsed.
        let got = parse_numstat("garbage line\n5\t2\tok.rs\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "ok.rs");
    }

    // ── build_summary ──────────────────────────────────────────────────────

    #[test]
    fn build_summary_combines_counts_totals_and_binary() {
        let porcelain = vec![
            ("src/foo.rs".to_string(), GitFileChangeType::Modified),
            ("img.png".to_string(), GitFileChangeType::Added),
            ("new.txt".to_string(), GitFileChangeType::Added),
        ];
        let numstat = vec![
            NumstatEntry {
                path: "src/foo.rs".to_string(),
                additions: Some(5),
                deletions: Some(2),
            },
            NumstatEntry {
                path: "img.png".to_string(),
                additions: None,
                deletions: None,
            },
        ];
        let mut untracked = HashMap::new();
        untracked.insert("new.txt".to_string(), 10);

        let summary = build_summary(&porcelain, &numstat, &untracked);

        assert_eq!(summary.total_additions, 15, "5 + 0 (binary) + 10 untracked");
        assert_eq!(summary.total_deletions, 2, "2 + 0 + 0");
        assert_eq!(
            summary.files,
            vec![
                GitFileEntry {
                    path: "src/foo.rs".to_string(),
                    change_type: GitFileChangeType::Modified,
                    additions: 5,
                    deletions: 2,
                    binary: false,
                },
                GitFileEntry {
                    path: "img.png".to_string(),
                    change_type: GitFileChangeType::Added,
                    additions: 0,
                    deletions: 0,
                    binary: true,
                },
                GitFileEntry {
                    path: "new.txt".to_string(),
                    change_type: GitFileChangeType::Added,
                    additions: 10,
                    deletions: 0,
                    binary: false,
                },
            ]
        );
    }

    #[test]
    fn build_summary_deleted_file_uses_numstat_deletions() {
        let porcelain = vec![("gone.rs".to_string(), GitFileChangeType::Deleted)];
        let numstat = vec![NumstatEntry {
            path: "gone.rs".to_string(),
            additions: Some(0),
            deletions: Some(4),
        }];
        let summary = build_summary(&porcelain, &numstat, &HashMap::new());
        assert_eq!(summary.total_deletions, 4);
        assert_eq!(summary.files[0].deletions, 4);
        assert_eq!(summary.files[0].change_type, GitFileChangeType::Deleted);
    }

    #[test]
    fn build_summary_missing_counts_default_to_zero() {
        // A porcelain file with neither numstat nor untracked info (e.g. the
        // no-commits fallback) becomes +0 -0.
        let porcelain = vec![("orphan.rs".to_string(), GitFileChangeType::Added)];
        let summary = build_summary(&porcelain, &[], &HashMap::new());
        assert_eq!(summary.total_additions, 0);
        assert_eq!(summary.total_deletions, 0);
        assert_eq!(summary.files[0].additions, 0);
        assert!(!summary.files[0].binary);
    }

    // ── sidebar_title ──────────────────────────────────────────────────────

    fn summary_with(branch: Option<&str>, file_count: usize) -> GitDiffSummary {
        GitDiffSummary {
            files: (0..file_count)
                .map(|i| GitFileEntry {
                    path: format!("f{i}.rs"),
                    change_type: GitFileChangeType::Modified,
                    additions: 1,
                    deletions: 0,
                    binary: false,
                })
                .collect(),
            total_additions: file_count as u32,
            total_deletions: 0,
            branch: branch.map(str::to_string),
        }
    }

    #[test]
    fn sidebar_title_no_data_is_git_status() {
        assert_eq!(sidebar_title(&None), "git status");
    }

    #[test]
    fn sidebar_title_clean_worktree() {
        assert_eq!(
            sidebar_title(&Some(summary_with(Some("main"), 0))),
            "main: clean"
        );
    }

    #[test]
    fn sidebar_title_branch_and_change_count() {
        assert_eq!(
            sidebar_title(&Some(summary_with(Some("feature/x"), 3))),
            "feature/x: 3 changed"
        );
        assert_eq!(
            sidebar_title(&Some(summary_with(Some("main"), 1))),
            "main: 1 changed"
        );
    }

    #[test]
    fn sidebar_title_detached_head_falls_back_to_head() {
        assert_eq!(sidebar_title(&Some(summary_with(None, 2))), "HEAD: 2 changed");
        assert_eq!(sidebar_title(&Some(summary_with(None, 0))), "HEAD: clean");
    }

    // ── sidebar_width ──────────────────────────────────────────────────────

    #[test]
    fn sidebar_width_closed_is_zero() {
        for w in [0u16, 20, 80, 200] {
            assert_eq!(sidebar_width(w, GitSidebarState::Closed), 0);
        }
    }

    #[test]
    fn sidebar_width_open_is_quarter_when_wide_enough() {
        assert_eq!(sidebar_width(80, GitSidebarState::Open), 20);
        assert_eq!(sidebar_width(200, GitSidebarState::Open), 50);
    }

    #[test]
    fn sidebar_width_open_at_minimum_threshold() {
        // 80 / 4 == 20 == MIN_SIDEBAR_WIDTH → allowed.
        assert_eq!(sidebar_width(80, GitSidebarState::Open), MIN_SIDEBAR_WIDTH);
    }

    #[test]
    fn sidebar_width_open_collapses_when_too_narrow() {
        // 79 / 4 == 19 < 20 → treated as closed.
        assert_eq!(sidebar_width(79, GitSidebarState::Open), 0);
        assert_eq!(sidebar_width(40, GitSidebarState::Open), 0);
        assert_eq!(sidebar_width(0, GitSidebarState::Open), 0);
    }

    #[test]
    fn sidebar_width_never_exceeds_quarter() {
        for w in [20u16, 80, 100, 123, 200, 255] {
            let sw = sidebar_width(w, GitSidebarState::Open);
            assert!(
                sw <= w / 4,
                "sidebar width {sw} for term {w} must be ≤ 25%"
            );
        }
    }

    // ── cancellation ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn poll_task_exits_promptly_on_cancel() {
        let tmp = tempfile::tempdir().unwrap();
        let summary: SharedGitDiffSummary = Arc::new(Mutex::new(None));
        let cancel = CancellationToken::new();
        let handle =
            start_git_diff_poll_task(tmp.path().to_path_buf(), summary, cancel.clone());

        // Let the task run at least one iteration and settle into its 2s sleep.
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        // The loop selects on `cancel.cancelled()`, so it must finish well
        // before the 2-second poll interval — not wait out the sleep.
        let res = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(
            res.is_ok(),
            "poll task did not exit within 1s of cancellation"
        );
        res.unwrap().expect("poll task panicked");
    }

    #[tokio::test]
    async fn poll_once_no_commits_reports_added_zero_counts() {
        let tmp = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .expect("git init should run");
        tokio::fs::write(tmp.path().join("new.txt"), "one\ntwo\n")
            .await
            .unwrap();

        let summary = poll_once(tmp.path())
            .await
            .expect("fresh git repo should still produce status data");

        assert_eq!(summary.total_additions, 0);
        assert_eq!(summary.total_deletions, 0);
        assert_eq!(summary.files.len(), 1);
        assert_eq!(summary.files[0].path, "new.txt");
        assert_eq!(summary.files[0].change_type, GitFileChangeType::Added);
        assert_eq!(summary.files[0].additions, 0);
        assert_eq!(summary.files[0].deletions, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn count_file_lines_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(outside.path(), "one\ntwo\n").await.unwrap();
        let link = tmp.path().join("outside-link");
        symlink(outside.path(), &link).unwrap();

        assert_eq!(count_file_lines(&link).await, 0);
    }

    #[test]
    fn repo_relative_file_path_rejects_paths_outside_root() {
        let root = Path::new("/repo");

        assert!(repo_relative_file_path(root, "/tmp/outside").is_none());
        assert!(repo_relative_file_path(root, "../outside").is_none());
        assert!(repo_relative_file_path(root, "src/../../outside").is_none());
        assert_eq!(
            repo_relative_file_path(root, "src/main.rs").unwrap(),
            PathBuf::from("/repo/src/main.rs")
        );
    }
}
