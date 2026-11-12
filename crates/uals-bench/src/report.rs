//! The report of one run: per-query results, usage scores and aggregates. Never holds code,
//! paths or query text, only ids, ranks and numbers.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::gold::{Half, QueryType};
use crate::metrics::{RankStats, UsageScore, mean, percentile, rank_stats};

/// The report format version; `Report::load` rejects other versions.
pub const SCHEMA_VERSION: u32 = 1;

/// Which part of the gold set a run covered.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filters {
    /// Only this half was run; all halves when `None`.
    pub half: Option<Half>,
    /// Only these types were run; all types when empty.
    pub types: Vec<QueryType>,
}

/// The outcome of one query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    /// The gold query id.
    pub id: String,
    /// The query type.
    #[serde(rename = "type")]
    pub kind: QueryType,
    /// The gold set half of the query.
    pub half: Half,
    /// 1-based rank of the first hit matching a `primary` entry.
    pub rank_primary: Option<u32>,
    /// 1-based rank of the first hit matching a `primary` or `secondary` entry.
    pub rank_any: Option<u32>,
    /// Characters / 4 of the compact JSON response; 0 when the call failed.
    pub tokens: u64,
    /// Wall time of the first call of this query, in milliseconds.
    pub latency_ms: f64,
    /// True when the engine returned an error: the query counts as a miss.
    pub failed: bool,
}

/// Aggregates over a group of queries.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Group {
    /// Number of queries.
    pub n: usize,
    /// Number of queries whose call failed.
    pub failed: usize,
    /// Ranks counting `primary` entries only.
    pub primary: RankStats,
    /// Ranks counting `primary` and `secondary` entries.
    pub with_secondary: RankStats,
    /// Mean response size in tokens (characters / 4).
    pub mean_tokens: f64,
}

/// Usage scores averaged over the symbols whose call succeeded (each symbol weighs the same).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageSummary {
    /// Number of symbols averaged.
    pub symbols: usize,
    /// Number of symbols whose usages call failed; they are not in the means.
    #[serde(default)]
    pub failed: usize,
    /// Mean of `UsageScore::file_recall`.
    pub file_recall: f64,
    /// Mean of `UsageScore::file_precision`.
    pub file_precision: f64,
    /// Mean of `UsageScore::line_recall`.
    pub line_recall: f64,
    /// Mean of `UsageScore::line_precision`.
    pub line_precision: f64,
}

/// Wall time of the search calls, in milliseconds. `cold_ms` is the first successful call of the
/// run; it depends on the query order and on which backends were already warm.
/// `first_*` is over the first call of every query; `warm_*` over the repeated calls.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Latency {
    /// The first successful call of this run; `None` when every call failed.
    pub cold_ms: Option<f64>,
    /// Median of the first calls.
    pub first_p50_ms: Option<f64>,
    /// 95th percentile of the first calls.
    pub first_p95_ms: Option<f64>,
    /// Number of repeated calls timed.
    pub warm_samples: usize,
    /// Median of the repeated calls.
    pub warm_p50_ms: Option<f64>,
    /// 95th percentile of the repeated calls.
    pub warm_p95_ms: Option<f64>,
}

/// The aggregates of a run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// All queries of the run.
    pub overall: Group,
    /// Per query type, keyed by `QueryType::as_str`.
    pub by_type: BTreeMap<String, Group>,
    /// Per half, keyed by `Half::as_str`.
    pub by_half: BTreeMap<String, Group>,
    /// `None` when the run had no gold usages.
    pub usages: Option<UsageSummary>,
    /// Timings of the search calls.
    pub latency: Latency,
}

/// Everything one run produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// Equals `SCHEMA_VERSION`.
    pub schema_version: u32,
    /// Name of the run.
    pub name: String,
    /// Label of the engine that answered.
    pub engine: String,
    /// Which part of the gold set was run.
    pub filters: Filters,
    /// Hits requested per query.
    pub limit: usize,
    /// Extra timed passes over all queries.
    pub warm_repeats: usize,
    /// One result per selected query, in gold order.
    pub queries: Vec<QueryResult>,
    /// One score per gold symbol.
    pub usages: Vec<UsageScore>,
    /// The aggregates of `queries` and `usages`.
    pub summary: Summary,
}

/// Aggregates `queries` into one group.
pub fn group_of(queries: &[&QueryResult]) -> Group {
    let ranks = |f: fn(&QueryResult) -> Option<u32>| queries.iter().map(|q| f(q)).collect::<Vec<_>>();
    let tokens: Vec<f64> = queries.iter().map(|q| q.tokens as f64).collect();
    Group {
        n: queries.len(),
        failed: queries.iter().filter(|q| q.failed).count(),
        primary: rank_stats(&ranks(|q| q.rank_primary)),
        with_secondary: rank_stats(&ranks(|q| q.rank_any)),
        mean_tokens: mean(&tokens),
    }
}

