//! The orchestrator against fake backends: deadlines, cancellation, panics, statuses, one call for
//! all repositories, fusion with symbol spans and snippets from disk, paging, the usages fallback,
//! index status, refresh and the background index update.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pretty_assertions::assert_eq;
use uals::orchestrator::{Backends, Orchestrator, SearchParams, UsagesParams, select_repos};
use uals_core::backend::{BackendError, SearchBackend, SpanProvider, UsageSource};
use uals_core::config::Config;
use uals_core::types::{
    BackendAnswer, BackendId, BackendResult, BackendStatus, Hit, IndexState, LineRange, QueryKind, Repo,
    SearchAllAnswer, SearchRequest, Snippet, SymbolKind, SymbolSpan, Usage, UsageAnswer, Workspace,
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

type SearchAllReply = dyn Fn(&[Repo]) -> Result<BackendAnswer, BackendError> + Send + Sync;

/// A search backend that searches all repositories in one call: it waits `delay`, then answers
/// with `reply`, and with `unavailable` next to a successful reply. It records the repositories of
/// every call.
struct FakeSearchAll {
    id: BackendId,
    delay: Duration,
    reply: Box<SearchAllReply>,
    /// Repositories it could not search, with the reason.
    unavailable: BTreeMap<String, String>,
    calls: Mutex<Vec<Vec<String>>>,
}

impl FakeSearchAll {
    fn new(
        id: BackendId,
        reply: impl Fn(&[Repo]) -> Result<BackendAnswer, BackendError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            id,
            delay: Duration::ZERO,
            reply: Box::new(reply),
            unavailable: BTreeMap::new(),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn slow(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Reports `repo` as a repository it could not search, for `reason`.
    fn cannot_search(mut self, repo: &str, reason: &str) -> Self {
        self.unavailable.insert(repo.into(), reason.into());
        self
    }

    /// The repository names of every call so far.
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().expect("call log").clone()
    }
}

#[async_trait]
impl SearchBackend for FakeSearchAll {
    fn id(&self) -> BackendId {
        self.id
    }

    fn supports(&self, _kind: QueryKind) -> bool {
        true
    }

    async fn search(&self, _repo: &Repo, _request: &SearchRequest) -> Result<BackendAnswer, BackendError> {
        Err(BackendError::Failed("asked for one repository".into()))
    }

    fn searches_all_at_once(&self) -> bool {
        true
    }

    async fn search_all(&self, repos: &[Repo], _request: &SearchRequest) -> Result<SearchAllAnswer, BackendError> {
        self.calls.lock().expect("call log").push(repos.iter().map(|repo| repo.name.clone()).collect());
        tokio::time::sleep(self.delay).await;
        let answer = (self.reply)(repos)?;
        Ok(SearchAllAnswer { answer, unavailable: self.unavailable.clone() })
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

/// A backend whose index work is scripted per repository name.
struct FakeIndexer {
    id: BackendId,
    /// Repositories that have an index.
    indexed: Vec<&'static str>,
    /// `index_state` never ends.
    state_hangs: bool,
    /// Repositories whose `refresh` never ends.
    refresh_hangs: Vec<&'static str>,
    /// Repositories whose `refresh` fails.
    refresh_fails: Vec<&'static str>,
    /// Every `refresh` call: the repository name and `full`.
    refreshed: Mutex<Vec<(String, bool)>>,
}

impl FakeIndexer {
    fn new(id: BackendId) -> Self {
        Self {
            id,
            indexed: Vec::new(),
            state_hangs: false,
            refresh_hangs: Vec::new(),
            refresh_fails: Vec::new(),
            refreshed: Mutex::new(Vec::new()),
        }
    }

    fn refreshed(&self) -> Vec<(String, bool)> {
        self.refreshed.lock().expect("refresh log").clone()
    }
}

#[async_trait]
impl SearchBackend for FakeIndexer {
    fn id(&self) -> BackendId {
        self.id
    }

    fn supports(&self, _kind: QueryKind) -> bool {
        true
    }

    async fn search(&self, _repo: &Repo, _request: &SearchRequest) -> Result<BackendAnswer, BackendError> {
        Ok(answer(Vec::new()))
    }

    async fn index_state(&self, repo: &Repo) -> IndexState {
        if self.state_hangs {
            std::future::pending::<()>().await;
        }
        IndexState {
            backend: self.id,
            repo: repo.name.clone(),
            indexed: self.indexed.contains(&repo.name.as_str()),
            updated_at_ms: None,
            stale: None,
            detail: None,
        }
    }

    async fn refresh(&self, repo: &Repo, full: bool) -> Result<(), BackendError> {
        self.refreshed.lock().expect("refresh log").push((repo.name.clone(), full));
        if self.refresh_hangs.contains(&repo.name.as_str()) {
            std::future::pending::<()>().await;
        }
        if self.refresh_fails.contains(&repo.name.as_str()) {
            return Err(BackendError::Failed("disk full".into()));
        }
        Ok(())
    }
}

/// Gives the same spans for every file and counts its calls.
struct FakeSpans {
    spans: Vec<SymbolSpan>,
    calls: AtomicUsize,
}

#[async_trait]
impl SpanProvider for FakeSpans {
    async fn spans(&self, _repo: &Repo, _files: &[PathBuf]) -> Result<Vec<SymbolSpan>, BackendError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.spans.clone())
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
async fn a_backend_that_searches_all_repositories_is_asked_once_and_its_answer_is_split_by_repository() {
    let repos =
        [repo("app", Path::new("/ws/app")), repo("web", Path::new("/ws/web")), repo("tools", Path::new("/ws/tools"))];
    let lexical = Arc::new(FakeSearch::new(BackendId::Lexical, |repo, _| {
        Ok(answer(vec![hit(BackendId::Lexical, repo, "A.kt", (3, 3), 0)]))
    }));
    let semble = Arc::new(FakeSearchAll::new(BackendId::Semble, |repos| {
        let (app, web, ghost) = (&repos[0], &repos[1], repo("ghost", Path::new("/ws/ghost")));
        Ok(answer(vec![
            hit(BackendId::Semble, web, "retry.ts", (1, 9), 0),
            hit(BackendId::Semble, app, "Retry.kt", (1, 9), 1),
            hit(BackendId::Semble, &ghost, "Ghost.kt", (1, 9), 2),
            hit(BackendId::Semble, web, "http.ts", (1, 9), 3),
        ]))
    }));
    let orchestrator = search_only(vec![lexical.clone(), semble.clone()], Config::default());

    let results = orchestrator.collect(&repos, &request("RetryPolicy", QueryKind::Identifier)).await;

    assert_eq!(semble.calls(), vec![vec!["app", "web", "tools"]]);
    assert_eq!(lexical.calls.load(Ordering::SeqCst), 3);
    let ok = |backend, repo: &str| (backend, repo.to_owned(), BackendStatus::Ok);
    assert_eq!(
        statuses(&results),
        vec![
            ok(BackendId::Lexical, "app"),
            ok(BackendId::Semble, "app"),
            ok(BackendId::Lexical, "web"),
            ok(BackendId::Semble, "web"),
            ok(BackendId::Lexical, "tools"),
            ok(BackendId::Semble, "tools"),
        ]
    );
    let semble_hits: Vec<(String, Vec<(PathBuf, u32)>)> = results
        .iter()
        .filter(|result| result.backend == BackendId::Semble)
        .map(|result| (result.repo.clone(), result.hits.iter().map(|hit| (hit.path.clone(), hit.rank)).collect()))
        .collect();
    assert_eq!(
        semble_hits,
        vec![
            ("app".to_owned(), vec![(PathBuf::from("/ws/app/Retry.kt"), 1)]),
            ("web".to_owned(), vec![(PathBuf::from("/ws/web/retry.ts"), 0), (PathBuf::from("/ws/web/http.ts"), 3)]),
            ("tools".to_owned(), vec![]),
        ],
        "ranks stay those over all repositories; the hit of a repository that was not asked is dropped"
    );
}

#[tokio::test]
async fn a_failed_or_late_call_for_all_repositories_is_reported_for_every_repository() {
    let repos = [repo("app", Path::new("/ws/app")), repo("web", Path::new("/ws/web"))];
    let unavailable: Arc<dyn SearchBackend> =
        Arc::new(FakeSearchAll::new(BackendId::Semble, |_| Err(BackendError::Unavailable("no index".into()))));
    let late: Arc<dyn SearchBackend> =
        Arc::new(FakeSearchAll::new(BackendId::Semble, |_| Ok(answer(Vec::new()))).slow(Duration::from_secs(10)));
    let natural = request("where is the backend host chosen", QueryKind::Natural);

    for (backend, status) in
        [(unavailable, BackendStatus::Unavailable { reason: "no index".into() }), (late, BackendStatus::Timeout)]
    {
        let mut config = Config::default();
        config.deadlines.semantic_cold_ms = 100;
        let results = search_only(vec![backend], config).collect(&repos, &natural).await;

        assert_eq!(
            statuses(&results),
            vec![(BackendId::Semble, "app".into(), status.clone()), (BackendId::Semble, "web".into(), status)]
        );
        assert!(results.iter().all(|result| result.hits.is_empty()));
    }
}

#[tokio::test]
async fn a_repository_the_backend_could_not_search_is_unavailable_and_the_others_keep_their_hits() {
    let repos = [repo("app", Path::new("/ws/app")), repo("web", Path::new("/ws/web"))];
    let semble = Arc::new(
        FakeSearchAll::new(BackendId::Semble, |repos| {
            Ok(BackendAnswer {
                stale: Some("index is old".into()),
                ..answer(vec![hit(BackendId::Semble, &repos[0], "Retry.kt", (1, 9), 0)])
            })
        })
        .cannot_search("web", "semble: Failed to index '/ws/web'"),
    );
    let natural = request("where is the backend host chosen", QueryKind::Natural);

    let results = search_only(vec![semble], Config::default()).collect(&repos, &natural).await;

    assert_eq!(
        statuses(&results),
        vec![
            (BackendId::Semble, "app".into(), BackendStatus::Stale { reason: "index is old".into() }),
            (
                BackendId::Semble,
                "web".into(),
                BackendStatus::Unavailable { reason: "semble: Failed to index '/ws/web'".into() }
            ),
        ]
    );
    let hit_counts: Vec<usize> = results.iter().map(|result| result.hits.len()).collect();
    assert_eq!(hit_counts, [1, 0]);
}

#[tokio::test]
async fn the_call_for_all_repositories_is_cold_while_any_of_them_is_cold() {
    let (app, web) = (repo("app", Path::new("/ws/app")), repo("web", Path::new("/ws/web")));
    let semble =
        Arc::new(FakeSearchAll::new(BackendId::Semble, |_| Ok(answer(Vec::new()))).slow(Duration::from_millis(300)));
    let mut config = Config::default();
    config.deadlines.semantic_ms = 50;
    config.deadlines.semantic_cold_ms = 3_000;
    let orchestrator = search_only(vec![semble], config);
    let natural = request("where is the backend host chosen", QueryKind::Natural);

    let app_only = orchestrator.collect(std::slice::from_ref(&app), &natural).await;
    let web_still_cold = orchestrator.collect(&[app.clone(), web.clone()], &natural).await;
    let both_warm = orchestrator.collect(&[app, web], &natural).await;

    assert_eq!(statuses(&app_only), vec![(BackendId::Semble, "app".into(), BackendStatus::Ok)]);
    assert!(web_still_cold.iter().all(|result| result.status == BackendStatus::Ok), "{web_still_cold:?}");
    assert!(both_warm.iter().all(|result| result.status == BackendStatus::Timeout), "{both_warm:?}");
}

#[tokio::test]
async fn the_answer_keeps_the_order_of_a_list_over_all_repositories() {
    let (app, web) = (repo("app", Path::new("/nonexistent/uals/app")), repo("web", Path::new("/nonexistent/uals/web")));
    let semble = Arc::new(FakeSearchAll::new(BackendId::Semble, |repos| {
        Ok(answer(vec![
            hit(BackendId::Semble, &repos[1], "retry.ts", (1, 9), 0),
            hit(BackendId::Semble, &repos[0], "Retry.kt", (1, 9), 1),
        ]))
    }));
    let orchestrator = search_only(vec![semble], Config::default());

    let response = orchestrator
        .search(&[app, web], &SearchParams::new("where is a failed request retried"))
        .await
        .expect("search");

    let repos: Vec<&str> = response.hits.iter().map(|hit| hit.repo.as_str()).collect();
    assert_eq!(repos, ["web", "app"], "with ranks per repository both would be first, and app would win by name");
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
async fn every_page_of_a_query_is_cut_from_the_same_candidate_pool() {
    let app = repo("app", Path::new("/nonexistent/uals/app"));
    let lexical = Arc::new(FakeSearch::new(BackendId::Lexical, |repo, request| {
        let hits = (0..request.fetch_limit)
            .map(|rank| hit(BackendId::Lexical, repo, &format!("F{rank}.kt"), (1, 1), rank as u32))
            .collect();
        Ok(answer(hits))
    }));
    let orchestrator = search_only(vec![lexical], Config::default());
    let mut params = SearchParams::new("/retry\\w+/");
    params.limit = Some(5);

    let first = orchestrator.search(std::slice::from_ref(&app), &params).await.expect("first page");
    params.offset = 5;
    let second = orchestrator.search(std::slice::from_ref(&app), &params).await.expect("second page");

    assert_eq!(first.total, second.total, "both pages come from one pool");
    let shown: Vec<&Path> = first.hits.iter().chain(&second.hits).map(|hit| hit.path.as_path()).collect();
    assert_eq!(shown.iter().collect::<BTreeSet<_>>().len(), 10, "pages repeat hits: {shown:?}");
}

#[tokio::test]
async fn hits_inside_one_symbol_merge_into_one_result_that_spans_it() {
    let app = repo("app", Path::new("/nonexistent/uals/app"));
    let ast_index = Arc::new(FakeSearch::new(BackendId::AstIndex, |repo, _| {
        Ok(answer(vec![hit(BackendId::AstIndex, repo, "A.kt", (3, 3), 0)]))
    }));
    let lexical = Arc::new(FakeSearch::new(BackendId::Lexical, |repo, _| {
        Ok(answer(vec![hit(BackendId::Lexical, repo, "A.kt", (15, 15), 0)]))
    }));
    let span = SymbolSpan {
        repo: "app".into(),
        path: app.root.join("A.kt"),
        name: "HostSelector".into(),
        kind: SymbolKind::Class,
        lines: LineRange::new(1, 20),
    };
    let spans = Arc::new(FakeSpans { spans: vec![span], calls: AtomicUsize::new(0) });
    let backends = Backends { search: vec![ast_index, lexical], usages: Vec::new(), spans: Some(spans.clone()) };
    let orchestrator = Orchestrator::new(backends, Config::default());

    let response = orchestrator.search(&[app], &SearchParams::new("HostSelector")).await.expect("search");

    let found: Vec<(LineRange, usize)> = response.hits.iter().map(|hit| (hit.lines, hit.sources.len())).collect();
    assert_eq!(found, vec![(LineRange::new(1, 20), 2)], "lines 3 and 15 are too far apart to merge without the span");
    assert_eq!(spans.calls.load(Ordering::SeqCst), 1, "one spans call per repository");
}

#[tokio::test(start_paused = true)]
async fn status_reports_a_backend_whose_index_state_does_not_answer_in_time() {
    let app = repo("app", Path::new("/ws/app"));
    let ast_index = Arc::new(FakeIndexer { indexed: vec!["app"], ..FakeIndexer::new(BackendId::AstIndex) });
    let semble = Arc::new(FakeIndexer { state_hangs: true, ..FakeIndexer::new(BackendId::Semble) });
    let orchestrator = search_only(vec![ast_index, semble], Config::default());

    let states = orchestrator.status(&[app]).await;

    let summary: Vec<(BackendId, bool, Option<String>)> =
        states.into_iter().map(|state| (state.backend, state.indexed, state.detail)).collect();
    assert_eq!(
        summary,
        vec![(BackendId::AstIndex, true, None), (BackendId::Semble, false, Some("no answer within 3000 ms".into()))]
    );
}

#[tokio::test(start_paused = true)]
async fn refresh_goes_on_with_the_next_repository_after_one_hangs_or_fails() {
    let (app, web) = (repo("app", Path::new("/ws/app")), repo("web", Path::new("/ws/web")));
    let ast_index = Arc::new(FakeIndexer { refresh_hangs: vec!["app"], ..FakeIndexer::new(BackendId::AstIndex) });
    let lexical = Arc::new(FakeIndexer { refresh_fails: vec!["app"], ..FakeIndexer::new(BackendId::Lexical) });
    let orchestrator = search_only(vec![ast_index.clone(), lexical], Config::default());

    let results = orchestrator.refresh(&[app, web], true).await;

    let outcomes: Vec<(BackendId, String, Result<(), BackendError>)> =
        results.into_iter().map(|result| (result.backend, result.repo, result.result)).collect();
    assert_eq!(
        outcomes,
        vec![
            (BackendId::AstIndex, "app".into(), Err(BackendError::Failed("no answer within 3600000 ms".into()))),
            (BackendId::AstIndex, "web".into(), Ok(())),
            (BackendId::Lexical, "app".into(), Err(BackendError::Failed("disk full".into()))),
            (BackendId::Lexical, "web".into(), Ok(())),
        ]
    );
    assert_eq!(ast_index.refreshed(), vec![("app".to_string(), true), ("web".to_string(), true)]);
}

#[tokio::test]
async fn only_existing_ast_indexes_are_updated_in_the_background() {
    let (app, web) = (repo("app", Path::new("/ws/app")), repo("web", Path::new("/ws/web")));
    let ast_index = Arc::new(FakeIndexer { indexed: vec!["app"], ..FakeIndexer::new(BackendId::AstIndex) });
    let orchestrator = search_only(vec![ast_index.clone()], Config::default());
    let without_ast_index = search_only(vec![Arc::new(FakeIndexer::new(BackendId::Lexical))], Config::default());

    let update = orchestrator.update_existing_ast_indexes_in_background(&[app.clone(), web]);
    update.expect("an ast-index backend is configured").await.expect("the update task ends");

    assert_eq!(ast_index.refreshed(), vec![("app".to_string(), false)]);
    assert!(without_ast_index.update_existing_ast_indexes_in_background(&[app]).is_none());
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
    let notes: Vec<(BackendStatus, Option<String>)> =
        first.repos.iter().map(|repo| (repo.status.clone(), repo.fallback.clone())).collect();
    assert_eq!(
        notes,
        vec![(BackendStatus::Ok, None), (BackendStatus::Ok, Some("ast-index unavailable (no index)".into()))],
        "a fallback answer says why the first source did not answer"
    );
}

#[tokio::test]
async fn when_no_usage_source_answers_the_repository_says_how_each_one_failed() {
    let app = repo("app", Path::new("/ws/app"));
    let ast_index = Arc::new(FakeUsages {
        id: BackendId::AstIndex,
        reply: Box::new(|_| Err(BackendError::Unavailable("no index".into()))),
    });
    let lexical = Arc::new(FakeUsages {
        id: BackendId::Lexical,
        reply: Box::new(|_| Err(BackendError::Failed("walk aborted".into()))),
    });
    let orchestrator = Orchestrator::new(
        Backends { search: Vec::new(), usages: vec![ast_index, lexical], spans: None },
        Config::default(),
    );
    let unconfigured = Orchestrator::new(Backends::default(), Config::default());

    let response = orchestrator.usages(std::slice::from_ref(&app), &UsagesParams::new("HostSelector")).await;
    let error = unconfigured.usages(&[app], &UsagesParams::new("HostSelector")).await.expect_err("no usage source");

    let response = response.expect("usages");
    let found = &response.repos[0];
    assert_eq!(
        (found.source, found.status.clone(), found.fallback.clone(), response.complete),
        (
            BackendId::Lexical,
            BackendStatus::Failed { reason: "walk aborted".into() },
            Some("ast-index unavailable (no index)".into()),
            false
        )
    );
    assert_eq!(error.to_string(), "no usage source is configured");
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
