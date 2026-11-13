//! Runs one query against every backend and every repository at once and fuses the answers.
//!
//! `search`: classify the query → ask the backends in parallel, each call on its own task under its
//! backend's deadline: one call per repository, or one call for all repositories when the backend
//! searches them at once → fetch the symbol spans of the files the hits point to → fuse → cut one
//! page → read the page's snippets from disk. A backend that fails, panics or misses its deadline
//! loses only its own hits; `SearchResponse::backends` says what happened to every (repository,
//! backend) pair.
//!
//! `usages`: per repository, the first usage source that answers (ast-index, then lexical). When
//! language servers are configured, the declarations of the name are looked up and each server
//! says which usages refer to which declaration (see `precise`).

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
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, warn};
use uals_core::backend::{BackendError, DefinitionSource, ReferenceResolver, SearchBackend, SpanProvider, UsageSource};
use uals_core::classify::classify;
use uals_core::config::Config;
use uals_core::fuse::fuse;
use uals_core::render::{self, Format};
use uals_core::types::{
    BackendAnswer, BackendId, BackendReport, BackendResult, BackendStatus, Definition, DefinitionReport, Hit,
    IndexState, QueryKind, Repo, RepoUsages, ResolvedReferences, SearchAllAnswer, SearchRequest, SearchResponse,
    SymbolSpan, UsageAnswer, UsagesResponse, Workspace,
};

use crate::precise;
use crate::snippets;

/// No backend is asked for more hits than this per repository.
const MAX_FETCH_LIMIT: usize = 500;
/// Page size of `usages` when the caller gives none.
pub const DEFAULT_USAGES_LIMIT: usize = 100;
/// The largest page `usages` returns, so one response stays a size a client can read.
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
/// An incremental ast-index update takes seconds; this only stops a hung one.
const AST_INDEX_UPDATE_DEADLINE: Duration = Duration::from_secs(600);
/// Finding the declarations of a name is one indexed query per repository.
const DEFINITIONS_DEADLINE: Duration = Duration::from_secs(3);
/// Added to the language-server limits of `LspConfig`, so the pool reports its own timeouts.
const REFERENCES_MARGIN: Duration = Duration::from_secs(2);

/// The backends the orchestrator asks.
#[derive(Clone, Default)]
pub struct Backends {
    /// Asked in parallel for every `search`, in this order in `SearchResponse::backends`.
    pub search: Vec<Arc<dyn SearchBackend>>,
    /// Asked in order for `usages`; the first one that answers for a repository wins.
    pub usages: Vec<Arc<dyn UsageSource>>,
    /// Symbol extents for the merge step. Without it, hits merge by line windows only.
    pub spans: Option<Arc<dyn SpanProvider>>,
    /// Declarations of a name, for precise `usages`. Precise usages need both this and
    /// `references`.
    pub definitions: Option<Arc<dyn DefinitionSource>>,
    /// Language servers that resolve the usages of one declaration.
    pub references: Option<Arc<dyn ReferenceResolver>>,
}

/// What `search` is asked.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchParams {
    /// The text to look for, as the user typed it.
    pub query: String,
    /// Forces the query kind; `None` lets `classify` decide.
    pub kind: Option<QueryKind>,
    /// Globs relative to each repository root; empty means everything.
    pub paths: Vec<String>,
    /// Page size; `None` means `OutputConfig::default_limit`. Clamped to `1..=max_limit`.
    pub limit: Option<usize>,
    /// How many fused hits to skip before the page starts.
    pub offset: usize,
    /// Decides how many snippet lines are read.
    pub format: Format,
}

impl SearchParams {
    /// A search for `query` with every other parameter at its default.
    pub fn new(query: impl Into<String>) -> Self {
        Self { query: query.into(), kind: None, paths: Vec::new(), limit: None, offset: 0, format: Format::default() }
    }
}

/// What `usages` is asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsagesParams {
    /// The symbol whose usages are listed.
    pub symbol: String,
    /// Page size; `None` means `DEFAULT_USAGES_LIMIT`. Clamped to `1..=MAX_USAGES_LIMIT`.
    pub limit: Option<usize>,
    /// How many usages to skip before the page starts.
    pub offset: usize,
}

impl UsagesParams {
    /// A usages request for `symbol` with the default page.
    pub fn new(symbol: impl Into<String>) -> Self {
        Self { symbol: symbol.into(), limit: None, offset: 0 }
    }
}

