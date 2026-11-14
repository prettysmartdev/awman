//! Deterministic routing of explicit parsed references. Layer 2 selects defaults.
use super::{
    http::{canonical_work_item_url, url_matches_base},
    GithubIssueSource, HttpIssueSource, Issue, IssueSource, IssueSourceError,
};
use crate::data::{
    config::{env::EnvSnapshot, work_items::WorkItemProviderConfig},
    message::UserMessageSink,
    work_item_ref::{GithubRepository, WorkItemRef},
};
use crate::engine::git::GitEngine;
use std::{collections::BTreeMap, path::Path, sync::Arc};

pub struct RoutedIssueSource {
    pub provider: String,
    pub input: String,
    pub source: Arc<dyn IssueSource>,
}
struct HttpEntry {
    validation: HttpIssueSource,
    source: Arc<dyn IssueSource>,
}
pub struct IssueSourceRouter {
    github: Arc<dyn IssueSource>,
    github_origin: GithubIssueSource,
    http: BTreeMap<String, HttpEntry>,
    /// Providers whose definition the transport refuses, with the reason. One
    /// bad entry fails only references that could reach it, never `github:`
    /// or another provider's `http:<name>:<id>`.
    invalid: BTreeMap<String, String>,
}
impl IssueSourceRouter {
    pub fn new(
        git_engine: Arc<GitEngine>,
        env: &EnvSnapshot,
        providers: &BTreeMap<String, WorkItemProviderConfig>,
    ) -> Result<Self, IssueSourceError> {
        let github = Arc::new(GithubIssueSource::new(git_engine.clone(), env.clone()));
        let mut http: BTreeMap<String, Arc<dyn IssueSource>> = BTreeMap::new();
        for (name, config) in providers {
            // An invalid definition is recorded by `with_sources`; it gets a
            // placeholder transport that is never reachable.
            let source: Arc<dyn IssueSource> = match HttpIssueSource::new(name, config, env) {
                Ok(source) => Arc::new(source),
                Err(_) => github.clone(),
            };
            http.insert(name.clone(), source);
        }
        Self::with_sources(git_engine, env, providers, github, http)
    }
    /// Replace transport implementations while retaining production URL policy.
    pub fn with_sources(
        git_engine: Arc<GitEngine>,
        env: &EnvSnapshot,
        providers: &BTreeMap<String, WorkItemProviderConfig>,
        github: Arc<dyn IssueSource>,
        mut http: BTreeMap<String, Arc<dyn IssueSource>>,
    ) -> Result<Self, IssueSourceError> {
        if !providers.keys().eq(http.keys()) {
            return Err(super::transport::error(
                "HTTP",
                "transport registry must contain exactly the configured provider names",
            ));
        }
        let mut entries = BTreeMap::new();
        let mut invalid = BTreeMap::new();
        for (name, config) in providers {
            let source = http.remove(name).expect("registry keys checked above");
            match HttpIssueSource::new(name, config, env) {
                Ok(validation) => {
                    entries.insert(name.clone(), HttpEntry { validation, source });
                }
                Err(error) => {
                    invalid.insert(name.clone(), error.to_string());
                }
            }
        }
        Ok(Self {
            github,
            github_origin: GithubIssueSource::new(git_engine, env.clone()),
            http: entries,
            invalid,
        })
    }
    pub fn github(&self) -> Arc<dyn IssueSource> {
        self.github.clone()
    }
    pub fn github_repository(&self, git_root: &Path) -> Result<GithubRepository, IssueSourceError> {
        self.github_origin.origin_repository(git_root)
    }
    /// The recorded refusal for provider `name`, if its definition is invalid.
    fn refused(&self, name: &str) -> Result<(), IssueSourceError> {
        match self.invalid.get(name) {
            Some(reason) => Err(super::transport::error(&format!("http:{name}"), reason)),
            None => Ok(()),
        }
    }
    pub fn http(&self, name: &str) -> Result<Arc<dyn IssueSource>, IssueSourceError> {
        self.refused(name)?;
        self.http
            .get(name)
            .map(|entry| entry.source.clone())
            .ok_or_else(|| IssueSourceError::NoMatchingProvider {
                input: format!("http:{name}"),
            })
    }
    pub fn route(&self, reference: &WorkItemRef) -> Result<RoutedIssueSource, IssueSourceError> {
        match reference {
            WorkItemRef::Github { repository, number } => {
                if *number == 0 {
                    return Err(super::transport::error(
                        "github",
                        "issue number must be positive",
                    ));
                }
                let input = match repository {
                    Some(repo) => format!("{}/{}#{number}", repo.owner, repo.repo),
                    None => number.to_string(),
                };
                Ok(RoutedIssueSource {
                    provider: "github".into(),
                    input,
                    source: self.github(),
                })
            }
            WorkItemRef::Http { provider, id } => {
                self.refused(provider)?;
                let entry = self.http.get(provider).ok_or_else(|| {
                    IssueSourceError::NoMatchingProvider {
                        input: format!("http:{provider}"),
                    }
                })?;
                Ok(RoutedIssueSource {
                    provider: format!("http:{provider}"),
                    input: entry.validation.url_for_id(id)?.into(),
                    source: entry.source.clone(),
                })
            }
            WorkItemRef::HttpUrl(url) => {
                let input = canonical_work_item_url(url)?;
                // An invalid provider might be the one that owns this URL, so
                // URL routing fails closed while any definition is refused.
                if let Some(name) = self.invalid.keys().next() {
                    self.refused(name)?;
                }
                let mut candidates: Vec<_> = self
                    .http
                    .iter()
                    .filter(|(_, entry)| url_matches_base(entry.validation.base_url(), url))
                    .collect();
                let specificity = |entry: &HttpEntry| {
                    entry
                        .validation
                        .base_url()
                        .path_segments()
                        .into_iter()
                        .flatten()
                        .filter(|part| !part.is_empty())
                        .count()
                };
                candidates.sort_by_key(|(_, entry)| std::cmp::Reverse(specificity(entry)));
                let Some((name, entry)) = candidates.first() else {
                    return Err(IssueSourceError::NoMatchingProvider { input });
                };
                let mut tied: Vec<_> = candidates
                    .iter()
                    .take_while(|(_, other)| specificity(other) == specificity(entry))
                    .map(|(name, _)| format!("http:{name}"))
                    .collect();
                if tied.len() > 1 {
                    tied.sort();
                    return Err(IssueSourceError::AmbiguousProvider { providers: tied });
                }
                Ok(RoutedIssueSource {
                    provider: format!("http:{name}"),
                    input,
                    source: entry.source.clone(),
                })
            }
            _ => Err(IssueSourceError::InvalidRef {
                provider: "work item".into(),
                input: String::new(),
                hint: "Layer 2 must resolve local and bare references before provider routing"
                    .into(),
            }),
        }
    }
    pub fn fetch_issue(
        &self,
        reference: &WorkItemRef,
        git_root: &Path,
    ) -> Result<(Issue, Arc<dyn IssueSource>), IssueSourceError> {
        let routed = self.route(reference)?;
        let issue = routed.source.fetch_issue(&routed.input, git_root)?;
        Ok((issue, routed.source))
    }
    pub fn fetch_issue_with_progress(
        &self,
        reference: &WorkItemRef,
        git_root: &Path,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(Issue, Arc<dyn IssueSource>), IssueSourceError> {
        let routed = self.route(reference)?;
        let issue = routed
            .source
            .fetch_issue_with_progress(&routed.input, git_root, sink)?;
        Ok((issue, routed.source))
    }
}
