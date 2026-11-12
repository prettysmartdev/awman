//! `RemoteSlug` — the one parser for a git remote URL's `owner/repo` pair.
//!
//! Three copies of this parse existed (WI 0114 F-29): the context-directory
//! resolver's, the GitHub issue provider's, and the skill library's prefix
//! stripping. They disagreed about which hosts and URL shapes they accepted,
//! and each applied its own normalisation afterwards. This is the shared
//! half — splitting a remote URL into host, owner and repo, verbatim. What
//! each caller does with the pieces stays its own: the context resolver
//! normalises them into a directory name, the issue provider keeps them
//! as-is for the GitHub API and refuses any host but `github.com`.

/// The pieces of a git remote URL, exactly as the URL spells them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSlug {
    /// The host, with any `user@` prefix stripped (`github.com`).
    pub host: String,
    pub owner: String,
    pub repo: String,
}

impl RemoteSlug {
    /// Parse a remote URL in either of the two shapes git writes:
    ///
    /// - scp-like SSH: `git@host:owner/repo[.git]`
    /// - URL: `scheme://[user@]host/owner/repo[.git]` (extra path segments
    ///   after `repo` are ignored)
    ///
    /// Returns `None` for anything else, including a bare `owner/repo` with
    /// no host — that is a user-typed reference, not a remote URL.
    pub fn parse(remote_url: &str) -> Option<Self> {
        let remote = remote_url.trim();

        // scp-like SSH: `git@host:owner/repo.git`. The `@` before the first
        // colon is what distinguishes it from a `scheme://` URL.
        if let Some(colon_idx) = remote.find(':') {
            if remote[..colon_idx].contains('@') {
                let host = remote[..colon_idx]
                    .rsplit_once('@')
                    .map(|(_, h)| h)
                    .unwrap_or(&remote[..colon_idx]);
                if let Some((owner, repo)) = split_owner_repo(&remote[colon_idx + 1..]) {
                    return Some(Self {
                        host: host.to_string(),
                        owner,
                        repo,
                    });
                }
            }
        }

        // URL form: `scheme://[user@]host/owner/repo.git`.
        if let Some(idx) = remote.find("://") {
            let after_scheme = &remote[idx + 3..];
            if let Some(path_start) = after_scheme.find('/') {
                let authority = &after_scheme[..path_start];
                let host = authority
                    .rsplit_once('@')
                    .map(|(_, h)| h)
                    .unwrap_or(authority);
                if let Some((owner, repo)) = split_owner_repo(&after_scheme[path_start + 1..]) {
                    return Some(Self {
                        host: host.to_string(),
                        owner,
                        repo,
                    });
                }
            }
        }

        None
    }

    /// Whether this remote is hosted at `host`, ignoring case.
    pub fn is_host(&self, host: &str) -> bool {
        self.host.eq_ignore_ascii_case(host)
    }

    /// `{owner}/{repo}` with both components normalised for use as a
    /// directory name (lowercase, non-alphanumeric replaced by `-`).
    pub fn normalised_path(&self) -> String {
        format!(
            "{}/{}",
            crate::data::fs::context_dirs::normalise_slug(&self.owner),
            crate::data::fs::context_dirs::normalise_slug(&self.repo)
        )
    }
}

/// Split a remote's path into `owner` and `repo`, dropping a `.git` suffix
/// and any segments beyond the second. Both must be non-empty.
fn split_owner_repo(path: &str) -> Option<(String, String)> {
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.splitn(3, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}
