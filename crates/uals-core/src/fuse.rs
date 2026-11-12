//! Weighted reciprocal rank fusion (RRF). Implemented by track T6; the rules below and the cases
//! in `tests/golden/fusion/` are the contract.
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

use crate::config::FusionConfig;
use crate::types::{FusedHit, Hit, QueryKind, SymbolSpan};

/// Fuses the hits of all backends and repositories into one ranked list.
pub fn fuse(
    query: &str,
    kind: QueryKind,
    hits: Vec<Hit>,
    spans: &[SymbolSpan],
    config: &FusionConfig,
) -> Vec<FusedHit> {
    let _ = (query, kind, hits, spans, config);
    todo!("track T6: implement the rules in the module docs")
}
