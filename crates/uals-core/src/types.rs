//! Data types shared by every crate. Frozen: only the coordinator changes this file.
//!
//! Conventions:
//! - Paths in hits and spans are absolute (`Repo::root` joined with the backend's relative path).
//! - Line numbers are 1-based and inclusive.
//! - A backend's `rank` is the 0-based position in its own ranked list for ONE repository.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One repository found in the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Repo {
    /// Unique short name: the path relative to the workspace root ("android"),
    /// or the folder name when the workspace is a single repository.
    pub name: String,
    /// Canonical absolute path to the repository root.
    pub root: PathBuf,
}

/// A folder to search, with the repositories inside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub root: PathBuf,
    /// Sorted by name.
    pub repos: Vec<Repo>,
}

/// A search backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendId {
    AstIndex,
    Lexical,
    Semble,
    ClaudeContext,
}

impl BackendId {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendId::AstIndex => "ast-index",
            BackendId::Lexical => "lexical",
            BackendId::Semble => "semble",
            BackendId::ClaudeContext => "claude-context",
        }
    }

    /// Semble and claude-context share one fusion weight: they are alternatives for the same tier.
    pub fn is_semantic(self) -> bool {
        matches!(self, BackendId::Semble | BackendId::ClaudeContext)
    }
}

impl fmt::Display for BackendId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What kind of question the query is. See `classify::classify`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryKind {
    /// A code name such as `HostSelector`, `mark_failed` or `sample.core.Analytics`.
    Identifier,
    /// A question in prose: "where is the backend host chosen".
    Natural,
    /// A regular expression: `/class \w+Store/`.
    Regex,
    /// An exact string in quotes: `"host_switched"`.
    Literal,
}

/// A 1-based inclusive line range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

impl LineRange {
    /// Builds a range; swaps the bounds if they come in reverse order.
    pub fn new(start: u32, end: u32) -> Self {
        if start <= end { Self { start, end } } else { Self { start: end, end: start } }
    }

    pub fn line(line: u32) -> Self {
        Self { start: line, end: line }
    }

    /// Number of lines in the range; at least 1, a range is never empty.
    pub fn line_count(&self) -> u32 {
        self.end - self.start + 1
    }

    pub fn contains_line(&self, line: u32) -> bool {
        self.start <= line && line <= self.end
    }

    /// Number of lines both ranges share; 0 if they do not touch.
    pub fn overlap(&self, other: &LineRange) -> u32 {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        if start <= end { end - start + 1 } else { 0 }
    }

    /// Number of lines strictly between the ranges; 0 if they overlap or touch.
    pub fn gap(&self, other: &LineRange) -> u32 {
        if self.end < other.start {
            other.start - self.end - 1
        } else if other.end < self.start {
            self.start - other.end - 1
        } else {
            0
        }
    }

    /// The smallest range that covers both.
    pub fn union(&self, other: &LineRange) -> LineRange {
        LineRange { start: self.start.min(other.start), end: self.end.max(other.end) }
    }
}

/// Kind of a code symbol, normalized from the backend's own names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Class,
    Interface,
    Object,
    Enum,
    TypeAlias,
    Function,
    Property,
    Constant,
    Module,
    #[serde(other)]
    Other,
}

/// One result from one backend, normalized.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub backend: BackendId,
    /// `Repo::name`.
    pub repo: String,
    /// Absolute path.
    pub path: PathBuf,
    pub lines: LineRange,
    /// Name of the symbol the backend reported (definition name, or the matched name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<SymbolKind>,
    /// True when the hit is the definition of `symbol` (not a usage).
    #[serde(default)]
    pub is_definition: bool,
    /// Text the backend returned for the hit, if any. May be long; render trims it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// 0-based position in this backend's ranked list for this repository.
    pub rank: u32,
    /// The backend's own score, if it has one. Not comparable across backends or repositories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// What the orchestrator asks one backend for one repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchRequest {
    /// The query after `classify::normalize` (quotes and slashes removed).
    pub query: String,
    pub kind: QueryKind,
    /// How many hits to return for this repository (already multiplied by the fetch factor).
    pub fetch_limit: usize,
    /// Optional globs relative to the repository root. Empty means the whole repository.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// A successful backend answer for one repository.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BackendAnswer {
    /// Ordered best first; `Hit::rank` equals the position in this vector.
    pub hits: Vec<Hit>,
    /// Set when the answer came from an index that may be older than the files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<String>,
    /// Set when the backend stopped early (its own deadline or a result cap) and `hits` is
    /// only part of the answer. Adapters that run blocking work cannot be cancelled by dropping
    /// the future, so they stop on their own and report it here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<String>,
}

/// How one backend did for one repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BackendStatus {
    Ok,
    /// Answered from an index that may be older than the files.
    Stale {
        reason: String,
    },
    /// Answered with part of the results (deadline or cap). When the answer is also stale, the
    /// orchestrator reports `Partial` and adds the stale reason to `reason`.
    Partial {
        reason: String,
    },
    /// Did not answer before the deadline; its hits are missing.
    Timeout,
    /// Cannot answer: no index, service down, or not configured.
    Unavailable {
        reason: String,
    },
    /// Unexpected error, such as unparsable output.
    Failed {
        reason: String,
    },
}

