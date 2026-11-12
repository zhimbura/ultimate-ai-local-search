//! Semble adapter: semantic search through Semble's MCP `search` tool. Findings and
//! recorded outputs: `docs/spikes/s1-rmcp.md` and `tests/fixtures/semble/`.
//!
//! Semble builds an index on the first `search` of a path and keeps it on disk, so there is no
//! separate "index" step: [`SearchBackend::warm`] simply runs a cheap search per repository.
//!
//! Given a list of paths, Semble merges their indexes and ranks the chunks of all of them in one
//! list: every `file_path` starts with a label of its repository, and `repos` maps the labels to the
//! resolved repository paths. So the adapter searches all repositories in one call
//! ([`SearchBackend::search_all`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::path_filter::PathFilter;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use uals_core::backend::{BackendError, McpToolCaller, SearchBackend, ToolOutput};
use uals_core::config::{SembleConfig, SembleContent};
use uals_core::types::{BackendAnswer, BackendId, Hit, IndexState, LineRange, QueryKind, Repo, SearchRequest};

/// With path globs, results are filtered after Semble ranked them, so ask for more.
const GLOB_OVERFETCH: usize = 4;
/// How many repositories `warm` indexes at once.
const WARM_CONCURRENCY: usize = 4;
/// How long one `warm` search may take. The first search of a large repository builds its index,
/// which takes minutes; a call that never answers must not hold a `warm` slot forever.
pub const WARM_SEARCH_TIMEOUT: Duration = Duration::from_secs(600);
/// The query used by `warm`: any text works, the point is to make Semble build the index.
const WARM_QUERY: &str = "main entry point";
/// `max_snippet_lines` value that makes Semble leave out the snippet text; the adapter reports
/// locations only.
const NO_SNIPPET_LINES: u32 = 0;

/// Semantic search over all repositories in one call.
pub struct SembleBackend {
    inner: Arc<Inner>,
    warm_timeout: Duration,
}

struct Inner {
    caller: Arc<dyn McpToolCaller>,
    config: SembleConfig,
    /// Repositories this process has searched successfully (so Semble has an index for them),
    /// with the Unix time in ms of the first success.
    indexed: Mutex<HashMap<PathBuf, u64>>,
}

impl SembleBackend {
    /// Searches through Semble's MCP `search` tool, called on `caller`.
    pub fn new(caller: Arc<dyn McpToolCaller>, config: SembleConfig) -> Self {
        Self {
            inner: Arc::new(Inner { caller, config, indexed: Mutex::new(HashMap::new()) }),
            warm_timeout: WARM_SEARCH_TIMEOUT,
        }
    }

    /// Gives each search of `warm` this long instead of [`WARM_SEARCH_TIMEOUT`].
    #[must_use]
    pub fn with_warm_timeout(mut self, timeout: Duration) -> Self {
        self.warm_timeout = timeout;
        self
    }
}

impl Inner {
    /// Calls Semble's `search` over `roots`, passed as one path for one root and as a list for
    /// several, and returns the parsed answer.
    async fn run_search(&self, roots: &[&Path], query: &str, top_k: usize) -> Result<SearchOutput, BackendError> {
        let repo = match roots {
            [root] => json!(root.to_string_lossy()),
            _ => json!(roots.iter().map(|root| root.to_string_lossy()).collect::<Vec<_>>()),
        };
        let mut arguments = json!({
            "query": query,
            "repo": repo,
            "top_k": top_k.max(1),
            "max_snippet_lines": NO_SNIPPET_LINES,
        });
        if let (Some(content), Some(map)) = (self.config.content, arguments.as_object_mut()) {
            map.insert("content".to_owned(), Value::String(content_name(content).to_owned()));
        }
        let output = self.caller.call_tool("search", arguments).await?;
        let found = parse_search_output(&output)?;
        for root in roots {
            self.mark_indexed(root);
        }
        Ok(found)
    }

    /// Remembers the time of the first successful search of `root`.
    fn mark_indexed(&self, root: &Path) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|d| u64::try_from(d.as_millis()).ok());
        if let (Ok(mut indexed), Some(now)) = (self.indexed.lock(), now) {
            indexed.entry(root.to_path_buf()).or_insert(now);
        }
    }
}

/// The name Semble's `content` argument expects.
fn content_name(content: SembleContent) -> &'static str {
    match content {
        SembleContent::Code => "code",
        SembleContent::Docs => "docs",
        SembleContent::Config => "config",
        SembleContent::All => "all",
    }
}

