//! Runs a gold set against an engine and builds the report.

use std::time::Instant;

use anyhow::{Result, ensure};

use uals_core::render::{Format, render_search};
use uals_core::types::UsagesResponse;

use crate::engine::Engine;
use crate::gold::{GoldQuery, GoldUsages, Half, QueryType};
use crate::metrics::{UsageScore, first_correct_rank, score_usages, tokens_of_text};
use crate::report::{Filters, QueryResult, Report, SCHEMA_VERSION, summarize};

/// Settings of one run.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Name of the run, shown in reports.
    pub name: String,
    /// Label of the engine for the report, e.g. `mock:responses.json`.
    pub engine: String,
    /// Only the queries of this half; all halves when `None`.
    pub half: Option<Half>,
    /// Only queries of these types; all types when empty.
    pub types: Vec<QueryType>,
    /// How many extra passes over all queries to time as warm calls.
    pub warm_repeats: usize,
    /// Hits requested per query.
    pub limit: usize,
    /// Usages requested per page; every page of a symbol is read.
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
    let results = run_first_pass(engine, &selected, options.limit).await;
    let cold_ms = results.iter().find(|result| !result.failed).map(|result| result.latency_ms);
    let warm_ms = time_warm_passes(engine, &selected, &results, options).await;
    let usage_scores = score_all_usages(engine, usages, options.usages_limit).await;

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

/// Milliseconds since `started`.
fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// Runs each query once and scores the response; a failed call becomes a miss.
async fn run_first_pass<E: Engine>(engine: &E, selected: &[&GoldQuery], limit: usize) -> Vec<QueryResult> {
    let mut results = Vec::with_capacity(selected.len());
    for gold in selected {
        let started = Instant::now();
        let outcome = engine.search(&gold.query, None, limit).await;
        let latency_ms = elapsed_ms(started);
        let mut result = QueryResult {
            id: gold.id.clone(),
            kind: gold.kind,
            half: gold.half,
            rank_primary: None,
            rank_any: None,
            tokens: 0,
            latency_ms,
            failed: false,
        };
        match outcome {
            Ok(response) => {
                result.rank_primary = first_correct_rank(engine.repos(), &response.hits, &gold.expected, false);
                result.rank_any = first_correct_rank(engine.repos(), &response.hits, &gold.expected, true);
                result.tokens = tokens_of_text(&render_search(&response, Format::Concise, engine.output()));
            }
            Err(error) => {
                eprintln!("query {}: {error:#}", gold.id);
                result.failed = true;
            }
        }
        results.push(result);
    }
    results
}

/// Times `options.warm_repeats` extra passes over the queries that did not fail the first pass.
/// `results[i]` is the first-pass result of `selected[i]`.
async fn time_warm_passes<E: Engine>(
    engine: &E,
    selected: &[&GoldQuery],
    results: &[QueryResult],
    options: &RunOptions,
) -> Vec<f64> {
    let mut warm_ms = Vec::new();
    for _ in 0..options.warm_repeats {
        for (gold, result) in selected.iter().zip(results) {
            if result.failed {
                continue;
            }
            let started = Instant::now();
            if engine.search(&gold.query, None, options.limit).await.is_ok() {
                warm_ms.push(elapsed_ms(started));
            }
        }
    }
    warm_ms
}

/// Scores the usages of every gold symbol; a failed call gives a score marked `failed`.
async fn score_all_usages<E: Engine>(engine: &E, usages: &[GoldUsages], page_size: usize) -> Vec<UsageScore> {
    let mut scores = Vec::with_capacity(usages.len());
    for gold in usages {
        match all_usages(engine, &gold.symbol, page_size).await {
            Ok(response) => scores.push(score_usages(engine.repos(), gold, &response)),
            Err(error) => {
                eprintln!("usages {}: {error:#}", gold.id);
                scores.push(UsageScore::failed(&gold.id));
            }
        }
    }
    scores
}

