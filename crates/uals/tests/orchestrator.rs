//! The orchestrator against fake backends: deadlines, cancellation, panics, statuses, fusion with
//! snippets from disk, paging, and the usages fallback.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pretty_assertions::assert_eq;
use uals::orchestrator::{Backends, Orchestrator, SearchParams, UsagesParams, select_repos};
use uals_core::backend::{BackendError, SearchBackend, UsageSource};
use uals_core::config::Config;
use uals_core::types::{
    BackendAnswer, BackendId, BackendResult, BackendStatus, Hit, IndexState, LineRange, QueryKind, Repo, SearchRequest,
    Snippet, SymbolKind, Usage, UsageAnswer, Workspace,
};

type SearchReply = dyn Fn(&Repo, &SearchRequest) -> Result<BackendAnswer, BackendError> + Send + Sync;
type UsagesReply = dyn Fn(&Repo) -> Result<UsageAnswer, BackendError> + Send + Sync;

/// A search backend that waits `delay`, then answers with `reply`.
struct FakeSearch {
    id: BackendId,
    delay: Duration,
    reply: Box<SearchReply>,
    calls: AtomicUsize,
    /// Set when a call's future is dropped before it finished, that is, when it was cancelled.
    cancelled: Arc<AtomicBool>,
}

impl FakeSearch {
    fn new(
        id: BackendId,
        reply: impl Fn(&Repo, &SearchRequest) -> Result<BackendAnswer, BackendError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            id,
            delay: Duration::ZERO,
            reply: Box::new(reply),
            calls: AtomicUsize::new(0),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn slow(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

struct CancelProbe {
    cancelled: Arc<AtomicBool>,
    finished: bool,
}

impl Drop for CancelProbe {
    fn drop(&mut self) {
        if !self.finished {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl SearchBackend for FakeSearch {
    fn id(&self) -> BackendId {
        self.id
    }

    fn supports(&self, _kind: QueryKind) -> bool {
        true
    }

    async fn search(&self, repo: &Repo, request: &SearchRequest) -> Result<BackendAnswer, BackendError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut probe = CancelProbe { cancelled: Arc::clone(&self.cancelled), finished: false };
        tokio::time::sleep(self.delay).await;
        probe.finished = true;
        (self.reply)(repo, request)
    }

    async fn index_state(&self, repo: &Repo) -> IndexState {
        IndexState {
            backend: self.id,
            repo: repo.name.clone(),
            indexed: true,
            updated_at_ms: None,
            stale: None,
            detail: None,
        }
    }
}

struct FakeUsages {
    id: BackendId,
    reply: Box<UsagesReply>,
}

#[async_trait]
impl UsageSource for FakeUsages {
    fn id(&self) -> BackendId {
        self.id
    }

    async fn usages(&self, repo: &Repo, _symbol: &str, _limit: usize) -> Result<UsageAnswer, BackendError> {
        (self.reply)(repo)
    }
}

fn repo(name: &str, root: &Path) -> Repo {
    Repo { name: name.into(), root: root.to_path_buf() }
}

fn hit(backend: BackendId, repo: &Repo, file: &str, lines: (u32, u32), rank: u32) -> Hit {
    Hit {
        backend,
        repo: repo.name.clone(),
        path: repo.root.join(file),
        lines: LineRange::new(lines.0, lines.1),
        symbol: None,
        symbol_kind: None,
        is_definition: false,
        snippet: None,
        rank,
        score: None,
    }
}

fn definition(backend: BackendId, repo: &Repo, file: &str, lines: (u32, u32), name: &str) -> Hit {
    Hit {
        symbol: Some(name.into()),
        symbol_kind: Some(SymbolKind::Class),
        is_definition: true,
        ..hit(backend, repo, file, lines, 0)
    }
}

fn answer(hits: Vec<Hit>) -> BackendAnswer {
    BackendAnswer { hits, stale: None, partial: None }
}

fn request(query: &str, kind: QueryKind) -> SearchRequest {
    SearchRequest { query: query.into(), kind, fetch_limit: 10, paths: Vec::new() }
}

fn usage(repo: &Repo, file: &str, line: u32) -> Usage {
    Usage { repo: repo.name.clone(), path: repo.root.join(file), line, context: None }
}

fn search_only(search: Vec<Arc<dyn SearchBackend>>, config: Config) -> Orchestrator {
    Orchestrator::new(Backends { search, usages: Vec::new(), spans: None }, config)
}

fn statuses(results: &[BackendResult]) -> Vec<(BackendId, String, BackendStatus)> {
    results.iter().map(|result| (result.backend, result.repo.clone(), result.status.clone())).collect()
}

#[tokio::test]
async fn a_slow_backend_times_out_and_is_cancelled_without_delaying_the_others() {
    let app = repo("app", Path::new("/ws/app"));
    let slow =
        Arc::new(FakeSearch::new(BackendId::AstIndex, |_, _| Ok(answer(Vec::new()))).slow(Duration::from_secs(10)));
    let lexical = Arc::new(FakeSearch::new(BackendId::Lexical, |repo, _| {
        Ok(answer(vec![hit(BackendId::Lexical, repo, "A.kt", (3, 3), 0)]))
    }));
    let mut config = Config::default();
    config.deadlines.ast_index_ms = 100;
    let orchestrator = search_only(vec![slow.clone(), lexical], config);

    let started = Instant::now();
    let results = orchestrator.collect(&[app], &request("HostSelector", QueryKind::Identifier)).await;

    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    assert_eq!(
        statuses(&results),
        vec![
            (BackendId::AstIndex, "app".into(), BackendStatus::Timeout),
            (BackendId::Lexical, "app".into(), BackendStatus::Ok),
        ]
    );
    assert_eq!(results[1].hits.len(), 1);
    assert!(slow.cancelled.load(Ordering::SeqCst), "a timed-out call must be dropped, so its child processes die");
}

#[tokio::test]
async fn a_panicking_backend_is_reported_as_failed_and_the_others_still_answer() {
    let app = repo("app", Path::new("/ws/app"));
    let broken = Arc::new(FakeSearch::new(BackendId::AstIndex, |_, _| panic!("index format changed")));
    let lexical = Arc::new(FakeSearch::new(BackendId::Lexical, |repo, _| {
        Ok(answer(vec![hit(BackendId::Lexical, repo, "A.kt", (3, 3), 0)]))
    }));
    let orchestrator = search_only(vec![broken, lexical], Config::default());

    let results = orchestrator.collect(&[app], &request("HostSelector", QueryKind::Identifier)).await;

    assert_eq!(
        statuses(&results),
        vec![
            (
                BackendId::AstIndex,
                "app".into(),
                BackendStatus::Failed { reason: "panicked: index format changed".into() }
            ),
            (BackendId::Lexical, "app".into(), BackendStatus::Ok),
        ]
    );
}

#[tokio::test]
async fn only_backends_with_a_weight_for_the_query_kind_are_asked() {
    let app = repo("app", Path::new("/ws/app"));
    let ast_index = Arc::new(FakeSearch::new(BackendId::AstIndex, |_, _| Ok(answer(Vec::new()))));
    let lexical = Arc::new(FakeSearch::new(BackendId::Lexical, |_, _| Ok(answer(Vec::new()))));
    let semble = Arc::new(FakeSearch::new(BackendId::Semble, |_, _| Ok(answer(Vec::new()))));
    let orchestrator = search_only(vec![ast_index.clone(), lexical.clone(), semble.clone()], Config::default());

    let results = orchestrator.collect(&[app], &request("class \\w+Store", QueryKind::Regex)).await;

    assert_eq!(statuses(&results), vec![(BackendId::Lexical, "app".into(), BackendStatus::Ok)]);
    let calls = [&ast_index, &lexical, &semble].map(|backend| backend.calls.load(Ordering::SeqCst));
    assert_eq!(calls, [0, 1, 0]);
}

#[tokio::test]
async fn statuses_say_why_hits_are_missing_or_incomplete() {
    let (app, web) = (repo("app", Path::new("/ws/app")), repo("web", Path::new("/ws/web")));
    let ast_index = Arc::new(FakeSearch::new(BackendId::AstIndex, |repo, _| match repo.name.as_str() {
        "app" => Ok(BackendAnswer {
            stale: Some("index is 3 days old".into()),
            ..answer(vec![hit(BackendId::AstIndex, repo, "A.kt", (1, 9), 0)])
        }),
        _ => Err(BackendError::Unavailable("no index".into())),
    }));
    let lexical = Arc::new(FakeSearch::new(BackendId::Lexical, |repo, _| match repo.name.as_str() {
        "app" => Ok(BackendAnswer {
            partial: Some("stopped at the deadline".into()),
            stale: Some("old".into()),
            ..answer(vec![hit(BackendId::Lexical, repo, "A.kt", (3, 3), 0)])
        }),
        _ => Err(BackendError::Failed("unexpected output".into())),
    }));
    let orchestrator = search_only(vec![ast_index, lexical], Config::default());

    let results = orchestrator.collect(&[app, web], &request("HostSelector", QueryKind::Identifier)).await;

    assert_eq!(
        statuses(&results),
        vec![
            (BackendId::AstIndex, "app".into(), BackendStatus::Stale { reason: "index is 3 days old".into() }),
            (
                BackendId::Lexical,
                "app".into(),
                BackendStatus::Partial { reason: "stopped at the deadline; old".into() }
            ),
            (BackendId::AstIndex, "web".into(), BackendStatus::Unavailable { reason: "no index".into() }),
            (BackendId::Lexical, "web".into(), BackendStatus::Failed { reason: "unexpected output".into() }),
        ]
    );
    let hit_counts: Vec<usize> = results.iter().map(|result| result.hits.len()).collect();
    assert_eq!(hit_counts, [1, 1, 0, 0], "stale and partial answers keep their hits");
}

#[tokio::test]
async fn semantic_gets_the_cold_deadline_until_it_has_answered_once() {
    let app = repo("app", Path::new("/ws/app"));
    let semble =
        Arc::new(FakeSearch::new(BackendId::Semble, |_, _| Ok(answer(Vec::new()))).slow(Duration::from_millis(300)));
    let mut config = Config::default();
    config.deadlines.semantic_ms = 50;
    config.deadlines.semantic_cold_ms = 3_000;
    let orchestrator = search_only(vec![semble], config);
    let natural = request("where is the backend host chosen", QueryKind::Natural);

    let first = orchestrator.collect(std::slice::from_ref(&app), &natural).await;
    let second = orchestrator.collect(std::slice::from_ref(&app), &natural).await;

    assert_eq!(first[0].status, BackendStatus::Ok);
    assert_eq!(second[0].status, BackendStatus::Timeout);
}

#[tokio::test]
async fn search_fuses_agreeing_backends_and_reads_the_snippet_from_disk() {
    let folder = tempfile::tempdir().expect("temp dir");
    let source = "package sample\n\nclass HostSelector(\n    private val store: HostStore,\n) {\n    fun pick(): String = store.first()\n}\n";
    std::fs::create_dir_all(folder.path().join("app/src")).expect("create folders");
    std::fs::write(folder.path().join("app/src/HostSelector.kt"), source).expect("write source");
    let app = repo("app", &folder.path().join("app"));
    let file = "src/HostSelector.kt";
    let ast_index = Arc::new(FakeSearch::new(BackendId::AstIndex, move |repo, _| {
        Ok(answer(vec![definition(BackendId::AstIndex, repo, file, (3, 7), "HostSelector")]))
    }));
    let lexical = Arc::new(FakeSearch::new(BackendId::Lexical, move |repo, _| {
        Ok(answer(vec![definition(BackendId::Lexical, repo, file, (3, 3), "HostSelector")]))
    }));
    let semble = Arc::new(FakeSearch::new(BackendId::Semble, move |repo, _| {
        Ok(answer(vec![hit(BackendId::Semble, repo, file, (1, 7), 0)]))
    }));
    let orchestrator = search_only(vec![ast_index, lexical, semble], Config::default());

    let response = orchestrator.search(&[app], &SearchParams::new("HostSelector")).await.expect("search");

    assert_eq!((response.kind, response.total, response.next_offset), (QueryKind::Identifier, 1, None));
    let found = &response.hits[0];
    assert!(found.pinned && found.is_definition, "{found:?}");
    let sources: Vec<BackendId> = found.sources.iter().map(|source| source.backend).collect();
    assert_eq!(sources, [BackendId::AstIndex, BackendId::Lexical, BackendId::Semble]);
    let expected = Snippet { lines: found.lines, text: source.lines().collect::<Vec<_>>()[0..7].join("\n") };
    assert_eq!((found.lines, found.snippet.clone()), (LineRange::new(1, 7), Some(expected)));
    assert_eq!(response.backends.len(), 3);
}

#[tokio::test]
async fn search_pages_and_points_to_the_next_page() {
    let app = repo("app", Path::new("/nonexistent/uals/app"));
    let lexical = Arc::new(FakeSearch::new(BackendId::Lexical, |repo, _| {
        Ok(answer(vec![
            hit(BackendId::Lexical, repo, "A.kt", (3, 3), 0),
            hit(BackendId::Lexical, repo, "B.kt", (5, 5), 1),
            hit(BackendId::Lexical, repo, "C.kt", (7, 7), 2),
        ]))
    }));
    let orchestrator = search_only(vec![lexical], Config::default());
    let mut params = SearchParams::new("/retry\\w+/");
    params.limit = Some(2);

    let first = orchestrator.search(std::slice::from_ref(&app), &params).await.expect("first page");
    params.offset = 2;
    let second = orchestrator.search(std::slice::from_ref(&app), &params).await.expect("second page");

    let files = |hits: &[uals_core::types::FusedHit]| -> Vec<String> {
        hits.iter().map(|hit| hit.path.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect()
    };
    assert_eq!((first.kind, first.total, first.next_offset), (QueryKind::Regex, 3, Some(2)));
    assert_eq!(files(&first.hits), ["A.kt", "B.kt"]);
    assert_eq!((second.total, second.offset, second.next_offset), (3, 2, None));
    assert_eq!(files(&second.hits), ["C.kt"]);
    assert!(first.hits.iter().all(|hit| hit.snippet.is_none()), "files that do not exist give no snippet");
}

#[tokio::test]
async fn an_empty_query_is_rejected() {
    let orchestrator = search_only(Vec::new(), Config::default());
    let error = orchestrator.search(&[], &SearchParams::new("  ")).await.expect_err("an empty query fails");
    assert_eq!(error.to_string(), "the query is empty");
}

#[tokio::test]
async fn usages_fall_back_to_the_next_source_and_page_across_repositories() {
    let (app, web) = (repo("app", Path::new("/ws/app")), repo("web", Path::new("/ws/web")));
    let ast_index = Arc::new(FakeUsages {
        id: BackendId::AstIndex,
        reply: Box::new(|repo| match repo.name.as_str() {
            "app" => Ok(UsageAnswer {
                usages: vec![usage(repo, "A.kt", 9), usage(repo, "A.kt", 3), usage(repo, "A.kt", 3)],
                complete: true,
                stale: None,
            }),
            _ => Err(BackendError::Unavailable("no index".into())),
        }),
    });
    let lexical = Arc::new(FakeUsages {
        id: BackendId::Lexical,
        reply: Box::new(|repo| {
            Ok(UsageAnswer {
                usages: vec![usage(repo, "b.ts", 5), usage(repo, "b.ts", 1)],
                complete: false,
                stale: None,
            })
        }),
    });
    let orchestrator = Orchestrator::new(
        Backends { search: Vec::new(), usages: vec![ast_index, lexical], spans: None },
        Config::default(),
    );
    let repos = [app.clone(), web.clone()];
    let mut params = UsagesParams::new("HostSelector");
    params.limit = Some(2);

    let first = orchestrator.usages(&repos, &params).await.expect("first page");
    params.offset = 2;
    let second = orchestrator.usages(&repos, &params).await.expect("second page");

    let listed = |response: &uals_core::types::UsagesResponse| -> Vec<(String, BackendId, Vec<u32>)> {
        response
            .repos
            .iter()
            .map(|repo| (repo.repo.clone(), repo.source, repo.usages.iter().map(|usage| usage.line).collect()))
            .collect()
    };
    assert_eq!((first.total, first.next_offset, first.complete), (4, Some(2), false));
    assert_eq!(
        listed(&first),
        vec![("app".into(), BackendId::AstIndex, vec![3, 9]), ("web".into(), BackendId::Lexical, vec![])]
    );
    assert_eq!((second.total, second.offset, second.next_offset), (4, 2, None));
    assert_eq!(
        listed(&second),
        vec![("app".into(), BackendId::AstIndex, vec![]), ("web".into(), BackendId::Lexical, vec![1, 5])]
    );
    assert!(first.repos.iter().all(|repo| repo.status == BackendStatus::Ok));
}

#[test]
fn unknown_repository_names_are_rejected_with_the_known_ones() {
    let workspace = Workspace {
        root: "/ws".into(),
        repos: vec![repo("app", Path::new("/ws/app")), repo("web", Path::new("/ws/web"))],
    };

    let picked = select_repos(&workspace, &["web".into()]).expect("known name");
    let error = select_repos(&workspace, &["api".into()]).expect_err("unknown name");

    assert_eq!(picked, vec![repo("web", Path::new("/ws/web"))]);
    assert_eq!(error.to_string(), "unknown repository \"api\"; this workspace has: app, web");
    assert_eq!(select_repos(&workspace, &[]).expect("all"), workspace.repos);
}
