//! Work item resolution (WI 0147): every reference form against the local,
//! GitHub and `http:<name>` sources, through the production resolver and
//! router with spy transports. No network: the spies record what the
//! router hands them, and the router keeps its production URL policy.
use crate::support;
use awman::command::work_item::{
    prepare_work_item_input, ResolvedWorkItem, WorkItemCreationTarget, WorkItemError,
    WorkItemOrigin, WorkItemResolver,
};
use awman::data::config::work_items::{
    WorkItemConfigError, WorkItemProviderConfig, WorkItemSource,
};
use awman::data::fs::work_item_files::LocalWorkItemFileError;
use awman::data::message::{UserMessage, UserMessageSink};
use awman::data::session::{Session, SessionOpenOptions};
use awman::data::work_item::StagedInlineWorkItem;
use awman::data::work_item_ref::WorkItemRefError;
use awman::engine::git::GitEngine;
use awman::engine::issue::{Issue, IssueSource, IssueSourceError, IssueSourceRouter};
use awman_test_support::fixtures::{ConfigTree, RepoBuilder};
use awman_test_support::TestSandbox;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const BASE: &str = "https://tracker.example/api/items";

/// Records every input the router hands it; never touches the network.
struct Spy {
    provider: &'static str,
    calls: Mutex<Vec<String>>,
}

