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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_scp_like_ssh_form() {
        let slug = RemoteSlug::parse("git@github.com:Org/Repo.git").unwrap();
        assert_eq!(slug.host, "github.com");
        assert_eq!(slug.owner, "Org");
        assert_eq!(slug.repo, "Repo");
    }

    #[test]
    fn parses_the_url_form_and_ignores_extra_segments() {
        let slug = RemoteSlug::parse("https://gitlab.example/Org/Repo/extra").unwrap();
        assert_eq!(slug.host, "gitlab.example");
        assert_eq!(slug.owner, "Org");
        assert_eq!(slug.repo, "Repo");
    }

    #[test]
    fn strips_userinfo_from_the_host() {
        let slug = RemoteSlug::parse("ssh://git@github.com/org/repo.git").unwrap();
        assert!(slug.is_host("github.com"));
        assert!(slug.is_host("GitHub.com"), "host match is case-insensitive");
    }

    #[test]
    fn keeps_owner_and_repo_verbatim() {
        let slug = RemoteSlug::parse("https://github.com/My.Org/My_Repo.git").unwrap();
        assert_eq!(slug.owner, "My.Org");
        assert_eq!(slug.repo, "My_Repo");
        assert_eq!(slug.normalised_path(), "my-org/my-repo");
    }

    #[test]
    fn rejects_a_bare_owner_repo_reference() {
        assert_eq!(RemoteSlug::parse("owner/repo"), None);
    }

    #[test]
    fn rejects_a_remote_with_no_repo_segment() {
        assert_eq!(RemoteSlug::parse("https://github.com/org"), None);
        assert_eq!(RemoteSlug::parse("git@github.com:org"), None);
        assert_eq!(RemoteSlug::parse("https://github.com//repo"), None);
    }
}
