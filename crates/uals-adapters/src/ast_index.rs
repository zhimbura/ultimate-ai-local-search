//! ast-index adapter: runs the `ast-index` CLI with the repository root as the working
//! directory, never anywhere else (from another folder it walks up to the nearest marker and may
//! create stub cache folders). Findings and recorded outputs: `docs/spikes/s2-ast-index.md` and
//! `tests/fixtures/ast_index/`.
//!
//! The adapter is coupled to ast-index internals: it computes the cache path of the database
//! (`index_db_path`, the djb2 hash of the root) and queries its SQLite tables (`files`,
//! `symbols`, `metadata`). Checked against ast-index 3.56.0; the live tests catch a layout change.

mod parse;
mod runner;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use uals_core::backend::{BackendError, SearchBackend, SpanProvider, UsageSource};
use uals_core::classify::bare_name;
use uals_core::config::AstIndexConfig;
use uals_core::types::{
    BackendAnswer, BackendId, Hit, IndexState, LineRange, QueryKind, Repo, SearchRequest, SymbolKind, SymbolSpan,
    Usage, UsageAnswer,
};

use crate::path_filter::PathFilter;
use parse::{
    Explore, ExploreSymbol, FileStampRow, QueryResult, STATE_SQL, SpanRow, StateRow, SymbolList, UsageItem, UsageList,
    definition_extents_sql, file_stamps_sql, missing_index, normalize_kind, parse_json, spans_sql,
};
use runner::require_folder;
pub use runner::{AstIndexRunner, CommandOutput, ProcessRunner};

/// Files per `IN (...)` list in one `spans` query.
const SPAN_CHUNK: usize = 500;
/// Row limit of the `spans` query: every symbol of up to [`SPAN_CHUNK`] files is wanted, and
/// the CLI's default of 100 would cut the list.
const SPAN_ROW_LIMIT: usize = 1_000_000;
/// Definitions requested from `symbol`; ast-index returns exact, prefix, then substring matches.
const DEFINITION_LIMIT: usize = 200;
/// Usages requested when a path filter may drop some of them.
const FILTERED_USAGE_LIMIT: usize = 1000;

/// Symbols, usages and symbol spans from ast-index.
pub struct AstIndexBackend {
    runner: Arc<dyn AstIndexRunner>,
}

impl AstIndexBackend {
    /// Runs the `ast-index` binary named in `config`.
    pub fn new(config: AstIndexConfig) -> Self {
        Self { runner: Arc::new(ProcessRunner::new(config.binary)) }
    }

    /// Builds the backend over another runner. Tests use it to feed recorded outputs.
    pub fn with_runner(runner: Arc<dyn AstIndexRunner>) -> Self {
        Self { runner }
    }

    /// `Unavailable` when `repo` has no index. Without one, ast-index answers some commands with an
    /// empty list, as if the symbol were not used, and creates a stub cache folder for the
    /// repository; so nothing is run. A missing folder is named as the cause.
    fn require_index(&self, repo: &Repo) -> Result<(), BackendError> {
        if self.runner.index_exists(&repo.root) {
            return Ok(());
        }
        require_folder(&repo.root)?;
        Err(missing_index())
    }

    async fn run_json<T: serde::de::DeserializeOwned>(
        &self,
        repo: &Repo,
        command: &str,
        args: &[&str],
    ) -> Result<T, BackendError> {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        let output = self.runner.run(&repo.root, &args).await?;
        if !output.success() {
            tracing::debug!(repo = %repo.name, command, exit_code = ?output.exit_code, "ast-index exited with an error");
        }
        parse_json(command, &output)
    }

    /// Runs `sql` and returns at most `limit` rows; the CLI's default of 100 would cut a long list.
    async fn run_query<T: serde::de::DeserializeOwned>(
        &self,
        repo: &Repo,
        sql: &str,
        limit: usize,
    ) -> Result<Vec<T>, BackendError> {
        let limit = limit.to_string();
        let result: QueryResult<T> =
            self.run_json(repo, "query", &["query", sql, "--limit", &limit, "--format", "json"]).await?;
        Ok(result.rows)
    }

