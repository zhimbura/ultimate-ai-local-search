//! Grouping hits that point to the same code. Implemented by track T6; the rules below and the
//! cases in `tests/golden/fusion/` are the contract.
//!
//! Each hit (repo, path, lines) gets a merge unit. Only spans of the same repo and path count:
//! 1. Among the spans that share at least one line with the hit, the one with the highest
//!    overlap ratio: shared lines divided by the lines of the union of both ranges. Ties go to
//!    the smaller span, then to the one that starts first, then to the name. So a one-line hit
//!    lands in the innermost span around it, and a definition hit that covers a whole class
//!    lands in that class, not in its biggest method.
//! 2. No span shares a line with the hit: the hit joins a line window. Window hits of the same
//!    repo and path are chained when their ranges overlap or the gap between them is at most
//!    `gap` lines; the window is the union of their ranges. A window never absorbs a hit that has
//!    a span unit.
//!
//! Hits with the same unit form one group, whatever backend they come from.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::types::{Hit, LineRange, SymbolKind, SymbolSpan};

/// Hits that share a merge unit.
#[derive(Debug, Clone, PartialEq)]
pub struct HitGroup {
    pub repo: String,
    /// Absolute path.
    pub path: PathBuf,
    /// The span's lines (rule 1), or the window's union (rule 2).
    pub lines: LineRange,
    /// The span's symbol; `None` for a window.
    pub symbol: Option<String>,
    pub symbol_kind: Option<SymbolKind>,
    /// The group's hits, in input order.
    pub hits: Vec<Hit>,
}

/// A span and its position in the input slice.
type IndexedSpan<'a> = (usize, &'a SymbolSpan);

/// Groups `hits` by merge unit. Groups come out sorted by repo, path and start line.
pub fn group(hits: Vec<Hit>, spans: &[SymbolSpan], gap: u32) -> Vec<HitGroup> {
    let mut spans_by_file: BTreeMap<(&str, &Path), Vec<IndexedSpan>> = BTreeMap::new();
    for (index, span) in spans.iter().enumerate() {
        spans_by_file.entry((span.repo.as_str(), span.path.as_path())).or_default().push((index, span));
    }

    // Rule 1: a hit joins the span it overlaps best. What no span touches goes to rule 2, per file.
    let mut span_hits: Vec<Vec<Hit>> = vec![Vec::new(); spans.len()];
    let mut loose: BTreeMap<(String, PathBuf), Vec<Hit>> = BTreeMap::new();
    for hit in hits {
        let candidates = spans_by_file.get(&(hit.repo.as_str(), hit.path.as_path()));
        match candidates.and_then(|candidates| best_span(hit.lines, candidates)) {
            Some(index) => span_hits[index].push(hit),
            None => loose.entry((hit.repo.clone(), hit.path.clone())).or_default().push(hit),
        }
    }

    let mut groups: Vec<HitGroup> = Vec::new();
    for (span, hits) in spans.iter().zip(span_hits).filter(|(_, hits)| !hits.is_empty()) {
        groups.push(HitGroup {
            repo: span.repo.clone(),
            path: span.path.clone(),
            lines: span.lines,
            symbol: Some(span.name.clone()),
            symbol_kind: Some(span.kind),
            hits,
        });
    }
    for ((repo, path), hits) in loose {
        for (lines, hits) in chain_windows(hits, gap) {
            groups.push(HitGroup {
                repo: repo.clone(),
                path: path.clone(),
                lines,
                symbol: None,
                symbol_kind: None,
                hits,
            });
        }
    }
    groups.sort_by(|a, b| {
        (&a.repo, &a.path, a.lines.start, a.lines.end).cmp(&(&b.repo, &b.path, b.lines.start, b.lines.end))
    });
    groups
}

