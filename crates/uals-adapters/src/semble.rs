//! Semble adapter (track T4): semantic search through Semble's MCP `search` tool. Findings and
//! recorded outputs: `docs/spikes/s1-rmcp.md` and `tests/fixtures/semble/`.

use std::sync::Arc;

use async_trait::async_trait;
use uals_core::backend::{BackendError, McpToolCaller, SearchBackend};
use uals_core::config::SembleConfig;
use uals_core::types::{BackendAnswer, BackendId, IndexState, QueryKind, Repo, SearchRequest};

/// Semantic search over one repository at a time.
pub struct SembleBackend {
    caller: Arc<dyn McpToolCaller>,
    config: SembleConfig,
}

impl SembleBackend {
    pub fn new(caller: Arc<dyn McpToolCaller>, config: SembleConfig) -> Self {
        Self { caller, config }
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
        let _ = (&self.caller, &self.config, repo, request);
        todo!("track T4")
    }

    async fn index_state(&self, repo: &Repo) -> IndexState {
        let _ = repo;
        todo!("track T4")
    }
}
