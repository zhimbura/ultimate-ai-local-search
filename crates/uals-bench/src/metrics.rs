//! Metric math: correctness of a hit, ranks, Hit@k, MRR, usage recall and precision, percentiles.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use uals_core::types::{FusedHit, LineRange, Repo, UsagesResponse};

use crate::gold::{Expected, GoldUsages, Role};

/// Only the first `TOP_N` hits count; a correct hit below that gives reciprocal rank 0.
pub const TOP_N: usize = 30;

/// `path` relative to the root of repository `repo`, with `/` separators.
/// `None` when the repository is unknown or the path lies outside its root.
pub fn relative_path(repos: &[Repo], repo: &str, path: &Path) -> Option<String> {
    let root = &repos.iter().find(|r| r.name == repo)?.root;
    let relative = path.strip_prefix(root).ok()?;
    let parts: Vec<_> = relative.components().map(|c| c.as_os_str().to_string_lossy()).collect();
    Some(parts.join("/"))
}

/// A hit is correct when the repository matches, the path relative to the repository root
/// equals the expected path, and the line ranges share at least one line.
pub fn is_correct(repos: &[Repo], hit: &FusedHit, expected: &Expected) -> bool {
    hit.repo == expected.repo
        && relative_path(repos, &hit.repo, &hit.path).as_deref() == Some(expected.path.as_str())
        && hit.lines.overlap(&LineRange::new(expected.lines[0], expected.lines[1])) > 0
}

/// 1-based rank of the first correct hit among the first `TOP_N`. With `include_secondary`
/// a `secondary` entry counts as correct too; otherwise only `primary` entries do.
pub fn first_correct_rank(
    repos: &[Repo],
    hits: &[FusedHit],
    expected: &[Expected],
    include_secondary: bool,
) -> Option<u32> {
    hits.iter()
        .take(TOP_N)
        .position(|hit| {
            expected.iter().filter(|e| include_secondary || e.role == Role::Primary).any(|e| is_correct(repos, hit, e))
        })
        .map(|position| position as u32 + 1)
}

/// `1 / rank`, or 0 when there is no correct hit in the first `TOP_N`.
pub fn reciprocal_rank(rank: Option<u32>) -> f64 {
    match rank {
        Some(rank) if rank >= 1 && rank as usize <= TOP_N => 1.0 / f64::from(rank),
        _ => 0.0,
    }
}

/// 1 when the first correct hit is within the top `k`, else 0.
pub fn hit_at(rank: Option<u32>, k: u32) -> f64 {
    match rank {
        Some(rank) if rank <= k => 1.0,
        _ => 0.0,
    }
}

/// Mean of a slice; 0 for an empty one.
pub fn mean(values: &[f64]) -> f64 {
    if values.is_empty() { 0.0 } else { values.iter().sum::<f64>() / values.len() as f64 }
}

/// Hit@k and MRR over a group of queries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct RankStats {
    /// Share of queries with a correct hit at rank 1.
    pub hit_at_1: f64,
    /// Share of queries with a correct hit within the top 5.
    pub hit_at_5: f64,
    /// Share of queries with a correct hit within the top 10.
    pub hit_at_10: f64,
    /// Mean reciprocal rank.
    pub mrr: f64,
}

/// `ranks` holds the first correct rank of each query (`None` = nothing correct).
pub fn rank_stats(ranks: &[Option<u32>]) -> RankStats {
    let column = |f: &dyn Fn(Option<u32>) -> f64| mean(&ranks.iter().map(|&r| f(r)).collect::<Vec<_>>());
    RankStats {
        hit_at_1: column(&|r| hit_at(r, 1)),
        hit_at_5: column(&|r| hit_at(r, 5)),
        hit_at_10: column(&|r| hit_at(r, 10)),
        mrr: column(&reciprocal_rank),
    }
}

/// Output size of a response: characters of its compact JSON divided by 4, rounded up.
pub fn tokens_of<T: Serialize>(value: &T) -> u64 {
    let chars = serde_json::to_string(value).map(|s| s.chars().count()).unwrap_or(0);
    chars.div_ceil(4) as u64
}