#[async_trait]
impl SearchBackend for SembleBackend {
    fn id(&self) -> BackendId {
        BackendId::Semble
    }

    fn supports(&self, kind: QueryKind) -> bool {
        matches!(kind, QueryKind::Identifier | QueryKind::Natural)
    }

    async fn search(&self, repo: &Repo, request: &SearchRequest) -> Result<BackendAnswer, BackendError> {
        self.search_all(std::slice::from_ref(repo), request).await
    }

    fn searches_all_at_once(&self) -> bool {
        true
    }

    async fn search_all(&self, repos: &[Repo], request: &SearchRequest) -> Result<BackendAnswer, BackendError> {
        let Some(first) = repos.first() else {
            return Ok(BackendAnswer::default());
        };
        let filter = PathFilter::new(&request.paths)?;
        let top_k =
            if filter.is_active() { request.fetch_limit.saturating_mul(GLOB_OVERFETCH) } else { request.fetch_limit };
        // Semble drops repeated paths too; it labels the paths of its answer only for several roots.
        let mut roots: Vec<&Path> = Vec::with_capacity(repos.len());
        for repo in repos {
            if !roots.contains(&repo.root.as_path()) {
                roots.push(&repo.root);
            }
        }
        let found = self.inner.run_search(&roots, &request.query, top_k).await?;
        let owners = if roots.len() == 1 { Owners::One(first) } else { Owners::by_label(repos, found.repos).await? };

        let mut hits = Vec::new();
        for result in found.results {
            if hits.len() == request.fetch_limit {
                break;
            }
            let (repo, relative) = owners.locate(&result.file_path)?;
            if !filter.allows(relative) {
                continue;
            }
            let path = repo.root.join(relative);
            let rank = u32::try_from(hits.len()).unwrap_or(u32::MAX);
            hits.push(Hit {
                backend: BackendId::Semble,
                repo: repo.name.clone(),
                path,
                lines: LineRange::new(result.start_line.max(1), result.end_line.max(1)),
                symbol: None,
                symbol_kind: None,
                is_definition: false,
                snippet: result.content.filter(|content| !content.is_empty()),
                rank,
                score: result.score,
            });
        }
        Ok(BackendAnswer { hits, stale: None, partial: None })
    }

    /// Reports only what this process knows: whether it already searched the repository (that
    /// builds the index). Semble keeps indexes in its own cache folder keyed by a hash of the
    /// path; reading it would couple uals to Semble internals, and asking Semble would index.
    async fn index_state(&self, repo: &Repo) -> IndexState {
        let known = self.inner.indexed.lock().ok().and_then(|indexed| indexed.get(&repo.root).copied());
        let (indexed, detail) = match known {
            Some(_) => (true, "indexed by Semble in this session; whether the files changed since is not checked"),
            None => (
                false,
                "not checked: Semble builds the index on the first search and its on-disk cache is not inspected, \
                 so an index from an earlier run may exist",
            ),
        };
        IndexState {
            backend: BackendId::Semble,
            repo: repo.name.clone(),
            indexed,
            updated_at_ms: known,
            stale: None,
            detail: Some(detail.to_owned()),
        }
    }

    /// Starts a one-result search per repository in the background (a few at a time) so Semble
    /// builds the indexes; returns at once.
    async fn warm(&self, repos: &[Repo]) -> Result<(), BackendError> {
        let permits = Arc::new(Semaphore::new(WARM_CONCURRENCY));
        for repo in repos {
            let inner = Arc::clone(&self.inner);
            let repo = repo.clone();
            let permits = Arc::clone(&permits);
            let timeout = self.warm_timeout;
            tokio::spawn(async move {
                let Ok(_permit) = permits.acquire().await else { return };
                match tokio::time::timeout(timeout, inner.run_search(&[repo.root.as_path()], WARM_QUERY, 1)).await {
                    Ok(Ok(_)) => tracing::debug!(repo = %repo.name, "semble index is ready"),
                    Ok(Err(error)) => tracing::debug!(repo = %repo.name, %error, "semble warm-up failed"),
                    Err(_) => tracing::debug!(repo = %repo.name, ?timeout, "semble warm-up timed out"),
                }
            });
        }
        Ok(())
    }
}

