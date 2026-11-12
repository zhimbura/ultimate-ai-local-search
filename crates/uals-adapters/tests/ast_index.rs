//! Public-trait tests of the ast-index adapter over recorded outputs
//! (`tests/fixtures/ast_index/`), plus one live test that needs the real binary.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pretty_assertions::assert_eq;
use uals_adapters::ast_index::{AstIndexBackend, AstIndexRunner, CommandOutput, ProcessRunner};
use uals_core::backend::{BackendError, SearchBackend, SpanProvider, UsageSource};
use uals_core::config::AstIndexConfig;
use uals_core::types::{BackendId, QueryKind, Repo, SearchRequest, SymbolKind};

fn fixture_text(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ast_index").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

fn ok(text: String) -> CommandOutput {
    CommandOutput { stdout: text, stderr: String::new(), exit_code: Some(0) }
}

fn recorded(name: &str) -> CommandOutput {
    ok(fixture_text(name))
}

type Route = Box<dyn Fn(&[String]) -> Option<CommandOutput> + Send + Sync>;

/// Answers commands from recorded outputs and remembers what was run.
struct FakeRunner {
    routes: Vec<Route>,
    calls: Mutex<Vec<(PathBuf, Vec<String>)>>,
    index_exists: bool,
}

impl FakeRunner {
    fn new() -> Self {
        Self { routes: Vec::new(), calls: Mutex::new(Vec::new()), index_exists: true }
    }

    /// Answers every command whose first argument is `command`.
    fn on(mut self, command: &'static str, output: CommandOutput) -> Self {
        self.routes
            .push(Box::new(move |args| (args.first().map(String::as_str) == Some(command)).then(|| output.clone())));
        self
    }

    /// Answers `query` runs whose SQL contains `needle`.
    fn on_query(mut self, needle: &'static str, output: CommandOutput) -> Self {
        self.routes.push(Box::new(move |args| {
            (args.first().map(String::as_str) == Some("query") && args.get(1).is_some_and(|sql| sql.contains(needle)))
                .then(|| output.clone())
        }));
        self
    }

    fn without_index(mut self) -> Self {
        self.index_exists = false;
        self
    }

    fn calls(&self) -> Vec<(PathBuf, Vec<String>)> {
        self.calls.lock().map(|calls| calls.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl AstIndexRunner for FakeRunner {
    async fn run(&self, root: &Path, args: &[String]) -> Result<CommandOutput, BackendError> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push((root.to_path_buf(), args.to_vec()));
        }
        self.routes
            .iter()
            .find_map(|route| route(args))
            .ok_or_else(|| BackendError::Failed(format!("no recorded output for {args:?}")))
    }

    fn index_exists(&self, _root: &Path) -> bool {
        self.index_exists
    }
}

fn repo() -> Repo {
    Repo { name: "sample-android".into(), root: PathBuf::from("/ws/sample-android") }
}

fn backend(runner: &Arc<FakeRunner>) -> AstIndexBackend {
    AstIndexBackend::with_runner(runner.clone())
}

fn identifier(query: &str, fetch_limit: usize) -> SearchRequest {
    SearchRequest { query: query.into(), kind: QueryKind::Identifier, fetch_limit, paths: Vec::new() }
}

fn summary(answer: &uals_core::types::BackendAnswer) -> Vec<(String, u32, u32, bool, Option<SymbolKind>)> {
    answer
        .hits
        .iter()
        .map(|hit| {
            let relative =
                hit.path.strip_prefix("/ws/sample-android").map(|p| p.display().to_string()).unwrap_or_default();
            (relative, hit.lines.start, hit.lines.end, hit.is_definition, hit.symbol_kind)
        })
        .collect()
}

#[test]
fn supports_only_identifier_and_natural() {
    let backend = AstIndexBackend::new(AstIndexConfig::default());
    assert!(backend.supports(QueryKind::Identifier));
    assert!(backend.supports(QueryKind::Natural));
    assert!(!backend.supports(QueryKind::Regex));
    assert!(!backend.supports(QueryKind::Literal));
    assert_eq!(SearchBackend::id(&backend), BackendId::AstIndex);
}

#[tokio::test]
async fn identifier_ranks_exact_then_prefix_then_substring_and_types_before_functions() {
    let runner = Arc::new(
        FakeRunner::new()
            .on("symbol", recorded("symbol_rank_mixed.json"))
            .on_query("FROM symbols", recorded("query_symbol_ranges.json"))
            .on("usages", recorded("usages_hostselector.json")),
    );
    let answer = backend(&runner).search(&repo(), &identifier("sample.network.HostSelector", 20)).await.unwrap();

    let definitions: Vec<_> = answer
        .hits
        .iter()
        .filter(|hit| hit.is_definition)
        .map(|hit| (hit.symbol.clone().unwrap(), hit.symbol_kind.unwrap()))
        .collect();
    assert_eq!(
        definitions,
        vec![
            ("HostSelector".to_string(), SymbolKind::Class),
            ("HostSelector".to_string(), SymbolKind::Function),
            ("HostSelector".to_string(), SymbolKind::Property),
            ("HostSelectorConfig".to_string(), SymbolKind::Class),
            ("hostSelectorOf".to_string(), SymbolKind::Function),
            ("InMemoryHostSelector".to_string(), SymbolKind::Class),
        ]
    );
    // The import row is gone; the real class has its extent from the index (10..33).
    let first = &answer.hits[0];
    assert_eq!(first.path, PathBuf::from("/ws/sample-android/network/src/main/kotlin/sample/network/HostSelector.kt"));
    assert_eq!((first.lines.start, first.lines.end), (10, 33));
    // Definitions the index query does not know keep their declaration line.
    assert_eq!((answer.hits[1].lines.start, answer.hits[1].lines.end), (30, 30));
    // Usages follow the definitions and are not definitions.
    let usages: Vec<_> = answer.hits.iter().skip(6).collect();
    assert_eq!(usages.len(), 2);
    assert!(usages.iter().all(|hit| !hit.is_definition && hit.symbol.as_deref() == Some("HostSelector")));
    assert_eq!(usages[0].snippet.as_deref(), Some("val selector = HostSelector("));
    // Rank is the position.
    let ranks: Vec<u32> = answer.hits.iter().map(|hit| hit.rank).collect();
    assert_eq!(ranks, (0..8).collect::<Vec<u32>>());
    // Everything ran in the repository root.
    assert!(runner.calls().iter().all(|(root, _)| root == Path::new("/ws/sample-android")));
}

#[tokio::test]
async fn identifier_on_the_recorded_fixture_gives_definition_then_usages() {
    let runner = Arc::new(
        FakeRunner::new()
            .on("symbol", recorded("symbol_hostselector.json"))
            .on_query("FROM symbols", recorded("query_symbol_ranges.json"))
            .on("usages", recorded("usages_hostselector.json")),
    );
    let answer = backend(&runner).search(&repo(), &identifier("HostSelector", 10)).await.unwrap();
    assert_eq!(
        summary(&answer),
        vec![
            (
                "network/src/main/kotlin/sample/network/HostSelector.kt".to_string(),
                10,
                33,
                true,
                Some(SymbolKind::Class)
            ),
            ("app/src/main/kotlin/sample/app/App.kt".to_string(), 11, 11, false, None),
            ("network/src/main/kotlin/sample/network/ConfigLoader.kt".to_string(), 16, 16, false, None),
        ]
    );
    assert_eq!(answer.stale, None);
    assert_eq!(answer.partial, None);
}

#[tokio::test]
async fn identifier_respects_fetch_limit_and_skips_usages_when_definitions_fill_it() {
    let runner = Arc::new(
        FakeRunner::new()
            .on("symbol", recorded("symbol_hostselector.json"))
            .on_query("FROM symbols", recorded("query_symbol_ranges.json"))
            .on("usages", recorded("usages_hostselector.json")),
    );
    let two = backend(&runner).search(&repo(), &identifier("HostSelector", 2)).await.unwrap();
    assert_eq!(two.hits.len(), 2);

    let runner = Arc::new(
        FakeRunner::new()
            .on("symbol", recorded("symbol_hostselector.json"))
            .on_query("FROM symbols", recorded("query_symbol_ranges.json")),
    );
    let one = backend(&runner).search(&repo(), &identifier("HostSelector", 1)).await.unwrap();
    assert_eq!(one.hits.len(), 1);
    assert!(runner.calls().iter().all(|(_, args)| args[0] != "usages"));
}

#[tokio::test]
async fn identifier_puts_test_usages_after_production_usages() {
    let runner = Arc::new(
        FakeRunner::new()
            .on("symbol", recorded("symbol_hostselector.json"))
            .on_query("FROM symbols", recorded("query_symbol_ranges.json"))
            .on("usages", recorded("usages_test_first.json")),
    );
    let answer = backend(&runner).search(&repo(), &identifier("HostSelector", 10)).await.unwrap();
    let paths: Vec<String> = answer.hits.iter().skip(1).map(|hit| hit.path.display().to_string()).collect();
    assert_eq!(
        paths,
        vec![
            "/ws/sample-android/app/src/main/kotlin/sample/app/App.kt",
            "/ws/sample-android/network/src/main/kotlin/sample/network/ConfigLoader.kt",
            "/ws/sample-android/network/src/test/kotlin/sample/network/HostSelectorTest.kt",
        ]
    );
}

#[tokio::test]
async fn path_globs_keep_only_matching_hits() {
    let runner = Arc::new(
        FakeRunner::new()
            .on("symbol", recorded("symbol_hostselector.json"))
            .on_query("FROM symbols", recorded("query_symbol_ranges.json"))
            .on("usages", recorded("usages_test_first.json")),
    );
    let mut request = identifier("HostSelector", 10);
    request.paths = vec!["network/src/main/**".into()];
    let answer = backend(&runner).search(&repo(), &request).await.unwrap();
    let paths: Vec<String> = answer.hits.iter().map(|hit| hit.path.display().to_string()).collect();
    assert_eq!(
        paths,
        vec![
            "/ws/sample-android/network/src/main/kotlin/sample/network/HostSelector.kt",
            "/ws/sample-android/network/src/main/kotlin/sample/network/ConfigLoader.kt",
        ]
    );

    // A folder name works too, and a glob that matches nothing leaves nothing.
    request.paths = vec!["app".into()];
    let answer = backend(&runner).search(&repo(), &request).await.unwrap();
    assert_eq!(answer.hits.len(), 1);
    request.paths = vec!["core/**".into()];
    assert!(backend(&runner).search(&repo(), &request).await.unwrap().hits.is_empty());
}

#[tokio::test]
async fn path_negations_alone_drop_only_the_matching_hits() {
    let runner = Arc::new(
        FakeRunner::new()
            .on("symbol", recorded("symbol_hostselector.json"))
            .on_query("FROM symbols", recorded("query_symbol_ranges.json"))
            .on("usages", recorded("usages_test_first.json")),
    );
    let mut request = identifier("HostSelector", 10);
    let all = backend(&runner).search(&repo(), &request).await.unwrap();
    request.paths = vec!["!app".into()];
    let rest = backend(&runner).search(&repo(), &request).await.unwrap();
    assert_eq!(rest.hits.len(), all.hits.len() - 1);
    assert!(rest.hits.iter().all(|hit| !hit.path.to_str().unwrap().contains("/app/")));
}

#[tokio::test]
async fn natural_keeps_the_order_of_explore() {
    let runner = Arc::new(FakeRunner::new().on("explore", recorded("explore_retry_backoff.json")));
    let request = SearchRequest {
        query: "retry with backoff".into(),
        kind: QueryKind::Natural,
        fetch_limit: 10,
        paths: Vec::new(),
    };
    let answer = backend(&runner).search(&repo(), &request).await.unwrap();
    assert_eq!(
        summary(&answer),
        vec![
            ("core/src/main/kotlin/sample/core/RetryPolicy.kt".to_string(), 7, 18, true, Some(SymbolKind::Class)),
            ("core/src/main/kotlin/sample/core/RetryPolicy.kt".to_string(), 12, 12, true, Some(SymbolKind::Function)),
            ("core/src/test/kotlin/sample/core/RetryPolicyTest.kt".to_string(), 6, 14, true, Some(SymbolKind::Class)),
        ]
    );
    assert_eq!(answer.hits.iter().map(|hit| hit.rank).collect::<Vec<_>>(), vec![0, 1, 2]);
    assert!(answer.hits[0].score.is_some_and(|score| score > 57.0));
    // The whole query is one argument.
    let calls = runner.calls();
    assert!(calls[0].1.iter().any(|arg| arg == "retry with backoff"));

    let limited = SearchRequest { fetch_limit: 2, ..request.clone() };
    assert_eq!(backend(&runner).search(&repo(), &limited).await.unwrap().hits.len(), 2);
    let filtered = SearchRequest { paths: vec!["**/test/**".into()], ..request };
    assert_eq!(backend(&runner).search(&repo(), &filtered).await.unwrap().hits.len(), 1);
}

#[tokio::test]
async fn regex_and_literal_are_refused() {
    let runner = Arc::new(FakeRunner::new());
    let request = SearchRequest { query: "x".into(), kind: QueryKind::Regex, fetch_limit: 5, paths: Vec::new() };
    assert!(matches!(backend(&runner).search(&repo(), &request).await, Err(BackendError::Unavailable(_))));
}

#[tokio::test]
async fn usages_lists_every_site_and_reports_a_cut() {
    let runner = Arc::new(FakeRunner::new().on("usages", recorded("usages_hostselector.json")));
    let backend = backend(&runner);
    let all = backend.usages(&repo(), "sample.network.HostSelector", 50).await.unwrap();
    assert!(all.complete);
    assert_eq!(all.usages.len(), 2);
    assert_eq!(all.usages[0].repo, "sample-android");
    assert_eq!(all.usages[0].path, PathBuf::from("/ws/sample-android/app/src/main/kotlin/sample/app/App.kt"));
    assert_eq!(all.usages[0].line, 11);
    assert_eq!(all.usages[0].context.as_deref(), Some("val selector = HostSelector("));
    let name_arg = runner.calls()[0].1.last().cloned();
    assert_eq!(name_arg.as_deref(), Some("HostSelector"));

    let cut = backend.usages(&repo(), "HostSelector", 1).await.unwrap();
    assert!(!cut.complete);
    assert_eq!(cut.usages.len(), 1);
}

#[tokio::test]
async fn spans_come_from_one_query_per_chunk() {
    let runner = Arc::new(FakeRunner::new().on_query("f.path IN", recorded("query_symbol_ranges.json")));
    let backend = backend(&runner);
    let files: Vec<PathBuf> = [
        "network/src/main/kotlin/sample/network/HostSelector.kt",
        "core/src/main/kotlin/sample/core/RetryPolicy.kt",
        "network/src/main/kotlin/sample/network/HostStore.kt",
    ]
    .iter()
    .map(|path| Path::new("/ws/sample-android").join(path))
    .collect();
    let spans = backend.spans(&repo(), &files).await.unwrap();
    assert_eq!(spans.len(), 14);
    assert_eq!(runner.calls().len(), 1);
    let selector = spans.iter().find(|span| span.name == "HostSelector").unwrap();
    assert_eq!(selector.kind, SymbolKind::Class);
    assert_eq!((selector.lines.start, selector.lines.end), (10, 33));
    assert_eq!(
        selector.path,
        PathBuf::from("/ws/sample-android/network/src/main/kotlin/sample/network/HostSelector.kt")
    );
    assert_eq!(spans.iter().find(|span| span.name == "HostStore").unwrap().kind, SymbolKind::Interface);
    assert_eq!(spans.iter().find(|span| span.name == "failed").unwrap().kind, SymbolKind::Property);

    // 1200 files: three queries of at most 500 paths, files outside the repository are ignored.
    let runner = Arc::new(FakeRunner::new().on_query("f.path IN", recorded("query_symbol_ranges.json")));
    let mut many: Vec<PathBuf> =
        (0..1200).map(|index| PathBuf::from(format!("/ws/sample-android/src/File{index}.kt"))).collect();
    many.push(PathBuf::from("/elsewhere/Other.kt"));
    let spans = AstIndexBackend::with_runner(runner.clone()).spans(&repo(), &many).await.unwrap();
    assert_eq!(runner.calls().len(), 3);
    assert_eq!(spans.len(), 3 * 14);
    assert!(
        runner
            .calls()
            .iter()
            .all(|(_, args)| args[1].matches("'/ws").count() == 0 && args[1].matches("src/File").count() <= 500)
    );
    assert!(AstIndexBackend::with_runner(Arc::new(FakeRunner::new())).spans(&repo(), &[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn spans_drop_imports_and_packages_and_fill_missing_end_lines() {
    let runner = Arc::new(FakeRunner::new().on_query("f.path IN", recorded("query_spans_python.json")));
    let files = vec![PathBuf::from("/ws/sample-android/sample_tools/cache.py")];
    let spans = backend(&runner).spans(&repo(), &files).await.unwrap();
    let names: Vec<_> = spans.iter().map(|span| (span.name.as_str(), span.lines.start, span.lines.end)).collect();
    assert_eq!(names, vec![("LruCache", 4, 20), ("get", 9, 9)]);
}

#[tokio::test]
async fn spans_escape_quotes_in_paths() {
    let runner = Arc::new(FakeRunner::new().on("query", recorded("query_spans_python.json")));
    let files = vec![PathBuf::from("/ws/sample-android/it's.kt")];
    backend(&runner).spans(&repo(), &files).await.unwrap();
    assert!(runner.calls()[0].1[1].contains("'it''s.kt'"));
}

#[tokio::test]
async fn missing_index_is_unavailable_with_the_fix() {
    let runner = Arc::new(
        FakeRunner::new()
            .on("symbol", recorded("missing_index_symbol.txt"))
            .on("usages", recorded("missing_index_symbol.txt")),
    );
    let backend = backend(&runner);
    for result in [
        backend.search(&repo(), &identifier("HostSelector", 5)).await.map(|_| ()),
        backend.usages(&repo(), "HostSelector", 5).await.map(|_| ()),
    ] {
        match result {
            Err(BackendError::Unavailable(reason)) => assert!(reason.contains("ast-index rebuild"), "{reason}"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }
    // `query` against a database without tables fails with a SQLite error instead.
    let failing = Arc::new(FakeRunner::new().on(
        "query",
        CommandOutput { stdout: String::new(), stderr: "Error: no such table: files\n".into(), exit_code: Some(1) },
    ));
    let files = vec![PathBuf::from("/ws/sample-android/A.kt")];
    assert!(matches!(
        AstIndexBackend::with_runner(failing).spans(&repo(), &files).await,
        Err(BackendError::Unavailable(_))
    ));
}

#[tokio::test]
async fn unparsable_output_is_a_failure() {
    for garbage in ["{\"items\": [", "this is not json at all", "", "[1, 2"] {
        let runner = Arc::new(FakeRunner::new().on("symbol", ok(garbage.into())).on("usages", ok(garbage.into())));
        let result = backend(&runner).search(&repo(), &identifier("HostSelector", 5)).await;
        assert!(matches!(result, Err(BackendError::Failed(_))), "{garbage:?} gave {result:?}");
    }
    let wrong_shape = Arc::new(FakeRunner::new().on("symbol", ok("{\"items\": 5}".into())));
    assert!(matches!(
        backend(&wrong_shape).search(&repo(), &identifier("HostSelector", 5)).await,
        Err(BackendError::Failed(_))
    ));
    let crashed = Arc::new(
        FakeRunner::new()
            .on("usages", CommandOutput { stdout: String::new(), stderr: "boom\n".into(), exit_code: Some(101) }),
    );
    assert!(matches!(backend(&crashed).usages(&repo(), "X", 5).await, Err(BackendError::Failed(_))));
}

#[tokio::test]
async fn missing_binary_is_unavailable() {
    let backend =
        AstIndexBackend::new(AstIndexConfig { binary: "uals-no-such-ast-index-binary".into(), update_on_start: false });
    let error = backend.search(&repo(), &identifier("HostSelector", 5)).await.unwrap_err();
    assert_eq!(error, BackendError::Unavailable("ast-index is not installed".into()));
    let error = backend.refresh(&repo(), false).await.unwrap_err();
    assert_eq!(error, BackendError::Unavailable("ast-index is not installed".into()));
}

#[tokio::test]
async fn index_state_reads_metadata_without_indexing() {
    let runner = Arc::new(FakeRunner::new().on_query("last_update_at", recorded("query_state.json")));
    let state = backend(&runner).index_state(&repo()).await;
    assert!(state.indexed);
    assert_eq!(state.backend, BackendId::AstIndex);
    assert_eq!(state.repo, "sample-android");
    assert_eq!(state.updated_at_ms, Some(1_794_478_971_901));
    assert_eq!(state.detail.as_deref(), Some("7 files, 27 symbols"));

    // No database: nothing is run at all, so no stub cache folder appears.
    let none = Arc::new(FakeRunner::new().without_index());
    let state = backend(&none).index_state(&repo()).await;
    assert!(!state.indexed);
    assert!(state.detail.is_some_and(|detail| detail.contains("ast-index rebuild")));
    assert!(none.calls().is_empty());
}

#[tokio::test]
async fn refresh_runs_update_or_rebuild_in_the_root() {
    let runner = Arc::new(
        FakeRunner::new().on("update", ok("Checking for changes...\n".into())).on("rebuild", ok(String::new())),
    );
    let backend = backend(&runner);
    backend.refresh(&repo(), false).await.unwrap();
    backend.refresh(&repo(), true).await.unwrap();
    let calls = runner.calls();
    assert_eq!(calls[0], (PathBuf::from("/ws/sample-android"), vec!["update".to_string()]));
    assert_eq!(calls[1], (PathBuf::from("/ws/sample-android"), vec!["rebuild".to_string()]));

    let failing = Arc::new(
        FakeRunner::new()
            .on("update", CommandOutput { stdout: String::new(), stderr: "disk full\n".into(), exit_code: Some(1) }),
    );
    assert!(matches!(
        AstIndexBackend::with_runner(failing).refresh(&repo(), false).await,
        Err(BackendError::Failed(_))
    ));
}

/// Live run on a temp copy of the public fixture. Skipped when `ast-index` is not on PATH.
#[tokio::test]
async fn live_run_on_a_copy_of_the_android_fixture() {
    if which::which("ast-index").is_err() || which::which("git").is_err() {
        eprintln!("skipped: ast-index or git is not on PATH");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace/sample-android");
    let root = temp.path().join("sample-android");
    copy_dir(&source, &root);
    let git = std::process::Command::new("git").arg("init").arg("-q").current_dir(&root).status().unwrap();
    assert!(git.success());
    let root = root.canonicalize().unwrap();
    let repo = Repo { name: "sample-android".into(), root: root.clone() };
    let cache = temp.path().join("cache");
    let runner = ProcessRunner::new("ast-index").with_env("AST_INDEX_CACHE_DIR", cache.as_os_str());
    let backend = AstIndexBackend::with_runner(Arc::new(runner));

    // Before indexing: not indexed, and asking did not create a cache folder.
    assert!(!backend.index_state(&repo).await.indexed);
    assert!(!cache.exists(), "index_state must not create cache folders");
    assert!(matches!(backend.search(&repo, &identifier("HostSelector", 5)).await, Err(BackendError::Unavailable(_))));

    backend.refresh(&repo, true).await.unwrap();
    let state = backend.index_state(&repo).await;
    assert!(state.indexed, "{state:?}");
    assert!(state.updated_at_ms.is_some());

    let answer = backend.search(&repo, &identifier("sample.network.HostSelector", 10)).await.unwrap();
    let first = &answer.hits[0];
    assert!(first.is_definition);
    assert_eq!(first.symbol.as_deref(), Some("HostSelector"));
    assert_eq!(first.symbol_kind, Some(SymbolKind::Class));
    assert_eq!(first.path, root.join("network/src/main/kotlin/sample/network/HostSelector.kt"));
    assert!(first.lines.end > first.lines.start);
    assert!(answer.hits.iter().skip(1).all(|hit| !hit.is_definition));
    assert!(answer.hits.len() >= 3, "{answer:?}");

    let usages = backend.usages(&repo, "HostSelector", 50).await.unwrap();
    assert!(usages.complete);
    assert!(usages.usages.iter().any(|usage| usage.path.ends_with("app/src/main/kotlin/sample/app/App.kt")));

    let spans = backend.spans(&repo, std::slice::from_ref(&first.path)).await.unwrap();
    assert!(spans.iter().any(|span| span.name == "markFailed" && span.kind == SymbolKind::Function));

    let natural = SearchRequest {
        query: "retry with backoff".into(),
        kind: QueryKind::Natural,
        fetch_limit: 5,
        paths: Vec::new(),
    };
    let answer = backend.search(&repo, &natural).await.unwrap();
    assert_eq!(answer.hits[0].symbol.as_deref(), Some("RetryPolicy"));
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}