/// Recall and precision of one symbol's usages, by file and by line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageScore {
    /// The gold usages id.
    pub id: String,
    /// Expected files that were found, over expected files.
    pub file_recall: f64,
    /// Expected files that were found, over found files.
    pub file_precision: f64,
    /// Expected lines that were found, over expected lines.
    pub line_recall: f64,
    /// Expected lines that were found, over found lines.
    pub line_precision: f64,
    /// Number of expected files.
    pub expected_files: usize,
    /// Number of found files.
    pub found_files: usize,
    /// Number of expected lines.
    pub expected_lines: usize,
    /// Number of found lines.
    pub found_lines: usize,
}

/// `part / whole`; 0 when `whole` is 0.
fn ratio(part: usize, whole: usize) -> f64 {
    if whole == 0 { 0.0 } else { part as f64 / whole as f64 }
}

/// Compares the usages the engine found with the gold ones.
///
/// - The definition line itself is not a usage: it is dropped from the found set.
/// - Found usages are deduplicated by (repo, path, line).
/// - Recall = correct / expected. Precision = correct / found; 0 when nothing was found.
/// - File level compares the sets of (repo, path); line level the sets of (repo, path, line).
pub fn score_usages(repos: &[Repo], gold: &GoldUsages, response: &UsagesResponse) -> UsageScore {
    let def = &gold.definition;
    let mut found: BTreeSet<(String, String, u32)> = BTreeSet::new();
    for repo_usages in &response.repos {
        for usage in &repo_usages.usages {
            let path = relative_path(repos, &usage.repo, &usage.path)
                .unwrap_or_else(|| usage.path.to_string_lossy().into_owned());
            if usage.repo == def.repo && path == def.path && usage.line == def.line {
                continue;
            }
            found.insert((usage.repo.clone(), path, usage.line));
        }
    }
    let expected: BTreeSet<(String, String, u32)> = gold
        .expected_usages
        .iter()
        .flat_map(|u| u.lines.iter().map(move |&l| (u.repo.clone(), u.path.clone(), l)))
        .collect();

    let files = |set: &BTreeSet<(String, String, u32)>| -> BTreeSet<(String, String)> {
        set.iter().map(|(r, p, _)| (r.clone(), p.clone())).collect()
    };
    let (expected_files, found_files) = (files(&expected), files(&found));
    let line_hits = expected.intersection(&found).count();
    let file_hits = expected_files.intersection(&found_files).count();
    UsageScore {
        id: gold.id.clone(),
        file_recall: ratio(file_hits, expected_files.len()),
        file_precision: ratio(file_hits, found_files.len()),
        line_recall: ratio(line_hits, expected.len()),
        line_precision: ratio(line_hits, found.len()),
        expected_files: expected_files.len(),
        found_files: found_files.len(),
        expected_lines: expected.len(),
        found_lines: found.len(),
    }
}

