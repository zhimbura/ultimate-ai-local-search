//! ast-index adapter (track T2): runs the `ast-index` CLI with the repository root as the working
//! directory, never anywhere else (from another folder it walks up to the nearest marker and may
//! create stub cache folders). Findings and recorded outputs: `docs/spikes/s2-ast-index.md` and
//! `tests/fixtures/ast_index/`.

mod parse;
mod runner;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ignore::overrides::{Override, OverrideBuilder};
use uals_core::backend::{BackendError, SearchBackend, SpanProvider, UsageSource};
use uals_core::classify::bare_name;
use uals_core::config::AstIndexConfig;
use uals_core::types::{
    BackendAnswer, BackendId, Hit, IndexState, LineRange, QueryKind, Repo, SearchRequest, SymbolKind, SymbolSpan,
    Usage, UsageAnswer,
};

use parse::{
    Explore, QueryResult, STATE_SQL, SpanRow, StateRow, SymbolList, UsageList, normalize_kind, parse_json, spans_sql,
};
pub use runner::{AstIndexRunner, CommandOutput, ProcessRunner};

/// Files per `IN (...)` list in one `spans` query.
const SPAN_CHUNK: usize = 500;
/// Definitions requested from `symbol`; ast-index returns exact, prefix, then substring matches.
const DEFINITION_LIMIT: usize = 200;
/// Usages requested when a path filter may drop some of them.
const FILTERED_USAGE_LIMIT: usize = 1000;
/// Row limit for `query`; the CLI default of 100 would cut a span list.
const QUERY_LIMIT: &str = "1000000";

/// Symbols, usages and symbol spans from ast-index.
pub struct AstIndexBackend {
    runner: Arc<dyn AstIndexRunner>,
}

impl AstIndexBackend {
    pub fn new(config: AstIndexConfig) -> Self {
        Self { runner: Arc::new(ProcessRunner::new(config.binary)) }
    }