/// The index of the span with the best overlap ratio with `lines`, among spans that share a line.
fn best_span(lines: LineRange, candidates: &[IndexedSpan]) -> Option<usize> {
    let mut best: Option<IndexedSpan> = None;
    for &(index, span) in candidates {
        if span.lines.overlap(&lines) == 0 {
            continue;
        }
        if best.is_none_or(|(_, current)| compare_spans(lines, span, current) == Ordering::Less) {
            best = Some((index, span));
        }
    }
    best.map(|(index, _)| index)
}

/// `Less` when `a` is the better home for `hit`: higher overlap ratio, then smaller, then earlier
/// start, then name.
fn compare_spans(hit: LineRange, a: &SymbolSpan, b: &SymbolSpan) -> Ordering {
    let ratio =
        |span: &SymbolSpan| (u64::from(span.lines.overlap(&hit)), u64::from(span.lines.union(&hit).line_count()));
    let (a_shared, a_union) = ratio(a);
    let (b_shared, b_union) = ratio(b);
    // a_shared / a_union > b_shared / b_union, compared without floats.
    (b_shared * a_union)
        .cmp(&(a_shared * b_union))
        .then(a.lines.line_count().cmp(&b.lines.line_count()))
        .then(a.lines.start.cmp(&b.lines.start))
        .then_with(|| a.name.cmp(&b.name))
}