/// Nearest-rank percentile (`p` in 0..=100) of unsorted samples; `None` for no samples.
pub fn percentile(samples: &[f64], p: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    Some(sorted[rank.min(sorted.len()) - 1])
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pretty_assertions::assert_eq;
    use uals_core::types::{RepoUsages, Usage};

    use super::*;
    use crate::gold::{Definition, ExpectedUsage};

    fn repos() -> Vec<Repo> {
        vec![
            Repo { name: "a".into(), root: PathBuf::from("/ws/a") },
            Repo { name: "b".into(), root: PathBuf::from("/ws/b") },
        ]
    }

    fn hit(repo: &str, path: &str, start: u32, end: u32) -> FusedHit {
        FusedHit {
            repo: repo.into(),
            path: PathBuf::from(path),
            lines: LineRange::new(start, end),
            symbol: None,
            symbol_kind: None,
            is_definition: false,
            pinned: false,
            focus_line: start,
            snippet: None,
            score: 1.0,
            sources: vec![],
        }
    }

    fn exp(repo: &str, path: &str, start: u32, end: u32, role: Role) -> Expected {
        Expected { repo: repo.into(), path: path.into(), lines: [start, end], role }
    }

    #[test]
    fn overlap_edge_cases() {
        let repos = repos();
        let e = exp("a", "src/x.kt", 10, 20, Role::Primary);
        // touching at one line counts, one line away does not
        assert!(is_correct(&repos, &hit("a", "/ws/a/src/x.kt", 20, 30), &e));
        assert!(is_correct(&repos, &hit("a", "/ws/a/src/x.kt", 1, 10), &e));
        assert!(!is_correct(&repos, &hit("a", "/ws/a/src/x.kt", 21, 30), &e));
        assert!(!is_correct(&repos, &hit("a", "/ws/a/src/x.kt", 1, 9), &e));
        // a hit that contains the expected range, and the reverse
        assert!(is_correct(&repos, &hit("a", "/ws/a/src/x.kt", 1, 100), &e));
        assert!(is_correct(&repos, &hit("a", "/ws/a/src/x.kt", 15, 15), &e));
    }

    #[test]
    fn repo_and_path_must_match() {
        let repos = repos();
        let e = exp("a", "src/x.kt", 1, 5, Role::Primary);
        assert!(!is_correct(&repos, &hit("b", "/ws/b/src/x.kt", 1, 5), &e), "wrong repo");
        assert!(!is_correct(&repos, &hit("a", "/ws/a/src/y.kt", 1, 5), &e), "wrong path");
        assert!(!is_correct(&repos, &hit("a", "/ws/b/src/x.kt", 1, 5), &e), "path outside the repo root");
        assert!(!is_correct(&repos, &hit("a", "/ws/a/lib/src/x.kt", 1, 5), &e), "only the relative path counts");
        assert!(!is_correct(
            &repos,
            &hit("zzz", "/ws/zzz/src/x.kt", 1, 5),
            &exp("zzz", "src/x.kt", 1, 5, Role::Primary)
        ));
    }

    #[test]
    fn first_rank_counts_roles() {
        let repos = repos();
        let hits = vec![hit("a", "/ws/a/n.kt", 1, 2), hit("a", "/ws/a/sec.kt", 1, 9), hit("a", "/ws/a/pri.kt", 1, 9)];
        let expected = vec![exp("a", "pri.kt", 1, 3, Role::Primary), exp("a", "sec.kt", 5, 6, Role::Secondary)];
        assert_eq!(first_correct_rank(&repos, &hits, &expected, false), Some(3));
        assert_eq!(first_correct_rank(&repos, &hits, &expected, true), Some(2));
        assert_eq!(first_correct_rank(&repos, &hits[..1], &expected, true), None);
        assert_eq!(first_correct_rank(&repos, &[], &expected, true), None);
    }

    #[test]
    fn hits_below_the_top_30_do_not_count() {
        let repos = repos();
        let mut hits: Vec<_> = (0..40).map(|i| hit("a", &format!("/ws/a/f{i}.kt"), 1, 1)).collect();
        let expected = vec![exp("a", "f29.kt", 1, 1, Role::Primary)];
        assert_eq!(first_correct_rank(&repos, &hits, &expected, false), Some(30));
        let expected = vec![exp("a", "f30.kt", 1, 1, Role::Primary)];
        assert_eq!(first_correct_rank(&repos, &hits, &expected, false), None);
        hits.truncate(5);
        assert_eq!(reciprocal_rank(Some(31)), 0.0);
    }

    #[test]
    fn mrr_and_hit_at_k_by_hand() {
        // ranks 1, 2, 5, 11, none -> RR = 1, 1/2, 1/5, 1/11, 0
        let ranks = [Some(1), Some(2), Some(5), Some(11), None];
        let stats = rank_stats(&ranks);
        assert_eq!(stats.hit_at_1, 1.0 / 5.0);
        assert_eq!(stats.hit_at_5, 3.0 / 5.0);
        assert_eq!(stats.hit_at_10, 3.0 / 5.0);
        let expected_mrr = (1.0 + 0.5 + 0.2 + 1.0 / 11.0 + 0.0) / 5.0;
        assert!((stats.mrr - expected_mrr).abs() < 1e-12);
        assert_eq!(rank_stats(&[]), RankStats::default());
        assert_eq!(hit_at(Some(10), 10), 1.0);
        assert_eq!(hit_at(Some(11), 10), 0.0);
    }

    fn usage(repo: &str, path: &str, line: u32) -> Usage {
        Usage { repo: repo.into(), path: PathBuf::from(path), line, context: None }
    }

    fn gold() -> GoldUsages {
        GoldUsages {
            id: "u1".into(),
            symbol: "S".into(),
            definition: Definition { repo: "a".into(), path: "def.kt".into(), line: 1 },
            expected_usages: vec![
                ExpectedUsage { repo: "a".into(), path: "x.kt".into(), lines: vec![3, 7] },
                ExpectedUsage { repo: "b".into(), path: "y.ts".into(), lines: vec![2] },
            ],
            method: None,
        }
    }

    fn response(usages: Vec<Usage>) -> UsagesResponse {
        use uals_core::types::{BackendId, BackendStatus};
        UsagesResponse {
            symbol: "S".into(),
            total: usages.len(),
            offset: 0,
            repos: vec![RepoUsages {
                repo: "a".into(),
                source: BackendId::AstIndex,
                status: BackendStatus::Ok,
                usages,
            }],
            next_offset: None,
            complete: true,
        }
    }

    #[test]
    fn usages_recall_and_precision_by_hand() {
        // expected lines: a/x.kt:3, a/x.kt:7, b/y.ts:2 (3 lines, 2 files)
        // found: a/x.kt:3 (ok), a/x.kt:3 again (dup), a/x.kt:9 (extra), a/z.kt:1 (extra file),
        //        a/def.kt:1 (the definition, dropped)
        let found = response(vec![
            usage("a", "/ws/a/x.kt", 3),
            usage("a", "/ws/a/x.kt", 3),
            usage("a", "/ws/a/x.kt", 9),
            usage("a", "/ws/a/z.kt", 1),
            usage("a", "/ws/a/def.kt", 1),
        ]);
        let score = score_usages(&repos(), &gold(), &found);
        assert_eq!((score.expected_lines, score.found_lines), (3, 3));
        assert_eq!(score.line_recall, 1.0 / 3.0);
        assert_eq!(score.line_precision, 1.0 / 3.0);
        // files: expected {a/x.kt, b/y.ts}, found {a/x.kt, a/z.kt}
        assert_eq!((score.expected_files, score.found_files), (2, 2));
        assert_eq!(score.file_recall, 0.5);
        assert_eq!(score.file_precision, 0.5);
    }

    #[test]
    fn usages_perfect_and_empty() {
        let perfect =
            response(vec![usage("a", "/ws/a/x.kt", 3), usage("a", "/ws/a/x.kt", 7), usage("b", "/ws/b/y.ts", 2)]);
        let score = score_usages(&repos(), &gold(), &perfect);
        assert_eq!(
            (score.line_recall, score.line_precision, score.file_recall, score.file_precision),
            (1.0, 1.0, 1.0, 1.0)
        );
        let score = score_usages(&repos(), &gold(), &response(vec![]));
        assert_eq!(
            (score.line_recall, score.line_precision, score.file_recall, score.file_precision),
            (0.0, 0.0, 0.0, 0.0)
        );
    }

    #[test]
    fn right_file_wrong_line_gives_file_credit_only() {
        let found = response(vec![usage("a", "/ws/a/x.kt", 4)]);
        let score = score_usages(&repos(), &gold(), &found);
        assert_eq!(score.file_recall, 0.5);
        assert_eq!(score.file_precision, 1.0);
        assert_eq!(score.line_recall, 0.0);
        assert_eq!(score.line_precision, 0.0);
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let samples = [15.0, 20.0, 35.0, 40.0, 50.0];
        assert_eq!(percentile(&samples, 50.0), Some(35.0)); // ceil(0.5*5)=3 -> 35
        assert_eq!(percentile(&samples, 95.0), Some(50.0)); // ceil(4.75)=5
        assert_eq!(percentile(&samples, 0.0), Some(15.0));
        assert_eq!(percentile(&[7.0], 95.0), Some(7.0));
        assert_eq!(percentile(&[], 50.0), None);
        let ten: Vec<f64> = (1..=10).map(f64::from).collect();
        assert_eq!(percentile(&ten, 50.0), Some(5.0));
        assert_eq!(percentile(&ten, 95.0), Some(10.0));
    }

    #[test]
    fn tokens_are_chars_over_four_rounded_up() {
        assert_eq!(tokens_of(&"abc"), 2); // "abc" with quotes = 5 chars -> ceil(5/4)
        assert_eq!(tokens_of(&[1, 2, 3]), 2); // [1,2,3] = 7 chars
        assert_eq!(tokens_of(&"abcdef"), 2); // 8 chars -> exactly 2
        assert_eq!(tokens_of(&"abcdefg"), 3); // 9 chars
    }
}
