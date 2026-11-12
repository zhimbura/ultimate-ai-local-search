//! The real engine: the `uals` orchestrator run in process over the repositories of a folder.
//!
//! `warm_up` runs before the hits of a run are measured. Semble builds its index on the first
//! search of a repository, which takes minutes in a large one, while the cold deadline of semantic
//! search is seconds; claude-context keeps its index between processes. Without a warm-up the first
//! queries of a Semble run would go without semantic hits and skew the choice between them.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use uals::orchestrator::{Orchestrator, SearchParams, UsagesParams};
use uals::setup;
use uals_core::config::{Config, OutputConfig, SemanticChoice};
use uals_core::render::Format;
use uals_core::types::{BackendReport, BackendStatus, QueryKind, Repo, SearchResponse, UsagesResponse};
use uals_core::workspace;

use crate::engine::Engine;

/// The probe query of the warm-up: natural language, so that semantic search takes part.
const WARM_UP_QUERY: &str = "where is the main entry point of the application";
/// The pause between two probes of the warm-up.
const WARM_UP_INTERVAL: Duration = Duration::from_secs(5);
/// How long the warm-up may take unless the command line says otherwise.
pub const DEFAULT_WARM_UP_TIMEOUT: Duration = Duration::from_secs(1800);

/// How the real engine is configured; the fields are what the command line can set.
#[derive(Debug, Clone)]
pub struct RealOptions {
    /// The config file; the default lookup of `Config::load` when `None`.
    pub config: Option<PathBuf>,
    /// Replaces `semantic.default` of the config, so one config can be measured with each backend.
    pub semantic: Option<SemanticChoice>,
    /// How long `warm_up` may wait for semantic search to stop timing out.
    pub warm_up_timeout: Duration,
}

impl Default for RealOptions {
    fn default() -> Self {
        Self { config: None, semantic: None, warm_up_timeout: DEFAULT_WARM_UP_TIMEOUT }
    }
}

/// The name of a semantic choice as written in the config and on the command line.
pub fn semantic_name(choice: SemanticChoice) -> &'static str {
    match choice {
        SemanticChoice::Semble => "semble",
        SemanticChoice::ClaudeContext => "claude-context",
        SemanticChoice::None => "none",
    }
}

/// The repositories where a semantic backend timed out, sorted and without duplicates. Empty
/// when semantic search answered everywhere, in any way but a timeout: `Partial` does not count,
/// because claude-context reports it whenever it stops at its result cap.
pub fn semantic_timeouts(backends: &[BackendReport]) -> Vec<&str> {
    let mut repos: Vec<&str> = backends
        .iter()
        .filter(|report| report.backend.is_semantic() && report.status == BackendStatus::Timeout)
        .map(|report| report.repo.as_str())
        .collect();
    repos.sort_unstable();
    repos.dedup();
    repos
}

/// Searches with the orchestrator the way `uals` does. The first call of a run is not warmed up
/// (the runner times it as the cold latency); `warm_up` then waits until semantic search answers.
pub struct RealEngine {
    orchestrator: Orchestrator,
    repos: Vec<Repo>,
    semantic: SemanticChoice,
    warm_up_timeout: Duration,
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
        Ok(Self {
            orchestrator: setup::orchestrator(config),
            repos,
            semantic,
            warm_up_timeout: options.warm_up_timeout,
        })
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

    /// Prepares the backends, then probes with a natural-language search every
    /// `WARM_UP_INTERVAL` until no semantic backend times out. Fails after `warm_up_timeout`.
    async fn warm_up(&self) -> Result<()> {
        self.orchestrator.warm(&self.repos).await;
        let started = Instant::now();
        loop {
            let probe = self.search(WARM_UP_QUERY, Some(QueryKind::Natural), 1).await?;
            let waiting = semantic_timeouts(&probe.backends);
            let seconds = started.elapsed().as_secs();
            if waiting.is_empty() {
                eprintln!("warm-up: semantic search answers everywhere after {seconds} s");
                return Ok(());
            }
            if started.elapsed() >= self.warm_up_timeout {
                bail!("warm-up gave up after {seconds} s; semantic search still times out in: {}", waiting.join(", "));
            }
            eprintln!("warm-up: {seconds} s, semantic search still times out in: {}", waiting.join(", "));
            tokio::time::sleep(WARM_UP_INTERVAL).await;
        }
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

#[cfg(test)]
mod tests {
    use uals_core::types::BackendId;

    use super::*;

    fn report(backend: BackendId, repo: &str, status: BackendStatus) -> BackendReport {
        BackendReport { backend, repo: repo.into(), status, elapsed_ms: 1 }
    }

    #[test]
    fn only_a_semantic_timeout_means_the_engine_is_still_warming() {
        let partial = || BackendStatus::Partial { reason: "50 results".into() };
        let cases: Vec<(Vec<BackendReport>, Vec<&str>)> = vec![
            (vec![], vec![]),
            (vec![report(BackendId::Semble, "a", BackendStatus::Ok)], vec![]),
            (vec![report(BackendId::ClaudeContext, "a", partial())], vec![]),
            (vec![report(BackendId::Semble, "a", BackendStatus::Unavailable { reason: "x".into() })], vec![]),
            (vec![report(BackendId::Semble, "a", BackendStatus::Failed { reason: "x".into() })], vec![]),
            (vec![report(BackendId::Lexical, "a", BackendStatus::Timeout)], vec![]),
            (vec![report(BackendId::AstIndex, "a", BackendStatus::Timeout)], vec![]),
            (
                vec![
                    report(BackendId::Semble, "b", BackendStatus::Timeout),
                    report(BackendId::Semble, "a", BackendStatus::Timeout),
                    report(BackendId::Semble, "c", BackendStatus::Ok),
                    report(BackendId::Lexical, "c", BackendStatus::Timeout),
                ],
                vec!["a", "b"],
            ),
        ];
        for (backends, waiting) in cases {
            assert_eq!(semantic_timeouts(&backends), waiting, "{backends:?}");
        }
    }
}
