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
//!   printed. The first hit always stays (without its snippet if need be), so a page never comes
//!   out empty and `next:` always moves forward.
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

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::OutputConfig;
use crate::types::{
    BackendReport, BackendStatus, FusedHit, LineRange, QueryKind, RepoUsages, SearchResponse, Snippet, SymbolKind,
    UsagesResponse,
};

/// Snippet lines longer than this many characters are cut.
const MAX_LINE_CHARS: usize = 200;
const SEPARATOR: &str = " · ";

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
    let factor = if format == Format::Detailed { 2 } else { 1 };
    let capacity = config.max_snippet_lines.saturating_mul(factor);
    let capacity = u32::try_from(capacity).unwrap_or(u32::MAX).max(1);
    let lines = hit.lines;
    if lines.line_count() <= capacity {
        return lines;
    }
    let focus = hit.focus_line.clamp(lines.start, lines.end);
    let last_start = lines.end - (capacity - 1);
    let start = focus.saturating_sub(capacity / 2).clamp(lines.start, last_start);
    LineRange { start, end: start + (capacity - 1) }
}

/// The page `[offset, offset + limit)` of `items`, and the offset of the next page if items remain.
pub fn page<T>(items: Vec<T>, offset: usize, limit: usize) -> (Vec<T>, Option<usize>) {
    let total = items.len();
    let end = offset.saturating_add(limit).min(total);
    let next = (end < total).then_some(end.max(offset));
    let items = items.into_iter().skip(offset).take(limit).collect();
    (items, next)
}

pub fn render_search(response: &SearchResponse, format: Format, config: &OutputConfig) -> String {
    let render = |view: &SearchResponse| match format {
        Format::Json => to_json(view),
        Format::Concise | Format::Detailed => search_text(view, format),
    };
    let fits = |text: &str| text.chars().count() <= config.max_output_chars;

    let mut view = response.clone();
    let mut text = render(&view);
    let mut index = view.hits.len();
    while !fits(&text) && index > 0 {
        index -= 1;
        if view.hits[index].snippet.take().is_some() {
            text = render(&view);
        }
    }
    while !fits(&text) && view.hits.len() > 1 {
        view.hits.pop();
        view.next_offset = Some(view.offset + view.hits.len());
        text = render(&view);
    }
    text
}

pub fn render_usages(response: &UsagesResponse, format: Format, config: &OutputConfig) -> String {
    let render = |view: &UsagesResponse| match format {
        Format::Json => to_json(view),
        Format::Concise | Format::Detailed => usages_text(view, format),
    };
    let fits = |view: &UsagesResponse| render(view).chars().count() <= config.max_output_chars;

    let shown = shown_usages(response);
    if shown <= 1 || fits(response) {
        return render(response);
    }
    // The largest number of usages (at least one) that fits: fewer usages never make the text longer.
    let (mut low, mut high) = (1, shown - 1);
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if fits(&keep_usages(response, middle)) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    render(&keep_usages(response, low))
}

fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|err| format!("{{\"error\":\"cannot encode the response: {err}\"}}"))
}

fn kind_name(kind: QueryKind) -> &'static str {
    match kind {
        QueryKind::Identifier => "identifier",
        QueryKind::Natural => "natural",
        QueryKind::Regex => "regex",
        QueryKind::Literal => "literal",
    }
}

fn symbol_kind_name(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Class => "class",
        SymbolKind::Interface => "interface",
        SymbolKind::Object => "object",
        SymbolKind::Enum => "enum",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::Function => "function",
        SymbolKind::Property => "property",
        SymbolKind::Constant => "constant",
        SymbolKind::Module => "module",
        SymbolKind::Other => "symbol",
    }
}

// ---- search ----

fn search_text(response: &SearchResponse, format: Format) -> String {
    let shown = response.hits.len();
    let range =
        if shown == 0 { "0".to_string() } else { format!("{}-{}", response.offset + 1, response.offset + shown) };
    let mut lines = vec![format!(
        "search \"{}\"{SEPARATOR}{}{SEPARATOR}{range} of {}",
        response.query,
        kind_name(response.kind),
        response.total
    )];
    for (index, hit) in response.hits.iter().enumerate() {
        lines.push(hit_line(response.offset + index + 1, hit, format));
        if let Some(snippet) = &hit.snippet {
            push_snippet(&mut lines, snippet);
        }
    }
    if !response.backends.is_empty() {
        lines.push(backends_line(&response.backends));
    }
    if let Some(next) = response.next_offset {
        lines.push(format!("next: offset={next}"));
    }
    lines.join("\n")
}

fn hit_line(number: usize, hit: &FusedHit, format: Format) -> String {
    let mut parts = vec![hit.repo.clone(), format!("{}:{}-{}", hit.path.display(), hit.lines.start, hit.lines.end)];
    if let Some(symbol) = &hit.symbol {
        parts.push(match hit.symbol_kind {
            Some(kind) => format!("{} {symbol}", symbol_kind_name(kind)),
            None => symbol.clone(),
        });
    }
    if hit.is_definition {
        parts.push("definition".into());
    }
    if hit.pinned {
        parts.push("pinned".into());
    }
    if format == Format::Detailed {
        parts.push(format!("score {:.5}", hit.score));
    }
    if !hit.sources.is_empty() {
        let sources: Vec<String> = hit.sources.iter().map(|s| format!("{}#{}", s.backend, s.rank + 1)).collect();
        parts.push(sources.join(" "));
    }
    format!("{number}. {}", parts.join(SEPARATOR))
}

