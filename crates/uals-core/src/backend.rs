//! Traits that adapters implement. Frozen: only the coordinator changes this file.
//!
//! Every method works on ONE repository. The orchestrator runs repositories and backends in
//! parallel, applies the deadlines from `config::Deadlines`, and turns errors and timeouts into
//! `BackendStatus`. Implementations must be cancel-safe: the orchestrator drops the future on
//! timeout, and any child process started for the call must die with it (`kill_on_drop`).

use std::path::PathBuf;

use async_trait::async_trait;
use thiserror::Error;

use crate::types::{BackendAnswer, BackendId, IndexState, QueryKind, Repo, SearchRequest, SymbolSpan, UsageAnswer};

/// Why a backend could not answer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BackendError {
    /// Expected and recoverable: no index, service down, not configured, binary missing.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Unexpected: unparsable output, protocol error, crash.
    #[error("failed: {0}")]
    Failed(String),
}

/// A backend that answers `search` queries.
#[async_trait]
pub trait SearchBackend: Send + Sync {
    fn id(&self) -> BackendId;

    /// Whether this backend should be asked at all for this kind of query.
    fn supports(&self, kind: QueryKind) -> bool;

    /// Searches one repository. Hits must be ordered best first, with `rank` equal to the position.
    async fn search(&self, repo: &Repo, request: &SearchRequest) -> Result<BackendAnswer, BackendError>;

    /// Reports the state of this backend's index for one repository. Must be fast and must not
    /// change anything (no indexing, no stub cache folders).
    async fn index_state(&self, repo: &Repo) -> IndexState;

    /// Prepares the backend for these repositories: starts child processes, starts indexing in
    /// the background. Returns quickly; long work continues in the background.
    async fn warm(&self, _repos: &[Repo]) -> Result<(), BackendError> {
        Ok(())
    }

    /// Brings the index of one repository up to date. `full` rebuilds it from scratch.
    async fn refresh(&self, _repo: &Repo, _full: bool) -> Result<(), BackendError> {
        Ok(())
    }
}

/// A source that lists every usage of a symbol (the `usages` tool). Completeness matters more
/// than ranking here.
#[async_trait]
pub trait UsageSource: Send + Sync {
    fn id(&self) -> BackendId;

    async fn usages(&self, repo: &Repo, symbol: &str, limit: usize) -> Result<UsageAnswer, BackendError>;
}

/// Gives symbol extents so the merge step can group hits by the symbol they fall into.
#[async_trait]
pub trait SpanProvider: Send + Sync {
    /// Returns the spans of all symbols in the given files (absolute paths inside `repo`),
    /// in one round trip. Files without spans are simply absent from the result.
    async fn spans(&self, repo: &Repo, files: &[PathBuf]) -> Result<Vec<SymbolSpan>, BackendError>;
}

/// The result of one MCP `tools/call`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolOutput {
    /// Every `text` content item, in order.
    pub texts: Vec<String>,
    /// `structuredContent`, when the server sent it.
    pub structured: Option<serde_json::Value>,
    /// The `isError` flag of the result.
    pub is_error: bool,
}

/// Calls tools on an MCP server. Implemented by `uals_adapters::mcp_child::McpChild` over a
/// long-lived child process; tests use fakes.
#[async_trait]
pub trait McpToolCaller: Send + Sync {
    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<ToolOutput, BackendError>;
}
