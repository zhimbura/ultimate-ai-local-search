//! `uals doctor`: checks that uals and its backends are installed and work, and says how to fix
//! what does not. The checks run in a fixed order: config, ast-index, the semantic backend, the
//! registration in Claude Code. A warning means uals still works, with less; a failure means a
//! backend or the config is broken.
//!
//! The pure parts (versions, the registration in the Claude Code config, the probe requests) are
//! small functions with unit tests; the commands and the HTTP probes are tested through the binary.

mod probe;
mod semantic;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use tokio::process::Command;
use uals_core::config::{AstIndexConfig, Config, SemanticChoice};

use crate::args::FormatArg;

/// The oldest ast-index uals was tested with.
const AST_INDEX_TESTED: Version = Version([3, 56, 0]);
/// How long `<program> --version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Skip,
}

/// The result of one check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    /// What to do about it; set unless the status is `ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Check {
    fn new(name: &str, status: Status, detail: impl Into<String>, hint: Option<String>) -> Self {
        Self { name: name.to_owned(), status, detail: detail.into(), hint }
    }

    pub(crate) fn ok(name: &str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Ok, detail, None)
    }

    pub(crate) fn warn(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(name, Status::Warn, detail, Some(hint.into()))
    }

    pub(crate) fn fail(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(name, Status::Fail, detail, Some(hint.into()))
    }

    pub(crate) fn skip(name: &str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Skip, detail, None)
    }
}

/// Every check, in the order they ran.
#[derive(Debug, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
    /// True when no check failed; warnings are allowed.
    pub ok: bool,
}

impl Report {
    fn new(checks: Vec<Check>) -> Self {
        let ok = checks.iter().all(|check| check.status != Status::Fail);
        Self { checks, ok }
    }

    /// One line per check, the hint on the next line, and the counts on the last line.
    fn render_text(&self) -> String {
        let mut lines = Vec::new();
        for check in &self.checks {
            let mark = match check.status {
                Status::Ok => '✓',
                Status::Warn => '!',
                Status::Fail => '✗',
                Status::Skip => '-',
            };
            lines.push(format!("{mark} {}: {}", check.name, check.detail));
            if let Some(hint) = &check.hint {
                lines.push(format!("    hint: {hint}"));
            }
        }
        let count = |status| self.checks.iter().filter(|check| check.status == status).count();
        lines.push(format!(
            "{} ok, {} warn, {} fail, {} skip",
            count(Status::Ok),
            count(Status::Warn),
            count(Status::Fail),
            count(Status::Skip)
        ));
        lines.join("\n")
    }
}

