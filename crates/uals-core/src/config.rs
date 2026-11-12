//! User configuration, read from TOML. Frozen: only the coordinator changes this file.
//!
//! Lookup order: an explicit path, then `$UALS_CONFIG`, then `$XDG_CONFIG_HOME/uals/config.toml`,
//! then `~/.config/uals/config.toml`. A missing file means "all defaults". Every field has a
//! default, so a config file only lists what it changes. Unknown fields are rejected, so typos
//! fail loudly. Do not put secrets in the file: list variable names in `pass_env` instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::{BackendId, QueryKind};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("invalid config {path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
    #[error("invalid config {path}: {reason}")]
    Invalid { path: PathBuf, reason: String },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub semantic: SemanticConfig,
    pub ast_index: AstIndexConfig,
    pub lexical: LexicalConfig,
    pub deadlines: Deadlines,
    pub fusion: FusionConfig,
    pub output: OutputConfig,
    /// Named workspaces, e.g. `[workspaces.kids]`. Optional: any folder works without an entry.
    pub workspaces: BTreeMap<String, WorkspaceConfig>,
}

impl Config {
    /// Loads the config from `explicit`, or from the default location. A missing default file
    /// gives the defaults; a missing explicit file is an error.
    pub fn load(explicit: Option<&Path>) -> Result<Config, ConfigError> {
        let path = match explicit {
            Some(path) => path.to_path_buf(),
            None => match Self::default_path() {
                Some(path) if path.exists() => path,
                _ => return Ok(Config::default()),
            },
        };
        let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read { path: path.clone(), source })?;
        let config = Self::parse(&text).map_err(|source| ConfigError::Parse { path: path.clone(), source })?;
        config.validate().map_err(|reason| ConfigError::Invalid { path, reason })?;
        Ok(config)
    }

    pub fn parse(text: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(text)
    }

    /// Checks what serde cannot: patterns compile, factors and limits are in range.
    pub fn validate(&self) -> Result<(), String> {
        for rule in &self.lexical.downrank {
            regex::Regex::new(&rule.pattern)
                .map_err(|err| format!("lexical.downrank pattern {:?}: {err}", rule.pattern))?;
            if !(rule.factor > 0.0 && rule.factor <= 1.0) {
                return Err(format!(
                    "lexical.downrank factor {} for {:?} must be in (0, 1]",
                    rule.factor, rule.pattern
                ));
            }
        }
        if self.output.default_limit == 0 || self.output.default_limit > self.output.max_limit {
            return Err("output.default_limit must be between 1 and output.max_limit".into());
        }
        if self.fusion.fetch_factor == 0 {
            return Err("fusion.fetch_factor must be at least 1".into());
        }
        Ok(())
    }

    /// `$UALS_CONFIG`, else `$XDG_CONFIG_HOME/uals/config.toml`, else `~/.config/uals/config.toml`
    /// (also on macOS, on purpose: one place for CLI tools).
    pub fn default_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("UALS_CONFIG") {
            return Some(PathBuf::from(path));
        }
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => dirs::home_dir()?.join(".config"),
        };
        Some(base.join("uals").join("config.toml"))
    }

    /// The workspace entry whose `root` equals `root`, if any.
    pub fn workspace_for(&self, root: &Path) -> Option<&WorkspaceConfig> {
        self.workspaces.values().find(|ws| ws.root == root)
    }
}

/// Which semantic backend `search` uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SemanticChoice {
    /// The default until the benchmark gate decides otherwise.
    #[default]
    Semble,
    ClaudeContext,
    None,
}