/// Reads the usages of `symbol` page by page until the engine has no next offset, and joins the
/// pages into one response: usages of a repository are concatenated, `complete` holds only when
/// every page was complete. A next offset that does not move forward is an error.
async fn all_usages<E: Engine>(engine: &E, symbol: &str, page_size: usize) -> Result<UsagesResponse> {
    let mut all = engine.usages(symbol, page_size, 0).await?;
    let mut next = all.next_offset.take();
    while let Some(offset) = next {
        let page = engine.usages(symbol, page_size, offset).await?;
        next = page.next_offset;
        ensure!(next.is_none_or(|following| following > offset), "usages paging does not move past offset {offset}");
        all.complete &= page.complete;
        for repo in page.repos {
            match all.repos.iter_mut().find(|known| known.repo == repo.repo) {
                Some(known) => known.usages.extend(repo.usages),
                None => all.repos.push(repo),
            }
        }
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use uals_core::config::OutputConfig;
    use uals_core::types::{QueryKind, Repo, SearchResponse, UsagesResponse};

    use super::*;
    use crate::engine::MockEngine;
    use crate::gold::{Definition, Expected, ExpectedUsage, Role};

    static DEFAULT_OUTPUT: LazyLock<OutputConfig> = LazyLock::new(OutputConfig::default);

    /// Counts calls; answers every query with an empty result.
    struct Counting {
        calls: AtomicUsize,
        repos: Vec<Repo>,
        output: OutputConfig,
    }

    impl Engine for Counting {
        fn repos(&self) -> &[Repo] {
            &self.repos
        }

        fn output(&self) -> &OutputConfig {
            &self.output
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

        async fn usages(&self, _: &str, _: usize, _: usize) -> Result<UsagesResponse> {
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
        let engine = Counting { calls: AtomicUsize::new(0), repos: vec![], output: OutputConfig::default() };
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

    /// Fails its first search at once; later searches take 30 ms. Usages are not available.
    struct FailsFirst {
        calls: AtomicUsize,
    }

    impl Engine for FailsFirst {
        fn repos(&self) -> &[Repo] {
            &[]
        }

        fn output(&self) -> &OutputConfig {
            &DEFAULT_OUTPUT
        }

        async fn search(&self, query: &str, kind: Option<QueryKind>, limit: usize) -> Result<SearchResponse> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                anyhow::bail!("backend starting");
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            Counting { calls: AtomicUsize::new(0), repos: vec![], output: OutputConfig::default() }
                .search(query, kind, limit)
                .await
        }

        async fn usages(&self, _: &str, _: usize, _: usize) -> Result<UsagesResponse> {
            anyhow::bail!("not used")
        }
    }

    #[tokio::test]
    async fn cold_latency_is_the_first_successful_call() {
        let engine = FailsFirst { calls: AtomicUsize::new(0) };
        let queries = [gold("1", QueryType::Natural, Half::A), gold("2", QueryType::Natural, Half::A)];
        let report = run(&engine, &queries, &[], &options(0)).await.unwrap();
        assert!(report.queries[0].failed && !report.queries[1].failed);
        let cold = report.summary.latency.cold_ms.expect("one call succeeded");
        assert!(cold >= 30.0, "cold_ms {cold} is the failed call, not the first successful one");
    }

    #[tokio::test]
    async fn failed_usages_call_is_counted_and_kept_out_of_the_averages() {
        let engine = MockEngine::from_json(
            r#"{"repos": [{"name": "r", "root": "/ws/r"}],
                "usages": {"Known": {"symbol": "Known", "total": 1, "offset": 0, "complete": true,
                  "repos": [{"repo": "r", "source": "ast-index", "status": {"state": "ok"},
                             "usages": [{"repo": "r", "path": "/ws/r/b.kt", "line": 4}]}]}}}"#,
        )
        .unwrap();
        let expected = || vec![("r", "b.kt", vec![4])];
        let gold_usages = [usage("u1", "Known", expected()), usage("u2", "Missing", expected())];
        let report = run(&engine, &[], &gold_usages, &options(0)).await.unwrap();
        let summary = report.summary.usages.expect("usages were run");
        assert_eq!(summary.line_recall, 1.0, "the failed symbol must not pull the mean down");
        assert_eq!((summary.symbols, summary.failed), (1, 1));
        assert!(report.usages[1].failed && !report.usages[0].failed);
    }

    fn usage(id: &str, symbol: &str, expected: Vec<(&str, &str, Vec<u32>)>) -> GoldUsages {
        GoldUsages {
            id: id.into(),
            symbol: symbol.into(),
            definition: Definition { repo: "r".into(), path: "a.kt".into(), line: 1 },
            expected_usages: expected
                .into_iter()
                .map(|(repo, path, lines)| ExpectedUsage { repo: repo.into(), path: path.into(), lines })
                .collect(),
            method: None,
        }
    }

    /// Three usages of `S`: two in repository `r`, one in `q`.
    fn three_usages(complete: bool) -> MockEngine {
        MockEngine::from_json(&format!(
            r#"{{"repos": [{{"name": "r", "root": "/ws/r"}}, {{"name": "q", "root": "/ws/q"}}],
                "usages": {{"S": {{"symbol": "S", "total": 3, "offset": 0, "complete": {complete},
                  "repos": [
                    {{"repo": "r", "source": "ast-index", "status": {{"state": "ok"}}, "usages": [
                        {{"repo": "r", "path": "/ws/r/b.kt", "line": 3}}, {{"repo": "r", "path": "/ws/r/b.kt", "line": 4}}]}},
                    {{"repo": "q", "source": "lexical", "status": {{"state": "ok"}}, "usages": [
                        {{"repo": "q", "path": "/ws/q/c.kt", "line": 5}}]}}]}}}}}}"#
        ))
        .unwrap()
    }

    fn all_three() -> GoldUsages {
        usage("u1", "S", vec![("r", "b.kt", vec![3, 4]), ("q", "c.kt", vec![5])])
    }

    #[tokio::test]
    async fn usages_are_paged_until_the_engine_has_no_next_offset() {
        let engine = three_usages(true);
        let report = run(&engine, &[], &[all_three()], &RunOptions { usages_limit: 2, ..options(0) }).await.unwrap();
        let score = &report.usages[0];
        assert_eq!((score.line_recall, score.found_lines), (1.0, 3), "a page held only two usages");
        assert!(!score.incomplete && !score.failed);
    }

    #[tokio::test]
    async fn an_incomplete_answer_is_marked_and_counted() {
        let engine = three_usages(false);
        let report = run(&engine, &[], &[all_three()], &RunOptions { usages_limit: 2, ..options(0) }).await.unwrap();
        assert!(report.usages[0].incomplete);
        assert_eq!(report.usages[0].line_recall, 1.0);
        let summary = report.summary.usages.unwrap();
        assert_eq!((summary.symbols, summary.incomplete), (1, 1));
    }

    /// Always answers with the same next offset.
    struct Stuck;

    impl Engine for Stuck {
        fn repos(&self) -> &[Repo] {
            &[]
        }

        fn output(&self) -> &OutputConfig {
            &DEFAULT_OUTPUT
        }

        async fn search(&self, _: &str, _: Option<QueryKind>, _: usize) -> Result<SearchResponse> {
            anyhow::bail!("not used")
        }

        async fn usages(&self, symbol: &str, limit: usize, _: usize) -> Result<UsagesResponse> {
            let mut page = three_usages(true).usages(symbol, limit, 0).await?;
            page.next_offset = Some(1);
            Ok(page)
        }
    }

    #[tokio::test]
    async fn paging_that_does_not_advance_fails_the_call() {
        let report = run(&Stuck, &[], &[all_three()], &options(0)).await.unwrap();
        assert!(report.usages[0].failed);
    }

    const ONE_HIT: &str = r#"{"repos": [{"name": "r", "root": "/ws/r"}],
        "search": {"query 1": {"query": "query 1", "kind": "natural", "total": 1, "offset": 0, "backends": [],
          "hits": [{"repo": "r", "path": "/ws/r/a", "lines": {"start": 1, "end": 2}, "is_definition": false,
                    "pinned": false, "focus_line": 1, "score": 1.0, "sources": []}]}}}"#;

    #[tokio::test]
    async fn tokens_are_those_of_the_text_an_agent_receives() {
        let engine = MockEngine::from_json(ONE_HIT).unwrap();
        let queries = [gold("1", QueryType::Natural, Half::A)];
        let report = run(&engine, &queries, &[], &options(0)).await.unwrap();
        let response = engine.search("query 1", None, 30).await.unwrap();
        let text =
            uals_core::render::render_search(&response, uals_core::render::Format::Concise, &OutputConfig::default());
        assert_eq!(report.queries[0].tokens, text.chars().count().div_ceil(4) as u64);
    }

    #[tokio::test]
    async fn tokens_follow_the_output_budget_of_the_engine() {
        let mut wide = MockEngine::from_json(ONE_HIT).unwrap();
        let response = wide.search.get_mut("query 1").unwrap();
        let hit = response.hits[0].clone();
        response.hits.extend([hit.clone(), hit]);
        let narrow =
            MockEngine { output: OutputConfig { max_output_chars: 20, ..OutputConfig::default() }, ..wide.clone() };
        let queries = [gold("1", QueryType::Natural, Half::A)];
        let wide = run(&wide, &queries, &[], &options(0)).await.unwrap();
        let narrow = run(&narrow, &queries, &[], &options(0)).await.unwrap();
        assert!(narrow.queries[0].tokens < wide.queries[0].tokens, "the budget drops hits the agent never sees");
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
        assert_eq!(report.summary.latency.cold_ms, None, "no call succeeded");
    }
}