/// Runs every check, prints the report and exits with 1 when a check failed.
pub async fn run(explicit_config: Option<&Path>, format: FormatArg) -> Result<ExitCode> {
    let report = diagnose(explicit_config).await;
    match format {
        FormatArg::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        FormatArg::Concise | FormatArg::Detailed => println!("{}", report.render_text()),
    }
    Ok(if report.ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// All checks. A config that cannot be loaded fails its check, and the checks that depend on the
/// config are skipped.
pub async fn diagnose(explicit_config: Option<&Path>) -> Report {
    let (config, config_check) = check_config(explicit_config);
    let mut checks = vec![config_check];
    match config {
        Some(config) => {
            checks.push(check_ast_index(&config.ast_index).await);
            checks.extend(semantic::check(&config.semantic).await);
            checks.push(check_mcp(config.semantic.default));
        }
        None => {
            let reason = "not checked: the config is not valid";
            checks.push(Check::skip("ast-index", reason));
            checks.push(Check::skip("semantic", reason));
            checks.push(Check::skip("mcp", reason));
        }
    }
    Report::new(checks)
}

/// The config, when it loads, and the check that tells which file it came from.
fn check_config(explicit: Option<&Path>) -> (Option<Config>, Check) {
    // The file `Config::load` reads: the explicit one, else the default one when it exists.
    let file = explicit.map(Path::to_path_buf).or_else(|| Config::default_path().filter(|path| path.exists()));
    match Config::load(explicit) {
        Ok(config) => {
            let detail = match file {
                Some(file) => format!("loaded {}", file.display()),
                None => "no file: defaults".to_owned(),
            };
            (Some(config), Check::ok("config", detail))
        }
        Err(error) => {
            (None, Check::fail("config", error.to_string(), "fix the file, or remove it to use the defaults"))
        }
    }
}

/// ast-index is optional: without it, or with an old one, uals still searches, so every problem
/// is a warning.
async fn check_ast_index(config: &AstIndexConfig) -> Check {
    const NAME: &str = "ast-index";
    let install_hint = "run install.sh to install ast-index";
    let Some(path) = resolve_program(&config.binary) else {
        return Check::warn(NAME, format!("{} not found: symbol search is off", config.binary), install_hint);
    };
    match read_version(&path).await {
        Err(problem) => Check::warn(NAME, problem, install_hint),
        Ok(version) if version < AST_INDEX_TESTED => Check::warn(
            NAME,
            format!(
                "{version} at {} is older than {AST_INDEX_TESTED}; uals was tested with {AST_INDEX_TESTED}",
                path.display()
            ),
            "run install.sh to update ast-index",
        ),
        Ok(version) => Check::ok(NAME, format!("{version} at {}", path.display())),
    }
}

/// Whether Claude Code knows uals, and whether a second claude-context server runs next to it.
/// Every problem is a warning: uals works without being registered.
fn check_mcp(semantic: SemanticChoice) -> Check {
    const NAME: &str = "mcp";
    let register_hint = format!(
        "scripts/configure-mcp.sh --uals-bin {}",
        std::env::current_exe().map_or_else(|_| "<path to uals>".to_owned(), |path| path.display().to_string())
    );
    let Some(path) = claude_config_path() else {
        return Check::warn(NAME, "cannot find the home folder, so the Claude Code config is unknown", register_hint);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Check::warn(
                NAME,
                format!("{} does not exist: uals is not registered", path.display()),
                register_hint,
            );
        }
        Err(error) => return Check::warn(NAME, format!("cannot read {}: {error}", path.display()), register_hint),
    };
    let registrations = match serde_json::from_str::<Value>(&text) {
        Ok(value) => Registrations::from_config(&value),
        Err(error) => {
            return Check::warn(NAME, format!("{} is not valid JSON: {error}", path.display()), register_hint);
        }
    };
    let (mut detail, mut hints) = match registrations.uals_places() {
        Some(places) => (format!("uals is registered in {} ({places})", path.display()), vec![]),
        None => (format!("uals is not registered in {}", path.display()), vec![register_hint]),
    };
    if semantic == SemanticChoice::ClaudeContext && registrations.claude_context {
        detail.push_str(
            "; claude-context is also registered directly, and two claude-context servers race on ~/.context",
        );
        hints.push("scripts/configure-mcp.sh --remove-claude-context".to_owned());
    }
    if hints.is_empty() { Check::ok(NAME, detail) } else { Check::warn(NAME, detail, hints.join("; ")) }
}

/// `$CLAUDE_CONFIG`, else `~/.claude.json`.
fn claude_config_path() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG") {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => dirs::home_dir().map(|home| home.join(".claude.json")),
    }
}

/// Which MCP servers the Claude Code config has. Only names are read, never the `env` values.
#[derive(Debug, Default, PartialEq, Eq)]
struct Registrations {
    uals_top_level: bool,
    /// How many projects have a `uals` entry.
    uals_projects: usize,
    /// A `claude-context` entry at the top level or in any project.
    claude_context: bool,
}

impl Registrations {
    fn from_config(config: &Value) -> Self {
        let has = |servers: Option<&Value>, name: &str| servers.is_some_and(|servers| servers.get(name).is_some());
        let top = config.get("mcpServers");
        let projects: Vec<&Value> =
            config.get("projects").and_then(Value::as_object).map(|map| map.values().collect()).unwrap_or_default();
        Self {
            uals_top_level: has(top, "uals"),
            uals_projects: projects.iter().filter(|project| has(project.get("mcpServers"), "uals")).count(),
            claude_context: has(top, "claude-context")
                || projects.iter().any(|project| has(project.get("mcpServers"), "claude-context")),
        }
    }

