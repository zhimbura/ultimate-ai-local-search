//! claude-context adapter: semantic search through `@zilliz/claude-context-mcp` 0.1.15 (Milvus
//! and an embedding server behind it), driven through [`McpToolCaller`]. Findings and recorded
//! outputs: `docs/spikes/s1-rmcp.md` and `tests/fixtures/claude_context/`.
//!
//! # How a call goes
//!
//! - Every call is about ONE repository: `path` is the absolute `repo.root`, one collection per
//!   absolute path.
//! - Before any tool call the adapter probes Milvus with a TCP connect (address from
//!   `config.env["MILVUS_ADDRESS"]`, else the process environment when `MILVUS_ADDRESS` is in
//!   `pass_env`, else `127.0.0.1:19530`; 300 ms). Without Milvus the server process crashes about
//!   150 ms after start and every call dies with `TransportClosed`, so a failed probe answers
//!   `Unavailable` and no tool is called.
//! - A closed transport (`TransportClosed`) reaches the adapter as `Unavailable` from the caller; the
//!   adapter adds that the server exits when Milvus is down.
//!
//! # Tools used
//!
//! - `search_code {path, query, limit}` with `limit = min(fetch_limit, 50)` (the server's cap).
//! - `get_indexing_status {path}` for [`SearchBackend::index_state`].
//! - `index_codebase {path, force, splitter: "ast", ignorePatterns, customExtensions}` to index.
//!   The server indexes in the background and returns at once.
//!
//! # `warm` and `refresh`
//!
//! - `warm(repos)` starts indexing (`force: false`) for repositories that are neither indexed nor
//!   being indexed (a failed earlier attempt is retried). It checks Milvus, starts one detached
//!   task and returns. The task handles the repositories one after another (each call syncs with
//!   Milvus inside the server, and one process owns one snapshot), each under
//!   [`WARM_REPO_TIMEOUT`], and logs the outcome per repository.
//! - `refresh(repo, full: true)` calls `index_codebase` with `force: true`: the server drops the
//!   collection and rebuilds it in the background.
//! - `refresh(repo, full: false)`: `handlers.js` has no tool for an incremental update. The server
//!   syncs every indexed codebase on its own every 5 minutes (`sync.js`), and it also watches the
//!   file `~/.context/.sync-trigger`: a change to it starts a sync of ALL indexed codebases after
//!   a 2 s debounce. The adapter writes the current time to that file (directory: `$HOME/.context`,
//!   override with [`ClaudeContextBackend::with_context_dir`]) and returns; it cannot wait for the
//!   sync. A repository that is not indexed yet is indexed instead, as in `warm`. The sync is
//!   skipped when the server runs with `CLAUDE_CONTEXT_TRIGGER_WATCHER=false`.
//!
//! # One process per `~/.context`
//!
//! claude-context reads and rewrites the global file `~/.context/mcp-codebase-snapshot.json` (and
//! `~/.context/merkle/`, `mcp-sync.lock`) and lists every codebase the user ever indexed. Two
//! claude-context processes against the same `~/.context` race on that file and corrupt each
//! other's state. The adapter cannot prevent this: it is the orchestrator's job to run at most one
//! claude-context process per home directory (and to reap orphans left by a killed parent).

mod parse;

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::json;
use tracing::{debug, warn};
use uals_core::backend::{BackendError, McpToolCaller, SearchBackend, ToolOutput};
use uals_core::config::ClaudeContextConfig;
use uals_core::types::{BackendAnswer, BackendId, Hit, IndexState, QueryKind, Repo, SearchRequest};

use crate::ast_index::PathFilter;

use self::parse::{
    IndexReply, ParsedResult, SearchReply, StatusReply, abbreviate, parse_index, parse_search, parse_status,
};

