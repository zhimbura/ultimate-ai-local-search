//! The real engine through its public entry point, on a copy of the public fixture workspace.
//! The semantic backend is `none` and ast-index has no index, so the lexical backend answers;
//! the tests do not need ast-index installed.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use uals_bench::engine::{AnyEngine, Engine};
use uals_bench::metrics::relative_path;
use uals_bench::real::RealOptions;
use uals_core::config::SemanticChoice;
use uals_core::types::BackendId;

const HOST_SELECTOR: &str = "network/src/main/kotlin/sample/network/HostSelector.kt";

/// A copy of the fixture workspace with `git init` in each project, and a config file that asks
/// for semble: the tests switch it off with the command line override.
struct Workspace {
    _dir: TempDir,
    root: PathBuf,
    config: PathBuf,
}

fn workspace() -> Workspace {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace");
    let root = dir.path().join("workspace");
    copy_dir(&source, &root);
    for entry in std::fs::read_dir(&root).expect("read workspace") {
        let project = entry.expect("entry").path();
        if project.is_dir() {
            let status = Command::new("git")
                .args(["init", "-q"])
                .current_dir(&project)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .expect("git is installed");
            assert!(status.success());
        }
    }
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "[semantic]\ndefault = \"semble\"\n").expect("write config");
    Workspace { root: root.canonicalize().expect("canonical root"), config, _dir: dir }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create dir");
    for entry in std::fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy file");
        }
    }
}

fn open(workspace: &Workspace, semantic: Option<SemanticChoice>) -> AnyEngine {
    let options = RealOptions { config: Some(workspace.config.clone()), semantic };
    AnyEngine::from_spec(&format!("real:{}", workspace.root.display()), &options).expect("open the real engine")
}

#[tokio::test]
async fn finds_an_identifier_through_the_lexical_backend() {
    let workspace = workspace();
    let engine = open(&workspace, Some(SemanticChoice::None));
    assert_eq!(engine.repos().len(), 3);

    let response = engine.search("HostSelector", None, 30).await.expect("search");

    let found = response.hits.iter().any(|hit| {
        hit.repo == "sample-android"
            && relative_path(engine.repos(), &hit.repo, &hit.path).as_deref() == Some(HOST_SELECTOR)
    });
    assert!(found, "no hit in {HOST_SELECTOR}: {:?}", response.hits.iter().map(|hit| &hit.path).collect::<Vec<_>>());
    assert!(response.hits.iter().any(|hit| hit.sources.iter().any(|source| source.backend == BackendId::Lexical)));
    assert!(response.backends.iter().all(|report| !report.backend.is_semantic()), "semantic is off");
}

#[tokio::test]
async fn lists_the_usages_of_a_symbol() {
    let workspace = workspace();
    let engine = open(&workspace, Some(SemanticChoice::None));

    let response = engine.usages("HostSelector", 500, 0).await.expect("usages");

    let files: Vec<String> = response
        .repos
        .iter()
        .flat_map(|repo| &repo.usages)
        .filter_map(|usage| relative_path(engine.repos(), &usage.repo, &usage.path))
        .collect();
    for expected in ["network/src/main/kotlin/sample/network/ConfigLoader.kt", "app/src/main/kotlin/sample/app/App.kt"]
    {
        assert!(files.iter().any(|file| file == expected), "no usage in {expected}: {files:?}");
    }
}

#[test]
fn the_semantic_override_wins_over_the_config_and_names_the_report_label() {
    let workspace = workspace();
    let label = |semantic| open(&workspace, semantic).label();
    assert_eq!(label(Some(SemanticChoice::None)).as_deref(), Some("real:none"));
    assert_eq!(label(Some(SemanticChoice::ClaudeContext)).as_deref(), Some("real:claude-context"));
    assert_eq!(label(None).as_deref(), Some("real:semble"), "without an override the config decides");
}

#[test]
fn options_of_the_real_engine_are_refused_for_the_mock() {
    let options = RealOptions { config: None, semantic: Some(SemanticChoice::None) };
    let error = AnyEngine::from_spec("mock:responses.json", &options).err().expect("refused").to_string();
    assert!(error.contains("real engine only"), "{error}");
}

#[test]
fn the_binary_runs_the_gold_set_against_the_real_engine() {
    let workspace = workspace();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let out = workspace.root.parent().expect("parent").join("report.json");
    let engine = format!("real:{}", workspace.root.display());
    let output = Command::new(env!("CARGO_BIN_EXE_uals-bench"))
        .args(["run", "--name", "real", "--warm-repeats", "1", "--semantic", "none"])
        .args(["--gold", fixtures.join("gold.jsonl").to_str().unwrap()])
        .args(["--usages", fixtures.join("usages.jsonl").to_str().unwrap()])
        .args(["--config", workspace.config.to_str().unwrap(), "--engine", &engine])
        .args(["--out", out.to_str().unwrap()])
        .output()
        .expect("run uals-bench");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let text = std::fs::read_to_string(&out).expect("report");
    let report: serde_json::Value = serde_json::from_str(&text).expect("report JSON");
    assert_eq!(report["engine"], "real:none");
    assert_eq!(report["summary"]["overall"]["n"], 12);
    assert_eq!(report["summary"]["overall"]["failed"], 0);
    assert!(report["summary"]["overall"]["primary"]["mrr"].as_f64().unwrap() > 0.0);
    assert_eq!(report["summary"]["usages"]["failed"], 0);
    assert!(report["summary"]["usages"]["line_recall"].as_f64().unwrap() > 0.0);
    assert!(!text.contains(workspace.root.to_str().unwrap()), "a report holds no paths");
}