/// The result of refreshing one backend's index of one repository.
#[derive(Debug, Clone, PartialEq)]
pub struct RefreshResult {
    pub backend: BackendId,
    /// The repository name.
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
    /// An orchestrator that asks `backends` under the limits and weights of `config`.
    pub fn new(backends: Backends, config: Config) -> Self {
        Self { backends, config, warm: Mutex::new(HashSet::new()) }
    }

    /// The configuration the orchestrator runs with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Searches `repos` and returns one fused page.
    pub async fn search(&self, repos: &[Repo], params: &SearchParams) -> Result<SearchResponse> {
        let limit = self.page_size(params.limit);
        let request = self.build_request(params)?;
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
    /// weight is above 0), in parallel, and returns one result per (repository, backend) pair:
    /// repositories in the given order, backends in `Backends::search` order. A backend that
    /// searches all repositories at once gets one call, cold while any repository is cold, and its
    /// answer is split by repository; every other backend gets one call per repository. The raw
    /// material of `search`.
    pub async fn collect(&self, repos: &[Repo], request: &SearchRequest) -> Vec<BackendResult> {
        let request = Arc::new(request.clone());
        // Labelled with (place of the repository, order of the backend, backend).
        let mut per_repo = Batch::new();
        // Labelled with (order of the backend, backend).
        let mut all_at_once = Batch::new();
        for (order, backend) in self.backends.search.iter().enumerate() {
            if !self.asks(backend.as_ref(), request.kind) {
                continue;
            }
            let id = backend.id();
            if backend.searches_all_at_once() {
                if !repos.is_empty() {
                    let cold = repos.iter().any(|repo| self.is_cold(id, &repo.root));
                    all_at_once.push((order, id), search_all_call(backend, repos, &request, self.deadline(id, cold)));
                }
                continue;
            }
            for (place, repo) in repos.iter().enumerate() {
                let deadline = self.deadline(id, self.is_cold(id, &repo.root));
                per_repo.push((place, order, id), search_call(backend, repo, &request, deadline));
            }
        }
        let (per_repo, all_at_once) = tokio::join!(per_repo.run(), all_at_once.run());
        // Each result is keyed by (place of the repository, order of the backend).
        let mut results = Vec::new();
        for ((place, order, backend), timed) in per_repo {
            results.push(((place, order), self.record_search(backend, &repos[place], timed)));
        }
        for ((order, backend), timed) in all_at_once {
            let split = split_by_repo(backend, repos, timed);
            for ((place, repo), timed) in repos.iter().enumerate().zip(split) {
                results.push(((place, order), self.record_search(backend, repo, timed)));
            }
        }
        results.sort_by_key(|&(slot, _)| slot);
        results.into_iter().map(|(_, result)| result).collect()
    }

    /// Lists every usage of `symbol` in `repos`, grouped by repository, sorted by path and line.
    /// Every repository is listed on every page, with its source and status. Fails when the
    /// symbol is empty or no usage source is configured.
    pub async fn usages(&self, repos: &[Repo], params: &UsagesParams) -> Result<UsagesResponse> {
        let symbol = params.symbol.trim();
        if symbol.is_empty() {
            bail!("the symbol is empty");
        }
        let limit = params.limit.unwrap_or(DEFAULT_USAGES_LIMIT).clamp(1, MAX_USAGES_LIMIT);
        let scan_limit = params.offset.saturating_add(limit).max(USAGES_SCAN_LIMIT);
        let (container, name) = precise::split_qualifier(symbol);
        let definitions = self.definitions(repos, container, name).await;
        // A qualified symbol searches by its bare name only when its declaration was found;
        // otherwise it is searched as typed, as without language servers.
        let search = if definitions.is_empty() { symbol } else { name };
        let answers = join_all(repos.iter().map(|repo| self.usages_in(repo, search, scan_limit))).await;
        let mut answers = answers.into_iter().collect::<Result<Vec<_>>>()?;
        let reports = match &self.backends.references {
            Some(resolver) if !definitions.is_empty() => {
                let resolved = self.resolve(resolver, repos, &definitions).await;
                let mut found: Vec<&mut RepoUsages> = answers.iter_mut().map(|answer| &mut answer.found).collect();
                precise::merge(repos, &mut found, definitions, resolved, container.is_some())
            }
            _ => Vec::new(),
        };
        Ok(usages_page(symbol, answers, reports, params.offset, limit))
    }

    /// The declarations named `name` in `repos`, those inside `container` when it is given, at most
    /// `precise::MAX_DEFINITIONS`. Empty when precise usages are not configured.
    async fn definitions(&self, repos: &[Repo], container: Option<&str>, name: &str) -> Vec<Definition> {
        let (Some(source), Some(_)) = (&self.backends.definitions, &self.backends.references) else {
            return Vec::new();
        };
        let mut batch = Batch::new();
        for repo in repos {
            let (source, repo_owned, name) = (Arc::clone(source), repo.clone(), name.to_owned());
            batch.push(
                repo.name.clone(),
                Call::new(DEFINITIONS_DEADLINE, async move { source.definitions(&repo_owned, &name).await }),
            );
        }
        let mut definitions = Vec::new();
        for (repo, timed) in batch.run().await {
            match timed.result {
                Ok(found) => definitions.extend(found),
                Err(status) => debug!(repo, ?status, "no declarations; usages are by name"),
            }
        }
        if let Some(container) = container {
            definitions.retain(|definition| definition.container.as_deref() == Some(container));
        }
        if definitions.len() > precise::MAX_DEFINITIONS {
            debug!(name, found = definitions.len(), "too many declarations; the first ones are resolved");
            definitions.truncate(precise::MAX_DEFINITIONS);
        }
        definitions
    }

    /// Asks the language servers about every definition at once; one answer per definition.
    async fn resolve(
        &self,
        resolver: &Arc<dyn ReferenceResolver>,
        repos: &[Repo],
        definitions: &[Definition],
    ) -> Vec<Result<ResolvedReferences, BackendStatus>> {
        // A cold call starts the server (`initialize`, up to `request_ms`), waits for the project
        // (`ready_wait_ms`) and asks (`request_ms`). A shorter deadline would kill a slow server
        // while it starts, on every call.
        let lsp = &self.config.lsp;
        let budget = lsp.ready_wait_ms.saturating_add(lsp.request_ms.saturating_mul(2));
        let deadline = Duration::from_millis(budget) + REFERENCES_MARGIN;
        let calls = definitions.iter().map(|definition| {
            let repo = repos.iter().find(|repo| repo.name == definition.repo).cloned();
            let (resolver, definition) = (Arc::clone(resolver), definition.clone());
            Call::new(deadline, async move {
                let repo =
                    repo.ok_or_else(|| BackendError::Failed(format!("unknown repository {}", definition.repo)))?;
                resolver.references(&repo, &definition).await
            })
        });
        run_all(calls.collect()).await.into_iter().map(|timed| timed.result).collect()
    }

    /// One line per language server, for `status`.
    pub fn language_servers(&self) -> Vec<String> {
        self.backends.references.as_ref().map(|resolver| resolver.servers()).unwrap_or_default()
    }

    /// The index state of every search backend in every repository.
    pub async fn status(&self, repos: &[Repo]) -> Vec<IndexState> {
        let mut batch = Batch::new();
        for repo in repos {
            for backend in &self.backends.search {
                let (backend_owned, repo_owned) = (Arc::clone(backend), repo.clone());
                let call = Call::new(STATUS_DEADLINE, async move { Ok(backend_owned.index_state(&repo_owned).await) });
                batch.push((backend.id(), repo.name.clone()), call);
            }
        }
        batch
            .run()
            .await
            .into_iter()
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
    /// work in parallel; each one refreshes the repositories one after another, each repository
    /// under `REFRESH_DEADLINE`. Results come per backend, repositories in the given order.
    pub async fn refresh(&self, repos: &[Repo], full: bool) -> Vec<RefreshResult> {
        let turns = self.backends.search.iter().map(|backend| refresh_in_turn(Arc::clone(backend), repos, full));
        let results: Vec<RefreshResult> = join_all(turns).await.into_iter().flatten().collect();
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

    /// Lets every search backend prepare for `repos` (start child processes, start background
    /// indexing) under `WARM_DEADLINE`, and logs the outcome. Returns quickly; the work goes on
    /// in the background.
    pub async fn warm(&self, repos: &[Repo]) {
        let mut batch = Batch::new();
        for backend in &self.backends.search {
            let (backend_owned, repos_owned) = (Arc::clone(backend), repos.to_vec());
            batch.push(backend.id(), Call::new(WARM_DEADLINE, async move { backend_owned.warm(&repos_owned).await }));
        }
        for (backend, timed) in batch.run().await {
            match timed.result {
                Ok(()) => debug!(backend = %backend, elapsed_ms = timed.elapsed_ms, "warmed"),
                Err(status) => warn!(backend = %backend, ?status, "warm-up failed"),
            }
        }
    }

    /// Spawns an incremental update of the ast-index indexes that already exist in `repos`, one
    /// repository after another, each under `AST_INDEX_UPDATE_DEADLINE`. A repository without an
    /// index gets none: creating one is a full `refresh`. Awaiting the returned handle waits for
    /// the updates; dropping it leaves them running. Returns `None` when no ast-index backend is
    /// configured.
    pub fn update_existing_ast_indexes_in_background(&self, repos: &[Repo]) -> Option<JoinHandle<()>> {
        let ast_index = self.backends.search.iter().find(|backend| backend.id() == BackendId::AstIndex)?;
        let (ast_index, repos) = (Arc::clone(ast_index), repos.to_vec());
        let handle = tokio::spawn(async move {
            for repo in &repos {
                let update = async {
                    if !ast_index.index_state(repo).await.indexed {
                        return None;
                    }
                    Some(ast_index.refresh(repo, false).await)
                };
                match tokio::time::timeout(AST_INDEX_UPDATE_DEADLINE, update).await {
                    Ok(None) => debug!(repo = %repo.name, "no ast-index index; not creating one"),
                    Ok(Some(Ok(()))) => debug!(repo = %repo.name, "ast-index updated"),
                    Ok(Some(Err(error))) => warn!(repo = %repo.name, %error, "ast-index update failed"),
                    Err(_) => warn!(repo = %repo.name, "ast-index update stopped at its deadline"),
                }
            }
        });
        Some(handle)
    }

    /// The backend request for `params`: the classified query and how many hits each backend
    /// is asked for.
    fn build_request(&self, params: &SearchParams) -> Result<SearchRequest> {
        let classified = classify(&params.query, params.kind);
        if classified.text.trim().is_empty() {
            bail!("the query is empty");
        }
        // Fusion needs more candidates than one page holds, since backends rank differently. The
        // depth does not depend on the page, so every page is cut from the same candidate pool.
        let fetch_limit =
            self.config.output.max_limit.saturating_mul(self.config.fusion.fetch_factor).min(MAX_FETCH_LIMIT);
        Ok(SearchRequest { query: classified.text, kind: classified.kind, fetch_limit, paths: params.paths.clone() })
    }

    /// Turns the outcome of one search call into a `BackendResult`, notes that the pair answered
    /// and logs it.
    fn record_search(&self, backend: BackendId, repo: &Repo, timed: Timed<BackendAnswer>) -> BackendResult {
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
    }

    /// Asks the usage sources in order until one answers for `repo`. Fails only when no usage
    /// source is configured.
    async fn usages_in(&self, repo: &Repo, symbol: &str, limit: usize) -> Result<RepoAnswer> {
        let mut failures = Vec::new();
        for source in &self.backends.usages {
            let id = source.id();
            let call = usages_call(source, repo, symbol, limit, self.deadline(id, false));
            match run_single(call).await.result {
                Ok(answer) => return Ok(answered(repo, id, answer, &failures)),
                Err(status) => {
                    debug!(repo = %repo.name, source = %id, ?status, "usage source did not answer; trying the next one");
                    failures.push((id, status));
                }
            }
        }
        let Some((source, status)) = failures.pop() else {
            bail!("no usage source is configured");
        };
        warn!(repo = %repo.name, source = %source, ?status, "no usage source answered");
        Ok(unanswered(repo, source, status, &failures))
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
        let mut batch = Batch::new();
        for repo in repos {
            let Some(paths) = files.get(repo.name.as_str()) else {
                continue;
            };
            let paths: Vec<PathBuf> = paths.iter().map(|path| path.to_path_buf()).collect();
            let (provider, repo_owned) = (Arc::clone(provider), repo.clone());
            batch.push(
                repo.name.as_str(),
                Call::new(SPANS_DEADLINE, async move { provider.spans(&repo_owned, &paths).await }),
            );
        }
        let mut spans = Vec::new();
        for (repo, timed) in batch.run().await {
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

    /// The deadline of one call; `cold` is true until the backend has answered for the repository.
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

/// A search call of `backend` in `repo` under `deadline`.
fn search_call(
    backend: &Arc<dyn SearchBackend>,
    repo: &Repo,
    request: &Arc<SearchRequest>,
    deadline: Duration,
) -> Call<BackendAnswer> {
    let (backend, repo, request) = (Arc::clone(backend), repo.clone(), Arc::clone(request));
    Call::new(deadline, async move { backend.search(&repo, &request).await })
}

/// One search call of `backend` over all `repos` under `deadline`.
fn search_all_call(
    backend: &Arc<dyn SearchBackend>,
    repos: &[Repo],
    request: &Arc<SearchRequest>,
    deadline: Duration,
) -> Call<SearchAllAnswer> {
    let (backend, repos, request) = (Arc::clone(backend), repos.to_vec(), Arc::clone(request));
    Call::new(deadline, async move { backend.search_all(&repos, &request).await })
}

/// Splits the outcome of one `search_all` call of `backend` into one outcome per repository of
/// `repos`, in that order, each with the call's time. A failed or late call fails every
/// repository. A repository the backend could not search gets `Unavailable` with its reason. Every
/// other repository gets its own hits, with their ranks over all repositories, and the call's
/// `stale` and `partial` notes. Hits of any other repository are a backend bug: they are dropped
/// and logged.
fn split_by_repo(backend: BackendId, repos: &[Repo], timed: Timed<SearchAllAnswer>) -> Vec<Timed<BackendAnswer>> {
    let Timed { result, elapsed_ms } = timed;
    let SearchAllAnswer { answer: BackendAnswer { hits, stale, partial }, mut unavailable } = match result {
        Ok(found) => found,
        Err(status) => return repos.iter().map(|_| Timed { result: Err(status.clone()), elapsed_ms }).collect(),
    };
    let mut by_repo: BTreeMap<String, Vec<Hit>> = BTreeMap::new();
    for hit in hits {
        by_repo.entry(hit.repo.clone()).or_default().push(hit);
    }
    let split = repos
        .iter()
        .map(|repo| {
            let result = match unavailable.remove(&repo.name) {
                Some(reason) => Err(BackendStatus::Unavailable { reason }),
                None => {
                    let hits = by_repo.remove(&repo.name).unwrap_or_default();
                    Ok(BackendAnswer { hits, stale: stale.clone(), partial: partial.clone() })
                }
            };
            Timed { result, elapsed_ms }
        })
        .collect();
    if !by_repo.is_empty() {
        let dropped: Vec<&String> = by_repo.keys().collect();
        warn!(
            backend = %backend,
            repos = ?dropped,
            "dropped hits of repositories that were not asked or that the backend reported unavailable"
        );
    }
    split
}

/// A usages call of `source` in `repo` under `deadline`.
fn usages_call(
    source: &Arc<dyn UsageSource>,
    repo: &Repo,
    symbol: &str,
    limit: usize,
    deadline: Duration,
) -> Call<UsageAnswer> {
    let (source, repo, symbol) = (Arc::clone(source), repo.clone(), symbol.to_owned());
    Call::new(deadline, async move { source.usages(&repo, &symbol, limit).await })
}

/// Refreshes `repos` with `backend` one after another, each under `REFRESH_DEADLINE`, and
/// returns one result per repository. A repository that fails, panics or misses the deadline
/// does not stop the next one.
async fn refresh_in_turn(backend: Arc<dyn SearchBackend>, repos: &[Repo], full: bool) -> Vec<RefreshResult> {
    let mut done = Vec::with_capacity(repos.len());
    for repo in repos {
        let (backend_owned, repo_owned) = (Arc::clone(&backend), repo.clone());
        let call = Call::new(REFRESH_DEADLINE, async move { backend_owned.refresh(&repo_owned, full).await });
        let timed = run_one(call).await;
        let result = timed.result.map_err(|status| match status {
            BackendStatus::Unavailable { reason } => BackendError::Unavailable(reason),
            other => BackendError::Failed(describe(&other, REFRESH_DEADLINE)),
        });
        done.push(RefreshResult {
            backend: backend.id(),
            repo: repo.name.clone(),
            result,
            elapsed_ms: timed.elapsed_ms,
        });
    }
    done
}

/// The answer of the source `id` for `repo`, with usages sorted by path and line and one entry
/// per line. `failures` are the sources asked before `id` and how they failed.
fn answered(
    repo: &Repo,
    id: BackendId,
    mut answer: UsageAnswer,
    failures: &[(BackendId, BackendStatus)],
) -> RepoAnswer {
    answer.usages.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    answer.usages.dedup_by(|a, b| a.path == b.path && a.line == b.line);
    debug!(repo = %repo.name, source = %id, usages = answer.usages.len(), complete = answer.complete, "usages");
    let status = match answer.stale {
        Some(reason) => BackendStatus::Stale { reason },
        None => BackendStatus::Ok,
    };
    let fallback = fallback_note(failures);
    let found = RepoUsages { repo: repo.name.clone(), source: id, status, fallback, usages: answer.usages };
    RepoAnswer { found, complete: answer.complete }
}

/// The empty answer for a repository no usage source answered: the last source asked and its
/// failure, with the failures of the sources before it as the fallback note.
fn unanswered(
    repo: &Repo,
    source: BackendId,
    status: BackendStatus,
    earlier: &[(BackendId, BackendStatus)],
) -> RepoAnswer {
    let found =
        RepoUsages { repo: repo.name.clone(), source, status, fallback: fallback_note(earlier), usages: Vec::new() };
    RepoAnswer { found, complete: false }
}

/// How each source in `failures` failed, as `ast-index unavailable (no index)`, joined with
/// `; `; `None` when the list is empty.
fn fallback_note(failures: &[(BackendId, BackendStatus)]) -> Option<String> {
    let notes: Vec<String> = failures
        .iter()
        .map(|(id, status)| match status.reason() {
            Some(reason) => format!("{id} {} ({reason})", status.state()),
            None => format!("{id} {}", status.state()),
        })
        .collect();
    (!notes.is_empty()).then(|| notes.join("; "))
}

/// One page of the usages of all repositories together; each usage lands in its repository's
/// list, and `total` counts all of them.
fn usages_page(
    symbol: &str,
    answers: Vec<RepoAnswer>,
    definitions: Vec<DefinitionReport>,
    offset: usize,
    limit: usize,
) -> UsagesResponse {
    let mut complete = true;
    let mut repos = Vec::with_capacity(answers.len());
    let mut all = Vec::new();
    for (index, mut answer) in answers.into_iter().enumerate() {
        complete &= answer.complete;
        all.extend(std::mem::take(&mut answer.found.usages).into_iter().map(|usage| (index, usage)));
        repos.push(answer.found);
    }
    let total = all.len();
    let (page, next_offset) = render::page(all, offset, limit);
    for (index, usage) in page {
        if let Some(repo) = repos.get_mut(index) {
            repo.usages.push(usage);
        }
    }
    UsagesResponse { symbol: symbol.to_owned(), total, offset, repos, next_offset, complete, definitions }
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

/// Calls that run together, each tagged with what was asked, so a result always comes back
/// next to its label.
struct Batch<L, T> {
    labels: Vec<L>,
    calls: Vec<Call<T>>,
}

impl<L, T: Send + 'static> Batch<L, T> {
    fn new() -> Self {
        Self { labels: Vec::new(), calls: Vec::new() }
    }

    fn push(&mut self, label: L, call: Call<T>) {
        self.labels.push(label);
        self.calls.push(call);
    }

    /// Runs the calls with `run_all` and pairs every result with its label, in push order.
    async fn run(self) -> Vec<(L, Timed<T>)> {
        self.labels.into_iter().zip(run_all(self.calls).await).collect()
    }
}

/// Runs one call on its own task, like `run_all` does for many.
async fn run_single<T: Send + 'static>(call: Call<T>) -> Timed<T> {
    run_all(vec![call]).await.pop().unwrap_or_else(lost_timed)
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
    slots.into_iter().map(|slot| slot.unwrap_or_else(lost_timed)).collect()
}

/// Runs the call under its deadline and turns every way it can end into a `Timed`.
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

/// The result of a call whose task ended without reporting one.
fn lost_timed<T>() -> Timed<T> {
    Timed { result: Err(BackendStatus::Failed { reason: "the task ended without a result".into() }), elapsed_ms: 0 }
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