/// The server refuses more than 50 results (`Math.min(limit, 50)`, handlers.js:672).
const MAX_RESULTS: usize = 50;
const DEFAULT_MILVUS_ADDRESS: &str = "127.0.0.1:19530";
/// Port added to a `MILVUS_ADDRESS` that names none: Milvus' own port, or 443 behind `https://`.
const DEFAULT_MILVUS_PORT: u16 = 19530;
const HTTPS_PORT: u16 = 443;
/// The name resolution of the probe cannot be interrupted, so the probe stops waiting for it
/// this long after the connect budget.
const PROBE_RESOLVE_GRACE: Duration = Duration::from_millis(100);
const NOTE_STILL_INDEXING: &str = "claude-context is still indexing this repository, results may be incomplete";
const NOTE_RESULT_CAP: &str = "claude-context returns at most 50 results";
const MILVUS_ADDRESS_VAR: &str = "MILVUS_ADDRESS";
/// How long the Milvus probe waits. Milvus is local; a slower answer counts as down.
pub const MILVUS_PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// How long `warm` waits for the server to start indexing one repository before it moves on.
/// A hung call must not stall the repositories behind it; the server answers the first indexing of
/// a large repository in the background, so this only bounds a call that never answers.
pub const WARM_REPO_TIMEOUT: Duration = Duration::from_secs(600);
/// File the server watches to start a sync (`sync.js`, `setupTriggerWatcher`).
const SYNC_TRIGGER_FILE: &str = ".sync-trigger";

/// Semantic search over one repository at a time; one claude-context collection per repository.
#[derive(Clone)]
pub struct ClaudeContextBackend {
    caller: Arc<dyn McpToolCaller>,
    config: ClaudeContextConfig,
    context_dir: Option<PathBuf>,
    warm_timeout: Duration,
}

impl ClaudeContextBackend {
    /// Searches through the claude-context MCP server behind `caller`.
    pub fn new(caller: Arc<dyn McpToolCaller>, config: ClaudeContextConfig) -> Self {
        Self { caller, config, context_dir: None, warm_timeout: WARM_REPO_TIMEOUT }
    }

