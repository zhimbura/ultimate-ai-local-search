//! Parsers for the texts `@zilliz/claude-context-mcp` 0.1.15 returns.
//!
//! Every format below is read from the package source (`dist/handlers.js`), not recorded from a
//! live run: Milvus was down when the adapter was written (spike S1). The line numbers refer to
//! that file; re-check the formats against a live server when Milvus is available.
//!
//! The server returns one text block per call and no structured content.

use uals_core::types::LineRange;

/// One `search_code` result block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedResult {
    /// Path relative to the indexed codebase root, as the server printed it.
    pub rel_path: String,
    pub lines: LineRange,
    /// The code between the fences; `None` when empty.
    pub code: Option<String>,
}

/// What `search_code` said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SearchReply {
    Results {
        /// The indexed codebase the paths are relative to. It can be a parent of the requested
        /// path (`Requested path ... is covered by indexed codebase`).
        root: Option<String>,
        indexing: bool,
        /// The `N` of `Found N results`.
        declared: usize,
        results: Vec<ParsedResult>,
    },
    NoResults {
        indexing: bool,
    },
    NotIndexed,
    IndexLost,
    /// Milvus or the embedding service could not be reached while searching.
    Unreachable(String),
    Failed(String),
}

/// `get_indexing_status` outcome.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum StatusReply {
    Indexed {
        /// Tracked codebase that covers the requested path, when it is not the path itself.
        covered_by: Option<String>,
        /// "7 files, 12 chunks".
        stats: Option<String>,
    },
    Indexing {
        percent: Option<f64>,
    },
    Failed {
        error: String,
    },
    NotIndexed,
    Error(String),
}

/// `index_codebase` outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum IndexReply {
    Started,
    AlreadyIndexing,
    AlreadyIndexed,
    Unreachable(String),
    Failed(String),
}

const RESULT_START: &str = "\n\n1. Code snippet (";

/// Parses a `search_code` text (`handleSearchCode`, handlers.js:567-747).
pub(super) fn parse_search(text: &str, is_error: bool) -> SearchReply {
    // Error texts: handlers.js:580 (path), 626 and 636 (not indexed), 681 (lost), 742 (other).
    if text.contains("is not indexed. Please index it first") {
        return SearchReply::NotIndexed;
    }
    if text.contains("has been lost (collection not found in Milvus)") {
        return SearchReply::IndexLost;
    }
    if is_error {
        return error_reply(text);
    }
    // Success texts: 686-692 (no results) and 710-716 (results).
    if text.starts_with("No results found for query:") {
        return SearchReply::NoResults { indexing: text.contains("still being indexed") };
    }
    if text.starts_with("Found ") && text.contains(" results for query: ") {
        return parse_results(text);
    }
    // The collection limit message (core `COLLECTION_LIMIT_MESSAGE`) comes back as a success.
    SearchReply::Failed(format!("unexpected claude-context reply: {}", abbreviate(text)))
}

fn error_reply(text: &str) -> SearchReply {
    if looks_unreachable(text) {
        SearchReply::Unreachable(abbreviate(text))
    } else {
        SearchReply::Failed(abbreviate(text))
    }
}

fn parse_results(text: &str) -> SearchReply {
    let Some(start) = text.find(RESULT_START) else {
        return SearchReply::Failed(format!("claude-context result list not found: {}", abbreviate(text)));
    };
    let header = &text[..start];
    let declared = header
        .strip_prefix("Found ")
        .and_then(|rest| rest.split_once(' '))
        .and_then(|(count, _)| count.parse::<usize>().ok())
        .unwrap_or(0);
    let mut rest = &text[start + 2..];
    let mut results = Vec::new();
    let mut index = 1usize;
    loop {
        // Blocks are separated by a blank line and numbered; look for the next number.
        let next_marker = format!("\n\n{}. Code snippet (", index + 1);
        let (block, next) = match rest.find(&next_marker) {
            Some(pos) => (&rest[..pos], Some(&rest[pos + 2..])),
            None => (rest, None),
        };
        match parse_block(block, index) {
            Ok(result) => results.push(result),
            Err(why) => return SearchReply::Failed(format!("unparsable claude-context result {index}: {why}")),
        }
        match next {
            Some(next) => {
                rest = next;
                index += 1;
            }
            None => break,
        }
    }
    SearchReply::Results {
        root: header_root(header),
        indexing: header.contains("Indexing in Progress") || text.contains("still being indexed"),
        declared,
        results,
    }
}

/// The codebase root in `Found N results for query: "<q>" in codebase '<root>'` (handlers.js:710).
fn header_root(header: &str) -> Option<String> {
    let marker = "\" in codebase '";
    let after = &header[header.rfind(marker)? + marker.len()..];
    let end = after.find("'\n").or_else(|| after.strip_suffix('\'').map(str::len))?;
    let root = &after[..end];
    (!root.is_empty()).then(|| root.to_string())
}