    /// Where uals is registered as text for a report, `None` when nowhere.
    fn uals_places(&self) -> Option<String> {
        match (self.uals_top_level, self.uals_projects) {
            (false, 0) => None,
            (true, 0) => Some("top level".to_owned()),
            (false, n) => Some(format!("{n} project{}", plural(n))),
            (true, n) => Some(format!("top level and {n} project{}", plural(n))),
        }
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// A version of up to three numbers; a missing number is 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version([u64; 3]);

impl Version {
    /// The first word of `text` that starts with a number (after an optional `v`), as a version:
    /// `ast-index 3.56.0` and `v22.13.1` give 3.56.0 and 22.13.1.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let word = text
            .split_whitespace()
            .map(|word| word.trim_start_matches('v'))
            .find(|word| word.chars().next().is_some_and(|first| first.is_ascii_digit()))?;
        let mut numbers = [0u64; 3];
        for (slot, part) in numbers.iter_mut().zip(word.split('.')) {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            *slot = digits.parse().ok()?;
        }
        Some(Self(numbers))
    }

    pub(crate) const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self([major, minor, patch])
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0[0], self.0[1], self.0[2])
    }
}

/// The version of the program at `path`, or the text of what went wrong, ready for a report line.
pub(crate) async fn read_version(path: &Path) -> Result<Version, String> {
    let text = program_version(path, &[]).await.map_err(|error| format!("cannot run {}: {error}", path.display()))?;
    Version::parse(&text).ok_or_else(|| format!("{}: unrecognised version {text:?}", path.display()))
}

/// The executable `name` names: a path with folders as is, a bare name looked up in `PATH`.
pub(crate) fn resolve_program(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return is_executable(path).then(|| path.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|folder| folder.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// What `<program> <args> --version` prints, the first line of stdout (stderr when stdout is empty).
pub(crate) async fn program_version(program: &Path, args: &[String]) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args).arg("--version").stdin(std::process::Stdio::null()).kill_on_drop(true);
    let output = match tokio::time::timeout(VERSION_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(error.to_string()),
        Err(_) => return Err(format!("--version did not answer within {}s", VERSION_TIMEOUT.as_secs())),
    };
    let first_line = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes).lines().map(str::trim).find(|line| !line.is_empty()).map(str::to_owned)
    };
    let printed = first_line(&output.stdout).or_else(|| first_line(&output.stderr));
    match (output.status.success(), printed) {
        (true, Some(line)) => Ok(line),
        (true, None) => Err("--version printed nothing".to_owned()),
        (false, printed) => Err(format!(
            "--version failed ({}){}",
            output.status,
            printed.map(|line| format!(": {line}")).unwrap_or_default()
        )),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn versions_are_read_from_the_first_number_and_compared_by_parts() {
        assert_eq!(Version::parse("ast-index 3.56.0"), Some(Version::new(3, 56, 0)));
        assert_eq!(Version::parse("v22.13.1\n"), Some(Version::new(22, 13, 1)));
        assert_eq!(Version::parse("0.7.3"), Some(Version::new(0, 7, 3)));
        assert_eq!(Version::parse("semble 0.8"), Some(Version::new(0, 8, 0)));
        assert_eq!(Version::parse("v22.13.1-rc.1"), Some(Version::new(22, 13, 1)));
        assert_eq!(Version::parse("no version here"), None);
        assert!(Version::new(3, 9, 0) < Version::new(3, 56, 0), "parts compare as numbers, not as text");
        assert!(Version::new(22, 12, 0) < Version::new(22, 13, 1));
    }

    #[test]
    fn registrations_come_from_the_top_level_and_from_projects() {
        let config = json!({
            "mcpServers": {"claude-context": {"env": {"OPENAI_API_KEY": "secret"}}},
            "projects": {
                "/a": {"mcpServers": {"uals": {}}},
                "/b": {"mcpServers": {"uals": {}, "other": {}}},
                "/c": {},
            }
        });

        let found = Registrations::from_config(&config);

        assert_eq!(found, Registrations { uals_top_level: false, uals_projects: 2, claude_context: true });
        assert_eq!(found.uals_places().as_deref(), Some("2 projects"));
        assert_eq!(
            Registrations::from_config(&json!({"mcpServers": {"uals": {}}})).uals_places().as_deref(),
            Some("top level")
        );
        assert_eq!(Registrations::from_config(&json!({})).uals_places(), None);
    }
}