    /// Definitions of the bare name first (best name match, then types before functions), then
    /// usages that are not definition lines, production code before tests.
    async fn identifier_hits(&self, repo: &Repo, request: &SearchRequest) -> Result<Vec<Hit>, BackendError> {
        let name = bare_name(&request.query);
        let limit = request.fetch_limit;
        if name.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let filter = PathFilter::new(&request.paths)?;
        let mut definitions = self.find_definitions(repo, name, &filter).await?;
        definitions.truncate(limit);
        self.fill_end_lines(repo, &mut definitions).await;

        let mut hits: Vec<Hit> = definitions.iter().map(|definition| definition_hit(repo, definition)).collect();
        let remaining = limit - hits.len();
        if remaining > 0 {
            let usages_limit = if filter.is_active() { FILTERED_USAGE_LIMIT } else { limit };
            let taken: HashSet<(&str, u32)> = definitions.iter().map(|d| (d.path.as_str(), d.line)).collect();
            let usages = self.usage_items(repo, name, usages_limit).await?;
            let mut usage_items: Vec<UsageItem> = usages
                .items
                .into_iter()
                .filter(|item| filter.allows(&item.path) && !taken.contains(&(item.path.as_str(), item.line)))
                .collect();
            // Production code before tests; ast-index's order otherwise.
            usage_items.sort_by_key(|item| item.test);
            usage_items.truncate(remaining);
            hits.extend(usage_items.into_iter().map(|item| usage_hit(repo, name, item)));
        }
        assign_ranks(&mut hits);
        Ok(hits)
    }

    /// The distinct definitions `symbol` finds for `name` inside the filter, best match first.
    async fn find_definitions(
        &self,
        repo: &Repo,
        name: &str,
        filter: &PathFilter,
    ) -> Result<Vec<Definition>, BackendError> {
        let definitions_limit = DEFINITION_LIMIT.to_string();
        let found: SymbolList = self
            .run_json(
                repo,
                "symbol",
                &["symbol", "--fuzzy", "--limit", &definitions_limit, "--format", "json", "--", name],
            )
            .await?;

        let mut seen = HashSet::new();
        let mut definitions: Vec<Definition> = found
            .items
            .into_iter()
            .filter_map(|item| {
                let kind = normalize_kind(&item.kind)?;
                (filter.allows(&item.path) && seen.insert((item.path.clone(), item.line, item.name.clone())))
                    .then_some(Definition { name: item.name, kind, line: item.line, end_line: None, path: item.path })
            })
            .collect();
        definitions.sort_by_key(|definition| (name_tier(&definition.name, name), kind_class(definition.kind)));
        Ok(definitions)
    }

    /// `symbol` has no `end_line`; one `query` over the definition files adds it. A failure only
    /// costs the extent, so the definition keeps its single line.
    async fn fill_end_lines(&self, repo: &Repo, definitions: &mut [Definition]) {
        if definitions.is_empty() {
            return;
        }
        let wanted: Vec<(&str, &str, u32)> =
            definitions.iter().map(|d| (d.path.as_str(), d.name.as_str(), d.line)).collect();
        match self.run_query::<SpanRow>(repo, &definition_extents_sql(&wanted), wanted.len()).await {
            Ok(rows) => {
                for definition in definitions {
                    definition.end_line = rows
                        .iter()
                        .find(|row| {
                            row.path == definition.path && row.name == definition.name && row.line == definition.line
                        })
                        .and_then(|row| row.end_line);
                }
            }
            Err(error) => {
                tracing::warn!(repo = %repo.name, %error, "cannot read symbol extents; definitions keep one line")
            }
        }
    }

    async fn usage_items(&self, repo: &Repo, name: &str, limit: usize) -> Result<UsageList, BackendError> {
        let limit = limit.to_string();
        self.run_json(repo, "usages", &["usages", "--limit", &limit, "--format", "json", "--", name]).await
    }

    /// The single metadata row of the index, `None` when the index holds no metadata.
    async fn read_state_row(&self, repo: &Repo) -> Result<Option<StateRow>, BackendError> {
        let args = ["query".to_string(), STATE_SQL.to_string(), "--format".to_string(), "json".to_string()];
        let output = self.runner.run(&repo.root, &args).await?;
        let result: QueryResult<StateRow> = parse_json("query", &output)?;
        Ok(result.rows.into_iter().next())
    }

