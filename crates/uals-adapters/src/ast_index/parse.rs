//! Shapes of the `ast-index --format json` output (see `docs/spikes/s2-ast-index.md`), the
//! checks that turn a raw run into JSON or a `BackendError`, and the SQL the adapter sends.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use uals_core::backend::BackendError;
use uals_core::types::SymbolKind;

use super::runner::CommandOutput;

/// Parses the stdout of a command that supports `--format json`.
///
/// A missing index is not an error for ast-index: it exits 0 and prints plain text. Output that
/// does not start with `{` or `[` is therefore either that text (`Unavailable`) or garbage
/// (`Failed`).
pub fn parse_json<T: DeserializeOwned>(command: &str, output: &CommandOutput) -> Result<T, BackendError> {
    let stdout = output.stdout.trim();
    if stdout.starts_with("Index not found") {
        return Err(missing_index());
    }
    if !output.success() {
        let stderr = output.stderr.trim();
        if stderr.contains("no such table") {
            return Err(missing_index());
        }
        let reason = stderr.lines().next().unwrap_or("no error text");
        return Err(BackendError::Failed(format!("ast-index {command} exited with {:?}: {reason}", output.exit_code)));
    }
    if !(stdout.starts_with('{') || stdout.starts_with('[')) {
        if stdout.contains("ast-index rebuild") {
            return Err(missing_index());
        }
        return Err(BackendError::Failed(format!("ast-index {command}: output is not JSON: {}", preview(stdout))));
    }
    serde_json::from_str(stdout)
        .map_err(|error| BackendError::Failed(format!("ast-index {command}: unparsable output: {error}")))
}

/// The error for a repository without an index. The orchestrator adds the repository name.
pub fn missing_index() -> BackendError {
    BackendError::Unavailable("ast-index has no index for this repository; run `ast-index rebuild` in it".into())
}

fn preview(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    line.chars().take(80).collect()
}

#[derive(Debug, Deserialize)]
pub struct SymbolList {
    #[serde(default)]
    pub items: Vec<SymbolItem>,
}

#[derive(Debug, Deserialize)]
pub struct SymbolItem {
    pub name: String,
    #[serde(default)]
    pub kind: String,
    pub line: u32,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct UsageList {
    #[serde(default)]
    pub items: Vec<UsageItem>,
    #[serde(default)]
    pub pagination: Pagination,
}

#[derive(Debug, Default, Deserialize)]
pub struct Pagination {
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Deserialize)]
pub struct UsageItem {
    pub line: u32,
    #[serde(default)]
    pub context: Option<String>,
    pub path: String,
    #[serde(default)]
    pub test: bool,
}

#[derive(Debug, Deserialize)]
pub struct Explore {
    #[serde(default)]
    pub symbols: Vec<ExploreSymbol>,
    #[serde(default)]
    pub files: Vec<ExploreFile>,
}

#[derive(Debug, Deserialize)]
pub struct ExploreSymbol {
    pub name: String,
    #[serde(default)]
    pub kind: String,
    pub line: u32,
    pub path: String,
    #[serde(default)]
    pub score: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct ExploreFile {
    pub path: String,
    #[serde(default)]
    pub outline: Vec<OutlineEntry>,
}

#[derive(Debug, Deserialize)]
pub struct OutlineEntry {
    pub name: String,
    pub line: u32,
    #[serde(default)]
    pub end_line: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct QueryResult<T> {
    #[serde(default = "Vec::new")]
    pub rows: Vec<T>,
}

#[derive(Debug, Deserialize)]
pub struct SpanRow {
    pub path: String,
    pub name: String,
    #[serde(default)]
    pub kind: String,
    pub line: u32,
    #[serde(default)]
    pub end_line: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct StateRow {
    #[serde(default)]
    pub updated: Option<String>,
    #[serde(default)]
    pub files: u64,
    #[serde(default)]
    pub symbols: u64,
}

/// Maps ast-index's kind names to `SymbolKind`. `None` for rows that are not definitions:
/// imports and packages.
pub fn normalize_kind(kind: &str) -> Option<SymbolKind> {
    match kind.to_ascii_lowercase().as_str() {
        "import" | "package" => None,
        "class" => Some(SymbolKind::Class),
        "interface" => Some(SymbolKind::Interface),
        "object" => Some(SymbolKind::Object),
        "enum" => Some(SymbolKind::Enum),
        "typealias" | "type_alias" => Some(SymbolKind::TypeAlias),
        "function" | "procedure" => Some(SymbolKind::Function),
        "property" => Some(SymbolKind::Property),
        "constant" => Some(SymbolKind::Constant),
        _ => Some(SymbolKind::Other),
    }
}

/// SQL for the symbol extents of the given project-relative paths.
pub fn spans_sql(paths: &[String]) -> String {
    let list: Vec<String> = paths.iter().map(|path| format!("'{}'", path.replace('\'', "''"))).collect();
    format!(
        "SELECT f.path AS path, s.name AS name, s.kind AS kind, s.line AS line, s.end_line AS end_line \
         FROM symbols s JOIN files f ON f.id = s.file_id WHERE f.path IN ({}) ORDER BY f.path, s.line",
        list.join(",")
    )
}

/// SQL for the index state: last update time (a string of milliseconds) and the counts.
pub const STATE_SQL: &str = "SELECT (SELECT value FROM metadata WHERE key = 'last_update_at') AS updated, \
     (SELECT COUNT(*) FROM files) AS files, (SELECT COUNT(*) FROM symbols) AS symbols";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_sql_escapes_quotes() {
        let sql = spans_sql(&["a/it's.kt".to_string(), "b.kt".to_string()]);
        assert!(sql.contains("IN ('a/it''s.kt','b.kt')"), "{sql}");
    }

    #[test]
    fn kinds_are_normalized() {
        assert_eq!(normalize_kind("class"), Some(SymbolKind::Class));
        assert_eq!(normalize_kind("typealias"), Some(SymbolKind::TypeAlias));
        assert_eq!(normalize_kind("procedure"), Some(SymbolKind::Function));
        assert_eq!(normalize_kind("annotation"), Some(SymbolKind::Other));
        assert_eq!(normalize_kind("import"), None);
        assert_eq!(normalize_kind("package"), None);
    }
}
