//! Comparison of two runs on the same queries.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::metrics::{hit_at, reciprocal_rank};
use crate::report::{QueryResult, Report};
use crate::stats::{Bootstrap, RESAMPLES, SEED, paired_bootstrap};

/// A per-query value whose mean is compared. The `-any` variants also count `secondary` entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[value(rename_all = "kebab-case")]
pub enum Metric {
    Mrr,
    MrrAny,
    Hit1,
    Hit1Any,
    Hit5,
    Hit5Any,
    Hit10,
    Hit10Any,
}

impl Metric {
    pub fn name(self) -> &'static str {
        match self {
            Metric::Mrr => "mrr",
            Metric::MrrAny => "mrr-any",
            Metric::Hit1 => "hit1",
            Metric::Hit1Any => "hit1-any",
            Metric::Hit5 => "hit5",
            Metric::Hit5Any => "hit5-any",
            Metric::Hit10 => "hit10",
            Metric::Hit10Any => "hit10-any",
        }
    }

    pub fn value(self, q: &QueryResult) -> f64 {
        let (rank, k) = match self {
            Metric::Mrr => return reciprocal_rank(q.rank_primary),
            Metric::MrrAny => return reciprocal_rank(q.rank_any),
            Metric::Hit1 => (q.rank_primary, 1),
            Metric::Hit1Any => (q.rank_any, 1),
            Metric::Hit5 => (q.rank_primary, 5),
            Metric::Hit5Any => (q.rank_any, 5),
            Metric::Hit10 => (q.rank_primary, 10),
            Metric::Hit10Any => (q.rank_any, 10),
        };
        hit_at(rank, k)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupComparison {
    /// `overall` or a query type.
    pub group: String,
    #[serde(flatten)]
    pub result: Bootstrap,
}

/// `diff` is always `b - a`: positive means run B is better.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    pub a: String,
    pub b: String,
    pub metric: Metric,
    pub paired: usize,
    pub only_in_a: usize,
    pub only_in_b: usize,
    pub resamples: usize,
    pub seed: u64,
    pub groups: Vec<GroupComparison>,
}

/// Pairs the queries of both runs by id and bootstraps the difference of `metric`, overall and
/// per query type. Queries present in only one run are counted and left out.
pub fn compare(a: &Report, b: &Report, metric: Metric) -> Comparison {
    let b_by_id: HashMap<&str, &QueryResult> = b.queries.iter().map(|q| (q.id.as_str(), q)).collect();
    let mut overall: (Vec<f64>, Vec<f64>) = (Vec::new(), Vec::new());
    let mut per_type: BTreeMap<&str, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for qa in &a.queries {
        let Some(qb) = b_by_id.get(qa.id.as_str()) else { continue };
        let (va, vb) = (metric.value(qa), metric.value(qb));
        overall.0.push(va);
        overall.1.push(vb);
        let entry = per_type.entry(qa.kind.as_str()).or_default();
        entry.0.push(va);
        entry.1.push(vb);
    }
    let paired = overall.0.len();
    let mut groups = Vec::new();
    let named = std::iter::once(("overall", overall)).chain(per_type);
    for (group, (va, vb)) in named {
        if let Some(result) = paired_bootstrap(&va, &vb, RESAMPLES, SEED) {
            groups.push(GroupComparison { group: group.to_string(), result });
        }
    }
    Comparison {
        a: a.name.clone(),
        b: b.name.clone(),
        metric,
        paired,
        only_in_a: a.queries.len() - paired,
        only_in_b: b.queries.len() - paired,
        resamples: RESAMPLES,
        seed: SEED,
        groups,
    }
}

impl Comparison {
    /// A Markdown table; `diff` is B minus A.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Metric `{}`, A = `{}`, B = `{}`; difference is B - A; paired bootstrap, {} resamples, 95% CI.",
            self.metric.name(),
            self.a,
            self.b,
            self.resamples
        );
        let _ = writeln!(
            out,
            "{} paired queries ({} only in A, {} only in B).\n",
            self.paired, self.only_in_a, self.only_in_b
        );
        out.push_str("| group | n | A | B | diff | 95% CI | significant |\n|---|---:|---:|---:|---:|---|---|\n");
        for g in &self.groups {
            let r = &g.result;
            let _ = writeln!(
                out,
                "| {} | {} | {:.3} | {:.3} | {:+.3} | [{:+.3}, {:+.3}] | {} |",
                g.group,
                r.n,
                r.mean_a,
                r.mean_b,
                r.diff,
                r.ci_low,
                r.ci_high,
                if r.significant { "yes" } else { "no" }
            );
        }
        out
    }
}
