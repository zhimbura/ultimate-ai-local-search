//! The parameters of `search`, `usages` and `status`. One definition serves the MCP tools (serde
//! and JSON Schema) and the command line (clap), so both take the same parameters with the same
//! meaning.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use schemars::JsonSchema;
use serde::Deserialize;
use uals_core::config::Config;
use uals_core::render::Format;
use uals_core::types::{QueryKind, Repo, Workspace};
use uals_core::workspace;

use crate::orchestrator::{SearchParams, UsagesParams, select_repos};

/// The variable that names the workspace folder when no `workspace` parameter is given.
pub const WORKSPACE_VAR: &str = "UALS_WORKSPACE";

/// The kind of a query.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum KindArg {
    /// Decide from the text.
    #[default]
    Auto,
    /// A code name such as `HostSelector`, `mark_failed` or `sample.core.Analytics`.
    Identifier,
    /// Words that describe the code.
    Natural,
    /// A regular expression.
    Regex,
    /// An exact string.
    Literal,
}

impl KindArg {
    /// The kind to force, or `None` to let the classifier decide.
    pub fn kind(self) -> Option<QueryKind> {
        match self {
            KindArg::Auto => None,
            KindArg::Identifier => Some(QueryKind::Identifier),
            KindArg::Natural => Some(QueryKind::Natural),
            KindArg::Regex => Some(QueryKind::Regex),
            KindArg::Literal => Some(QueryKind::Literal),
        }
    }
}

/// How a response is written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum FormatArg {
    /// Compact text with code snippets.
    #[default]
    Concise,
    /// Concise plus scores, raw source ranks and longer snippets.
    Detailed,
    /// The response as JSON.
    Json,
}

impl From<FormatArg> for Format {
    fn from(format: FormatArg) -> Self {
        match format {
            FormatArg::Concise => Format::Concise,
            FormatArg::Detailed => Format::Detailed,
            FormatArg::Json => Format::Json,
        }
    }
}

/// The parameters of `search`.
#[derive(Debug, Clone, Deserialize, JsonSchema, Args)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    /// What to look for. `/…/` is a regular expression and `"…"` an exact string: both search the
    /// text only. A code name with an uppercase letter, `_`, `.`, `::` or `()` is an identifier:
    /// definitions first, then text and meaning. Anything else, one lowercase word included, is
    /// searched by meaning and symbol names, not as text; `kind` overrides the choice.
    pub query: String,
    /// The folder whose repositories are searched. Default: the client's root folder, then
    /// $UALS_WORKSPACE, then the current folder.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// Names of the repositories to search, as `status` lists them. Default: all of them.
    #[arg(long = "repo", value_name = "NAME")]
    #[serde(default)]
    pub repos: Vec<String>,
    /// The query kind. Default: decided from the text.
    #[arg(long, value_enum, default_value_t)]
    #[serde(default)]
    pub kind: KindArg,
    /// Globs relative to each repository root, gitignore style: `src/**/*.kt` selects, `!**/test/**`
    /// excludes. Default: every file.
    #[arg(long = "path", value_name = "GLOB")]
    #[serde(default)]
    pub paths: Vec<String>,
    /// Hits per page. Default: `output.default_limit` of the config (12); at most
    /// `output.max_limit` (30).
    #[arg(long)]
    pub limit: Option<usize>,
    /// Hits to skip: the `next: offset=N` of the previous page.
    #[arg(long, default_value_t = 0)]
    #[serde(default)]
    pub offset: usize,
    /// The output format.
    #[arg(long, value_enum, default_value_t)]
    #[serde(default)]
    pub format: FormatArg,
}

impl SearchArgs {
    /// What the orchestrator is asked.
    pub fn params(&self) -> SearchParams {
        SearchParams {
            query: self.query.clone(),
            kind: self.kind.kind(),
            paths: self.paths.clone(),
            limit: self.limit,
            offset: self.offset,
            format: self.format.into(),
        }
    }
}

/// The parameters of `usages`.
#[derive(Debug, Clone, Deserialize, JsonSchema, Args)]
#[serde(deny_unknown_fields)]
pub struct UsagesArgs {
    /// The symbol whose usages are listed, e.g. `HostSelector` or `markFailed`.
    pub symbol: String,
    /// The folder whose repositories are searched. Default: the client's root folder, then
    /// $UALS_WORKSPACE, then the current folder.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// Names of the repositories to search, as `status` lists them. Default: all of them.
    #[arg(long = "repo", value_name = "NAME")]
    #[serde(default)]
    pub repos: Vec<String>,
    /// Usages per page. Default: 100; at most 1000.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Usages to skip: the `next: offset=N` of the previous page.
    #[arg(long, default_value_t = 0)]
    #[serde(default)]
    pub offset: usize,
    /// The output format.
    #[arg(long, value_enum, default_value_t)]
    #[serde(default)]
    pub format: FormatArg,
}

