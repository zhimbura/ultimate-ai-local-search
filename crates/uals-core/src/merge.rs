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

use std::path::PathBuf;

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

/// Groups `hits` by merge unit. Groups come out sorted by repo, path and start line.
pub fn group(hits: Vec<Hit>, spans: &[SymbolSpan], gap: u32) -> Vec<HitGroup> {
    let _ = (hits, spans, gap);
    todo!("track T6: implement the rules in the module docs")
}
