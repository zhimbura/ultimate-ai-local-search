//! Weighted reciprocal rank fusion (RRF). The rules below and the cases in `tests/golden/fusion/`
//! are the contract.
//!
//! `fuse(query, kind, hits, spans, config)`:
//! 1. Drop hits whose backend weight for `kind` is 0 (`FusionConfig::weight`), before grouping, so
//!    they cannot widen a line window.
//! 2. Group the rest with `merge::group(hits, spans, config.merge_gap_lines)`.
//! 3. A hit contributes `weight(kind, backend) / (rrf_k + rank + 1)`. Ranks are per backend and per
//!    repository, so every repository's list counts the same; raw backend scores are never used.
//! 4. Group score: the sum over backends of each backend's largest contribution in the group (several
//!    hits of one backend in one group count once).
//! 5. Pinning, identifier queries only: a group is a candidate when one of its hits has
//!    `is_definition` and `symbol == classify::bare_name(query)` (exact, case-sensitive). The first
//!    `max_pinned` candidates in the order of rule 6 are pinned.
//! 6. Order: pinned groups first; then score, higher first (scores within 1e-9 are equal); then
//!    the number of distinct backends, more first; then repo, path and start line, ascending.
//!
//! Fields of each `FusedHit`:
//! - `repo`, `path`, `lines`: the group's.
//! - `symbol`, `symbol_kind`: the group's span; for a window, those of the best-contributing hit
//!   that has a symbol.
//! - `is_definition`: any hit of the group is a definition.
//! - `focus_line`: the start line of the best-contributing hit, clamped into `lines`. Ties between
//!   hits: lower rank, then earlier start line, then backend order (`BackendId` order).
//! - `sources`: one entry per backend with its best rank, ordered by contribution, larger first;
//!   ties in backend order.
//! - `score`: the group score. `snippet`: `None` (the orchestrator fills it).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::classify::bare_name;
use crate::config::FusionConfig;
use crate::merge::{HitGroup, group};
use crate::types::{BackendId, FusedHit, Hit, QueryKind, SourceRef, SymbolSpan};

/// Scores closer than this are equal.
const SCORE_TOLERANCE: f64 = 1e-9;

/// A fused group before ordering and pinning.
struct Candidate {
    fused: FusedHit,
    /// Has a definition hit named like the query.
    defines_query: bool,
}

/// Fuses the hits of all backends and repositories into one ranked list.
pub fn fuse(
    query: &str,
    kind: QueryKind,
    hits: Vec<Hit>,
    spans: &[SymbolSpan],
    config: &FusionConfig,
) -> Vec<FusedHit> {
    let hits: Vec<Hit> = hits.into_iter().filter(|hit| config.weight(kind, hit.backend) > 0.0).collect();
    let target = (kind == QueryKind::Identifier).then(|| bare_name(query));
    let mut candidates: Vec<Candidate> =
        group(hits, spans, config.merge_gap_lines).into_iter().map(|g| fuse_group(g, kind, target, config)).collect();

    sort_by_score(&mut candidates);
    let max_pins = if target.is_some() { config.max_pinned } else { 0 };
    pin_definitions(&mut candidates, max_pins);
    // Stable: pinned groups keep their score order, and so do the rest.
    candidates.sort_by_key(|candidate| !candidate.fused.pinned);
    candidates.into_iter().map(|candidate| candidate.fused).collect()
}

/// Pins the first `max_pins` candidates (in their current order) that define the queried name.
fn pin_definitions(candidates: &mut [Candidate], max_pins: usize) {
    let mut free_pins = max_pins;
    for candidate in candidates {
        if candidate.defines_query && free_pins > 0 {
            candidate.fused.pinned = true;
            free_pins -= 1;
        }
    }
}

/// What one hit adds to its group's score.
fn contribution(hit: &Hit, kind: QueryKind, config: &FusionConfig) -> f64 {
    config.weight(kind, hit.backend) / (config.rrf_k + f64::from(hit.rank) + 1.0)
}

/// Scores one group and fills the fields of its `FusedHit`.
fn fuse_group(group: HitGroup, kind: QueryKind, target: Option<&str>, config: &FusionConfig) -> Candidate {
    let contributions: Vec<f64> = group.hits.iter().map(|hit| contribution(hit, kind, config)).collect();
    let (score, sources) = score_and_sources(&group.hits, &contributions);

    let ranked: Vec<(&Hit, f64)> = group.hits.iter().zip(contributions.iter().copied()).collect();
    let focus = best_of(ranked.iter().copied());
    let named = best_of(ranked.iter().copied().filter(|(hit, _)| hit.symbol.is_some()));
    let (symbol, symbol_kind) = match (&group.symbol, named) {
        (Some(symbol), _) => (Some(symbol.clone()), group.symbol_kind),
        (None, Some(hit)) => (hit.symbol.clone(), hit.symbol_kind),
        (None, None) => (None, None),
    };
    let focus_line = focus.map_or(group.lines.start, |hit| hit.lines.start.clamp(group.lines.start, group.lines.end));
    let defines_query = target
        .is_some_and(|name| group.hits.iter().any(|hit| hit.is_definition && hit.symbol.as_deref() == Some(name)));

    Candidate {
        defines_query,
        fused: FusedHit {
            repo: group.repo,
            path: group.path,
            lines: group.lines,
            symbol,
            symbol_kind,
            is_definition: group.hits.iter().any(|hit| hit.is_definition),
            pinned: false,
            focus_line,
            snippet: None,
            score,
            sources,
        },
    }
}