/// Adds the snippet's lines, indented, with right-aligned line numbers.
fn push_snippet(out: &mut Vec<String>, snippet: &Snippet) {
    let text_lines: Vec<&str> = snippet.text.lines().collect();
    let Some(count) = u32::try_from(text_lines.len()).ok().filter(|count| *count > 0) else { return };
    let width = snippet.lines.start.saturating_add(count - 1).to_string().len();
    for (number, text) in (snippet.lines.start..).zip(text_lines) {
        out.push(format!("    {number:>width$} | {}", cut_line(text)).trim_end().to_string());
    }
}

fn cut_line(line: &str) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_string(),
    }
}

fn backends_line(reports: &[BackendReport]) -> String {
    let mut by_repo: Vec<(&str, Vec<String>)> = Vec::new();
    for report in reports {
        let entry = report_text(report);
        match by_repo.iter_mut().find(|(repo, _)| *repo == report.repo) {
            Some((_, entries)) => entries.push(entry),
            None => by_repo.push((report.repo.as_str(), vec![entry])),
        }
    }
    let repos: Vec<String> = by_repo.iter().map(|(repo, entries)| format!("{repo}: {}", entries.join(", "))).collect();
    format!("backends: {}", repos.join("; "))
}

fn report_text(report: &BackendReport) -> String {
    let (state, reason) = match &report.status {
        BackendStatus::Ok => ("ok", None),
        BackendStatus::Stale { reason } => ("stale", Some(reason)),
        BackendStatus::Partial { reason } => ("partial", Some(reason)),
        BackendStatus::Timeout => ("timeout", None),
        BackendStatus::Unavailable { reason } => ("unavailable", Some(reason)),
        BackendStatus::Failed { reason } => ("failed", Some(reason)),
    };
    let reason = reason.filter(|reason| !reason.is_empty()).map(|reason| format!(" ({reason})"));
    format!("{} {state}{} {}ms", report.backend, reason.unwrap_or_default(), report.elapsed_ms)
}

// ---- usages ----

fn shown_usages(response: &UsagesResponse) -> usize {
    response.repos.iter().map(|repo| repo.usages.len()).sum()
}

/// A copy of `response` with only the first `keep` usages; `next_offset` points past the last one.
fn keep_usages(response: &UsagesResponse, keep: usize) -> UsagesResponse {
    let mut view = response.clone();
    let mut left = keep;
    for repo in &mut view.repos {
        repo.usages.truncate(left);
        left -= repo.usages.len();
    }
    view.repos.retain(|repo| {
        !repo.usages.is_empty() || response.repos.iter().any(|r| r.repo == repo.repo && r.usages.is_empty())
    });
    view.next_offset = Some(view.offset + keep);
    view
}

fn usages_text(response: &UsagesResponse, format: Format) -> String {
    let shown = shown_usages(response);
    let files: usize = response
        .repos
        .iter()
        .map(|repo| {
            repo.usages.iter().map(|usage| usage.path.as_path()).collect::<std::collections::BTreeSet<_>>().len()
        })
        .sum();
    let count = if shown < response.total { format!("{shown} of {}", response.total) } else { shown.to_string() };
    let state = if response.complete { "complete" } else { "incomplete" };
    let mut lines =
        vec![format!("usages of \"{}\"{SEPARATOR}{count} in {files} files{SEPARATOR}{state}", response.symbol)];
    for repo in &response.repos {
        lines.push(repo_heading(repo));
        push_usage_rows(&mut lines, repo, format);
    }
    if let Some(next) = response.next_offset {
        lines.push(format!("next: offset={next}"));
    }
    lines.join("\n")
}

fn repo_heading(repo: &RepoUsages) -> String {
    match status_note(&repo.status) {
        Some(note) => format!("{} ({}: {note})", repo.repo, repo.source),
        None => format!("{} ({})", repo.repo, repo.source),
    }
}

/// Why a repository's usages are not a plain answer of the source; `None` for a clean one.
fn status_note(status: &BackendStatus) -> Option<String> {
    match status {
        BackendStatus::Ok => None,
        BackendStatus::Timeout => Some("timeout".into()),
        BackendStatus::Stale { reason } => Some(format!("stale: {reason}")),
        BackendStatus::Partial { reason } => Some(format!("partial: {reason}")),
        BackendStatus::Unavailable { reason } => Some(reason.clone()),
        BackendStatus::Failed { reason } => Some(format!("failed: {reason}")),
    }
}

fn push_usage_rows(out: &mut Vec<String>, repo: &RepoUsages, format: Format) {
    let mut by_path: BTreeMap<&Path, Vec<&crate::types::Usage>> = BTreeMap::new();
    let mut order: Vec<&Path> = Vec::new();
    for usage in &repo.usages {
        let entry = by_path.entry(usage.path.as_path()).or_default();
        if entry.is_empty() {
            order.push(usage.path.as_path());
        }
        entry.push(usage);
    }
    for path in order {
        let usages = &by_path[path];
        if format == Format::Detailed {
            out.push(format!("  {}", path.display()));
            for usage in usages {
                let context = usage.context.as_deref().map(|text| format!(" {}", cut_line(text))).unwrap_or_default();
                out.push(format!("    {}:{context}", usage.line));
            }
        } else {
            let numbers: Vec<String> = usages.iter().map(|usage| usage.line.to_string()).collect();
            out.push(format!("  {}: {}", path.display(), numbers.join(", ")));
        }
    }
}