    /// Builds the backend over another runner. Tests use it to feed recorded outputs.
    pub fn with_runner(runner: Arc<dyn AstIndexRunner>) -> Self {
        Self { runner }
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

    async fn run_query<T: serde::de::DeserializeOwned>(&self, repo: &Repo, sql: &str) -> Result<Vec<T>, BackendError> {
        let result: QueryResult<T> =
            self.run_json(repo, "query", &["query", sql, "--limit", QUERY_LIMIT, "--format", "json"]).await?;
        Ok(result.rows)
    }

    async fn identifier_hits(&self, repo: &Repo, request: &SearchRequest) -> Result<Vec<Hit>, BackendError> {
        let name = bare_name(&request.query);
        let limit = request.fetch_limit;
        if name.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let filter = PathFilter::new(repo, &request.paths)?;
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
        definitions.truncate(limit);
        self.fill_end_lines(repo, &mut definitions).await;

        let mut hits: Vec<Hit> = definitions
            .iter()
            .map(|definition| {
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
            })
            .collect();

        let remaining = limit - hits.len();
        if remaining > 0 {
            let usages_limit = if filter.is_active() { FILTERED_USAGE_LIMIT } else { remaining + hits.len() };
            let usages = self.usage_items(repo, name, usages_limit).await?;
            let taken: HashSet<(String, u32)> = definitions.iter().map(|d| (d.path.clone(), d.line)).collect();
            let mut usage_hits: Vec<_> = usages
                .items
                .into_iter()
                .filter(|item| filter.allows(&item.path) && !taken.contains(&(item.path.clone(), item.line)))
                .collect();
            // Production code before tests; ast-index's order otherwise.
            usage_hits.sort_by_key(|item| item.test);
            usage_hits.truncate(remaining);
            hits.extend(usage_hits.into_iter().map(|item| Hit {
                backend: BackendId::AstIndex,
                repo: repo.name.clone(),
                path: repo.root.join(&item.path),
                lines: LineRange::line(item.line),
                symbol: Some(name.to_string()),
                symbol_kind: None,
                is_definition: false,
                snippet: item.context,
                rank: 0,
                score: None,
            }));
        }
        assign_ranks(&mut hits);
        Ok(hits)
    }

    /// `symbol` has no `end_line`; one `query` over the definition files adds it. A failure only
    /// costs the extent, so the definition keeps its single line.
    async fn fill_end_lines(&self, repo: &Repo, definitions: &mut [Definition]) {
        if definitions.is_empty() {
            return;
        }
        let mut paths: Vec<String> = definitions.iter().map(|definition| definition.path.clone()).collect();
        paths.sort();
        paths.dedup();
        match self.run_query::<SpanRow>(repo, &spans_sql(&paths)).await {
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

    async fn natural_hits(&self, repo: &Repo, request: &SearchRequest) -> Result<Vec<Hit>, BackendError> {
        if request.fetch_limit == 0 || request.query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let filter = PathFilter::new(repo, &request.paths)?;
        let explored: Explore =
            self.run_json(repo, "explore", &["explore", "--format", "json", "--", request.query.trim()]).await?;
        let mut hits: Vec<Hit> = explored
            .symbols
            .iter()
            .filter_map(|symbol| {
                let kind = normalize_kind(&symbol.kind)?;
                if !filter.allows(&symbol.path) {
                    return None;
                }
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
            })
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

/// `SearchRequest::paths`: globs relative to the repository root.
struct PathFilter {
    overrides: Option<Override>,
}

impl PathFilter {
    fn new(repo: &Repo, globs: &[String]) -> Result<Self, BackendError> {
        if globs.is_empty() {
            return Ok(Self { overrides: None });
        }
        let mut builder = OverrideBuilder::new(&repo.root);
        for glob in globs {
            builder.add(glob).map_err(|error| BackendError::Failed(format!("invalid path glob `{glob}`: {error}")))?;
        }
        let overrides =
            builder.build().map_err(|error| BackendError::Failed(format!("invalid path globs: {error}")))?;
        Ok(Self { overrides: Some(overrides) })
    }

    fn is_active(&self) -> bool {
        self.overrides.is_some()
    }

    /// True when `relative` (a file path as ast-index prints it) or one of its folders matches.
    fn allows(&self, relative: &str) -> bool {
        let Some(overrides) = &self.overrides else {
            return true;
        };
        let path = Path::new(relative);
        if overrides.matched(path, false).is_whitelist() {
            return true;
        }
        path.ancestors()
            .skip(1)
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .any(|ancestor| overrides.matched(ancestor, true).is_whitelist())
    }
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
        let hits = match request.kind {
            QueryKind::Identifier => self.identifier_hits(repo, request).await?,
            QueryKind::Natural => self.natural_hits(repo, request).await?,
            QueryKind::Regex | QueryKind::Literal => {
                return Err(BackendError::Unavailable(
                    "ast-index does not search regular expressions or literals".into(),
                ));
            }
        };
        Ok(BackendAnswer { hits, stale: None, partial: None })
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
        let args = ["query".to_string(), STATE_SQL.to_string(), "--format".to_string(), "json".to_string()];
        let result = match self.runner.run(&repo.root, &args).await {
            Ok(output) => parse_json::<QueryResult<StateRow>>("query", &output),
            Err(error) => Err(error),
        };
        match result {
            Ok(result) => match result.rows.into_iter().next() {
                Some(row) => {
                    state.indexed = true;
                    state.updated_at_ms = row.updated.and_then(|value| value.trim().parse().ok());
                    state.detail = Some(format!("{} files, {} symbols", row.files, row.symbols));
                }
                None => state.detail = Some("index metadata is empty".into()),
            },
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
            let reason = output.stderr.trim().lines().next().unwrap_or("no error text").to_string();
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
        let name = bare_name(symbol);
        if name.is_empty() {
            return Ok(UsageAnswer { usages: Vec::new(), complete: true, stale: None });
        }
        if limit == 0 {
            return Ok(UsageAnswer { usages: Vec::new(), complete: false, stale: None });
        }
        let list = self.usage_items(repo, name, limit).await?;
        let complete = !list.pagination.truncated && list.items.len() <= limit;
        let usages = list
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
        Ok(UsageAnswer { usages, complete, stale: None })
    }
}

#[async_trait]
impl SpanProvider for AstIndexBackend {
    async fn spans(&self, repo: &Repo, files: &[PathBuf]) -> Result<Vec<SymbolSpan>, BackendError> {
        let mut relative: Vec<String> = files.iter().filter_map(|file| relative_path(&repo.root, file)).collect();
        relative.sort();
        relative.dedup();
        let mut spans = Vec::new();
        for chunk in relative.chunks(SPAN_CHUNK) {
            for row in self.run_query::<SpanRow>(repo, &spans_sql(chunk)).await? {
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
