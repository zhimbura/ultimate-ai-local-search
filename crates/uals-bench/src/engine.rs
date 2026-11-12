//! The seam between the harness and the search engine.
//!
//! The real orchestrator is wired in a later phase. Until then `MockEngine` replays canned
//! responses from a JSON file, so the whole harness runs without any backend.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use uals_core::types::{QueryKind, Repo, SearchResponse, UsagesResponse};

/// What the harness needs from a search engine.
pub trait Engine {
    /// The repositories of the workspace; their roots turn absolute hit paths into relative ones.
    fn repos(&self) -> &[Repo];

    fn search(
        &self,
        query: &str,
        kind: Option<QueryKind>,
        limit: usize,
    ) -> impl Future<Output = Result<SearchResponse>> + Send;

    fn usages(&self, symbol: &str, limit: usize) -> impl Future<Output = Result<UsagesResponse>> + Send;
}

/// The content of a mock responses file.
///
/// ```json
/// { "repos":  [{"name": "sample-web", "root": "/ws/sample-web"}],
///   "search": {"<query text>": <SearchResponse>},
///   "usages": {"<symbol>": <UsagesResponse>} }
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MockEngine {
    pub repos: Vec<Repo>,
    #[serde(default)]
    pub search: HashMap<String, SearchResponse>,
    #[serde(default)]
    pub usages: HashMap<String, UsagesResponse>,
}

impl MockEngine {
    pub fn from_json(text: &str) -> Result<Self> {
        serde_json::from_str(text).context("invalid mock responses JSON")
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        Self::from_json(&text).with_context(|| format!("in {}", path.display()))
    }
}

impl Engine for MockEngine {
    fn repos(&self) -> &[Repo] {
        &self.repos
    }

    async fn search(&self, query: &str, _kind: Option<QueryKind>, limit: usize) -> Result<SearchResponse> {
        let mut response =
            self.search.get(query).cloned().ok_or_else(|| anyhow!("no canned search response for \"{query}\""))?;
        response.hits.truncate(limit);
        Ok(response)
    }

    async fn usages(&self, symbol: &str, limit: usize) -> Result<UsagesResponse> {
        let mut response =
            self.usages.get(symbol).cloned().ok_or_else(|| anyhow!("no canned usages response for \"{symbol}\""))?;
        let mut left = limit;
        for repo in &mut response.repos {
            repo.usages.truncate(left);
            left -= repo.usages.len();
        }
        Ok(response)
    }
}

/// An engine picked on the command line.
#[derive(Debug, Clone)]
pub enum AnyEngine {
    Mock(MockEngine),
}

impl AnyEngine {
    /// `mock:<responses.json>`. The real engine is added when the orchestrator is wired.
    pub fn from_spec(spec: &str) -> Result<Self> {
        match spec.split_once(':') {
            Some(("mock", path)) if !path.is_empty() => Ok(AnyEngine::Mock(MockEngine::from_file(Path::new(path))?)),
            _ => bail!("unknown engine \"{spec}\": expected mock:<responses.json>"),
        }
    }
}

impl Engine for AnyEngine {
    fn repos(&self) -> &[Repo] {
        match self {
            AnyEngine::Mock(engine) => engine.repos(),
        }
    }

    async fn search(&self, query: &str, kind: Option<QueryKind>, limit: usize) -> Result<SearchResponse> {
        match self {
            AnyEngine::Mock(engine) => engine.search(query, kind, limit).await,
        }
    }

    async fn usages(&self, symbol: &str, limit: usize) -> Result<UsagesResponse> {
        match self {
            AnyEngine::Mock(engine) => engine.usages(symbol, limit).await,
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
        assert!(engine.usages("s", 5).await.is_err());
    }

    #[test]
    fn spec_parsing() {
        assert!(AnyEngine::from_spec("real").unwrap_err().to_string().contains("expected mock:"));
        assert!(AnyEngine::from_spec("mock:").is_err());
        assert!(AnyEngine::from_spec("mock:/does/not/exist.json").unwrap_err().to_string().contains("cannot read"));
    }
}