impl Spy {
    fn new(provider: &'static str) -> Arc<Self> {
        Arc::new(Self {
            provider,
            calls: Mutex::new(Vec::new()),
        })
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl IssueSource for Spy {
    fn provider_name(&self) -> &str {
        self.provider
    }
    fn provider_prefix(&self) -> &str {
        ""
    }
    fn issue_identifier(&self, issue: &Issue) -> String {
        issue.id.clone()
    }
    fn fetch_issue(&self, input: &str, _git_root: &Path) -> Result<Issue, IssueSourceError> {
        self.calls.lock().unwrap().push(input.to_string());
        if self.provider == "github" {
            // `N` alone means origin; the real provider resolves it.
            let (repo, number) = match input.split_once('#') {
                Some((repo, n)) => (repo.to_string(), n.to_string()),
                None => ("Acme/Widgets".to_string(), input.to_string()),
            };
            Ok(Issue {
                source_id: format!("https://github.com/{repo}/issues/{number}"),
                id: number.clone(),
                number: number.parse().ok(),
                title: format!("GitHub issue {number}\nwith a second line"),
                body: "remote body".into(),
                provider: "github".into(),
            })
        } else {
            let id = input.rsplit('/').next().unwrap_or_default().to_string();
            Ok(Issue {
                source_id: input.to_string(),
                id: id.clone(),
                number: None,
                title: format!("Tracker {id}"),
                body: "tracker body".into(),
                provider: "http:tracker".into(),
            })
        }
    }
}

#[derive(Default)]
struct Sink(Vec<UserMessage>);
impl UserMessageSink for Sink {
    fn write_message(&mut self, msg: UserMessage) {
        self.0.push(msg);
    }
    fn replay_queued(&mut self) {}
}

struct World {
    repo: PathBuf,
    session: Session,
    git: Arc<GitEngine>,
    github: Arc<Spy>,
    http: Arc<Spy>,
    router: Arc<IssueSourceRouter>,
}

impl World {
    fn resolver(&self) -> WorkItemResolver {
        WorkItemResolver::with_router(
            self.session.clone(),
            Arc::clone(&self.git),
            Arc::clone(&self.router),
        )
    }
    fn calls(&self) -> (Vec<String>, Vec<String>) {
        (self.github.calls(), self.http.calls())
    }
    async fn resolve(&self, reference: &str) -> Result<ResolvedWorkItem, WorkItemError> {
        let mut sink = Sink::default();
        self.resolver()
            .resolve(Some(reference), &mut sink)
            .await
            .map(|item| item.expect("a reference resolves to an item"))
    }
}

fn providers() -> BTreeMap<String, WorkItemProviderConfig> {
    BTreeMap::from([(
        "tracker".to_string(),
        WorkItemProviderConfig {
            base_url: BASE.into(),
            token_env: "TRACKER_TOKEN".into(),
        },
    )])
}

/// A checkout with numbered and unnumbered work items, `origin` on GitHub
/// (unless `origin` is false), the given repo `source` and the `tracker`
/// provider in the global config.
fn world(sandbox: &TestSandbox, source: Option<&str>, origin: bool) -> World {
    let repo = RepoBuilder::new(sandbox, "repo")
        .file(
            "aspec/work-items/0042-the-answer.md",
            "# The answer\n\nlocal 42\n",
        )
        .file("aspec/work-items/0007-seven.md", "# Seven\n\nlocal 7\n")
        .file("docs/notes.md", "no heading here\n")
        .init();
    if origin {
        repo.git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/Acme/Widgets.git",
        ]);
    }
    let root = repo.path().to_path_buf();
    let mut tree = ConfigTree::new().global(json!({
        "workItems": { "providers": { "tracker": { "baseUrl": BASE, "tokenEnv": "TRACKER_TOKEN" } } }
    }));
    if let Some(source) = source {
        tree = tree.repo(json!({ "workItems": { "source": source } }));
    }
    tree.write(sandbox, Some(&root));
    let env = support::env_snapshot(sandbox);
    let session = Session::open_at_git_root(
        root.clone(),
        root.clone(),
        SessionOpenOptions {
            env: Some(env.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let git = Arc::new(GitEngine::with_env(env.clone()));
    let github = Spy::new("github");
    let http = Spy::new("http:tracker");
    let router = IssueSourceRouter::with_sources(
        Arc::clone(&git),
        &env,
        &providers(),
        github.clone(),
        BTreeMap::from([("tracker".to_string(), http.clone() as Arc<dyn IssueSource>)]),
    )
    .unwrap();
    World {
        repo: root,
        session,
        git,
        github,
        http,
        router: Arc::new(router),
    }
}

#[derive(Debug, PartialEq)]
enum Expect {
    /// A local file (repo-relative), with no provider call.
    Local(&'static str),
    /// One GitHub call with this input.
    Github(&'static str),
    /// One HTTP call with this URL.
    Http(&'static str),
}

async fn check(w: &World, reference: &str, expect: Expect) -> ResolvedWorkItem {
    let before = w.calls();
    let item = w
        .resolve(reference)
        .await
        .unwrap_or_else(|e| panic!("{reference}: {e}"));
    let (github, http) = w.calls();
    let new_github = &github[before.0.len()..];
    let new_http = &http[before.1.len()..];
    match expect {
        Expect::Local(relative) => {
            assert!(new_github.is_empty() && new_http.is_empty(), "{reference}");
            let WorkItemOrigin::Local { path } = &item.origin else {
                panic!("{reference}: expected a local item, got {item:?}")
            };
            let root = std::fs::canonicalize(&w.repo).unwrap();
            assert_eq!(path, &root.join(relative), "{reference}");
        }
        Expect::Github(input) => {
            assert_eq!(new_github, [input.to_string()], "{reference}");
            assert!(new_http.is_empty(), "{reference}");
        }
        Expect::Http(url) => {
            assert_eq!(new_http, [url.to_string()], "{reference}");
            assert!(new_github.is_empty(), "{reference}");
        }
    }
    item
}

#[tokio::test(flavor = "multi_thread")]
async fn every_reference_form_routes_to_one_provider_under_every_source() {
    const ID: &str =
        "command::work_item_source::every_reference_form_routes_to_one_provider_under_every_source";
    for source in [None, Some("local"), Some("github"), Some("http:tracker")] {
        let sandbox = support::sandbox(ID);
        let w = world(&sandbox, source, true);
        // Explicit forms ignore workItems.source.
        check(
            &w,
            "https://github.com/acme/widgets/issues/7",
            Expect::Github("acme/widgets#7"),
        )
        .await;
        check(
            &w,
            &format!("{BASE}/PROJ-9"),
            Expect::Http("https://tracker.example/api/items/PROJ-9"),
        )
        .await;
        check(&w, "other/repo#5", Expect::Github("other/repo#5")).await;
        check(&w, "github:7", Expect::Github("7")).await;
        check(&w, "#7", Expect::Github("7")).await;
        check(
            &w,
            "local:42",
            Expect::Local("aspec/work-items/0042-the-answer.md"),
        )
        .await;
        check(
            &w,
            "http:tracker:PROJ 1/a",
            Expect::Http("https://tracker.example/api/items/PROJ%201%2Fa"),
        )
        .await;
        check(&w, "docs/notes.md", Expect::Local("docs/notes.md")).await;
        // Bare values follow the configured source, and only that source.
        match source {
            None | Some("local") => {
                let item = check(
                    &w,
                    "42",
                    Expect::Local("aspec/work-items/0042-the-answer.md"),
                )
                .await;
                assert_eq!(item.title, "The answer");
                check(
                    &w,
                    "WI-42",
                    Expect::Local("aspec/work-items/0042-the-answer.md"),
                )
                .await;
                check(
                    &w,
                    "0042",
                    Expect::Local("aspec/work-items/0042-the-answer.md"),
                )
                .await;
            }
            Some("github") => {
                check(&w, "42", Expect::Github("Acme/Widgets#42")).await;
                check(&w, "0042", Expect::Github("Acme/Widgets#42")).await;
                let before = w.calls();
                let err = w.resolve("WI-42").await.unwrap_err();
                assert!(
                    matches!(
                        err,
                        WorkItemError::Reference(WorkItemRefError::InvalidNumber { .. })
                    ),
                    "{err}"
                );
                assert_eq!(w.calls(), before, "an invalid bare value calls no provider");
            }
            Some("http:tracker") => {
                check(
                    &w,
                    "42",
                    Expect::Http("https://tracker.example/api/items/42"),
                )
                .await;
                check(
                    &w,
                    "PROJ-123",
                    Expect::Http("https://tracker.example/api/items/PROJ-123"),
                )
                .await;
            }
            _ => unreachable!(),
        }
        sandbox.finish();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_local_number_never_falls_back_and_suggests_github() {
    let s = support::sandbox(
        "command::work_item_source::a_missing_local_number_never_falls_back_and_suggests_github",
    );
    let w = world(&s, None, true);
    let err = w.resolve("99").await.unwrap_err();
    assert!(
        matches!(err, WorkItemError::LocalNotFound { number: 99 }),
        "{err}"
    );
    assert!(err.to_string().contains("--work-item github:99"), "{err}");
    assert_eq!(err.exit_code(), 4);
    assert_eq!(w.calls(), (vec![], vec![]));
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_source_is_a_hard_error_even_for_explicit_references() {
    let s = support::sandbox(
        "command::work_item_source::an_invalid_source_is_a_hard_error_even_for_explicit_references",
    );
    let w = world(&s, Some("jira"), true);
    for reference in ["42", "local:42", "github:7"] {
        let err = w.resolve(reference).await.unwrap_err();
        assert!(
            matches!(
                err,
                WorkItemError::Config(WorkItemConfigError::UnknownSource { .. })
            ),
            "{reference}: {err}"
        );
        assert_eq!(err.exit_code(), 2);
    }
    assert_eq!(w.calls(), (vec![], vec![]));
    s.finish();

    let s = support::sandbox(
        "command::work_item_source::an_invalid_source_is_a_hard_error_even_for_explicit_references",
    );
    let w = world(&s, Some("http:missing"), true);
    let err = w.resolve("42").await.unwrap_err();
    assert!(
        matches!(
            err,
            WorkItemError::Config(WorkItemConfigError::UnknownProvider { .. })
        ),
        "{err}"
    );
    assert!(w.resolver().creation_target().is_err());
    assert_eq!(w.calls(), (vec![], vec![]));
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_github_source_without_origin_names_the_setting() {
    let s = support::sandbox(
        "command::work_item_source::the_github_source_without_origin_names_the_setting",
    );
    let w = world(&s, Some("github"), false);
    let err = w.resolve("42").await.unwrap_err();
    assert!(
        matches!(err, WorkItemError::NoRemoteDetected { .. }),
        "{err}"
    );
    assert!(err.to_string().contains("workItems.source"), "{err}");
    let err = w.resolver().creation_target().unwrap_err();
    assert!(
        matches!(err, WorkItemError::NoRemoteDetected { .. }),
        "{err}"
    );
    assert_eq!(w.calls(), (vec![], vec![]));
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn ambiguous_numbers_list_every_matching_file() {
    let s =
        support::sandbox("command::work_item_source::ambiguous_numbers_list_every_matching_file");
    let w = world(&s, None, true);
    let dir = w.repo.join("aspec/work-items");
    std::fs::write(dir.join("0042-a-duplicate.md"), "# Duplicate\n").unwrap();
    let err = w.resolve("42").await.unwrap_err();
    let WorkItemError::AmbiguousWorkItem { number, paths } = &err else {
        panic!("expected ambiguity, got {err}")
    };
    assert_eq!(*number, 42);
    assert_eq!(paths.len(), 2);
    let text = err.to_string();
    assert!(
        text.contains("0042-a-duplicate.md") && text.contains("0042-the-answer.md"),
        "{text}"
    );
    assert_eq!(err.exit_code(), 2);
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_paths_cannot_escape_the_checkout() {
    let s =
        support::sandbox("command::work_item_source::explicit_paths_cannot_escape_the_checkout");
    let w = world(&s, None, true);
    let outside = w.repo.parent().unwrap().join("outside.md");
    std::fs::write(&outside, "# Secret\n").unwrap();
    let err = w.resolve("../outside.md").await.unwrap_err();
    assert!(
        matches!(
            err,
            WorkItemError::Files(LocalWorkItemFileError::OutsideCheckout { .. })
        ),
        "{err}"
    );
    let err = w.resolve(&outside.display().to_string()).await.unwrap_err();
    assert!(
        matches!(
            err,
            WorkItemError::Files(LocalWorkItemFileError::OutsideCheckout { .. })
        ),
        "{err}"
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, w.repo.join("docs/link.md")).unwrap();
        let err = w.resolve("docs/link.md").await.unwrap_err();
        assert!(
            matches!(
                err,
                WorkItemError::Files(LocalWorkItemFileError::OutsideCheckout { .. })
            ),
            "{err}"
        );
    }
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_separate_github_seven_from_file_seven_and_merge_aliases() {
    let s = support::sandbox(
        "command::work_item_source::keys_separate_github_seven_from_file_seven_and_merge_aliases",
    );
    let w = world(&s, None, true);
    let file = w.resolve("7").await.unwrap();
    let issue = w.resolve("github:7").await.unwrap();
    let url = w
        .resolve("https://github.com/acme/widgets/issues/7")
        .await
        .unwrap();
    assert_eq!(file.key, "0007");
    assert_eq!(file.number, Some(7));
    assert!(issue.key.starts_with("remote-"), "{}", issue.key);
    assert_ne!(file.key, issue.key);
    assert_eq!(
        issue.key, url.key,
        "an origin ref and its URL are one identity"
    );
    assert_eq!(
        issue.origin,
        WorkItemOrigin::Remote {
            source_id: "https://github.com/acme/widgets/issues/7".into(),
            provider: "github".into(),
        }
    );
    let notes = w.resolve("docs/notes.md").await.unwrap();
    assert_eq!(notes.number, None);
    assert!(notes.key.starts_with("local-"), "{}", notes.key);
    assert_eq!(notes.title, "notes");
    let tracker = w.resolve("http:tracker:PROJ-123").await.unwrap();
    assert_eq!(
        tracker.number, None,
        "no number is read out of a non-numeric id"
    );
    assert!(tracker.key.starts_with("remote-"));
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn resolution_reports_source_and_a_single_line_title_before_launch() {
    let s = support::sandbox(
        "command::work_item_source::resolution_reports_source_and_a_single_line_title_before_launch",
    );
    let w = world(&s, None, true);
    let mut sink = Sink::default();
    let item = w
        .resolver()
        .resolve(
            Some("https://github.com/acme/widgets/issues/7?token=x#frag"),
            &mut sink,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(item.title, "GitHub issue 7 with a second line");
    assert!(item.slug.as_deref().is_some_and(|slug| slug
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')));
    let notice = &sink.0.last().unwrap().text;
    assert!(
        notice.contains("https://github.com/acme/widgets/issues/7"),
        "{notice}"
    );
    assert!(notice.contains(&item.title), "{notice}");
    assert!(!notice.contains("token"), "{notice}");
    // No reference and no inline input: nothing resolves and nothing is read.
    let mut sink = Sink::default();
    assert!(w
        .resolver()
        .resolve(None, &mut sink)
        .await
        .unwrap()
        .is_none());
    assert!(sink.0.is_empty());
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn inline_input_is_source_independent_and_exclusive_with_references() {
    let s = support::sandbox(
        "command::work_item_source::inline_input_is_source_independent_and_exclusive_with_references",
    );
    let w = world(&s, Some("github"), true);
    let body = s.tmp().join("inline-body.md");
    std::fs::write(&body, "inline body").unwrap();
    let inline = StagedInlineWorkItem {
        source_id: "inline:push:session:cmd".into(),
        title: "Pushed item".into(),
        body_path: body,
    };
    let mut sink = Sink::default();
    let item = w
        .resolver()
        .with_inline(inline.clone())
        .resolve(None, &mut sink)
        .await
        .unwrap()
        .unwrap();
    assert!(item.is_inline());
    assert_eq!(item.content, "# Pushed item\n\ninline body");
    assert!(matches!(
        w.resolver().with_inline(inline.clone()).source_for(&item),
        Err(WorkItemError::InlineNotEditable)
    ));
    let err = w
        .resolver()
        .with_inline(inline)
        .resolve(Some("42"), &mut sink)
        .await
        .unwrap_err();
    assert!(
        matches!(err, WorkItemError::InlineReferenceConflict),
        "{err}"
    );
    assert_eq!(w.calls(), (vec![], vec![]));
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn session_inline_input_keeps_the_caller_title_and_id_and_lease_bodies_arrive_once() {
    let s = support::sandbox(
        "command::work_item_source::session_inline_input_keeps_the_caller_title_and_id_and_lease_bodies_arrive_once",
    );
    let w = world(&s, Some("github"), true);
    let body = s.tmp().join("inline-body.md");
    std::fs::write(&body, "Use PKCE").unwrap();
    // A pushed API work item rides on the (per-command) session: the worker
    // attaches the staged item, the resolver must see its title and id.
    let resolve_with = |staged: StagedInlineWorkItem| {
        let mut session = w.session.clone();
        session.set_staged_work_item(staged);
        WorkItemResolver::with_router(session, Arc::clone(&w.git), Arc::clone(&w.router))
    };
    let staged = StagedInlineWorkItem {
        source_id: "inline:push:session:PROJ-9".into(),
        title: "Fix login".into(),
        body_path: body.clone(),
    };
    let mut sink = Sink::default();
    let first = resolve_with(staged.clone())
        .resolve(None, &mut sink)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.title, "Fix login");
    assert_eq!(first.content, "# Fix login\n\nUse PKCE");
    assert_eq!(first.source_id(&w.repo), "inline:push:session:PROJ-9");
    // The same caller id on a later command, staged at another path, keeps
    // the same identity (dynamic resume).
    let other = s.tmp().join("other-body.md");
    std::fs::write(&other, "Use PKCE").unwrap();
    let again = resolve_with(StagedInlineWorkItem {
        body_path: other,
        ..staged
    })
    .resolve(None, &mut sink)
    .await
    .unwrap()
    .unwrap();
    assert_eq!(again.key, first.key);
    // A worktree session opened for the command keeps the typed input.
    let mut session = w.session.clone();
    session.set_staged_work_item(StagedInlineWorkItem {
        source_id: "inline:push:session:PROJ-9".into(),
        title: "Fix login".into(),
        body_path: body.clone(),
    });
    let worktree =
        Session::open_worktree_with_invoker_config(w.repo.clone(), w.repo.clone(), &session)
            .unwrap();
    assert_eq!(worktree.staged_work_item().unwrap().title, "Fix login");

    // A pulled lease body is path-only input. It is the whole prompt, once.
    let mut session = w.session.clone();
    session.set_inline_work_item(body);
    let lease = WorkItemResolver::with_router(session, Arc::clone(&w.git), Arc::clone(&w.router))
        .resolve(None, &mut sink)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lease.content, "Use PKCE");
    assert_eq!(w.calls(), (vec![], vec![]));
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn seeding_refuses_items_already_in_the_creation_target() {
    let s = support::sandbox(
        "command::work_item_source::seeding_refuses_items_already_in_the_creation_target",
    );
    let w = world(&s, None, true);
    let resolver = w.resolver();
    let target = resolver.creation_target().unwrap();
    assert!(matches!(target, WorkItemCreationTarget::Local { .. }));
    let existing = w.resolve("42").await.unwrap();
    let err = resolver
        .ensure_new_in_target(&existing, &target)
        .unwrap_err();
    assert!(matches!(err, WorkItemError::AlreadyExists { .. }), "{err}");
    assert!(err.to_string().contains("specs amend"), "{err}");
    let elsewhere = w.resolve("docs/notes.md").await.unwrap();
    resolver.ensure_new_in_target(&elsewhere, &target).unwrap();
    let issue = w.resolve("github:7").await.unwrap();
    resolver.ensure_new_in_target(&issue, &target).unwrap();
    s.finish();

    let s = support::sandbox(
        "command::work_item_source::seeding_refuses_items_already_in_the_creation_target",
    );
    let w = world(&s, Some("github"), true);
    let resolver = w.resolver();
    let target = resolver.creation_target().unwrap();
    let WorkItemCreationTarget::Remote {
        provider, scope, ..
    } = &target
    else {
        panic!("expected a remote target")
    };
    assert_eq!(
        (provider.as_str(), scope.as_str()),
        ("github", "github:acme/widgets")
    );
    let same = w.resolve("github:7").await.unwrap();
    assert!(matches!(
        resolver.ensure_new_in_target(&same, &target),
        Err(WorkItemError::AlreadyExists { .. })
    ));
    let foreign = w.resolve("other/repo#5").await.unwrap();
    resolver.ensure_new_in_target(&foreign, &target).unwrap();
    let local = w.resolve("local:42").await.unwrap();
    resolver.ensure_new_in_target(&local, &target).unwrap();
    s.finish();

    let s = support::sandbox(
        "command::work_item_source::seeding_refuses_items_already_in_the_creation_target",
    );
    let w = world(&s, Some("http:tracker"), true);
    let resolver = w.resolver();
    let target = resolver.creation_target().unwrap();
    let same = w.resolve("PROJ-1").await.unwrap();
    assert!(matches!(
        resolver.ensure_new_in_target(&same, &target),
        Err(WorkItemError::AlreadyExists { .. })
    ));
    let github = w.resolve("github:7").await.unwrap();
    resolver.ensure_new_in_target(&github, &target).unwrap();
    assert_eq!(w.calls().0.len(), 1);
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_input_is_staged_owner_only_with_an_unpredictable_name() {
    let s = support::sandbox(
        "command::work_item_source::remote_input_is_staged_owner_only_with_an_unpredictable_name",
    );
    let w = world(&s, None, true);
    let item = w.resolve("github:7").await.unwrap();
    let first = prepare_work_item_input(&item, &w.session).unwrap();
    let second = prepare_work_item_input(&item, &w.session).unwrap();
    assert_eq!(
        first.container_path,
        PathBuf::from(format!("/awman/work-items/{}.md", item.key))
    );
    assert!(first.overlay.is_some());
    let a = first.private_file.as_ref().unwrap().path().to_path_buf();
    let b = second.private_file.as_ref().unwrap().path().to_path_buf();
    assert_ne!(a.parent(), b.parent(), "each staging directory is unique");
    assert!(!a.to_string_lossy().contains(&item.key));
    assert_eq!(std::fs::read_to_string(&a).unwrap(), item.content);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&a), 0o600);
        // The staging root is owner-only, so nothing below it is reachable
        // by other users whatever mode the per-item directory has.
        assert_eq!(mode(a.parent().unwrap().parent().unwrap()), 0o700);
    }
    drop(first);
    assert!(!a.exists(), "dropping the guard deletes the staged file");
    // Local files are used in place: no staging and no extra mount.
    let local = w.resolve("local:42").await.unwrap();
    let prepared = prepare_work_item_input(&local, &w.session).unwrap();
    assert_eq!(
        prepared.container_path,
        PathBuf::from("/workspace/aspec/work-items/0042-the-answer.md")
    );
    assert!(prepared.overlay.is_none() && prepared.private_file.is_none());
    drop(second);
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn http_seed_duplicates_are_judged_by_the_configured_base() {
    let s = support::sandbox(
        "command::work_item_source::http_seed_duplicates_are_judged_by_the_configured_base",
    );
    let repo = RepoBuilder::new(&s, "repo")
        .file("README.md", "fixture")
        .init();
    let root = repo.path().to_path_buf();
    let archive = format!("{BASE}/archive");
    ConfigTree::new()
        .global(json!({ "workItems": { "providers": {
            "main": { "baseUrl": BASE, "tokenEnv": "TRACKER_TOKEN" },
            "alias": { "baseUrl": format!("{BASE}/"), "tokenEnv": "TRACKER_TOKEN" },
            "archive": { "baseUrl": archive, "tokenEnv": "TRACKER_TOKEN" },
        } } }))
        .repo(json!({ "workItems": { "source": "http:main" } }))
        .write(&s, Some(&root));
    let env = support::env_snapshot(&s);
    let session = Session::open_at_git_root(
        root.clone(),
        root,
        SessionOpenOptions {
            env: Some(env.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let resolver = WorkItemResolver::new(session, Arc::new(GitEngine::with_env(env)));
    let target = resolver.creation_target().unwrap();
    let seed = |provider: &str, source_id: String| ResolvedWorkItem {
        origin: WorkItemOrigin::Remote {
            source_id,
            provider: provider.into(),
        },
        number: None,
        key: "remote-0123456789abcdef".into(),
        title: "Seed".into(),
        slug: None,
        content: "# Seed".into(),
        issue: None,
    };
    // A distinct provider nested under the target base is a different scope.
    resolver
        .ensure_new_in_target(&seed("http:archive", format!("{archive}/A")), &target)
        .unwrap();
    // The target itself, and an alias of the same base, are duplicates.
    for provider in ["http:main", "http:alias"] {
        assert!(
            matches!(
                resolver.ensure_new_in_target(&seed(provider, format!("{BASE}/A")), &target),
                Err(WorkItemError::AlreadyExists { .. })
            ),
            "{provider}"
        );
    }
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn workflow_worktree_sessions_keep_the_invokers_work_item_settings() {
    let s = support::sandbox(
        "command::work_item_source::workflow_worktree_sessions_keep_the_invokers_work_item_settings",
    );
    let w = world(&s, Some("github"), true);
    // A branch checkout whose own repo config would retarget work items.
    let branch = RepoBuilder::new(&s, "branch")
        .file(
            ".awman/config.json",
            r#"{ "workItems": { "source": "local", "dir": "elsewhere" } }"#,
        )
        .init();
    let root = branch.path().to_path_buf();
    let worktree = Session::open_worktree_for(root.clone(), root.clone(), &w.session).unwrap();
    let config = worktree.effective_config();
    assert!(matches!(
        config.work_item_source().unwrap(),
        WorkItemSource::Github
    ));
    assert_eq!(
        config.work_items_dir(&root),
        w.session.effective_config().work_items_dir(&root)
    );
    s.finish();
}