impl SemanticChoice {
    pub fn backend(self) -> Option<BackendId> {
        match self {
            SemanticChoice::Semble => Some(BackendId::Semble),
            SemanticChoice::ClaudeContext => Some(BackendId::ClaudeContext),
            SemanticChoice::None => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticConfig {
    pub default: SemanticChoice,
    pub semble: SembleConfig,
    pub claude_context: ClaudeContextConfig,
}

/// How to start the Semble MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SembleConfig {
    /// Program and arguments of the MCP server (stdio).
    pub command: Vec<String>,
    /// Extra environment variables with literal values (no secrets).
    pub env: BTreeMap<String, String>,
    /// Names of variables to copy from uals's own environment.
    pub pass_env: Vec<String>,
    /// What Semble searches; `None` leaves Semble's default (code only).
    pub content: Option<SembleContent>,
}

impl Default for SembleConfig {
    fn default() -> Self {
        // `semble` without arguments is the stdio MCP server (semble 0.7.3).
        Self { command: vec!["semble".into()], env: BTreeMap::new(), pass_env: Vec::new(), content: None }
    }
}

/// The `content` argument of Semble's `search` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SembleContent {
    Code,
    Docs,
    Config,
    All,
}

/// How to start the claude-context MCP server and index repositories with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClaudeContextConfig {
    /// Node executable.
    pub node: String,
    /// Entry point of `@zilliz/claude-context-mcp` installed by uals (never `npx` at runtime).
    /// Empty means `~/.local/share/uals/claude-context/node_modules/@zilliz/claude-context-mcp/dist/index.js`.
    pub entry: Option<PathBuf>,
    /// Literal environment, e.g. `MILVUS_ADDRESS = "127.0.0.1:19530"`.
    pub env: BTreeMap<String, String>,
    /// Names of variables to copy from uals's own environment, e.g. `OPENAI_API_KEY`.
    pub pass_env: Vec<String>,
    /// Passed to `index_codebase`. The default `build/**` of claude-context matches only the
    /// root folder, so nested build output needs `**/build/**`.
    pub ignore_patterns: Vec<String>,
    /// Passed to `index_codebase`: extensions missing from claude-context's defaults.
    pub custom_extensions: Vec<String>,
}

impl Default for ClaudeContextConfig {
    fn default() -> Self {
        Self {
            node: "node".into(),
            entry: None,
            env: BTreeMap::new(),
            pass_env: Vec::new(),
            ignore_patterns: vec!["**/build/**".into(), "**/node_modules/**".into(), "**/.gradle/**".into()],
            custom_extensions: vec![".kts".into()],
        }
    }
}

