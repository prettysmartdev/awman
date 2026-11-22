//! The single repo-directory selection rule for every frontend and lease.
use crate::data::{
    context::ContextKey,
    fs::context_dirs::{parse_owner_repo, ContextDirResolver},
    session::{RepoContextBinding, Session},
};
use std::path::PathBuf;
pub struct RepoContextScope<'a> {
    resolver: &'a ContextDirResolver,
    session: &'a Session,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoContextSource {
    Staged,
    SessionKey(ContextKey),
    OverlayKey(ContextKey),
    Inferred { local_fallback: bool },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRepoContext {
    pub host_dir: PathBuf,
    pub source: RepoContextSource,
    pub ignored_overlay_key: Option<ContextKey>,
}
impl<'a> RepoContextScope<'a> {
    pub fn new(resolver: &'a ContextDirResolver, session: &'a Session) -> Self {
        Self { resolver, session }
    }
    pub fn resolve(
        &self,
        overlay_key: Option<&ContextKey>,
        remote_url: Option<&str>,
    ) -> ResolvedRepoContext {
        let (host_dir, source, ignored_overlay_key) = match self.session.repo_context() {
            RepoContextBinding::Staged(dir) => {
                (dir.clone(), RepoContextSource::Staged, overlay_key.cloned())
            }
            RepoContextBinding::Keyed(key) => (
                self.resolver.keyed_repo_dir(key),
                RepoContextSource::SessionKey(key.clone()),
                overlay_key.filter(|other| *other != key).cloned(),
            ),
            RepoContextBinding::Inferred => match overlay_key {
                Some(key) => (
                    self.resolver.keyed_repo_dir(key),
                    RepoContextSource::OverlayKey(key.clone()),
                    None,
                ),
                None => (
                    self.resolver.repo_dir(remote_url, self.session.git_root()),
                    RepoContextSource::Inferred {
                        local_fallback: remote_url.and_then(parse_owner_repo).is_none(),
                    },
                    None,
                ),
            },
        };
        ResolvedRepoContext {
            host_dir,
            source,
            ignored_overlay_key,
        }
    }
}
