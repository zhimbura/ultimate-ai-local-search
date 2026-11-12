//! Runs one query against every backend and every repository at once and fuses the answers.
//!
//! `search`: classify the query → ask every (repository, backend) pair in parallel, each call on
//! its own task under its backend's deadline → fetch the symbol spans of the files the hits point
//! to → fuse → cut one page → read the page's snippets from disk. A backend that fails, panics or
//! misses its deadline loses only its own hits; `SearchResponse::backends` says what happened to
//! every pair.
//!
//! `usages`: per repository, the first usage source that answers (ast-index, then lexical).

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use futures::FutureExt;
use futures::future::{BoxFuture, join_all};
use tokio::task::JoinSet;
use tracing::{debug, warn};
use uals_core::backend::{BackendError, SearchBackend, SpanProvider, UsageSource};
use uals_core::classify::classify;
use uals_core::config::Config;
use uals_core::fuse::fuse;
use uals_core::render::{self, Format};
use uals_core::types::{
    BackendAnswer, BackendId, BackendReport, BackendResult, BackendStatus, Hit, IndexState, QueryKind, Repo,
    RepoUsages, SearchRequest, SearchResponse, SymbolSpan, UsagesResponse, Workspace,
};

use crate::snippets;

/// No backend is asked for more hits than this per repository, whatever the page offset.
const MAX_FETCH_LIMIT: usize = 500;
/// Page size of `usages` when the caller gives none, and the largest page it accepts.
pub const DEFAULT_USAGES_LIMIT: usize = 100;
pub const MAX_USAGES_LIMIT: usize = 1_000;
/// Usage sources are asked for at least this many usages per repository, so `total` is exact
/// for all but the most used symbols.
const USAGES_SCAN_LIMIT: usize = 5_000;
/// Spans only improve merging and come after the search calls, so they get a short deadline.
const SPANS_DEADLINE: Duration = Duration::from_millis(500);
/// `index_state` must be fast; a backend that takes longer is reported as not answering.
const STATUS_DEADLINE: Duration = Duration::from_secs(3);
/// `warm` must return quickly; long work continues on the backends' own tasks.
const WARM_DEADLINE: Duration = Duration::from_secs(15);
/// A full rebuild of a large repository takes minutes; this only stops a hung backend.
const REFRESH_DEADLINE: Duration = Duration::from_secs(3_600);

/// The backends the orchestrator asks.
#[derive(Clone, Default)]
pub struct Backends {
    /// Asked in parallel for every `search`, in this order in `SearchResponse::backends`.
    pub search: Vec<Arc<dyn SearchBackend>>,
    /// Asked in order for `usages`; the first one that answers for a repository wins.
    pub usages: Vec<Arc<dyn UsageSource>>,
    /// Symbol extents for the merge step. Without it, hits merge by line windows only.
    pub spans: Option<Arc<dyn SpanProvider>>,
}

/// What `search` is asked.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchParams {
    pub query: String,
    /// Forces the query kind; `None` lets `classify` decide.
    pub kind: Option<QueryKind>,
    /// Globs relative to each repository root; empty means everything.
    pub paths: Vec<String>,
    /// Page size; `None` means `OutputConfig::default_limit`. Clamped to `1..=max_limit`.
    pub limit: Option<usize>,
    pub offset: usize,
    /// Decides how many snippet lines are read.
    pub format: Format,
}

impl SearchParams {
    pub fn new(query: impl Into<String>) -> Self {
        Self { query: query.into(), kind: None, paths: Vec::new(), limit: None, offset: 0, format: Format::default() }
    }
}

/// What `usages` is asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsagesParams {
    pub symbol: String,
    /// Page size; `None` means `DEFAULT_USAGES_LIMIT`. Clamped to `1..=MAX_USAGES_LIMIT`.
    pub limit: Option<usize>,
    pub offset: usize,
}

impl UsagesParams {
    pub fn new(symbol: impl Into<String>) -> Self {
        Self { symbol: symbol.into(), limit: None, offset: 0 }
    }
}

/// The result of refreshing one backend's index of one repository.
#[derive(Debug, Clone, PartialEq)]
pub struct RefreshResult {
    pub backend: BackendId,
    pub repo: String,
    pub result: Result<(), BackendError>,
    pub elapsed_ms: u64,
}