    /// Uses this directory instead of `$HOME/.context` for the sync trigger file. It must be the
    /// `.context` folder of the home directory the claude-context process runs with.
    #[must_use]
    pub fn with_context_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.context_dir = Some(dir.into());
        self
    }

    /// Gives each repository of `warm` this long instead of [`WARM_REPO_TIMEOUT`].
    #[must_use]
    pub fn with_warm_timeout(mut self, timeout: Duration) -> Self {
        self.warm_timeout = timeout;
        self
    }

    /// The `host:port` the Milvus probe connects to.
    pub fn milvus_address(&self) -> String {
        let configured = self.config.env.get(MILVUS_ADDRESS_VAR).cloned().or_else(|| {
            self.config
                .pass_env
                .iter()
                .any(|name| name == MILVUS_ADDRESS_VAR)
                .then(|| std::env::var(MILVUS_ADDRESS_VAR).ok())
                .flatten()
        });
        normalize_address(configured.as_deref().unwrap_or_default())
    }

    /// Checks that Milvus accepts TCP connections. `Err` holds the `Unavailable` message.
    pub async fn probe_milvus(&self) -> Result<(), String> {
        let address = self.milvus_address();
        match probe_tcp(&address, MILVUS_PROBE_TIMEOUT).await {
            Ok(()) => Ok(()),
            Err(why) => Err(format!("Milvus is not reachable at {address}: {why}")),
        }
    }

    /// `Unavailable` when Milvus does not accept connections.
    async fn ensure_milvus(&self) -> Result<(), BackendError> {
        self.probe_milvus().await.map_err(BackendError::Unavailable)
    }

    /// Calls a server tool; a dead server is reported as `Unavailable`.
    async fn call(&self, tool: &str, arguments: serde_json::Value) -> Result<ToolOutput, BackendError> {
        self.caller.call_tool(tool, arguments).await.map_err(add_milvus_hint_to_unavailable)
    }

    /// The indexing status of the repository.
    async fn status(&self, repo: &Repo) -> Result<StatusReply, BackendError> {
        let output = self.call("get_indexing_status", json!({ "path": root_str(repo)? })).await?;
        Ok(parse_status(&joined_text(&output)?, output.is_error))
    }

    /// Starts background indexing; "already indexed" and "already indexing" are fine.
    async fn start_indexing(&self, repo: &Repo, force: bool) -> Result<(), BackendError> {
        let arguments = json!({
            "path": root_str(repo)?,
            "force": force,
            "splitter": "ast",
            "ignorePatterns": self.config.ignore_patterns,
            "customExtensions": self.config.custom_extensions,
        });
        let output = self.call("index_codebase", arguments).await?;
        match parse_index(&joined_text(&output)?, output.is_error) {
            IndexReply::Started | IndexReply::AlreadyIndexing | IndexReply::AlreadyIndexed => Ok(()),
            IndexReply::Unreachable(why) => Err(BackendError::Unavailable(why)),
            IndexReply::Failed(why) => Err(BackendError::Failed(format!("index_codebase failed: {why}"))),
        }
    }

    /// Indexes the repository when it has no index and no index is being built.
    async fn index_if_missing(&self, repo: &Repo) -> Result<(), BackendError> {
        match self.status(repo).await? {
            StatusReply::NotIndexed | StatusReply::Failed { .. } => self.start_indexing(repo, false).await,
            StatusReply::Indexed { .. } | StatusReply::Indexing { .. } => Ok(()),
            StatusReply::Error(why) => Err(BackendError::Failed(format!("get_indexing_status failed: {why}"))),
        }
    }

    /// Starts indexing the repositories one after another and logs how each went.
    async fn warm_each(&self, repos: &[Repo]) {
        for repo in repos {
            match tokio::time::timeout(self.warm_timeout, self.index_if_missing(repo)).await {
                Ok(Ok(())) => debug!(repo = %repo.name, "claude-context index is started or ready"),
                Ok(Err(error)) => warn!(repo = %repo.name, %error, "claude-context warm failed"),
                Err(_) => warn!(repo = %repo.name, timeout = ?self.warm_timeout, "claude-context warm timed out"),
            }
        }
    }

    /// Asks the server for an incremental sync through its trigger file.
    async fn touch_sync_trigger(&self) -> Result<(), BackendError> {
        let dir = self
            .context_dir
            .clone()
            .or_else(|| home_dir().map(|home| home.join(".context")))
            .ok_or_else(|| BackendError::Failed("cannot find the home directory for ~/.context".into()))?;
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| BackendError::Failed(format!("cannot create {}: {e}", dir.display())))?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let file = dir.join(SYNC_TRIGGER_FILE);
        tokio::fs::write(&file, now.to_string())
            .await
            .map_err(|e| BackendError::Failed(format!("cannot write {}: {e}", file.display())))
    }
}

#[async_trait]
impl SearchBackend for ClaudeContextBackend {
    fn id(&self) -> BackendId {
        BackendId::ClaudeContext
    }

    fn supports(&self, kind: QueryKind) -> bool {
        matches!(kind, QueryKind::Identifier | QueryKind::Natural)
    }