impl ClaudeContextConfig {
    pub fn entry_path(&self) -> Option<PathBuf> {
        self.entry.clone().or_else(|| {
            dirs::home_dir().map(|home| {
                home.join(".local/share/uals/claude-context/node_modules/@zilliz/claude-context-mcp/dist/index.js")
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AstIndexConfig {
    /// Executable name or path.
    pub binary: String,
    /// When `uals mcp` starts, update the ast-index index of every repository that already has
    /// one. Creating an index is left to `uals refresh`.
    pub update_on_start: bool,
}

impl Default for AstIndexConfig {
    fn default() -> Self {
        Self { binary: "ast-index".into(), update_on_start: true }
    }
}

/// Settings of the in-process lexical search. Defaults come from spike S3 (`docs/spikes/s3-lexical.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LexicalConfig {
    /// Matches collected per file; the searcher stops reading the file after this many.
    pub max_matches_per_file: usize,
    /// Hits kept per file after ranking.
    pub max_hits_per_file: usize,
    /// Matches collected per repository before ranking. A soft cap: parallel threads may
    /// overshoot it, so the ranked list is truncated to it afterwards.
    pub max_candidates: usize,
    /// Matched lines are cut to this many characters.
    pub max_line_chars: usize,
    /// Larger files are skipped.
    pub max_file_size_bytes: u64,
    /// Paths whose hits rank lower. The first matching rule applies. Not applied when the
    /// query itself contains `test`, `mock` or `fake`.
    pub downrank: Vec<Downrank>,
}

/// A path rule for the lexical ranker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Downrank {
    /// Regular expression matched against the path relative to the repository root, with `/`
    /// separators and no leading `/`.
    pub pattern: String,
    /// Score multiplier in (0, 1].
    pub factor: f64,
}

impl Downrank {
    fn new(pattern: &str, factor: f64) -> Self {
        Self { pattern: pattern.into(), factor }
    }
}

impl Default for LexicalConfig {
    fn default() -> Self {
        const STRONG: f64 = 0.3;
        const MEDIUM: f64 = 0.5;
        const WEAK: f64 = 0.7;
        Self {
            max_matches_per_file: 10,
            max_hits_per_file: 3,
            max_candidates: 500,
            max_line_chars: 300,
            max_file_size_bytes: 2 * 1024 * 1024,
            downrank: vec![
                // Build output and generated code.
                Downrank::new(r"(^|/)(build|generated|\.gradle|node_modules|(kapt|ksp)[^/]*)/", STRONG),
                Downrank::new(r"\.min\.(js|css)$|\.(lock|map)$", STRONG),
                Downrank::new(r"(_Impl|_Factory)\.|(^|/)(Hilt_|Dagger[A-Z])|\$\$", STRONG),
                // Tests, fakes and sandboxes.
                Downrank::new(r"(^|/)(test|tests|testFixtures|sandbox|mocks?|fixtures?|testdata)/", MEDIUM),
                Downrank::new(r"(^|/)[a-z][A-Za-z]*Test/", MEDIUM),
                Downrank::new(r"Tests?\.(kt|java|ts|tsx|js|py|swift)$|(^|/)test_[^/]*\.py$", MEDIUM),
                Downrank::new(r"(^|/)(Fake|Mock|Stub)[A-Z][^/]*$", MEDIUM),
                // Translations, docs and tool folders.
                Downrank::new(r"(^|/)res/values-[a-z]{2}(-r[A-Z]{2})?/strings\.xml$", WEAK),
                Downrank::new(r"\.md$", WEAK),
                Downrank::new(r"(^|/)\.(idea|claude|agents)/", WEAK),
            ],
        }
    }
}

/// Per-backend deadlines for one repository call. A backend that misses its deadline is
/// reported as `Timeout` and the response goes out without it. The orchestrator enforces them by
/// dropping the future; adapters that run blocking work (lexical) must also stop on their own a
/// little earlier and return what they found as a partial answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Deadlines {
    pub ast_index_ms: u64,
    pub lexical_ms: u64,
    pub semantic_ms: u64,
    /// The first semantic call after start may load models; it gets this longer deadline.
    pub semantic_cold_ms: u64,
}

impl Default for Deadlines {
    fn default() -> Self {
        Self { ast_index_ms: 2_000, lexical_ms: 1_500, semantic_ms: 4_000, semantic_cold_ms: 5_000 }
    }
}

impl Deadlines {
    pub fn for_backend(&self, backend: BackendId, cold: bool) -> u64 {
        match backend {
            BackendId::AstIndex => self.ast_index_ms,
            BackendId::Lexical => self.lexical_ms,
            BackendId::Semble | BackendId::ClaudeContext if cold => self.semantic_cold_ms,
            BackendId::Semble | BackendId::ClaudeContext => self.semantic_ms,
        }
    }
}

/// Weights of the three tiers for one kind of query. 0 means "do not ask this tier".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TierWeights {
    pub ast_index: f64,
    pub lexical: f64,
    pub semantic: f64,
}

impl Default for TierWeights {
    fn default() -> Self {
        Self { ast_index: 0.0, lexical: 0.0, semantic: 0.0 }
    }
}

impl TierWeights {
    fn of(&self, backend: BackendId) -> f64 {
        match backend {
            BackendId::AstIndex => self.ast_index,
            BackendId::Lexical => self.lexical,
            BackendId::Semble | BackendId::ClaudeContext => self.semantic,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Weights {
    pub identifier: TierWeights,
    pub natural: TierWeights,
    pub regex: TierWeights,
    pub literal: TierWeights,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            identifier: TierWeights { ast_index: 1.0, lexical: 0.7, semantic: 0.3 },
            natural: TierWeights { ast_index: 0.7, lexical: 0.0, semantic: 1.0 },
            regex: TierWeights { ast_index: 0.0, lexical: 1.0, semantic: 0.0 },
            literal: TierWeights { ast_index: 0.0, lexical: 1.0, semantic: 0.0 },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FusionConfig {
    /// The RRF constant k: contribution = weight / (k + rank + 1).
    pub rrf_k: f64,
    /// Each backend is asked for `output.max_limit * fetch_factor` hits per repository, the same
    /// for every page, so all pages of a query come from one candidate pool.
    pub fetch_factor: usize,
    /// At most this many exact definitions are pinned to the top.
    pub max_pinned: usize,
    /// Hits without a symbol span in the same file merge when their ranges are this close.
    pub merge_gap_lines: u32,
    pub weights: Weights,
}

impl Default for FusionConfig {
    fn default() -> Self {
        Self { rrf_k: 60.0, fetch_factor: 5, max_pinned: 3, merge_gap_lines: 3, weights: Weights::default() }
    }
}

impl FusionConfig {
    /// The fusion weight of `backend` for queries of `kind`.
    pub fn weight(&self, kind: QueryKind, backend: BackendId) -> f64 {
        let tier = match kind {
            QueryKind::Identifier => &self.weights.identifier,
            QueryKind::Natural => &self.weights.natural,
            QueryKind::Regex => &self.weights.regex,
            QueryKind::Literal => &self.weights.literal,
        };
        tier.of(backend)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    pub default_limit: usize,
    pub max_limit: usize,
    pub max_snippet_lines: usize,
    /// Hard cap for one rendered response, about 4 characters per token.
    pub max_output_chars: usize,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self { default_limit: 12, max_limit: 30, max_snippet_lines: 20, max_output_chars: 24_000 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Absolute path of the workspace folder.
    pub root: PathBuf,
    /// Folder names or paths relative to `root` that discovery skips.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Explicit repositories. When set, discovery is skipped.
    #[serde(default)]
    pub repos: Vec<RepoConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub name: String,
    /// Absolute, or relative to the workspace root.
    pub path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let config = Config::parse(
            r#"
            [semantic]
            default = "claude-context"

            [semantic.claude_context]
            env = { MILVUS_ADDRESS = "127.0.0.1:19530" }
            pass_env = ["OPENAI_API_KEY"]

            [fusion.weights.identifier]
            semantic = 0.5

            [workspaces.kids]
            root = "/work/kids"
            exclude = ["docs"]
            "#,
        )
        .unwrap();
        assert_eq!(config.semantic.default, SemanticChoice::ClaudeContext);
        assert_eq!(config.semantic.claude_context.env["MILVUS_ADDRESS"], "127.0.0.1:19530");
        assert_eq!(config.semantic.claude_context.custom_extensions, vec![".kts".to_string()]);
        assert_eq!(config.fusion.weights.identifier.semantic, 0.5);
        // A partially written tier keeps 0 for the fields it omits: tiers are explicit on purpose.
        assert_eq!(config.fusion.weights.identifier.ast_index, 0.0);
        assert_eq!(config.fusion.weights.natural, Weights::default().natural);
        assert_eq!(config.output, OutputConfig::default());
        assert!(config.workspace_for(Path::new("/work/kids")).is_some());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(Config::parse("[fusion]\nrrf = 10\n").is_err());
    }

    #[test]
    fn defaults_are_valid() {
        assert_eq!(Config::default().validate(), Ok(()));
    }

    #[test]
    fn bad_downrank_rules_are_rejected() {
        let bad_pattern = Config::parse("[[lexical.downrank]]\npattern = \"(\"\nfactor = 0.5\n").unwrap();
        assert!(bad_pattern.validate().unwrap_err().contains("pattern"));
        let bad_factor = Config::parse("[[lexical.downrank]]\npattern = \"x\"\nfactor = 0\n").unwrap();
        assert!(bad_factor.validate().unwrap_err().contains("factor"));
    }

    #[test]
    fn default_downrank_rules_hit_the_intended_paths() {
        let rules: Vec<(regex::Regex, f64)> = LexicalConfig::default()
            .downrank
            .iter()
            .map(|rule| (regex::Regex::new(&rule.pattern).unwrap(), rule.factor))
            .collect();
        let factor = |path: &str| rules.iter().find(|(re, _)| re.is_match(path)).map(|(_, f)| *f).unwrap_or(1.0);
        assert_eq!(factor("app/build/generated/X.kt"), 0.3);
        assert_eq!(factor("core/src/test/kotlin/RetryPolicyTest.kt"), 0.5);
        assert_eq!(factor("shared/src/commonTest/kotlin/A.kt"), 0.5);
        assert_eq!(factor("network/src/main/kotlin/FakeHostStore.kt"), 0.5);
        assert_eq!(factor("app/src/main/res/values-de/strings.xml"), 0.7);
        assert_eq!(factor("app/src/main/res/values/strings.xml"), 1.0);
        assert_eq!(factor("network/src/main/kotlin/sample/network/HostSelector.kt"), 1.0);
        assert_eq!(factor("docs/contest/Latest.kt"), 1.0);
    }

    #[test]
    fn weights_follow_the_plan() {
        let fusion = FusionConfig::default();
        assert_eq!(fusion.weight(QueryKind::Identifier, BackendId::AstIndex), 1.0);
        assert_eq!(fusion.weight(QueryKind::Identifier, BackendId::Lexical), 0.7);
        assert_eq!(fusion.weight(QueryKind::Identifier, BackendId::ClaudeContext), 0.3);
        assert_eq!(fusion.weight(QueryKind::Natural, BackendId::Lexical), 0.0);
        assert_eq!(fusion.weight(QueryKind::Regex, BackendId::Semble), 0.0);
    }
}