    /// A reason when any of `files` differs on disk from what the index recorded (modification
    /// second or size, or gone), `None` when all match or the index cannot be asked.
    async fn stale_reason(&self, repo: &Repo, files: impl Iterator<Item = &Path>) -> Option<String> {
        let mut relative: Vec<String> = files.filter_map(|file| relative_path(&repo.root, file)).collect();
        relative.sort();
        relative.dedup();
        let mut changed = 0usize;
        for chunk in relative.chunks(SPAN_CHUNK) {
            let stamps = match self.run_query::<FileStampRow>(repo, &file_stamps_sql(chunk), chunk.len()).await {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::warn!(repo = %repo.name, %error, "cannot read file stamps; staleness is unknown");
                    return None;
                }
            };
            changed += stamps.iter().filter(|stamp| differs_on_disk(&repo.root, stamp)).count();
        }
        match changed {
            0 => None,
            1 => Some("1 file changed since the index was built".into()),
            count => Some(format!("{count} files changed since the index was built")),
        }
    }

    async fn natural_hits(&self, repo: &Repo, request: &SearchRequest) -> Result<Vec<Hit>, BackendError> {
        if request.fetch_limit == 0 || request.query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let filter = PathFilter::new(&request.paths)?;
        let explored: Explore =
            self.run_json(repo, "explore", &["explore", "--format", "json", "--", request.query.trim()]).await?;
        let mut hits: Vec<Hit> = explored
            .symbols
            .iter()
            .filter(|symbol| filter.allows(&symbol.path))
            .filter_map(|symbol| explore_hit(repo, &explored, symbol))
            .collect();
        hits.truncate(request.fetch_limit);
        assign_ranks(&mut hits);
        Ok(hits)
    }
}

/// A definition found by `symbol`, with the extent added from the index when known.
struct Definition {
    name: String,
    kind: SymbolKind,
    line: u32,
    end_line: Option<u32>,
    path: String,
}

/// The hit for one `explore` symbol: its extent comes from the outline of its file. `None` for
/// rows that are not definitions.
fn explore_hit(repo: &Repo, explored: &Explore, symbol: &ExploreSymbol) -> Option<Hit> {
    let kind = normalize_kind(&symbol.kind)?;
    let end = explored
        .files
        .iter()
        .filter(|file| file.path == symbol.path)
        .flat_map(|file| file.outline.iter())
        .find(|entry| entry.name == symbol.name && entry.line == symbol.line)
        .and_then(|entry| entry.end_line)
        .unwrap_or(symbol.line);
    Some(Hit {
        backend: BackendId::AstIndex,
        repo: repo.name.clone(),
        path: repo.root.join(&symbol.path),
        lines: LineRange::new(symbol.line, end),
        symbol: Some(symbol.name.clone()),
        symbol_kind: Some(kind),
        is_definition: true,
        snippet: None,
        rank: 0,
        score: symbol.score,
    })
}

fn definition_hit(repo: &Repo, definition: &Definition) -> Hit {
    let end = definition.end_line.unwrap_or(definition.line);
    Hit {
        backend: BackendId::AstIndex,
        repo: repo.name.clone(),
        path: repo.root.join(&definition.path),
        lines: LineRange::new(definition.line, end),
        symbol: Some(definition.name.clone()),
        symbol_kind: Some(definition.kind),
        is_definition: true,
        snippet: None,
        rank: 0,
        score: None,
    }
}

fn usage_hit(repo: &Repo, name: &str, item: UsageItem) -> Hit {
    Hit {
        backend: BackendId::AstIndex,
        repo: repo.name.clone(),
        path: repo.root.join(&item.path),
        lines: LineRange::line(item.line),
        symbol: Some(name.to_owned()),
        symbol_kind: None,
        is_definition: false,
        snippet: item.context,
        rank: 0,
        score: None,
    }
}

/// Sets each hit's rank to its position in the list.
fn assign_ranks(hits: &mut [Hit]) {
    for (position, hit) in hits.iter_mut().enumerate() {
        hit.rank = u32::try_from(position).unwrap_or(u32::MAX);
    }
}

/// 0 exact name, 1 prefix, 2 substring (the last two ignore case).
fn name_tier(name: &str, query: &str) -> u8 {
    if name == query {
        0
    } else if name.to_lowercase().starts_with(&query.to_lowercase()) {
        1
    } else {
        2
    }
}

/// Types first, then functions, then properties and constants.
fn kind_class(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Class | SymbolKind::Interface | SymbolKind::Object | SymbolKind::Enum | SymbolKind::TypeAlias => 0,
        SymbolKind::Function => 1,
        SymbolKind::Property | SymbolKind::Constant => 2,
        SymbolKind::Module | SymbolKind::Other => 3,
    }
}

/// The last line of `text` that is not blank, trimmed.
fn last_text_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|line| !line.is_empty())
}

/// Whether the file is gone or its modification second or size differs from the recorded stamp.
fn differs_on_disk(root: &Path, stamp: &FileStampRow) -> bool {
    let Ok(meta) = std::fs::metadata(root.join(&stamp.path)) else {
        return true;
    };
    let seconds = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|age| i64::try_from(age.as_secs()).ok());
    seconds != stamp.mtime || Some(meta.len()) != stamp.size
}

/// `path` relative to the repository root with `/` separators; `None` outside the root.
fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let parts: Vec<String> =
        relative.components().map(|part| part.as_os_str().to_string_lossy().into_owned()).collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[async_trait]
