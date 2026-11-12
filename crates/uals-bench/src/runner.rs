//! Runs a gold set against an engine and builds the report.

use std::time::Instant;

use anyhow::Result;

use crate::engine::Engine;
use crate::gold::{GoldQuery, GoldUsages, Half, QueryType};
use crate::metrics::{first_correct_rank, score_usages, tokens_of};
use crate::report::{Filters, QueryResult, Report, SCHEMA_VERSION, summarize};

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub name: String,
    /// Label of the engine for the report, e.g. `mock:responses.json`.
    pub engine: String,
    pub half: Option<Half>,
    pub types: Vec<QueryType>,
    /// How many extra passes over all queries to time as warm calls.
    pub warm_repeats: usize,
    /// Hits requested per query.
    pub limit: usize,
    /// Usages requested per symbol.
    pub usages_limit: usize,
}

/// Keeps the queries that pass the filters.
pub fn filter_queries<'a>(queries: &'a [GoldQuery], half: Option<Half>, types: &[QueryType]) -> Vec<&'a GoldQuery> {
    queries
        .iter()
        .filter(|q| half.is_none_or(|h| q.half == h))
        .filter(|q| types.is_empty() || types.contains(&q.kind))
        .collect()
}

/// Runs every selected query once, then `warm_repeats` more passes for the warm latency.
/// A query whose call fails counts as a miss; its error goes to stderr, not to the report.
pub async fn run<E: Engine>(
    engine: &E,
    queries: &[GoldQuery],
    usages: &[GoldUsages],
    options: &RunOptions,
) -> Result<Report> {
    let selected = filter_queries(queries, options.half, &options.types);
    let mut results = Vec::with_capacity(selected.len());
    let mut cold_ms = None;
    for gold in &selected {
        let started = Instant::now();
        let outcome = engine.search(&gold.query, None, options.limit).await;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        cold_ms.get_or_insert(elapsed_ms);
        results.push(match outcome {
            Ok(response) => QueryResult {
                id: gold.id.clone(),
                kind: gold.kind,
                half: gold.half,
                rank_primary: first_correct_rank(engine.repos(), &response.hits, &gold.expected, false),
                rank_any: first_correct_rank(engine.repos(), &response.hits, &gold.expected, true),
                tokens: tokens_of(&response),
                latency_ms: elapsed_ms,
                failed: false,
            },
            Err(error) => {
                eprintln!("query {}: {error:#}", gold.id);
                QueryResult {
                    id: gold.id.clone(),
                    kind: gold.kind,
                    half: gold.half,
                    rank_primary: None,
                    rank_any: None,
                    tokens: 0,
                    latency_ms: elapsed_ms,
                    failed: true,
                }
            }
        });
    }

    let mut warm_ms = Vec::new();
    for _ in 0..options.warm_repeats {
        for (gold, result) in selected.iter().zip(&results) {
            if result.failed {
                continue;
            }
            let started = Instant::now();
            if engine.search(&gold.query, None, options.limit).await.is_ok() {
                warm_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            }
        }
    }

    let mut usage_scores = Vec::with_capacity(usages.len());
    for gold in usages {
        match engine.usages(&gold.symbol, options.usages_limit).await {
            Ok(response) => usage_scores.push(score_usages(engine.repos(), gold, &response)),
            Err(error) => {
                eprintln!("usages {}: {error:#}", gold.id);
                usage_scores.push(score_usages(engine.repos(), gold, &empty_usages(&gold.symbol)));
            }
        }
    }

    let summary = summarize(&results, &usage_scores, cold_ms, &warm_ms);
    Ok(Report {
        schema_version: SCHEMA_VERSION,
        name: options.name.clone(),
        engine: options.engine.clone(),
        filters: Filters { half: options.half, types: options.types.clone() },
        limit: options.limit,
        warm_repeats: options.warm_repeats,
        queries: results,
        usages: usage_scores,
        summary,
    })
}