/// Which repository each `file_path` of one answer belongs to.
enum Owners<'a> {
    /// One root was searched: the paths are relative to its root.
    One(&'a Repo),
    /// Several roots were searched: every path starts with the label of its repository.
    ByLabel(HashMap<String, &'a Repo>),
}

impl<'a> Owners<'a> {
    /// Finds the repository of every label in `labels` (label → resolved repository path).
    async fn by_label(repos: &'a [Repo], labels: HashMap<String, PathBuf>) -> Result<Self, BackendError> {
        let mut owners = HashMap::with_capacity(labels.len());
        for (label, source) in labels {
            let Some(repo) = repo_at(repos, &source).await else {
                return Err(BackendError::Failed(format!(
                    "semble: answered for {}, which was not asked",
                    source.display()
                )));
            };
            owners.insert(label, repo);
        }
        Ok(Self::ByLabel(owners))
    }

    /// The repository of `file_path` and the path relative to its root.
    fn locate<'p>(&self, file_path: &'p str) -> Result<(&'a Repo, &'p str), BackendError> {
        match self {
            Self::One(repo) => Ok((*repo, file_path)),
            Self::ByLabel(owners) => file_path
                .split_once('/')
                .and_then(|(label, relative)| owners.get(label).map(|repo| (*repo, relative)))
                .ok_or_else(|| {
                    BackendError::Failed(format!("semble: {file_path} starts with no known repository label"))
                }),
        }
    }
}

/// The repository whose root is `source`. Semble resolves the paths it gets, so a root that
/// goes through a symbolic link matches by its canonical path.
async fn repo_at<'a>(repos: &'a [Repo], source: &Path) -> Option<&'a Repo> {
    if let Some(repo) = repos.iter().find(|repo| repo.root == source) {
        return Some(repo);
    }
    for repo in repos {
        if tokio::fs::canonicalize(&repo.root).await.is_ok_and(|root| root == source) {
            return Some(repo);
        }
    }
    None
}

/// Semble's answer to `search`.
#[derive(Debug, Default, Deserialize)]
struct SearchOutput {
    results: Vec<RawResult>,
    /// Only when several roots were searched: the label that starts every `file_path`, mapped to
    /// the resolved path of its repository.
    #[serde(default)]
    repos: HashMap<String, PathBuf>,
}

/// One entry of Semble's `results` array.
#[derive(Debug, Deserialize)]
struct RawResult {
    file_path: String,
    start_line: u32,
    end_line: u32,
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    content: Option<String>,
}

/// Turns the tool output into an answer, mapping Semble's failure shapes to `BackendError`.
fn parse_search_output(output: &ToolOutput) -> Result<SearchOutput, BackendError> {
    let text = result_text(output);
    if output.is_error {
        return Err(BackendError::Failed(format!("semble: {}", first_line(text.as_deref().unwrap_or("tool error")))));
    }
    let Some(text) = text else {
        return Err(BackendError::Failed("semble: the answer has no text".to_owned()));
    };
    // Failures such as "Failed to index ..." come as plain text with `isError: false`.
    if !text.trim_start().starts_with('{') {
        return Err(BackendError::Unavailable(format!("semble: {}", first_line(&text))));
    }
    let value = parse_inner_json(&text)?;
    if value.get("results").is_some() {
        return serde_json::from_value(value)
            .map_err(|error| BackendError::Failed(format!("semble: unexpected result shape: {error}")));
    }
    // `{"error": "No results found."}`
    if value.get("error").is_some() {
        return Ok(SearchOutput::default());
    }
    Err(BackendError::Failed("semble: the answer has neither results nor error".to_owned()))
}

/// `structuredContent.result` (a JSON string) if present, else the first text item.
fn result_text(output: &ToolOutput) -> Option<String> {
    let structured = output.structured.as_ref().and_then(|s| s.get("result")).and_then(Value::as_str);
    structured.or_else(|| output.texts.first().map(String::as_str)).map(str::to_owned)
}

/// Parses the JSON string; when it holds yet another JSON string (double encoding), parses that too.
fn parse_inner_json(text: &str) -> Result<Value, BackendError> {
    let mut value: Value = serde_json::from_str(text)
        .map_err(|error| BackendError::Failed(format!("semble: unparsable answer: {error}")))?;
    if let Value::String(inner) = &value {
        value = serde_json::from_str(inner)
            .map_err(|error| BackendError::Failed(format!("semble: unparsable inner answer: {error}")))?;
    }
    Ok(value)
}

/// The first non-blank line, trimmed.
fn first_line(text: &str) -> &str {
    text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("")
}
