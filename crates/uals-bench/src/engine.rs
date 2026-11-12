//! The seam between the harness and the search engine.
//!
//! `RealEngine` runs the real search; `MockEngine` replays canned responses from a JSON file, so
//! the whole harness also runs without any backend.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use uals_core::config::OutputConfig;
use uals_core::types::{QueryKind, Repo, SearchResponse, UsagesResponse};

use crate::real::{RealEngine, RealOptions, semantic_name};

/// What the harness needs from a search engine.
pub trait Engine {
    /// The repositories of the workspace; their roots turn absolute hit paths into relative ones.
    fn repos(&self) -> &[Repo];

    /// The output settings the engine renders results with; they decide how much text an agent
    /// receives for one response.
    fn output(&self) -> &OutputConfig;

    /// Brings the engine to the state in which it answers at its best, e.g. waits until its
    /// indexes are built. Does nothing by default.
    fn warm_up(&self) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    /// Runs a search for `query` (kind detected when `None`) returning at most `limit` hits.
    fn search(
        &self,
        query: &str,
        kind: Option<QueryKind>,
        limit: usize,
    ) -> impl Future<Output = Result<SearchResponse>> + Send;

    /// Finds the usages of `symbol`: one page of at most `limit` usages, starting at `offset`.
    /// `UsagesResponse::next_offset` is the offset of the next page, `None` after the last one.
    fn usages(&self, symbol: &str, limit: usize, offset: usize) -> impl Future<Output = Result<UsagesResponse>> + Send;
}

/// The content of a mock responses file.
///
/// ```json
/// { "repos":  [{"name": "sample-web", "root": "/ws/sample-web"}],
///   "search": {"<query text>": <SearchResponse>},
///   "usages": {"<symbol>": <UsagesResponse>},
///   "output": {"max_output_chars": 12000} }
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MockEngine {
    /// The workspace repositories.
    pub repos: Vec<Repo>,
    /// Canned search responses by query text.
    #[serde(default)]
    pub search: HashMap<String, SearchResponse>,
    /// Canned usages responses by symbol.
    #[serde(default)]
    pub usages: HashMap<String, UsagesResponse>,
    /// The output settings of the engine the responses came from; the defaults when absent.
    #[serde(default)]
    pub output: OutputConfig,
}

impl MockEngine {
    /// Parses a mock responses document.
    pub fn from_json(text: &str) -> Result<Self> {
        serde_json::from_str(text).context("invalid mock responses JSON")
    }

    /// Reads and parses a mock responses file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        Self::from_json(&text).with_context(|| format!("in {}", path.display()))
    }
}

impl Engine for MockEngine {
    fn repos(&self) -> &[Repo] {
        &self.repos
    }

    fn output(&self) -> &OutputConfig {
        &self.output
    }

    async fn search(&self, query: &str, _kind: Option<QueryKind>, limit: usize) -> Result<SearchResponse> {
        let mut response =
            self.search.get(query).cloned().ok_or_else(|| anyhow!("no canned search response for \"{query}\""))?;
        response.hits.truncate(limit);
        Ok(response)
    }

    async fn usages(&self, symbol: &str, limit: usize, offset: usize) -> Result<UsagesResponse> {
        let mut response =
            self.usages.get(symbol).cloned().ok_or_else(|| anyhow!("no canned usages response for \"{symbol}\""))?;
        let total: usize = response.repos.iter().map(|repo| repo.usages.len()).sum();
        let (mut skip, mut left) = (offset, limit);
        for repo in &mut response.repos {
            let skipped = skip.min(repo.usages.len());
            repo.usages.drain(..skipped);
            skip -= skipped;
            repo.usages.truncate(left);
            left -= repo.usages.len();
        }
        response.offset = offset;
        response.next_offset = (offset.saturating_add(limit) < total).then_some(offset + limit);
        Ok(response)
    }
}

/// An engine picked on the command line.
pub enum AnyEngine {
    /// Replays canned responses.
    Mock(MockEngine),
    /// Runs the real search.
    Real(Box<RealEngine>),
}