/// The group score (rule 4) and its sources (one per backend, best rank, larger contribution first).
/// `contributions[i]` belongs to `hits[i]`.
fn score_and_sources(hits: &[Hit], contributions: &[f64]) -> (f64, Vec<SourceRef>) {
    // Per backend: its best contribution and the lowest rank it gave.
    let mut per_backend: BTreeMap<BackendId, (f64, u32)> = BTreeMap::new();
    for (hit, &value) in hits.iter().zip(contributions) {
        let entry = per_backend.entry(hit.backend).or_insert((value, hit.rank));
        entry.0 = entry.0.max(value);
        entry.1 = entry.1.min(hit.rank);
    }
    let score = per_backend.values().map(|&(value, _)| value).sum();
    let mut by_contribution: Vec<(BackendId, f64, u32)> =
        per_backend.into_iter().map(|(backend, (value, rank))| (backend, value, rank)).collect();
    by_contribution.sort_by(|a, b| b.1.total_cmp(&a.1)); // stable: ties stay in backend order
    let sources = by_contribution.into_iter().map(|(backend, _, rank)| SourceRef { backend, rank }).collect();
    (score, sources)
}

/// The hit with the largest contribution; ties go to the lower rank, the earlier start line, then
/// the backend order.
fn best_of<'a>(hits: impl Iterator<Item = (&'a Hit, f64)>) -> Option<&'a Hit> {
    let mut best: Option<(&Hit, f64)> = None;
    for (hit, value) in hits {
        let better = best.is_none_or(|(current, current_value)| {
            if (value - current_value).abs() > SCORE_TOLERANCE {
                value > current_value
            } else {
                (hit.rank, hit.lines.start, hit.backend) < (current.rank, current.lines.start, current.backend)
            }
        });
        if better {
            best = Some((hit, value));
        }
    }
    best.map(|(hit, _)| hit)
}

/// Orders by score (within the tolerance equal), then backend count, then repo, path and start line.
fn sort_by_score(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| b.fused.score.total_cmp(&a.fused.score));
    // Scores that differ by less than the tolerance form one tied run, ordered by the other keys.
    let mut start = 0;
    while start < candidates.len() {
        let top = candidates[start].fused.score;
        let end = start
            + candidates[start..].iter().take_while(|candidate| top - candidate.fused.score <= SCORE_TOLERANCE).count();
        candidates[start..end].sort_by(tie_break);
        start = end;
    }
}

