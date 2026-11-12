//! The real engine: the `uals` orchestrator run in process over the repositories of a folder.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use uals::orchestrator::{Orchestrator, SearchParams, UsagesParams};
use uals::setup;
use uals_core::config::{Config, OutputConfig, SemanticChoice};
use uals_core::render::Format;
use uals_core::types::{QueryKind, Repo, SearchResponse, UsagesResponse};
use uals_core::workspace;

use crate::engine::Engine;

/// How the real engine is configured; the fields are what the command line can set.
#[derive(Debug, Clone, Default)]
pub struct RealOptions {
    /// The config file; the default lookup of `Config::load` when `None`.
    pub config: Option<PathBuf>,
    /// Replaces `semantic.default` of the config, so one config can be measured with each backend.
    pub semantic: Option<SemanticChoice>,
}

/// The name of a semantic choice as written in the config and on the command line.
pub fn semantic_name(choice: SemanticChoice) -> &'static str {
    match choice {
        SemanticChoice::Semble => "semble",
        SemanticChoice::ClaudeContext => "claude-context",
        SemanticChoice::None => "none",
    }
}

/// Searches with the orchestrator the way `uals` does. Nothing is warmed up first, so the first
/// call of a run pays the start of ast-index and of the semantic server: that is the cold latency.
/// The warm latency comes from the repeated passes of the run.
pub struct RealEngine {
    orchestrator: Orchestrator,
    repos: Vec<Repo>,
    semantic: SemanticChoice,
}

impl RealEngine {
    /// Loads the config, applies `options`, finds the repositories under `root` and builds the
    /// orchestrator.
    pub fn open(root: &Path, options: &RealOptions) -> Result<Self> {
        let mut config = Config::load(options.config.as_deref())?;
        if let Some(choice) = options.semantic {
            config.semantic.default = choice;
        }
        let repos = workspace::discover(root, &config)
            .with_context(|| format!("cannot find the repositories under {}", root.display()))?
            .repos;
        let semantic = config.semantic.default;
        Ok(Self { orchestrator: setup::orchestrator(config), repos, semantic })
    }

    /// The semantic backend this engine runs with.
    pub fn semantic(&self) -> SemanticChoice {
        self.semantic
    }
}

impl Engine for RealEngine {
    fn repos(&self) -> &[Repo] {
        &self.repos
    }

    fn output(&self) -> &OutputConfig {
        &self.orchestrator.config().output
    }

    async fn search(&self, query: &str, kind: Option<QueryKind>, limit: usize) -> Result<SearchResponse> {
        let params = SearchParams { kind, limit: Some(limit), format: Format::Concise, ..SearchParams::new(query) };
        self.orchestrator.search(&self.repos, &params).await
    }

    async fn usages(&self, symbol: &str, limit: usize, offset: usize) -> Result<UsagesResponse> {
        let params = UsagesParams { limit: Some(limit), offset, ..UsagesParams::new(symbol) };
        self.orchestrator.usages(&self.repos, &params).await
    }
}