impl UsagesArgs {
    /// What the orchestrator is asked.
    pub fn params(&self) -> UsagesParams {
        UsagesParams { symbol: self.symbol.clone(), limit: self.limit, offset: self.offset }
    }
}

/// The parameters of `status`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema, Args)]
#[serde(deny_unknown_fields)]
pub struct StatusArgs {
    /// The folder whose repositories are checked. Default: the client's root folder, then
    /// $UALS_WORKSPACE, then the current folder.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// Names of the repositories to check. Default: all of them.
    #[arg(long = "repo", value_name = "NAME")]
    #[serde(default)]
    pub repos: Vec<String>,
    /// The output format; `detailed` is the same as `concise`.
    #[arg(long, value_enum, default_value_t)]
    #[serde(default)]
    pub format: FormatArg,
}

/// The folder to search: `explicit`, else `client_root`, else `$UALS_WORKSPACE`, else the
/// current folder.
pub fn workspace_folder(explicit: Option<&Path>, client_root: Option<&Path>) -> Result<PathBuf> {
    if let Some(folder) = explicit.or(client_root) {
        return Ok(folder.to_path_buf());
    }
    match std::env::var_os(WORKSPACE_VAR) {
        Some(folder) if !folder.is_empty() => Ok(PathBuf::from(folder)),
        _ => std::env::current_dir().context("cannot read the current folder"),
    }
}

/// The workspace of `folder`, and its repositories named in `names` (all of them when `names` is
/// empty).
pub fn repos_in(folder: &Path, names: &[String], config: &Config) -> Result<(Workspace, Vec<Repo>)> {
    let workspace = workspace::discover(folder, config)?;
    let repos = select_repos(&workspace, names)?;
    Ok((workspace, repos))
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use uals_core::render::Format;

    use super::*;
    use crate::cli::{Cli, Command};

    fn search_from_flags(flags: &[&str]) -> SearchArgs {
        match Cli::try_parse_from(["uals", "search"].into_iter().chain(flags.iter().copied())).expect("flags").command {
            Command::Search(args) => args,
            other => panic!("expected search, got {other:?}"),
        }
    }

    fn usages_from_flags(flags: &[&str]) -> UsagesArgs {
        match Cli::try_parse_from(["uals", "usages"].into_iter().chain(flags.iter().copied())).expect("flags").command {
            Command::Usages(args) => args,
            other => panic!("expected usages, got {other:?}"),
        }
    }

    fn full_search() -> SearchParams {
        SearchParams {
            query: "HostSelector".into(),
            kind: Some(QueryKind::Regex),
            paths: vec!["src/**/*.kt".into(), "!**/test/**".into()],
            limit: Some(7),
            offset: 14,
            format: Format::Json,
        }
    }

    #[test]
    fn search_params_carry_every_field_from_json_and_from_flags() {
        let from_json: SearchArgs = serde_json::from_value(json!({
            "query": "HostSelector", "kind": "regex", "paths": ["src/**/*.kt", "!**/test/**"],
            "limit": 7, "offset": 14, "format": "json"
        }))
        .expect("json");
        let from_flags = search_from_flags(&[
            "HostSelector",
            "--kind",
            "regex",
            "--path",
            "src/**/*.kt",
            "--path",
            "!**/test/**",
            "--limit",
            "7",
            "--offset",
            "14",
            "--format",
            "json",
        ]);

        assert_eq!(from_json.params(), full_search());
        assert_eq!(from_flags.params(), full_search());
    }

    #[test]
    fn search_params_default_when_only_the_query_is_given() {
        let from_json: SearchArgs = serde_json::from_value(json!({"query": "HostSelector"})).expect("json");
        let from_flags = search_from_flags(&["HostSelector"]);

        assert_eq!(from_json.params(), SearchParams::new("HostSelector"));
        assert_eq!(from_flags.params(), SearchParams::new("HostSelector"));
    }

    #[test]
    fn usages_params_carry_every_field_from_json_and_from_flags() {
        let expected = UsagesParams { symbol: "markFailed".into(), limit: Some(5), offset: 10 };
        let from_json: UsagesArgs =
            serde_json::from_value(json!({"symbol": "markFailed", "limit": 5, "offset": 10})).expect("json");
        let from_flags = usages_from_flags(&["markFailed", "--limit", "5", "--offset", "10"]);

        assert_eq!(from_json.params(), expected);
        assert_eq!(from_flags.params(), expected);
    }

    #[test]
    fn usages_params_default_when_only_the_symbol_is_given() {
        let expected = UsagesParams { symbol: "markFailed".into(), limit: None, offset: 0 };
        let from_json: UsagesArgs = serde_json::from_value(json!({"symbol": "markFailed"})).expect("json");

        assert_eq!(from_json.params(), expected);
        assert_eq!(usages_from_flags(&["markFailed"]).params(), expected);
    }
}