/// Orders tied candidates: more backends first, then repo, path, start and end line ascending.
fn tie_break(a: &Candidate, b: &Candidate) -> Ordering {
    let (a, b) = (&a.fused, &b.fused);
    b.sources
        .len()
        .cmp(&a.sources.len())
        .then_with(|| a.repo.cmp(&b.repo))
        .then_with(|| a.path.cmp(&b.path))
        .then(a.lines.start.cmp(&b.lines.start))
        .then(a.lines.end.cmp(&b.lines.end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{LineRange, SymbolKind};

    fn hit(backend: BackendId, repo: &str, path: &str, start: u32, end: u32, rank: u32) -> Hit {
        Hit {
            backend,
            repo: repo.into(),
            path: path.into(),
            lines: LineRange::new(start, end),
            symbol: None,
            symbol_kind: None,
            is_definition: false,
            snippet: None,
            rank,
            score: Some(9.0 - f64::from(rank)),
        }
    }

    fn definition(repo: &str, path: &str, name: &str, rank: u32) -> Hit {
        Hit {
            symbol: Some(name.into()),
            symbol_kind: Some(SymbolKind::Class),
            is_definition: true,
            ..hit(BackendId::AstIndex, repo, path, 1, 10, rank)
        }
    }

    fn run(query: &str, kind: QueryKind, hits: Vec<Hit>) -> Vec<FusedHit> {
        fuse(query, kind, hits, &[], &FusionConfig::default())
    }

    #[test]
    fn empty_input_gives_nothing() {
        assert!(run("Foo", QueryKind::Identifier, Vec::new()).is_empty());
    }

    #[test]
    fn only_weightless_hits_give_nothing() {
        let hits = vec![hit(BackendId::Lexical, "r", "/ws/r/a.kt", 1, 1, 0)];
        assert!(run("how does it work", QueryKind::Natural, hits).is_empty());
    }

    #[test]
    fn hits_in_unknown_files_fall_back_to_windows() {
        let hits = vec![
            hit(BackendId::Semble, "r", "/ws/r/unknown.kt", 10, 12, 0),
            hit(BackendId::Semble, "r", "/ws/r/unknown.kt", 14, 20, 1),
        ];
        let spans = [SymbolSpan {
            repo: "r".into(),
            path: "/ws/r/other.kt".into(),
            name: "f".into(),
            kind: SymbolKind::Function,
            lines: LineRange::new(1, 99),
        }];
        let fused = fuse("where", QueryKind::Natural, hits, &spans, &FusionConfig::default());
        assert_eq!(fused.len(), 1);
        assert_eq!(
            (fused[0].lines, fused[0].symbol.as_deref(), fused[0].focus_line),
            (LineRange::new(10, 20), None, 10)
        );
    }

    #[test]
    fn equal_scores_go_by_backend_count_then_location() {
        let config = FusionConfig::default();
        let (ast, lex, sem) = (BackendId::AstIndex, BackendId::Lexical, BackendId::Semble);
        // Identifier weights: ast-index 1.0, lexical 0.7, semantic 0.3. Repo "z" has two backends whose
        // sum equals the single ast-index contribution of "a" and "b" (1.0 / 61 = 0.7 / 61 + 0.3 / 61).
        let hits = vec![
            hit(ast, "b", "/ws/b/x.kt", 1, 1, 0),
            hit(ast, "a", "/ws/a/x.kt", 1, 1, 0),
            hit(lex, "z", "/ws/z/x.kt", 1, 1, 0),
            hit(sem, "z", "/ws/z/x.kt", 1, 1, 0),
        ];
        let fused = fuse("q", QueryKind::Identifier, hits, &[], &config);
        let repos: Vec<&str> = fused.iter().map(|f| f.repo.as_str()).collect();
        assert_eq!(repos, vec!["z", "a", "b"]);
    }

    #[test]
    fn pinning_needs_an_identifier_query_and_an_exact_name() {
        let hits = vec![definition("r", "/ws/r/Foo.kt", "Foo", 0), definition("r", "/ws/r/foo.kt", "foo", 1)];
        let identifier = run("Foo", QueryKind::Identifier, hits.clone());
        assert_eq!(identifier.iter().map(|f| f.pinned).collect::<Vec<_>>(), vec![true, false]);
        let natural = fuse("Foo", QueryKind::Natural, hits.clone(), &[], &FusionConfig::default());
        assert!(natural.iter().all(|f| !f.pinned));
        let qualified = run("pkg.Foo()", QueryKind::Identifier, hits);
        assert_eq!(qualified.iter().filter(|f| f.pinned).count(), 1);
    }

    #[test]
    fn max_pinned_zero_pins_nothing() {
        let config = FusionConfig { max_pinned: 0, ..FusionConfig::default() };
        let fused = fuse("Foo", QueryKind::Identifier, vec![definition("r", "/ws/r/Foo.kt", "Foo", 0)], &[], &config);
        assert!(!fused[0].pinned);
    }

    #[test]
    fn symbol_of_a_window_comes_from_the_best_hit_with_a_symbol() {
        let mut named = hit(BackendId::Lexical, "r", "/ws/r/a.kt", 11, 11, 5);
        named.symbol = Some("later".into());
        let mut better = hit(BackendId::AstIndex, "r", "/ws/r/a.kt", 12, 12, 1);
        better.symbol = Some("better".into());
        better.symbol_kind = Some(SymbolKind::Function);
        let plain = hit(BackendId::Semble, "r", "/ws/r/a.kt", 10, 10, 0);
        let fused = run("Foo", QueryKind::Identifier, vec![named, better, plain]);
        assert_eq!(fused[0].symbol.as_deref(), Some("better"));
        assert_eq!(fused[0].symbol_kind, Some(SymbolKind::Function));
        assert_eq!(fused[0].lines, LineRange::new(10, 12));
    }

    #[test]
    fn focus_ties_prefer_lower_rank_then_earlier_line_then_backend_order() {
        // Same backend, same rank is impossible in real input, but equal contributions across
        // backends with equal weights are not: semble and claude-context share a weight.
        let a = hit(BackendId::ClaudeContext, "r", "/ws/r/a.kt", 5, 6, 0);
        let b = hit(BackendId::Semble, "r", "/ws/r/a.kt", 3, 4, 0);
        let fused = run("where", QueryKind::Natural, vec![a, b]);
        assert_eq!(fused[0].focus_line, 3);
        let same_line = vec![
            hit(BackendId::ClaudeContext, "r", "/ws/r/a.kt", 3, 4, 0),
            hit(BackendId::Semble, "r", "/ws/r/a.kt", 3, 4, 0),
        ];
        let fused = run("where", QueryKind::Natural, same_line);
        assert_eq!(
            fused[0].sources.iter().map(|s| s.backend).collect::<Vec<_>>(),
            vec![BackendId::Semble, BackendId::ClaudeContext]
        );
    }

    #[test]
    fn snippet_is_left_for_the_orchestrator() {
        let mut with_text = hit(BackendId::Semble, "r", "/ws/r/a.kt", 1, 2, 0);
        with_text.snippet = Some("text".into());
        assert!(run("where", QueryKind::Natural, vec![with_text])[0].snippet.is_none());
    }
}