/// The repositories of `workspace` named in `names`, in that order, or all of them when `names`
/// is empty.
pub fn select_repos(workspace: &Workspace, names: &[String]) -> Result<Vec<Repo>> {
    if names.is_empty() {
        return Ok(workspace.repos.clone());
    }
    names
        .iter()
        .map(|name| {
            workspace.repos.iter().find(|repo| &repo.name == name).cloned().ok_or_else(|| {
                let known: Vec<&str> = workspace.repos.iter().map(|repo| repo.name.as_str()).collect();
                anyhow!("unknown repository {name:?}; this workspace has: {}", known.join(", "))
            })
        })
        .collect()
}

/// Asks the backends and fuses their answers. Share it with an `Arc`.
pub struct Orchestrator {
    backends: Backends,
    config: Config,
    /// (backend, repository root) pairs that answered at least once. Until a pair has answered,
    /// semantic backends get `Deadlines::semantic_cold_ms`.
    warm: Mutex<HashSet<(BackendId, PathBuf)>>,
}

impl Orchestrator {
    pub fn new(backends: Backends, config: Config) -> Self {
        Self { backends, config, warm: Mutex::new(HashSet::new()) }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Searches `repos` and returns one fused page.
    pub async fn search(&self, repos: &[Repo], params: &SearchParams) -> Result<SearchResponse> {
        let classified = classify(&params.query, params.kind);
        if classified.text.trim().is_empty() {
            bail!("the query is empty");
        }
        let limit = self.page_size(params.limit);
        let request = SearchRequest {
            query: classified.text,
            kind: classified.kind,
            fetch_limit: params
                .offset
                .saturating_add(limit)
                .saturating_mul(self.config.fusion.fetch_factor)
                .min(MAX_FETCH_LIMIT),
            paths: params.paths.clone(),
        };
        debug!(query = %request.query, kind = ?request.kind, fetch_limit = request.fetch_limit, repos = repos.len(), "search");

        let mut hits = Vec::new();
        let mut backends = Vec::new();
        for result in self.collect(repos, &request).await {
            hits.extend(result.hits);
            backends.push(BackendReport {
                backend: result.backend,
                repo: result.repo,
                status: result.status,
                elapsed_ms: result.elapsed_ms,
            });
        }
        let spans = self.spans(repos, &hits).await;
        let fused = fuse(&request.query, request.kind, hits, &spans, &self.config.fusion);
        let total = fused.len();
        let (mut page, next_offset) = render::page(fused, params.offset, limit);
        snippets::fill(&mut page, params.format, &self.config.output).await;
        Ok(SearchResponse {
            query: request.query,
            kind: request.kind,
            total,
            offset: params.offset,
            hits: page,
            next_offset,
            backends,
        })
    }

    /// Asks every backend that takes part in `request.kind` (it supports the kind and its fusion
    /// weight is above 0) in every repository, in parallel, and returns one result per pair:
    /// repositories in the given order, backends in `Backends::search` order. The raw material
    /// of `search`, also used by the benchmark to score backends one by one.
    pub async fn collect(&self, repos: &[Repo], request: &SearchRequest) -> Vec<BackendResult> {
        let request = Arc::new(request.clone());
        let mut asked = Vec::new();
        let mut calls = Vec::new();
        for repo in repos {
            for backend in self.backends.search.iter().filter(|backend| self.asks(backend.as_ref(), request.kind)) {
                let id = backend.id();
                let deadline = self.deadline(id, self.is_cold(id, &repo.root));
                let (backend, repo_owned, request) = (Arc::clone(backend), repo.clone(), Arc::clone(&request));
                calls.push(Call::new(deadline, async move { backend.search(&repo_owned, &request).await }));
                asked.push((id, repo));
            }
        }
        asked
            .into_iter()
            .zip(run_all(calls).await)
            .map(|((backend, repo), timed)| {
                let (status, hits) = match timed.result {
                    Ok(answer) => {
                        self.mark_warm(backend, &repo.root);
                        (answer_status(&answer), answer.hits)
                    }
                    Err(status) => (status, Vec::new()),
                };
                if matches!(status, BackendStatus::Failed { .. }) {
                    warn!(backend = %backend, repo = %repo.name, ?status, "backend failed");
                } else {
                    debug!(backend = %backend, repo = %repo.name, ?status, hits = hits.len(), elapsed_ms = timed.elapsed_ms, "backend answered");
                }
                BackendResult { backend, repo: repo.name.clone(), status, hits, elapsed_ms: timed.elapsed_ms }
            })
            .collect()
    }

    /// Lists every usage of `symbol` in `repos`, grouped by repository, sorted by path and line.
    /// Every repository is listed on every page, with its source and status.
    pub async fn usages(&self, repos: &[Repo], params: &UsagesParams) -> Result<UsagesResponse> {
        let symbol = params.symbol.trim();
        if symbol.is_empty() {
            bail!("the symbol is empty");
        }
        let limit = params.limit.unwrap_or(DEFAULT_USAGES_LIMIT).clamp(1, MAX_USAGES_LIMIT);
        let scan_limit = params.offset.saturating_add(limit).max(USAGES_SCAN_LIMIT);
        let answers = join_all(repos.iter().map(|repo| self.usages_in(repo, symbol, scan_limit))).await;

        let mut complete = true;
        let mut listed = Vec::with_capacity(answers.len());
        let mut all = Vec::new();
        for (index, mut answer) in answers.into_iter().enumerate() {
            complete &= answer.complete;
            all.extend(std::mem::take(&mut answer.found.usages).into_iter().map(|usage| (index, usage)));
            listed.push(answer.found);
        }
        let total = all.len();
        let (page, next_offset) = render::page(all, params.offset, limit);
        for (index, usage) in page {
            if let Some(repo) = listed.get_mut(index) {
                repo.usages.push(usage);
            }
        }
        Ok(UsagesResponse {
            symbol: symbol.to_owned(),
            total,
            offset: params.offset,
            repos: listed,
            next_offset,
            complete,
        })
    }

    /// The index state of every search backend in every repository.
    pub async fn status(&self, repos: &[Repo]) -> Vec<IndexState> {
        let mut asked = Vec::new();
        let mut calls = Vec::new();
        for repo in repos {
            for backend in &self.backends.search {
                let (backend_owned, repo_owned) = (Arc::clone(backend), repo.clone());
                calls.push(Call::new(STATUS_DEADLINE, async move { Ok(backend_owned.index_state(&repo_owned).await) }));
                asked.push((backend.id(), repo.name.clone()));
            }
        }
        asked
            .into_iter()
            .zip(run_all(calls).await)
            .map(|((backend, repo), timed)| match timed.result {
                Ok(state) => state,
                Err(status) => IndexState {
                    backend,
                    repo,
                    indexed: false,
                    updated_at_ms: None,
                    stale: None,
                    detail: Some(describe(&status, STATUS_DEADLINE)),
                },
            })
            .collect()
    }

    /// Brings every backend's index of `repos` up to date; `full` rebuilds from scratch. Backends
    /// work in parallel; each one refreshes the repositories one after another.
    pub async fn refresh(&self, repos: &[Repo], full: bool) -> Vec<RefreshResult> {
        let calls = self
            .backends
            .search
            .iter()
            .map(|backend| {
                let (backend, repos) = (Arc::clone(backend), repos.to_vec());
                Call::new(REFRESH_DEADLINE, async move {
                    let mut done = Vec::with_capacity(repos.len());
                    for repo in &repos {
                        let started = Instant::now();
                        let result = backend.refresh(repo, full).await;
                        let elapsed_ms = millis(started.elapsed());
                        done.push(RefreshResult { backend: backend.id(), repo: repo.name.clone(), result, elapsed_ms });
                    }
                    Ok(done)
                })
            })
            .collect();
        let mut results = Vec::new();
        for (backend, timed) in self.backends.search.iter().zip(run_all(calls).await) {
            match timed.result {
                Ok(done) => results.extend(done),
                Err(status) => {
                    let reason = describe(&status, REFRESH_DEADLINE);
                    warn!(backend = %backend.id(), reason, "refresh stopped");
                    results.extend(repos.iter().map(|repo| RefreshResult {
                        backend: backend.id(),
                        repo: repo.name.clone(),
                        result: Err(BackendError::Failed(reason.clone())),
                        elapsed_ms: timed.elapsed_ms,
                    }));
                }
            }
        }
        for result in &results {
            match &result.result {
                Ok(()) => {
                    debug!(backend = %result.backend, repo = %result.repo, elapsed_ms = result.elapsed_ms, "refreshed")
                }
                Err(error) => warn!(backend = %result.backend, repo = %result.repo, %error, "refresh failed"),
            }
        }
        results
    }

    /// Prepares the backends for `repos`: starts child processes and background indexing, and,
    /// when `ast_index.update_on_start` is set, runs `refresh` (an incremental update) of ast-index
    /// in every repository that already has an index. Returns quickly; the work goes on in the
    /// background. Repositories without an ast-index index are left alone: `uals refresh`
    /// creates indexes on request.
    pub async fn warm(&self, repos: &[Repo]) {
        let calls = self
            .backends
            .search
            .iter()
            .map(|backend| {
                let (backend, repos) = (Arc::clone(backend), repos.to_vec());
                Call::new(WARM_DEADLINE, async move { backend.warm(&repos).await })
            })
            .collect();
        for (backend, timed) in self.backends.search.iter().zip(run_all(calls).await) {
            match timed.result {
                Ok(()) => debug!(backend = %backend.id(), elapsed_ms = timed.elapsed_ms, "warmed"),
                Err(status) => warn!(backend = %backend.id(), ?status, "warm-up failed"),
            }
        }
        if !self.config.ast_index.update_on_start {
            return;
        }
        let Some(ast_index) = self.backends.search.iter().find(|backend| backend.id() == BackendId::AstIndex) else {
            return;
        };
        let (ast_index, repos) = (Arc::clone(ast_index), repos.to_vec());
        tokio::spawn(async move {
            for repo in &repos {
                if !ast_index.index_state(repo).await.indexed {
                    debug!(repo = %repo.name, "no ast-index index; not updating it on start");
                    continue;
                }
                match ast_index.refresh(repo, false).await {
                    Ok(()) => debug!(repo = %repo.name, "ast-index updated on start"),
                    Err(error) => warn!(repo = %repo.name, %error, "ast-index update on start failed"),
                }
            }
        });
    }

    /// Asks the usage sources in order until one answers for `repo`.
    async fn usages_in(&self, repo: &Repo, symbol: &str, limit: usize) -> RepoAnswer {
        let mut failure = None;
        for source in &self.backends.usages {
            let id = source.id();
            let (source, repo_owned, symbol_owned) = (Arc::clone(source), repo.clone(), symbol.to_owned());
            let call =
                Call::new(
                    self.deadline(id, false),
                    async move { source.usages(&repo_owned, &symbol_owned, limit).await },
                );
            let timed = run_all(vec![call]).await.pop().map_or(Err(lost_task()), |timed| timed.result);
            match timed {
                Ok(mut answer) => {
                    answer.usages.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
                    answer.usages.dedup_by(|a, b| a.path == b.path && a.line == b.line);
                    debug!(repo = %repo.name, source = %id, usages = answer.usages.len(), complete = answer.complete, "usages");
                    let status = match answer.stale {
                        Some(reason) => BackendStatus::Stale { reason },
                        None => BackendStatus::Ok,
                    };
                    let found = RepoUsages { repo: repo.name.clone(), source: id, status, usages: answer.usages };
                    return RepoAnswer { found, complete: answer.complete };
                }
                Err(status) => {
                    debug!(repo = %repo.name, source = %id, ?status, "usage source did not answer; trying the next one");
                    failure = Some((id, status));
                }
            }
        }
        let (source, status) = failure.unwrap_or_else(|| {
            (BackendId::Lexical, BackendStatus::Unavailable { reason: "no usage source is configured".into() })
        });
        RepoAnswer {
            found: RepoUsages { repo: repo.name.clone(), source, status, usages: Vec::new() },
            complete: false,
        }
    }

    /// The symbol spans of every file a hit points to, one provider call per repository.
    /// Spans only improve merging, so failures are logged and skipped.
    async fn spans(&self, repos: &[Repo], hits: &[Hit]) -> Vec<SymbolSpan> {
        let Some(provider) = &self.backends.spans else {
            return Vec::new();
        };
        let mut files: BTreeMap<&str, BTreeSet<&Path>> = BTreeMap::new();
        for hit in hits {
            files.entry(hit.repo.as_str()).or_default().insert(hit.path.as_path());
        }
        let mut asked = Vec::new();
        let mut calls = Vec::new();
        for repo in repos {
            let Some(paths) = files.get(repo.name.as_str()) else {
                continue;
            };
            let paths: Vec<PathBuf> = paths.iter().map(|path| path.to_path_buf()).collect();
            let (provider, repo_owned) = (Arc::clone(provider), repo.clone());
            calls.push(Call::new(SPANS_DEADLINE, async move { provider.spans(&repo_owned, &paths).await }));
            asked.push(repo.name.as_str());
        }
        let mut spans = Vec::new();
        for (repo, timed) in asked.into_iter().zip(run_all(calls).await) {
            match timed.result {
                Ok(found) => spans.extend(found),
                Err(status) => debug!(repo, ?status, "no symbol spans; hits merge by line windows"),
            }
        }
        spans
    }

    fn page_size(&self, limit: Option<usize>) -> usize {
        let output = &self.config.output;
        limit.unwrap_or(output.default_limit).clamp(1, output.max_limit)
    }

    /// Whether `backend` takes part in queries of `kind`.
    fn asks(&self, backend: &dyn SearchBackend, kind: QueryKind) -> bool {
        backend.supports(kind) && self.config.fusion.weight(kind, backend.id()) > 0.0
    }

    fn deadline(&self, backend: BackendId, cold: bool) -> Duration {
        Duration::from_millis(self.config.deadlines.for_backend(backend, cold))
    }

    fn is_cold(&self, backend: BackendId, root: &Path) -> bool {
        !self.warm.lock().unwrap_or_else(PoisonError::into_inner).contains(&(backend, root.to_path_buf()))
    }

    fn mark_warm(&self, backend: BackendId, root: &Path) {
        self.warm.lock().unwrap_or_else(PoisonError::into_inner).insert((backend, root.to_path_buf()));
    }
}

/// One repository's usages and whether the source listed all of them.
struct RepoAnswer {
    found: RepoUsages,
    complete: bool,
}

/// The status of an answer: `Partial` wins over `Stale` and carries both reasons.
fn answer_status(answer: &BackendAnswer) -> BackendStatus {
    match (&answer.partial, &answer.stale) {
        (Some(partial), Some(stale)) => BackendStatus::Partial { reason: format!("{partial}; {stale}") },
        (Some(partial), None) => BackendStatus::Partial { reason: partial.clone() },
        (None, Some(stale)) => BackendStatus::Stale { reason: stale.clone() },
        (None, None) => BackendStatus::Ok,
    }
}

/// A status as one line of text, for places that have only a text field.
fn describe(status: &BackendStatus, deadline: Duration) -> String {
    match status {
        BackendStatus::Timeout => format!("no answer within {} ms", deadline.as_millis()),
        BackendStatus::Ok => "ok".into(),
        BackendStatus::Stale { reason }
        | BackendStatus::Partial { reason }
        | BackendStatus::Unavailable { reason }
        | BackendStatus::Failed { reason } => reason.clone(),
    }
}

/// One backend call with its deadline.
struct Call<T> {
    deadline: Duration,
    future: BoxFuture<'static, Result<T, BackendError>>,
}

impl<T> Call<T> {
    fn new(deadline: Duration, future: impl Future<Output = Result<T, BackendError>> + Send + 'static) -> Self {
        Self { deadline, future: future.boxed() }
    }
}

/// The result of one call: the answer, or the status that explains why there is none.
struct Timed<T> {
    result: Result<T, BackendStatus>,
    elapsed_ms: u64,
}

/// Runs every call on its own task under its own deadline and returns the results in input order.
///
/// On timeout the call's future is dropped inside its task, so child processes it started die
/// with it (adapters use `kill_on_drop`). A panic becomes `Failed`, so one broken adapter cannot
/// take the response down. Dropping the returned future aborts every task still running.
async fn run_all<T: Send + 'static>(calls: Vec<Call<T>>) -> Vec<Timed<T>> {
    let mut slots: Vec<Option<Timed<T>>> = calls.iter().map(|_| None).collect();
    let mut tasks = JoinSet::new();
    for (index, call) in calls.into_iter().enumerate() {
        tasks.spawn(async move { (index, run_one(call).await) });
    }
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((index, timed)) => {
                if let Some(slot) = slots.get_mut(index) {
                    *slot = Some(timed);
                }
            }
            Err(error) => warn!(%error, "a backend task ended without a result"),
        }
    }
    slots.into_iter().map(|slot| slot.unwrap_or(Timed { result: Err(lost_task()), elapsed_ms: 0 })).collect()
}

async fn run_one<T>(call: Call<T>) -> Timed<T> {
    let started = Instant::now();
    let outcome = AssertUnwindSafe(tokio::time::timeout(call.deadline, call.future)).catch_unwind().await;
    let result = match outcome {
        Ok(Ok(Ok(value))) => Ok(value),
        Ok(Ok(Err(BackendError::Unavailable(reason)))) => Err(BackendStatus::Unavailable { reason }),
        Ok(Ok(Err(BackendError::Failed(reason)))) => Err(BackendStatus::Failed { reason }),
        Ok(Err(_elapsed)) => Err(BackendStatus::Timeout),
        Err(panic) => Err(BackendStatus::Failed { reason: format!("panicked: {}", panic_text(panic.as_ref())) }),
    };
    Timed { result, elapsed_ms: millis(started.elapsed()) }
}

fn lost_task() -> BackendStatus {
    BackendStatus::Failed { reason: "the task ended without a result".into() }
}

fn panic_text(payload: &(dyn Any + Send)) -> &str {
    if let Some(text) = payload.downcast_ref::<&str>() {
        text
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text
    } else {
        "no message"
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
