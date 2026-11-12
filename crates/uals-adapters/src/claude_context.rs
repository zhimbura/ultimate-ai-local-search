//! claude-context adapter (track T5): semantic search through the `search_code` tool of
//! `@zilliz/claude-context-mcp` 0.1.15, with Milvus and an embedding server behind it. Findings
//! and recorded outputs: `docs/spikes/s1-rmcp.md` and `tests/fixtures/claude_context/`.

use std::sync::Arc;

use async_trait::async_trait;
use uals_core::backend::{BackendError, McpToolCaller, SearchBackend};
use uals_core::config::ClaudeContextConfig;
use uals_core::types::{BackendAnswer, BackendId, IndexState, QueryKind, Repo, SearchRequest};

/// Semantic search over one repository at a time; one claude-context collection per repository.
pub struct ClaudeContextBackend {
    caller: Arc<dyn McpToolCaller>,
    config: ClaudeContextConfig,
}

impl ClaudeContextBackend {
    pub fn new(caller: Arc<dyn McpToolCaller>, config: ClaudeContextConfig) -> Self {
        Self { caller, config }
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
        let _ = (&self.caller, &self.config, repo, request);
        todo!("track T5")
    }

    async fn index_state(&self, repo: &Repo) -> IndexState {
        let _ = repo;
        todo!("track T5")
    }
}
