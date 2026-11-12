//! The checks of the semantic backend `semantic.default` names.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;
use uals_adapters::mcp_child::ChildSpec;
use uals_core::config::{ClaudeContextConfig, Config, SemanticChoice, SemanticConfig, SembleConfig};
use uals_core::types::{QueryKind, Repo, SearchRequest};

use super::{Check, Version, probe, program_version, read_version, resolve_program};
use crate::setup;

/// The Semble version uals was tested with.
const SEMBLE_TESTED: Version = Version::new(0, 7, 3);
const CLAUDE_CONTEXT_TESTED: Version = Version::new(0, 1, 15);
/// A dependency of claude-context loads ES modules with `require`, which Node supports from 22.12.
const NODE_MINIMUM: Version = Version::new(22, 12, 0);
/// How long the search through Semble may take. The first run may download Semble's model.
const SEMBLE_TIMEOUT: Duration = Duration::from_secs(120);
/// A search slower than this probably loaded the model.
const SLOW_SEARCH: Duration = Duration::from_secs(15);
const INSTALL: &str = "run install.sh";
const TEST_SOURCE: &str =
    "/// Greets a user by name.\nfn greet_user(name: &str) -> String {\n    format!(\"Hello, {name}!\")\n}\n";
const TEST_QUERY: &str = "function that greets a user by name";

pub(super) async fn check(config: &SemanticConfig) -> Vec<Check> {
    match config.default {
        SemanticChoice::None => vec![Check::skip("semantic", "semantic search is off (semantic.default = \"none\")")],
        SemanticChoice::Semble => vec![check_semble(&config.semble).await],
        SemanticChoice::ClaudeContext => check_claude_context(&config.claude_context).await,
    }
}

async fn check_semble(config: &SembleConfig) -> Check {
    const NAME: &str = "semble";
    let Some((program, args)) = config.command.split_first() else {
        return Check::fail(NAME, "semantic.semble.command is empty", "set it to [\"semble\"]");
    };
    let Some(path) = resolve_program(program) else {
        return Check::fail(NAME, format!("{program} not found"), format!("{INSTALL} to install Semble"));
    };
    // With arguments the command is not plain `semble`, and `--version` is not Semble's to answer.
    let version = if args.is_empty() { Some(program_version(&path, &[]).await) } else { None };
    let version_text = describe_semble_version(version.as_ref());

    let started = Instant::now();
    let search = search_one_file(config).await;
    let took = started.elapsed();
    let slow_note = if took > SLOW_SEARCH { " (the first run may download Semble's model)" } else { "" };
    if let Err(error) = search {
        return Check::fail(
            NAME,
            format!("{version_text}; the search failed after {:.1}s{slow_note}: {error}", took.as_secs_f64()),
            format!("run `{}` by hand to see why, or {INSTALL} to reinstall Semble", config.command.join(" ")),
        );
    }
    let detail = format!("{version_text} at {}; a search took {:.1}s{slow_note}", path.display(), took.as_secs_f64());
    match untested_semble_version(version.as_ref()) {
        Some(found) => Check::warn(
            NAME,
            format!("{detail}; uals was tested with {SEMBLE_TESTED}, this is {found}"),
            format!("{INSTALL} to install Semble {SEMBLE_TESTED}"),
        ),
        None => Check::ok(NAME, detail),
    }
}

/// The version part of the Semble report line. `None` is a custom command, whose version is not asked.
fn describe_semble_version(version: Option<&Result<String, String>>) -> String {
    match version {
        Some(Ok(text)) => Version::parse(text).map_or_else(|| format!("version {text:?}"), |found| found.to_string()),
        Some(Err(error)) => format!("version unknown ({error})"),
        None => "custom command, version not checked".to_owned(),
    }
}

/// The Semble version when it is not the tested one; `None` when it is, or when it is unknown.
fn untested_semble_version(version: Option<&Result<String, String>>) -> Option<Version> {
    let Some(Ok(text)) = version else {
        return None;
    };
    Version::parse(text).filter(|found| *found != SEMBLE_TESTED)
}