fn empty_usages(symbol: &str) -> uals_core::types::UsagesResponse {
    uals_core::types::UsagesResponse {
        symbol: symbol.to_string(),
        total: 0,
        offset: 0,
        repos: Vec::new(),
        next_offset: None,
        complete: false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use uals_core::types::{QueryKind, Repo, SearchResponse, UsagesResponse};

    use super::*;
    use crate::engine::MockEngine;
    use crate::gold::{Expected, Role};

    /// Counts calls; answers every query with an empty result.
    struct Counting {
        calls: AtomicUsize,
        repos: Vec<Repo>,
    }

    impl Engine for Counting {
        fn repos(&self) -> &[Repo] {
            &self.repos
        }

        async fn search(&self, query: &str, _: Option<QueryKind>, _: usize) -> Result<SearchResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(SearchResponse {
                query: query.into(),
                kind: QueryKind::Natural,
                total: 0,
                offset: 0,
                hits: vec![],
                next_offset: None,
                backends: vec![],
            })
        }

        async fn usages(&self, _: &str, _: usize) -> Result<UsagesResponse> {
            anyhow::bail!("not used")
        }
    }

    fn gold(id: &str, kind: QueryType, half: Half) -> GoldQuery {
        GoldQuery {
            id: id.into(),
            kind,
            half,
            query: format!("query {id}"),
            expected: vec![Expected { repo: "r".into(), path: "a".into(), lines: [1, 1], role: Role::Primary }],
            notes: None,
        }
    }

    fn options(warm_repeats: usize) -> RunOptions {
        RunOptions {
            name: "t".into(),
            engine: "test".into(),
            half: None,
            types: vec![],
            warm_repeats,
            limit: 30,
            usages_limit: 10,
        }
    }

    #[tokio::test]
    async fn warm_repeats_time_every_query_each_pass() {
        let engine = Counting { calls: AtomicUsize::new(0), repos: vec![] };
        let queries = [gold("1", QueryType::Natural, Half::A), gold("2", QueryType::Config, Half::B)];
        let report = run(&engine, &queries, &[], &options(4)).await.unwrap();
        assert_eq!(engine.calls.load(Ordering::SeqCst), 2 + 2 * 4);
        assert_eq!(report.summary.latency.warm_samples, 8);
        assert!(report.summary.latency.cold_ms.is_some());
        assert_eq!(report.queries.len(), 2);
        assert!(report.queries.iter().all(|q| q.rank_primary.is_none() && !q.failed));

        let none = run(&engine, &queries, &[], &options(0)).await.unwrap();
        assert_eq!(none.summary.latency.warm_samples, 0);
        assert_eq!(none.summary.latency.warm_p50_ms, None);
    }

    #[tokio::test]
    async fn filters_select_half_and_type() {
        let queries = [
            gold("1", QueryType::Natural, Half::A),
            gold("2", QueryType::Config, Half::B),
            gold("3", QueryType::Config, Half::A),
        ];
        assert_eq!(filter_queries(&queries, Some(Half::A), &[]).len(), 2);
        assert_eq!(filter_queries(&queries, None, &[QueryType::Config]).len(), 2);
        assert_eq!(filter_queries(&queries, Some(Half::A), &[QueryType::Config])[0].id, "3");
        assert_eq!(filter_queries(&queries, None, &[]).len(), 3);
    }

    #[tokio::test]
    async fn missing_canned_response_is_a_failed_miss_not_an_abort() {
        let engine = MockEngine::default();
        let queries = [gold("1", QueryType::Natural, Half::A)];
        let report = run(&engine, &queries, &[], &options(2)).await.unwrap();
        assert!(report.queries[0].failed);
        assert_eq!(report.summary.overall.failed, 1);
        assert_eq!(report.summary.overall.primary.mrr, 0.0);
        assert_eq!(report.summary.latency.warm_samples, 0, "failed queries are not repeated");
    }
}