/// One block (handlers.js:705-708):
/// `N. Code snippet (<lang>) [<base>]\n   Location: <path>:<a>-<b>\n   Rank: N\n   Context: \n```<lang>\n<code>\n```\n`.
fn parse_block(block: &str, index: usize) -> Result<ParsedResult, String> {
    let mut parts = block.splitn(5, '\n');
    let head = parts.next().unwrap_or_default();
    if !head.starts_with(&format!("{index}. Code snippet (")) {
        return Err(format!("block does not start with its number: {}", abbreviate(head)));
    }
    let location = parts.next().and_then(|line| line.strip_prefix("   Location: ")).ok_or("no Location line")?;
    let (rel_path, lines) = parse_location(location)?;
    if !parts.next().is_some_and(|line| line.starts_with("   Rank:")) {
        return Err("no Rank line".into());
    }
    if !parts.next().is_some_and(|line| line.trim_end() == "   Context:") {
        return Err("no Context line".into());
    }
    let fenced = parts.next().ok_or("no code block")?;
    let (fence, body) = fenced.split_once('\n').unwrap_or((fenced, ""));
    if !fence.starts_with("```") {
        return Err("code block does not start with a fence".into());
    }
    // The code itself may contain fences: the closing one is the last.
    let code = body.rfind("\n```").map_or_else(|| body.strip_suffix("```").unwrap_or(body), |pos| &body[..pos]);
    Ok(ParsedResult { rel_path, lines, code: (!code.trim().is_empty()).then(|| code.to_string()) })
}

/// `<path>:<start>-<end>`. The path can hold spaces and colons (also `C:\...`), so split from the right.
fn parse_location(location: &str) -> Result<(String, LineRange), String> {
    let location = location.trim_end();
    let (path, range) = location.rsplit_once(':').ok_or_else(|| format!("no line range in `{location}`"))?;
    let (start, end) = range.split_once('-').ok_or_else(|| format!("no line range in `{location}`"))?;
    let start: u32 = start.parse().map_err(|_| format!("bad start line in `{location}`"))?;
    let end: u32 = end.parse().map_err(|_| format!("bad end line in `{location}`"))?;
    if path.is_empty() {
        return Err(format!("empty path in `{location}`"));
    }
    // The server prints `relativePath` as the index stores it; on Windows that has backslashes.
    let path = if path.contains('\\') && !path.contains('/') { path.replace('\\', "/") } else { path.to_string() };
    Ok((path, LineRange::new(start.max(1), end.max(1))))
}

/// Parses a `get_indexing_status` text (`handleGetIndexingStatus`, handlers.js:870-975).
pub(super) fn parse_status(text: &str, is_error: bool) -> StatusReply {
    if is_error {
        return StatusReply::Error(abbreviate(text));
    }
    // 906-914
    if text.contains("is fully indexed and ready for search") {
        return StatusReply::Indexed {
            covered_by: quoted_after(text, "is covered by tracked codebase '"),
            stats: text.lines().find_map(|line| line.strip_prefix("📊 Statistics: ")).map(|s| s.trim().to_string()),
        };
    }
    // 919-930
    if text.contains("is currently being indexed") {
        let percent = text
            .split_once("Progress: ")
            .and_then(|(_, rest)| rest.split_once('%'))
            .and_then(|(number, _)| number.trim().parse::<f64>().ok());
        return StatusReply::Indexing { percent };
    }
    // 936-945
    if text.contains("indexing failed") {
        let error = text
            .lines()
            .find_map(|line| line.strip_prefix("🚨 Error: "))
            .map_or_else(|| "no error message".to_string(), |e| e.trim().to_string());
        return StatusReply::Failed { error };
    }
    // 950
    if text.contains("is not indexed. Please use the index_codebase tool") {
        return StatusReply::NotIndexed;
    }
    StatusReply::Error(format!("unexpected claude-context reply: {}", abbreviate(text)))
}

/// Parses an `index_codebase` text (`handleIndexCodebase`, handlers.js:299-490).
pub(super) fn parse_index(text: &str, is_error: bool) -> IndexReply {
    // 474
    if !is_error && text.starts_with("Started background indexing") {
        return IndexReply::Started;
    }
    // 358 and 393 come with `isError: true`, but they are not failures for us.
    if text.contains("is already being indexed in the background") {
        return IndexReply::AlreadyIndexing;
    }
    if text.contains("is already indexed. Use force=true") {
        return IndexReply::AlreadyIndexed;
    }
    if is_error && looks_unreachable(text) {
        return IndexReply::Unreachable(abbreviate(text));
    }
    IndexReply::Failed(abbreviate(text))
}

/// Milvus or an embedding service refused the connection (gRPC `14 UNAVAILABLE`, `ECONNREFUSED`).
fn looks_unreachable(text: &str) -> bool {
    text.contains("ECONNREFUSED") || text.contains("UNAVAILABLE")
}

fn quoted_after(text: &str, marker: &str) -> Option<String> {
    let after = &text[text.find(marker)? + marker.len()..];
    let end = after.find("'.").or_else(|| after.find('\''))?;
    Some(after[..end].to_string())
}

/// The first line of a text, cut at 300 characters.
pub(super) fn abbreviate(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(300).collect();
    if line.chars().count() > 300 {
        out.push_str("...");
    }
    out
}