impl AnyEngine {
    /// `mock:<responses.json>` or `real:<workspace folder>`. `options` only apply to `real`;
    /// they are an error for `mock`.
    pub fn from_spec(spec: &str, options: &RealOptions) -> Result<Self> {
        match spec.split_once(':') {
            Some(("mock", path)) if !path.is_empty() => {
                ensure!(
                    options.config.is_none() && options.semantic.is_none(),
                    "--config and --semantic apply to the real engine only"
                );
                Ok(AnyEngine::Mock(MockEngine::from_file(Path::new(path))?))
            }
            Some(("real", folder)) if !folder.is_empty() => {
                Ok(AnyEngine::Real(Box::new(RealEngine::open(Path::new(folder), options)?)))
            }
            _ => bail!("unknown engine \"{spec}\": expected mock:<responses.json> or real:<workspace folder>"),
        }
    }

    /// The label of the engine for the report when the spec cannot serve: a spec holds a folder
    /// path and a report holds no paths. `None` for the mock, whose spec is the label.
    pub fn label(&self) -> Option<String> {
        match self {
            AnyEngine::Mock(_) => None,
            AnyEngine::Real(engine) => Some(format!("real:{}", semantic_name(engine.semantic()))),
        }
    }
}

impl Engine for AnyEngine {
    fn repos(&self) -> &[Repo] {
        match self {
            AnyEngine::Mock(engine) => engine.repos(),
            AnyEngine::Real(engine) => engine.repos(),
        }
    }

    fn output(&self) -> &OutputConfig {
        match self {
            AnyEngine::Mock(engine) => engine.output(),
            AnyEngine::Real(engine) => engine.output(),
        }
    }

    async fn warm_up(&self) -> Result<()> {
        match self {
            AnyEngine::Mock(engine) => engine.warm_up().await,
            AnyEngine::Real(engine) => engine.warm_up().await,
        }
    }

    async fn search(&self, query: &str, kind: Option<QueryKind>, limit: usize) -> Result<SearchResponse> {
        match self {
            AnyEngine::Mock(engine) => engine.search(query, kind, limit).await,
            AnyEngine::Real(engine) => engine.search(query, kind, limit).await,
        }
    }

    async fn usages(&self, symbol: &str, limit: usize, offset: usize) -> Result<UsagesResponse> {
        match self {
            AnyEngine::Mock(engine) => engine.usages(symbol, limit, offset).await,
            AnyEngine::Real(engine) => engine.usages(symbol, limit, offset).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOCK: &str = r#"{
      "repos": [{"name": "r", "root": "/ws/r"}],
      "search": {"q": {"query": "q", "kind": "natural", "total": 3, "offset": 0, "backends": [],
        "hits": [
          {"repo": "r", "path": "/ws/r/a", "lines": {"start": 1, "end": 2}, "is_definition": false, "pinned": false, "focus_line": 1, "score": 1.0, "sources": []},
          {"repo": "r", "path": "/ws/r/b", "lines": {"start": 1, "end": 2}, "is_definition": false, "pinned": false, "focus_line": 1, "score": 0.5, "sources": []},
          {"repo": "r", "path": "/ws/r/c", "lines": {"start": 1, "end": 2}, "is_definition": false, "pinned": false, "focus_line": 1, "score": 0.2, "sources": []}]}}
    }"#;

    #[tokio::test]
    async fn mock_replays_and_truncates_to_the_limit() {
        let engine = MockEngine::from_json(MOCK).unwrap();
        assert_eq!(engine.repos()[0].name, "r");
        assert_eq!(engine.search("q", None, 30).await.unwrap().hits.len(), 3);
        assert_eq!(engine.search("q", None, 2).await.unwrap().hits.len(), 2);
        let error = engine.search("other", None, 30).await.unwrap_err();
        assert!(error.to_string().contains("no canned search response"));
        assert!(engine.usages("s", 5, 0).await.is_err());
    }

    #[test]
    fn spec_parsing() {
        let none = RealOptions::default();
        let error = |spec: &str, options: &RealOptions| AnyEngine::from_spec(spec, options).err().unwrap().to_string();
        assert!(error("semble", &none).contains("expected mock:"));
        assert!(error("mock:", &none).contains("expected mock:"));
        assert!(error("real:", &none).contains("expected mock:"));
        assert!(error("mock:/does/not/exist.json", &none).contains("cannot read"));
    }
}