    async fn search(&self, repo: &Repo, request: &SearchRequest) -> Result<BackendAnswer, BackendError> {
        self.ensure_milvus().await?;
        let filter = PathFilter::new(&repo.root, &request.paths)?;
        let limit = request.fetch_limit.clamp(1, MAX_RESULTS);
        let arguments = json!({ "path": root_str(repo)?, "query": request.query, "limit": limit });
        let output = self.call("search_code", arguments).await?;
        let text = joined_text(&output)?;
        match parse_search(&text, output.is_error) {
            SearchReply::Results { root, indexing, declared, results } => {
                if declared != results.len() {
                    warn!(declared, parsed = results.len(), "claude-context result count differs from its header");
                }
                let capped = request.fetch_limit > MAX_RESULTS && results.len() >= MAX_RESULTS;
                let base = root.map_or_else(|| repo.root.clone(), PathBuf::from);
                let mut hits = hits_in_repo(repo, &base, &filter, results);
                hits.truncate(request.fetch_limit);
                for (position, hit) in (0u32..).zip(hits.iter_mut()) {
                    hit.rank = position;
                }
                let notes: Vec<&str> = [indexing.then_some(NOTE_STILL_INDEXING), capped.then_some(NOTE_RESULT_CAP)]
                    .into_iter()
                    .flatten()
                    .collect();
                let partial = (!notes.is_empty()).then(|| notes.join("; "));
                Ok(BackendAnswer { hits, stale: None, partial })
            }
            SearchReply::NoResults { indexing } => Ok(BackendAnswer {
                hits: Vec::new(),
                stale: None,
                partial: indexing.then(|| NOTE_STILL_INDEXING.into()),
            }),
            SearchReply::NotIndexed => Err(BackendError::Unavailable(format!(
                "claude-context has not indexed {}; run `uals refresh` to index it",
                repo.root.display()
            ))),
            SearchReply::IndexLost => Err(BackendError::Unavailable(format!(
                "the claude-context index of {} is lost (its collection is missing in Milvus); run a full `uals refresh` to rebuild it",
                repo.root.display()
            ))),
            SearchReply::Unreachable(why) => Err(BackendError::Unavailable(format!(
                "claude-context cannot reach Milvus or the embedding service: {why}"
            ))),
            SearchReply::Failed(why) => Err(BackendError::Failed(format!("search_code failed: {why}"))),
        }
    }

    async fn index_state(&self, repo: &Repo) -> IndexState {
        let state = |indexed: bool, detail: Option<String>| IndexState {
            backend: BackendId::ClaudeContext,
            repo: repo.name.clone(),
            indexed,
            updated_at_ms: None,
            stale: None,
            detail,
        };
        if let Err(why) = self.probe_milvus().await {
            return state(false, Some(why));
        }
        match self.status(repo).await {
            Ok(StatusReply::Indexed { covered_by, stats }) => {
                let detail = match (stats, covered_by) {
                    (Some(stats), Some(root)) => Some(format!("{stats}; covered by the index of {root}")),
                    (Some(stats), None) => Some(stats),
                    (None, Some(root)) => Some(format!("covered by the index of {root}")),
                    (None, None) => None,
                };
                state(true, detail)
            }
            Ok(StatusReply::Indexing { percent }) => {
                // `percent` is 0..=100 with one decimal; show whole percents.
                let detail = percent.map_or_else(|| "indexing".to_string(), |p| format!("indexing {p:.0}%"));
                state(false, Some(detail))
            }
            Ok(StatusReply::Failed { error }) => state(false, Some(format!("indexing failed: {error}"))),
            Ok(StatusReply::NotIndexed) => state(false, Some("not indexed".into())),
            Ok(StatusReply::Error(why)) => state(false, Some(format!("claude-context status failed: {why}"))),
            Err(BackendError::Unavailable(why) | BackendError::Failed(why)) => state(false, Some(why)),
        }
    }

    async fn warm(&self, repos: &[Repo]) -> Result<(), BackendError> {
        if repos.is_empty() {
            return Ok(());
        }
        self.ensure_milvus().await?;
        let backend = self.clone();
        let repos = repos.to_vec();
        tokio::spawn(async move { backend.warm_each(&repos).await });
        Ok(())
    }

    async fn refresh(&self, repo: &Repo, full: bool) -> Result<(), BackendError> {
        self.ensure_milvus().await?;
        if full {
            return self.start_indexing(repo, true).await;
        }
        match self.status(repo).await? {
            StatusReply::NotIndexed | StatusReply::Failed { .. } => self.start_indexing(repo, false).await,
            StatusReply::Indexing { .. } => Ok(()),
            StatusReply::Indexed { .. } => self.touch_sync_trigger().await,
            StatusReply::Error(why) => Err(BackendError::Failed(format!("get_indexing_status failed: {why}"))),
        }
    }
}