/// Chains hits of one file into windows: ranges that overlap, or lie at most `gap` lines apart,
/// share a window. Returns each window's union with its hits in input order.
fn chain_windows(hits: Vec<Hit>, gap: u32) -> Vec<(LineRange, Vec<Hit>)> {
    let mut order: Vec<usize> = (0..hits.len()).collect();
    order.sort_by_key(|&index| (hits[index].lines.start, hits[index].lines.end, index));

    let mut windows: Vec<(LineRange, Vec<usize>)> = Vec::new();
    for index in order {
        let lines = hits[index].lines;
        match windows.last_mut() {
            Some((union, members)) if union.gap(&lines) <= gap => {
                *union = union.union(&lines);
                members.push(index);
            }
            _ => windows.push((lines, vec![index])),
        }
    }

    let mut slots: Vec<Option<Hit>> = hits.into_iter().map(Some).collect();
    windows
        .into_iter()
        .map(|(union, mut members)| {
            members.sort_unstable();
            let members = members.into_iter().filter_map(|index| slots[index].take()).collect();
            (union, members)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BackendId;

    fn hit(repo: &str, path: &str, start: u32, end: u32, rank: u32) -> Hit {
        Hit {
            backend: BackendId::Semble,
            repo: repo.into(),
            path: path.into(),
            lines: LineRange::new(start, end),
            symbol: None,
            symbol_kind: None,
            is_definition: false,
            snippet: None,
            rank,
            score: None,
        }
    }

    fn span(path: &str, name: &str, start: u32, end: u32) -> SymbolSpan {
        SymbolSpan {
            repo: "r".into(),
            path: path.into(),
            name: name.into(),
            kind: SymbolKind::Function,
            lines: LineRange::new(start, end),
        }
    }

    fn lines_of(groups: &[HitGroup]) -> Vec<(u32, u32, usize)> {
        groups.iter().map(|g| (g.lines.start, g.lines.end, g.hits.len())).collect()
    }

    #[test]
    fn empty_input_gives_no_groups() {
        assert!(group(Vec::new(), &[], 3).is_empty());
        assert!(group(Vec::new(), &[span("/ws/r/a.kt", "f", 1, 5)], 3).is_empty());
    }

    #[test]
    fn spans_without_hits_make_no_group() {
        let groups = group(vec![hit("r", "/ws/r/a.kt", 20, 21, 0)], &[span("/ws/r/a.kt", "f", 1, 5)], 3);
        assert_eq!(lines_of(&groups), vec![(20, 21, 1)]);
        assert_eq!(groups[0].symbol, None);
    }

    #[test]
    fn spans_of_other_files_and_repos_are_ignored() {
        let mut other_repo = span("/ws/r/a.kt", "f", 1, 50);
        other_repo.repo = "other".into();
        let spans = [other_repo, span("/ws/r/b.kt", "g", 1, 50)];
        let groups = group(vec![hit("r", "/ws/r/a.kt", 5, 6, 0)], &spans, 3);
        assert_eq!(lines_of(&groups), vec![(5, 6, 1)]);
        assert_eq!(groups[0].symbol_kind, None);
    }

    #[test]
    fn equal_ratio_and_size_goes_to_the_earlier_span_then_the_name() {
        let hit_on_border = hit("r", "/ws/r/a.kt", 5, 6, 0);
        let spans = [span("/ws/r/a.kt", "late", 6, 10), span("/ws/r/a.kt", "early", 1, 5)];
        let groups = group(vec![hit_on_border.clone()], &spans, 3);
        assert_eq!(groups[0].symbol.as_deref(), Some("early"));

        let same = [span("/ws/r/a.kt", "b", 1, 10), span("/ws/r/a.kt", "a", 1, 10)];
        let groups = group(vec![hit_on_border], &same, 3);
        assert_eq!(groups[0].symbol.as_deref(), Some("a"));
    }

    #[test]
    fn a_whole_class_hit_lands_in_the_class() {
        let spans = [span("/ws/r/a.kt", "Outer", 1, 30), span("/ws/r/a.kt", "big", 5, 28)];
        let groups = group(vec![hit("r", "/ws/r/a.kt", 1, 30, 0)], &spans, 3);
        assert_eq!(groups[0].symbol.as_deref(), Some("Outer"));
    }

    #[test]
    fn window_chains_by_gap_and_does_not_absorb_span_hits() {
        let spans = [span("/ws/r/a.kt", "f", 50, 60)];
        let hits = vec![
            hit("r", "/ws/r/a.kt", 20, 22, 0),
            hit("r", "/ws/r/a.kt", 55, 55, 1),
            hit("r", "/ws/r/a.kt", 26, 30, 2), // gap of 3 lines: joins
            hit("r", "/ws/r/a.kt", 34, 34, 3), // gap of 3 lines after the union: joins
            hit("r", "/ws/r/a.kt", 39, 40, 4), // gap of 4 lines: its own window
        ];
        let groups = group(hits, &spans, 3);
        assert_eq!(lines_of(&groups), vec![(20, 34, 3), (39, 40, 1), (50, 60, 1)]);
        let ranks: Vec<u32> = groups[0].hits.iter().map(|h| h.rank).collect();
        assert_eq!(ranks, vec![0, 2, 3], "input order inside a group");
    }

    #[test]
    fn a_long_hit_swallows_the_hits_inside_it() {
        let hits = vec![
            hit("r", "/ws/r/a.kt", 5, 5, 0),
            hit("r", "/ws/r/a.kt", 1, 100_000, 1),
            hit("r", "/ws/r/a.kt", 99_990, 99_991, 2),
        ];
        let groups = group(hits, &[], 0);
        assert_eq!(lines_of(&groups), vec![(1, 100_000, 3)]);
    }

    #[test]
    fn groups_are_sorted_by_repo_path_and_start() {
        let hits = vec![
            hit("b", "/ws/b/a.kt", 1, 1, 0),
            hit("a", "/ws/a/z.kt", 1, 1, 1),
            hit("a", "/ws/a/a.kt", 40, 40, 2),
            hit("a", "/ws/a/a.kt", 1, 1, 3),
        ];
        let keys: Vec<(String, u32)> =
            group(hits, &[], 3).iter().map(|g| (format!("{}{}", g.repo, g.path.display()), g.lines.start)).collect();
        assert_eq!(
            keys,
            vec![
                ("a/ws/a/a.kt".to_string(), 1),
                ("a/ws/a/a.kt".to_string(), 40),
                ("a/ws/a/z.kt".to_string(), 1),
                ("b/ws/b/a.kt".to_string(), 1)
            ]
        );
    }
}