/// The result of one backend for one repository, as collected by the orchestrator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendResult {
    pub backend: BackendId,
    pub repo: String,
    pub status: BackendStatus,
    #[serde(default)]
    pub hits: Vec<Hit>,
    #[serde(default)]
    pub elapsed_ms: u64,
}

/// The extent of a symbol in a file, used as the merge unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolSpan {
    pub repo: String,
    /// Absolute path.
    pub path: PathBuf,
    pub name: String,
    pub kind: SymbolKind,
    pub lines: LineRange,
}

/// A backend's contribution to a fused hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRef {
    pub backend: BackendId,
    /// The best (lowest) rank this backend gave to any hit in the group.
    pub rank: u32,
}

/// One merged result: several backend hits that point to the same code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FusedHit {
    pub repo: String,
    /// Absolute path.
    pub path: PathBuf,
    /// The merge unit: the symbol span, or the union of the grouped hit ranges.
    pub lines: LineRange,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<SymbolKind>,
    /// True when any grouped hit is a definition.
    pub is_definition: bool,
    /// True when the hit was pinned to the top as an exact definition of the query.
    pub pinned: bool,
    /// The line inside `lines` that the snippet should center on (the best-ranked hit's start).
    pub focus_line: u32,
    /// Filled by the orchestrator from the file on disk, for the lines `render::snippet_range` picks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<Snippet>,
    /// Fused score. Comparable only within one response.
    pub score: f64,
    /// Sorted by contribution, best first.
    pub sources: Vec<SourceRef>,
}

/// Source text of a fused hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    /// The lines `text` holds.
    pub lines: LineRange,
    /// The lines joined with `\n`, without a trailing newline. Long lines may be cut.
    pub text: String,
}

/// Status of one backend for one repository, reported with every response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendReport {
    pub backend: BackendId,
    pub repo: String,
    pub status: BackendStatus,
    pub elapsed_ms: u64,
}

/// The answer of the `search` tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub kind: QueryKind,
    /// Number of fused hits before pagination.
    pub total: usize,
    pub offset: usize,
    pub hits: Vec<FusedHit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    pub backends: Vec<BackendReport>,
}

/// One place where a symbol is used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub repo: String,
    /// Absolute path.
    pub path: PathBuf,
    pub line: u32,
    /// The source line, trimmed, if the backend gave it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// A usage source's answer for one repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageAnswer {
    pub usages: Vec<Usage>,
    /// False when the source hit its limit or could not search everything.
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<String>,
}

/// Usages found in one repository, and which source found them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoUsages {
    pub repo: String,
    pub source: BackendId,
    pub status: BackendStatus,
    pub usages: Vec<Usage>,
}

/// The answer of the `usages` tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsagesResponse {
    pub symbol: String,
    /// Number of usages before pagination, over all repositories.
    pub total: usize,
    pub offset: usize,
    pub repos: Vec<RepoUsages>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    /// True when every repository answered completely.
    pub complete: bool,
}

/// State of one backend's index for one repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexState {
    pub backend: BackendId,
    pub repo: String,
    pub indexed: bool,
    /// Unix time in milliseconds of the last index update, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<u64>,
    /// Set when the index may be older than the files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<String>,
    /// Free text: "indexing 40%", "not configured", "Milvus is not reachable".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_range_math() {
        let a = LineRange::new(10, 20);
        let b = LineRange::new(15, 30);
        let c = LineRange::new(24, 25);
        assert_eq!(a.line_count(), 11);
        assert_eq!(a.overlap(&b), 6);
        assert_eq!(a.overlap(&c), 0);
        assert_eq!(a.gap(&c), 3);
        assert_eq!(c.gap(&a), 3);
        assert_eq!(a.gap(&b), 0);
        assert_eq!(a.union(&c), LineRange::new(10, 25));
        assert_eq!(LineRange::new(5, 2), LineRange::new(2, 5));
        assert!(LineRange::line(7).contains_line(7));
    }

    #[test]
    fn backend_ids_use_kebab_case_names() {
        assert_eq!(serde_json::to_string(&BackendId::ClaudeContext).unwrap(), "\"claude-context\"");
        assert_eq!(BackendId::AstIndex.to_string(), "ast-index");
        assert!(BackendId::Semble.is_semantic());
        assert!(!BackendId::Lexical.is_semantic());
    }

    #[test]
    fn unknown_symbol_kind_becomes_other() {
        let kind: SymbolKind = serde_json::from_str("\"annotation\"").unwrap();
        assert_eq!(kind, SymbolKind::Other);
    }

    #[test]
    fn status_is_tagged() {
        let json = serde_json::to_string(&BackendStatus::Unavailable { reason: "no index".into() }).unwrap();
        assert_eq!(json, r#"{"state":"unavailable","reason":"no index"}"#);
    }
}