/// The mean usage scores of the symbols whose call succeeded; `None` when there are no scores.
fn usage_summary(all: &[UsageScore]) -> Option<UsageSummary> {
    if all.is_empty() {
        return None;
    }
    let scores: Vec<&UsageScore> = all.iter().filter(|score| !score.failed).collect();
    let avg = |f: fn(&UsageScore) -> f64| mean(&scores.iter().map(|score| f(score)).collect::<Vec<_>>());
    Some(UsageSummary {
        symbols: scores.len(),
        failed: all.len() - scores.len(),
        file_recall: avg(|s| s.file_recall),
        file_precision: avg(|s| s.file_precision),
        line_recall: avg(|s| s.line_recall),
        line_precision: avg(|s| s.line_precision),
    })
}

/// Builds the aggregates. `warm_ms` are the timings of the repeated calls.
pub fn summarize(queries: &[QueryResult], usages: &[UsageScore], cold_ms: Option<f64>, warm_ms: &[f64]) -> Summary {
    let all: Vec<&QueryResult> = queries.iter().collect();
    let mut by_type: BTreeMap<String, Vec<&QueryResult>> = BTreeMap::new();
    let mut by_half: BTreeMap<String, Vec<&QueryResult>> = BTreeMap::new();
    for q in queries {
        by_type.entry(q.kind.as_str().to_string()).or_default().push(q);
        by_half.entry(q.half.as_str().to_string()).or_default().push(q);
    }
    let first: Vec<f64> = queries.iter().map(|q| q.latency_ms).collect();
    Summary {
        overall: group_of(&all),
        by_type: by_type.into_iter().map(|(k, v)| (k, group_of(&v))).collect(),
        by_half: by_half.into_iter().map(|(k, v)| (k, group_of(&v))).collect(),
        usages: usage_summary(usages),
        latency: Latency {
            cold_ms,
            first_p50_ms: percentile(&first, 50.0),
            first_p95_ms: percentile(&first, 95.0),
            warm_samples: warm_ms.len(),
            warm_p50_ms: percentile(warm_ms, 50.0),
            warm_p95_ms: percentile(warm_ms, 95.0),
        },
    }
}

impl Report {
    /// Reads a report from `path`; fails on unreadable files and unsupported schema versions.
    pub fn load(path: &Path) -> Result<Report> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        let report: Report =
            serde_json::from_str(&text).with_context(|| format!("{} is not a uals-bench report", path.display()))?;
        if report.schema_version != SCHEMA_VERSION {
            bail!(
                "{}: report schema {} is not supported (expected {SCHEMA_VERSION})",
                path.display(),
                report.schema_version
            );
        }
        Ok(report)
    }

    /// Writes the report to `path` as pretty JSON.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text + "\n").with_context(|| format!("cannot write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(id: &str, kind: QueryType, half: Half, rank: Option<u32>, any: Option<u32>, tokens: u64) -> QueryResult {
        QueryResult {
            id: id.into(),
            kind,
            half,
            rank_primary: rank,
            rank_any: any,
            tokens,
            latency_ms: 1.0,
            failed: false,
        }
    }

    #[test]
    fn groups_split_by_type_and_half() {
        let queries = vec![
            q("1", QueryType::Identifier, Half::A, Some(1), Some(1), 100),
            q("2", QueryType::Identifier, Half::B, Some(2), Some(1), 300),
            q("3", QueryType::Natural, Half::A, None, Some(4), 200),
        ];
        let s = summarize(&queries, &[], Some(5.0), &[2.0, 4.0]);
        assert_eq!(s.overall.n, 3);
        assert_eq!(s.by_type["identifier"].n, 2);
        assert_eq!(s.by_type["identifier"].primary.mrr, 0.75);
        assert_eq!(s.by_type["natural"].primary.mrr, 0.0);
        assert_eq!(s.by_type["natural"].with_secondary.mrr, 0.25);
        assert_eq!(s.by_half["A"].n, 2);
        assert_eq!(s.overall.mean_tokens, 200.0);
        assert_eq!(s.latency.cold_ms, Some(5.0));
        assert_eq!(s.latency.warm_p50_ms, Some(2.0));
        assert_eq!(s.latency.warm_samples, 2);
        assert!(s.usages.is_none());
    }
}