/// Starts Semble the way uals does and searches a folder with one source file through the same
/// adapter. The server is stopped when this returns, and the folder is removed.
async fn search_one_file(config: &SembleConfig) -> Result<(), String> {
    let folder = ScratchFolder::new().map_err(|error| format!("cannot make a temporary folder: {error}"))?;
    let source = folder.path.join("repo/greeting.rs");
    // `create_new` does not follow a link that someone planted at the path.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&source)
        .and_then(|mut file| file.write_all(TEST_SOURCE.as_bytes()))
        .map_err(|error| format!("cannot write {}: {error}", source.display()))?;
    // The test index goes next to the test folder, not into the user's Semble cache.
    let mut semble = config.clone();
    semble.env.insert("SEMBLE_CACHE_LOCATION".to_owned(), folder.path.join("cache").display().to_string());
    let backend = setup::semantic_backend(&Config {
        semantic: SemanticConfig { default: SemanticChoice::Semble, semble, ..SemanticConfig::default() },
        ..Config::default()
    })
    .ok_or("the semantic backend is off")?;
    let repo = Repo { name: "doctor".to_owned(), root: folder.path.join("repo") };
    let request =
        SearchRequest { query: TEST_QUERY.to_owned(), kind: QueryKind::Natural, fetch_limit: 3, paths: vec![] };
    let answer = tokio::time::timeout(SEMBLE_TIMEOUT, backend.search(&repo, &request))
        .await
        .map_err(|_| format!("no answer within {}s", SEMBLE_TIMEOUT.as_secs()))?
        .map_err(|error| error.to_string())?;
    if answer.hits.is_empty() {
        return Err("the search of a folder with one source file found nothing".to_owned());
    }
    Ok(())
}

/// A new folder with an unpredictable name under the system temporary folder, with a `repo` folder
/// inside, removed on drop.
struct ScratchFolder {
    path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl ScratchFolder {
    fn new() -> std::io::Result<Self> {
        let dir = tempfile::Builder::new().prefix("uals-doctor-").tempdir()?;
        std::fs::create_dir(dir.path().join("repo"))?;
        // Semble gets the canonical path, as for a real repository.
        Ok(Self { path: dir.path().canonicalize()?, _dir: dir })
    }
}

/// node, the package, the embedder and Milvus, one line each. The server itself is not started:
/// on start it syncs every indexed codebase.
async fn check_claude_context(config: &ClaudeContextConfig) -> Vec<Check> {
    let mut checks = vec![check_node(config).await, check_package(config)];
    // The environment the server would get: the base variables, `pass_env` and the literals.
    let env = ChildSpec {
        name: "claude-context".into(),
        program: config.node.clone().into(),
        args: vec![],
        env: config.env.clone(),
        pass_env: config.pass_env.clone(),
        cwd: None,
    }
    .child_env();
    let (embedder, embedded) = probe::check_embedder(&env).await;
    checks.push(embedder);
    checks.push(probe::check_milvus(&env, embedded).await);
    checks
}

async fn check_node(config: &ClaudeContextConfig) -> Check {
    const NAME: &str = "claude-context node";
    let hint = format!("install Node {NODE_MINIMUM} or newer and set semantic.claude_context.node");
    let Some(path) = resolve_program(&config.node) else {
        return Check::fail(NAME, format!("{} not found", config.node), hint);
    };
    match read_version(&path).await {
        Err(problem) => Check::fail(NAME, problem, hint),
        Ok(version) if version >= NODE_MINIMUM => Check::ok(NAME, format!("{version} at {}", path.display())),
        Ok(version) => Check::fail(
            NAME,
            format!(
                "{version} at {} is older than {NODE_MINIMUM}; a dependency of claude-context needs require(ESM)",
                path.display()
            ),
            hint,
        ),
    }
}

fn check_package(config: &ClaudeContextConfig) -> Check {
    const NAME: &str = "claude-context package";
    let hint = "run install.sh --semantic claude-context";
    let Some(entry) = config.entry_path() else {
        return Check::fail(NAME, "cannot find the home folder, so the entry point is unknown", hint);
    };
    if !entry.is_file() {
        return Check::fail(NAME, format!("not installed: {} does not exist", entry.display()), hint);
    }
    match package_version(&entry) {
        None => Check::ok(NAME, format!("{} (no package.json beside it, version not checked)", entry.display())),
        Some(found) if Version::parse(&found) == Some(CLAUDE_CONTEXT_TESTED) => {
            Check::ok(NAME, format!("{found} at {}", entry.display()))
        }
        Some(found) => Check::warn(
            NAME,
            format!("{found} at {}; uals was tested with {CLAUDE_CONTEXT_TESTED}", entry.display()),
            format!("{hint} to install {CLAUDE_CONTEXT_TESTED}"),
        ),
    }
}

/// The `version` of the package whose `dist/index.js` is `entry`.
fn package_version(entry: &Path) -> Option<String> {
    let package = entry.parent()?.parent()?.join("package.json");
    let json: Value = serde_json::from_str(&std::fs::read_to_string(package).ok()?).ok()?;
    json.get("version")?.as_str().map(str::to_owned)
}
