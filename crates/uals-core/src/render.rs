//! Output for agents and people. Implemented by track T6; the formats below are the contract.
//!
//! **Search, `Concise` (default):**
//! ```text
//! search "HostSelector" · identifier · 1-3 of 3
//! 1. sample-android · /ws/sample-android/network/src/main/kotlin/sample/network/HostSelector.kt:3-30 · class HostSelector · definition · ast-index#1 lexical#1 semble#2
//!     3 | class HostSelector(
//!     4 |     private val store: HostStore,
//! 2. …
//! backends: sample-android: ast-index ok 12ms, lexical ok 240ms, semble timeout 4000ms
//! next: offset=12
//! ```
//! - Header: the query, the kind, the shown range and `total`.
//! - One line per hit: number, repo, `path:start-end`, the symbol with its kind if known,
//!   `definition` for definitions, `pinned` for pinned hits, and each source as `backend#rank`
//!   with 1-based ranks. Then the snippet: right-aligned line numbers, ` | `, the line; lines longer
//!   than 200 characters end in `…`.
//! - `backends:` lists every backend and repository; a non-ok status shows its reason in
//!   parentheses. Repositories with identical statuses may be joined.
//! - `next:` only when more hits remain.
//! - Budget: when the text exceeds `OutputConfig::max_output_chars`, drop snippets from the last
//!   hit upwards, then whole hits from the end. The header and `next:` show what was actually
//!   printed.
//!
//! **Search, `Detailed`:** the same, plus each hit's score and the raw source ranks, and snippets of
//! up to `2 × max_snippet_lines` lines.
//!
//! **Search, `Json`:** the response as compact JSON, trimmed with the same budget rule.
//!
//! **Usages, `Concise`:**
//! ```text
//! usages of "HostSelector" · 5 in 3 files · complete
//! sample-android (ast-index)
//!   /ws/sample-android/app/src/main/kotlin/sample/app/App.kt: 9, 14
//! sample-web (lexical: ast-index unavailable (no index))
//!   …
//! next: offset=100
//! ```
//! `incomplete` replaces `complete` when any repository answered partially.

use serde::{Deserialize, Serialize};

use crate::config::OutputConfig;
use crate::types::{FusedHit, LineRange, SearchResponse, UsagesResponse};

/// Output format of the `search` and `usages` tools.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    #[default]
    Concise,
    Detailed,
    Json,
}

/// The lines to read from disk for `hit`'s snippet: inside `hit.lines`, as close to centered on
/// `hit.focus_line` as the range allows, at most `max_snippet_lines` lines for `Concise` and
/// `Json` and `2 × max_snippet_lines` for `Detailed`.
pub fn snippet_range(hit: &FusedHit, format: Format, config: &OutputConfig) -> LineRange {
    let _ = (hit, format, config);
    todo!("track T6")
}

/// The page `[offset, offset + limit)` of `items`, and the offset of the next page if items remain.
pub fn page<T>(items: Vec<T>, offset: usize, limit: usize) -> (Vec<T>, Option<usize>) {
    let _ = (items, offset, limit);
    todo!("track T6")
}

pub fn render_search(response: &SearchResponse, format: Format, config: &OutputConfig) -> String {
    let _ = (response, format, config);
    todo!("track T6")
}

pub fn render_usages(response: &UsagesResponse, format: Format, config: &OutputConfig) -> String {
    let _ = (response, format, config);
    todo!("track T6")
}
