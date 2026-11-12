//! ast-index adapter (track T2): runs the `ast-index` CLI with the repository root as the working
//! directory, never anywhere else (from another folder it walks up to the nearest marker and may
//! create stub cache folders). Findings and recorded outputs: `docs/spikes/s2-ast-index.md` and
//! `tests/fixtures/ast_index/`.

use std::path::PathBuf;

use async_trait::async_trait;
use uals_core::backend::{BackendError, SearchBackend, SpanProvider, UsageSource};
use uals_core::config::AstIndexConfig;
use uals_core::types::{BackendAnswer, BackendId, IndexState, QueryKind, Repo, SearchRequest, SymbolSpan, UsageAnswer};

/// Symbols, usages and symbol spans from ast-index.
pub struct AstIndexBackend {
    config: AstIndexConfig,
}

impl AstIndexBackend {
    pub fn new(config: AstIndexConfig) -> Self {
        Self { config }
    }
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
        let _ = (&self.config, repo, request);
        todo!("track T2")
    }

    async fn index_state(&self, repo: &Repo) -> IndexState {
        let _ = repo;
        todo!("track T2")
    }

    async fn refresh(&self, repo: &Repo, full: bool) -> Result<(), BackendError> {
        let _ = (repo, full);
        todo!("track T2")
    }
}

#[async_trait]
impl UsageSource for AstIndexBackend {
    fn id(&self) -> BackendId {
        BackendId::AstIndex
    }

    async fn usages(&self, repo: &Repo, symbol: &str, limit: usize) -> Result<UsageAnswer, BackendError> {
        let _ = (repo, symbol, limit);
        todo!("track T2")
    }
}

#[async_trait]
impl SpanProvider for AstIndexBackend {
    async fn spans(&self, repo: &Repo, files: &[PathBuf]) -> Result<Vec<SymbolSpan>, BackendError> {
        let _ = (repo, files);
        todo!("track T2")
    }
}