/// The hits of one search reply that lie inside `repo` and the path filter. Paths in the reply are
/// relative to `base`, which can be a parent folder indexed as one codebase.
fn hits_in_repo(repo: &Repo, base: &Path, filter: &PathFilter, results: Vec<ParsedResult>) -> Vec<Hit> {
    let mut hits = Vec::with_capacity(results.len());
    for result in results {
        let path = base.join(&result.rel_path);
        let Ok(relative) = path.strip_prefix(&repo.root) else {
            debug!(path = %path.display(), "claude-context hit outside the repository");
            continue;
        };
        if !filter.allows(relative) {
            continue;
        }
        hits.push(Hit {
            backend: BackendId::ClaudeContext,
            repo: repo.name.clone(),
            path,
            lines: result.lines,
            symbol: None,
            symbol_kind: None,
            is_definition: false,
            snippet: result.code,
            rank: 0,
            score: None,
        });
    }
    hits
}

/// Connects to `address` (`host:port`) within `timeout`. `Err` holds the reason.
pub async fn probe_tcp(address: &str, timeout: Duration) -> Result<(), String> {
    let target = address.to_string();
    let attempt = tokio::task::spawn_blocking(move || {
        let mut last = "no address".to_string();
        for socket in target.to_socket_addrs().map_err(|e| e.to_string())? {
            match TcpStream::connect_timeout(&socket, timeout) {
                Ok(_) => return Ok(()),
                Err(e) => last = e.to_string(),
            }
        }
        Err(last)
    });
    // Name resolution cannot be interrupted; stop waiting for it after the same budget.
    match tokio::time::timeout(timeout + PROBE_RESOLVE_GRACE, attempt).await {
        Ok(Ok(result)) => result,
        Ok(Err(join)) => Err(join.to_string()),
        Err(_) => Err("timed out".into()),
    }
}

/// `MILVUS_ADDRESS` as `host:port`: removes a scheme, user info and path; adds the default port
/// and the brackets of a bare IPv6 address.
fn normalize_address(raw: &str) -> String {
    let raw = raw.trim();
    let (secure, rest) = match raw.split_once("://") {
        Some((scheme, rest)) => (scheme.eq_ignore_ascii_case("https"), rest),
        None => (false, raw),
    };
    let rest = rest.split('/').next().unwrap_or_default();
    let host = rest.rsplit('@').next().unwrap_or_default();
    if host.is_empty() {
        return DEFAULT_MILVUS_ADDRESS.to_string();
    }
    let port = if secure { HTTPS_PORT } else { DEFAULT_MILVUS_PORT };
    if host.starts_with('[') {
        return if host.contains("]:") { host.to_string() } else { format!("{host}:{port}") };
    }
    match host.matches(':').count() {
        0 => format!("{host}:{port}"),
        1 => host.to_string(),
        // More than one colon: a bare IPv6 address, which needs brackets before the port.
        _ => format!("[{host}]:{port}"),
    }
}

/// The repository root as the UTF-8 string the server takes.
fn root_str(repo: &Repo) -> Result<&str, BackendError> {
    repo.root
        .to_str()
        .ok_or_else(|| BackendError::Failed(format!("repository path is not valid UTF-8: {}", repo.root.display())))
}

/// The text blocks of a tool answer, joined; the server returns text only.
fn joined_text(output: &ToolOutput) -> Result<String, BackendError> {
    if output.texts.is_empty() {
        return Err(BackendError::Failed("claude-context returned no text".into()));
    }
    Ok(output.texts.join("\n"))
}

/// The caller reports a dead server as `Unavailable`; the message says why it matters here.
fn add_milvus_hint_to_unavailable(error: BackendError) -> BackendError {
    match error {
        BackendError::Unavailable(message) => BackendError::Unavailable(format!(
            "the claude-context server is not running ({}); it exits when Milvus is down",
            abbreviate(&message)
        )),
        other => other,
    }
}

/// The user's home directory (`HOME`, or `USERPROFILE` on Windows).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}