impl SearchBackend for AstIndexBackend {
    fn id(&self) -> BackendId {
        BackendId::AstIndex
    }

    fn supports(&self, kind: QueryKind) -> bool {
        matches!(kind, QueryKind::Identifier | QueryKind::Natural)
    }

    async fn search(&self, repo: &Repo, request: &SearchRequest) -> Result<BackendAnswer, BackendError> {
        self.require_index(repo)?;
        let hits = match request.kind {
            QueryKind::Identifier => self.identifier_hits(repo, request).await?,
            QueryKind::Natural => self.natural_hits(repo, request).await?,
            QueryKind::Regex | QueryKind::Literal => {
                return Err(BackendError::Unavailable(
                    "ast-index does not search regular expressions or literals".into(),
                ));
            }
        };
        let stale = self.stale_reason(repo, hits.iter().map(|hit| hit.path.as_path())).await;
        Ok(BackendAnswer { hits, stale, partial: None })
    }

    async fn index_state(&self, repo: &Repo) -> IndexState {
        let mut state = IndexState {
            backend: BackendId::AstIndex,
            repo: repo.name.clone(),
            indexed: false,
            updated_at_ms: None,
            stale: None,
            detail: None,
        };
        if !self.runner.index_exists(&repo.root) {
            state.detail = Some("no index; run `ast-index rebuild` in the repository".into());
            return state;
        }
        match self.read_state_row(repo).await {
            Ok(Some(row)) => {
                state.indexed = true;
                state.updated_at_ms = row.updated.and_then(|value| value.trim().parse().ok());
                state.detail = Some(format!("{} files, {} symbols", row.files, row.symbols));
            }
            Ok(None) => state.detail = Some("index metadata is empty".into()),
            Err(BackendError::Unavailable(reason) | BackendError::Failed(reason)) => state.detail = Some(reason),
        }
        state
    }

    async fn refresh(&self, repo: &Repo, full: bool) -> Result<(), BackendError> {
        let command = if full { "rebuild" } else { "update" };
        let output = self.runner.run(&repo.root, &[command.to_string()]).await?;
        if output.success() {
            Ok(())
        } else {
            let reason = last_text_line(&output.stderr).or_else(|| last_text_line(&output.stdout));
            let reason = reason.unwrap_or("no error text");
            Err(BackendError::Failed(format!("ast-index {command} exited with {:?}: {reason}", output.exit_code)))
        }
    }
}

#[async_trait]
impl UsageSource for AstIndexBackend {
    fn id(&self) -> BackendId {
        BackendId::AstIndex
    }

    async fn usages(&self, repo: &Repo, symbol: &str, limit: usize) -> Result<UsageAnswer, BackendError> {
        self.require_index(repo)?;
        let name = bare_name(symbol);
        if name.is_empty() {
            return Ok(UsageAnswer { usages: Vec::new(), complete: true, stale: None });
        }
        if limit == 0 {
            return Ok(UsageAnswer { usages: Vec::new(), complete: false, stale: None });
        }
        let list = self.usage_items(repo, name, limit).await?;
        let complete = !list.pagination.truncated && list.items.len() <= limit;
        let usages: Vec<Usage> = list
            .items
            .into_iter()
            .take(limit)
            .map(|item| Usage {
                repo: repo.name.clone(),
                path: repo.root.join(item.path),
                line: item.line,
                context: item.context,
            })
            .collect();
        let stale = self.stale_reason(repo, usages.iter().map(|usage| usage.path.as_path())).await;
        Ok(UsageAnswer { usages, complete, stale })
    }
}

#[async_trait]
impl SpanProvider for AstIndexBackend {
    async fn spans(&self, repo: &Repo, files: &[PathBuf]) -> Result<Vec<SymbolSpan>, BackendError> {
        self.require_index(repo)?;
        let mut relative: Vec<String> = files.iter().filter_map(|file| relative_path(&repo.root, file)).collect();
        relative.sort();
        relative.dedup();
        let mut spans = Vec::new();
        for chunk in relative.chunks(SPAN_CHUNK) {
            for row in self.run_query::<SpanRow>(repo, &spans_sql(chunk), SPAN_ROW_LIMIT).await? {
                let Some(kind) = normalize_kind(&row.kind) else {
                    continue;
                };
                spans.push(SymbolSpan {
                    repo: repo.name.clone(),
                    path: repo.root.join(&row.path),
                    name: row.name,
                    kind,
                    lines: LineRange::new(row.line, row.end_line.unwrap_or(row.line)),
                });
            }
        }
        Ok(spans)
    }
}
