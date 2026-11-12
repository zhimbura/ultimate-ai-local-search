//! In-process lexical search (track T3) with the ripgrep crates `ignore`, `grep-searcher` and
//! `grep-regex`, plus its ranker. Findings, the recommended walker config and the list of
//! regression tests: `docs/spikes/s3-lexical.md`.

use std::time::Duration;

use async_trait::async_trait;
use uals_core::backend::{BackendError, SearchBackend, UsageSource};
use uals_core::config::LexicalConfig;
use uals_core::types::{BackendAnswer, BackendId, IndexState, QueryKind, Repo, SearchRequest, UsageAnswer};

/// Text search over the files of a repository, honoring its ignore files.
pub struct LexicalBackend {
    config: LexicalConfig,
    deadline: Duration,
}

impl LexicalBackend {
    /// `deadline` is the orchestrator's deadline for this backend; the search stops a little
    /// earlier on its own and returns a partial answer, because blocking threads cannot be
    /// cancelled by dropping the future.
    pub fn new(config: LexicalConfig, deadline: Duration) -> Self {
        Self { config, deadline }
    }
}

#[async_trait]
impl SearchBackend for LexicalBackend {
    fn id(&self) -> BackendId {
        BackendId::Lexical
    }

    fn supports(&self, kind: QueryKind) -> bool {
        let _ = kind;
        true
    }

    async fn search(&self, repo: &Repo, request: &SearchRequest) -> Result<BackendAnswer, BackendError> {
        let _ = (&self.config, self.deadline, repo, request);
        todo!("track T3")
    }

    async fn index_state(&self, repo: &Repo) -> IndexState {
        let _ = repo;
        todo!("track T3")
    }
}

/// The fallback source of `usages` for repositories without an ast-index index.
#[async_trait]
impl UsageSource for LexicalBackend {
    fn id(&self) -> BackendId {
        BackendId::Lexical
    }

    async fn usages(&self, repo: &Repo, symbol: &str, limit: usize) -> Result<UsageAnswer, BackendError> {
        let _ = (repo, symbol, limit);
        todo!("track T3")
    }
}
