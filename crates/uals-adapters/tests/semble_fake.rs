//! `SembleBackend` parsing and error mapping against the recorded Semble 0.7.3 outputs
//! (`tests/fixtures/semble/*.json`), replayed by a fake `McpToolCaller`. No Semble needed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use uals_adapters::semble::SembleBackend;
use uals_core::backend::{BackendError, McpToolCaller, SearchBackend, ToolOutput};
use uals_core::config::{SembleConfig, SembleContent};
use uals_core::types::{BackendId, LineRange, QueryKind, Repo, SearchRequest};

const WORKSPACE: &str = "/work/space";

/// Replays one answer for every call and records the calls.
struct Replay {
    answer: Result<ToolOutput, BackendError>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl Replay {
    fn fixture(name: &str) -> Arc<Self> {
        Self::new(Ok(load_fixture(name)))
    }

    fn new(answer: Result<ToolOutput, BackendError>) -> Arc<Self> {
        Arc::new(Self { answer, calls: Mutex::new(Vec::new()) })
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl McpToolCaller for Replay {
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolOutput, BackendError> {
        self.calls.lock().unwrap().push((name.to_owned(), arguments));
        self.answer.clone()
    }
}

/// Reads `<name>.json` (a serialized `CallToolResult`) with `$WORKSPACE` substituted.
fn load_fixture(name: &str) -> ToolOutput {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/semble").join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let value: Value = serde_json::from_str(&text.replace("$WORKSPACE", WORKSPACE)).unwrap();
    let texts = value["content"]
        .as_array()
        .map(|items| {
            items.iter().filter(|i| i["type"] == "text").map(|i| i["text"].as_str().unwrap().to_owned()).collect()
        })
        .unwrap_or_default();
    ToolOutput {
        texts,
        structured: value.get("structuredContent").cloned(),
        is_error: value["isError"].as_bool().unwrap_or(false),
    }
}

fn text_output(text: &str, is_error: bool) -> ToolOutput {
    ToolOutput { texts: vec![text.to_owned()], structured: None, is_error }
}

fn repo(sub: &str) -> Repo {
    Repo { name: sub.to_owned(), root: PathBuf::from(WORKSPACE).join(sub) }
}

fn request(query: &str, fetch_limit: usize) -> SearchRequest {
    SearchRequest { query: query.to_owned(), kind: QueryKind::Natural, fetch_limit, paths: vec![] }
}

fn backend(replay: &Arc<Replay>, content: Option<SembleContent>) -> SembleBackend {
    SembleBackend::new(replay.clone(), SembleConfig { content, ..SembleConfig::default() })
}

#[tokio::test]
async fn maps_results_to_hits_with_absolute_paths_and_ranks() {
    let replay = Replay::fixture("search_retry_backoff_one");
    let answer = backend(&replay, None)
        .search(&repo("sample-android"), &request("retry with exponential backoff", 5))
        .await
        .unwrap();

    assert_eq!(answer.hits.len(), 5);
    assert_eq!(answer.stale, None);
    assert_eq!(answer.partial, None);
    let first = &answer.hits[0];
    assert_eq!(first.backend, BackendId::Semble);
    assert_eq!(first.repo, "sample-android");
    assert_eq!(first.path, PathBuf::from("/work/space/sample-android/core/src/main/kotlin/sample/core/RetryPolicy.kt"));
    assert_eq!(first.lines, LineRange::new(1, 18));
    assert!((first.score.unwrap() - 0.02447155744546866).abs() < 1e-12);
    assert_eq!(answer.hits[1].lines, LineRange::new(19, 43));
    let ranks: Vec<u32> = answer.hits.iter().map(|h| h.rank).collect();
    assert_eq!(ranks, vec![0, 1, 2, 3, 4]);
    assert!(answer.hits.windows(2).all(|w| w[0].score >= w[1].score), "best first");
}

#[tokio::test]
async fn sends_one_search_call_with_the_documented_arguments() {
    let replay = Replay::fixture("search_retry_backoff_one_top2");
    let answer = backend(&replay, None)
        .search(&repo("sample-android"), &request("retry with exponential backoff", 2))
        .await
        .unwrap();
    assert_eq!(answer.hits.len(), 2);

    assert_eq!(
        replay.calls(),
        vec![(
            "search".to_owned(),
            json!({
                "query": "retry with exponential backoff",
                "repo": "/work/space/sample-android",
                "top_k": 2,
                "max_snippet_lines": 0,
            })
        )],
        "no `content` key when the config leaves it unset"
    );
}

#[tokio::test]
async fn passes_content_from_the_config() {
    for (content, name) in [
        (SembleContent::Code, "code"),
        (SembleContent::Docs, "docs"),
        (SembleContent::Config, "config"),
        (SembleContent::All, "all"),
    ] {
        let replay = Replay::fixture("search_retry_backoff_content_all");
        backend(&replay, Some(content)).search(&repo("sample-android"), &request("q", 5)).await.unwrap();
        assert_eq!(replay.calls()[0].1["content"], json!(name));
    }
}

#[tokio::test]
async fn zero_fetch_limit_is_sent_as_one() {
    let replay = Replay::fixture("search_retry_backoff_one_top2");
    backend(&replay, None).search(&repo("sample-android"), &request("q", 0)).await.unwrap();
    assert_eq!(replay.calls()[0].1["top_k"], json!(1));
}

#[tokio::test]
async fn keeps_the_snippet_when_semble_sends_one() {
    let with = Replay::fixture("search_retry_backoff_one_snippet_full");
    let answer = backend(&with, None).search(&repo("sample-android"), &request("q", 3)).await.unwrap();
    assert!(answer.hits[0].snippet.as_deref().unwrap().contains("class RetryPolicy"));

    let without = Replay::fixture("search_retry_backoff_one_snippet0");
    let answer = backend(&without, None).search(&repo("sample-android"), &request("q", 5)).await.unwrap();
    assert!(!answer.hits.is_empty());
    assert!(answer.hits.iter().all(|h| h.snippet.is_none()));
}

#[tokio::test]
async fn nonsense_query_still_returns_what_semble_returns() {
    let replay = Replay::fixture("search_gibberish");
    let answer = backend(&replay, None).search(&repo("sample-tools"), &request("zzzqqqxxyy vvwwkk", 5)).await.unwrap();
    assert_eq!(answer.hits.len(), 3);
    assert!(answer.hits.iter().all(|h| h.path.starts_with("/work/space/sample-tools/sample_tools")));
}

#[tokio::test]
async fn workspace_root_repo_gives_root_relative_paths() {
    let replay = Replay::fixture("search_retry_backoff_workspace_root");
    let root = Repo { name: "space".into(), root: PathBuf::from(WORKSPACE) };
    let answer = backend(&replay, None).search(&root, &request("retry with exponential backoff", 5)).await.unwrap();
    assert_eq!(
        answer.hits[0].path,
        PathBuf::from("/work/space/sample-android/core/src/main/kotlin/sample/core/RetryPolicy.kt")
    );
}

#[tokio::test]
async fn text_failures_with_is_error_false_are_unavailable_with_the_first_line() {
    for (fixture, expected) in [
        (
            "search_missing_path",
            "semble: Failed to index '/work/space/does-not-exist': Path does not exist: /work/space/does-not-exist",
        ),
        (
            "search_empty_folder",
            "semble: Failed to index '/work/space-empty': No supported files found under /work/space-empty.",
        ),
        ("search_multi_one_missing", "semble: Failed to index"),
        ("search_file_path", "semble: Failed to index"),
        ("search_relative_path_missing", "semble: Failed to index"),
    ] {
        let replay = Replay::fixture(fixture);
        let error = backend(&replay, None).search(&repo("x"), &request("q", 5)).await.unwrap_err();
        match error {
            BackendError::Unavailable(message) => assert!(message.starts_with(expected), "{fixture}: {message}"),
            other => panic!("{fixture}: expected Unavailable, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn only_the_first_line_of_a_multiline_failure_is_reported() {
    let replay =
        Replay::new(Ok(text_output("Failed to index 'x': boom\nTraceback (most recent call last):\n  ...", false)));
    let error = backend(&replay, None).search(&repo("x"), &request("q", 5)).await.unwrap_err();
    assert_eq!(error, BackendError::Unavailable("semble: Failed to index 'x': boom".into()));
}

#[tokio::test]
async fn is_error_true_is_failed() {
    for fixture in ["search_missing_param", "search_top_k_zero", "unknown_tool"] {
        let replay = Replay::fixture(fixture);
        let error = backend(&replay, None).search(&repo("x"), &request("q", 5)).await.unwrap_err();
        match error {
            BackendError::Failed(message) => assert!(message.starts_with("semble: "), "{fixture}: {message}"),
            other => panic!("{fixture}: expected Failed, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn empty_answers_are_empty_not_errors() {
    for text in [r#"{"query": "q", "results": []}"#, r#"{"error": "No results found."}"#] {
        let replay = Replay::new(Ok(text_output(text, false)));
        let answer = backend(&replay, None).search(&repo("x"), &request("q", 5)).await.unwrap();
        assert!(answer.hits.is_empty(), "{text}");
    }
}

#[tokio::test]
async fn parses_the_structured_result_string_and_a_double_encoded_text() {
    let inner = r#"{"query": "q", "results": [{"file_path": "a/b.rs", "start_line": 3, "end_line": 9, "score": 0.5}]}"#;
    let structured = ToolOutput { texts: vec![], structured: Some(json!({"result": inner})), is_error: false };
    let double = text_output(&serde_json::to_string(inner).unwrap(), false);
    let double = ToolOutput { structured: None, ..double };
    // A double-encoded value starts with a quote, which the "does not start with {" rule rejects first;
    // the structured field is the supported route, so assert both behaviours explicitly.
    let answer = backend(&Replay::new(Ok(structured)), None).search(&repo("x"), &request("q", 5)).await.unwrap();
    assert_eq!(answer.hits[0].path, PathBuf::from("/work/space/x/a/b.rs"));
    assert_eq!(answer.hits[0].lines, LineRange::new(3, 9));
    let error = backend(&Replay::new(Ok(double)), None).search(&repo("x"), &request("q", 5)).await.unwrap_err();
    assert!(matches!(error, BackendError::Unavailable(_)), "{error:?}");
}

#[tokio::test]
async fn broken_json_and_odd_shapes_are_failed() {
    for text in [r#"{"query": "q", "results": ["#, r#"{"unexpected": true}"#, r#"{"results": [{"file_path": 5}]}"#] {
        let replay = Replay::new(Ok(text_output(text, false)));
        let error = backend(&replay, None).search(&repo("x"), &request("q", 5)).await.unwrap_err();
        assert!(matches!(error, BackendError::Failed(_)), "{text}: {error:?}");
    }
    let no_text = Replay::new(Ok(ToolOutput::default()));
    let error = backend(&no_text, None).search(&repo("x"), &request("q", 5)).await.unwrap_err();
    assert!(matches!(error, BackendError::Failed(_)), "{error:?}");
}

#[tokio::test]
async fn caller_errors_pass_through() {
    for error in [BackendError::Unavailable("semble is down".into()), BackendError::Failed("crashed".into())] {
        let replay = Replay::new(Err(error.clone()));
        assert_eq!(backend(&replay, None).search(&repo("x"), &request("q", 5)).await.unwrap_err(), error);
    }
}

#[tokio::test]
async fn path_globs_filter_results_and_ranks_follow_the_filtered_order() {
    let replay = Replay::fixture("search_retry_backoff_top100");
    let mut req = request("retry with exponential backoff", 4);
    req.paths = vec!["network/**/*.kt".into()];
    let answer = backend(&replay, None).search(&repo("sample-android"), &req).await.unwrap();

    assert_eq!(replay.calls()[0].1["top_k"], json!(16), "over-fetches when filtering");
    let paths: Vec<String> = answer
        .hits
        .iter()
        .map(|h| h.path.strip_prefix("/work/space/sample-android").unwrap().display().to_string())
        .collect();
    assert_eq!(
        paths,
        vec![
            "network/src/main/kotlin/sample/network/ConfigLoader.kt",
            "network/src/main/kotlin/sample/network/HostSelector.kt",
            "network/src/main/kotlin/sample/network/HostStore.kt",
            "network/src/main/kotlin/sample/network/ConfigLoader.kt",
        ]
    );
    assert_eq!(answer.hits.iter().map(|h| h.rank).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
}

#[tokio::test]
async fn path_globs_with_no_match_give_an_empty_answer_and_bad_globs_fail() {
    let replay = Replay::fixture("search_retry_backoff_top100");
    let mut req = request("q", 5);
    req.paths = vec!["does/not/exist/**".into()];
    assert!(backend(&replay, None).search(&repo("sample-android"), &req).await.unwrap().hits.is_empty());

    req.paths = vec!["{a".into()];
    let error = backend(&replay, None).search(&repo("sample-android"), &req).await.unwrap_err();
    assert!(matches!(error, BackendError::Failed(_)), "{error:?}");
}

#[tokio::test]
async fn supports_identifiers_and_natural_language_only() {
    let backend = backend(&Replay::fixture("search_gibberish"), None);
    assert_eq!(backend.id(), BackendId::Semble);
    assert!(backend.supports(QueryKind::Identifier));
    assert!(backend.supports(QueryKind::Natural));
    assert!(!backend.supports(QueryKind::Regex));
    assert!(!backend.supports(QueryKind::Literal));
}

#[tokio::test]
async fn index_state_never_calls_semble_and_learns_from_searches() {
    let replay = Replay::fixture("search_retry_backoff_one");
    let backend = backend(&replay, None);
    let repo = repo("sample-android");

    let before = backend.index_state(&repo).await;
    assert_eq!(before.backend, BackendId::Semble);
    assert_eq!(before.repo, "sample-android");
    assert!(!before.indexed);
    assert_eq!(before.updated_at_ms, None);
    assert!(before.detail.unwrap().contains("not checked"));

    backend.search(&repo, &request("q", 5)).await.unwrap();
    let after = backend.index_state(&repo).await;
    assert!(after.indexed);
    assert!(after.updated_at_ms.is_some());
    assert!(after.detail.unwrap().contains("in this session"));
    assert_eq!(replay.calls().len(), 1, "index_state made no call");

    let other = backend.index_state(&Repo { name: "other".into(), root: PathBuf::from("/work/space/other") }).await;
    assert!(!other.indexed);
}

#[tokio::test]
async fn failed_searches_do_not_mark_a_repo_indexed() {
    let replay = Replay::fixture("search_missing_path");
    let backend = backend(&replay, None);
    let _ = backend.search(&repo("x"), &request("q", 5)).await;
    assert!(!backend.index_state(&repo("x")).await.indexed);
}

#[tokio::test]
async fn warm_returns_at_once_and_searches_every_repo_in_the_background() {
    let replay = Replay::fixture("search_retry_backoff_one_top2");
    let backend = backend(&replay, Some(SembleContent::Docs));
    let repos = [repo("a"), repo("b"), repo("c")];
    backend.warm(&repos).await.unwrap();

    for _ in 0..100 {
        if replay.calls().len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let calls = replay.calls();
    assert_eq!(calls.len(), 3);
    let mut warmed: Vec<String> = calls
        .iter()
        .map(|(name, args)| {
            assert_eq!(name, "search");
            assert_eq!(args["top_k"], json!(1));
            assert_eq!(args["max_snippet_lines"], json!(0));
            assert_eq!(args["content"], json!("docs"));
            args["repo"].as_str().unwrap().to_owned()
        })
        .collect();
    warmed.sort();
    assert_eq!(warmed, vec!["/work/space/a", "/work/space/b", "/work/space/c"]);
    for name in ["a", "b", "c"] {
        for _ in 0..100 {
            if backend.index_state(&repo(name)).await.indexed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(backend.index_state(&repo(name)).await.indexed, "{name}");
    }
}

#[tokio::test]
async fn warm_ignores_failures() {
    let replay = Replay::new(Err(BackendError::Unavailable("down".into())));
    let backend = backend(&replay, None);
    backend.warm(&[repo("a")]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!backend.index_state(&repo("a")).await.indexed);
}

#[tokio::test]
async fn path_negations_alone_drop_only_the_matching_hits() {
    let replay = Replay::fixture("search_retry_backoff_one");
    let mut request = request("retry with exponential backoff", 5);
    request.paths = vec!["!app".to_owned()];
    let answer = backend(&replay, None).search(&repo("sample-android"), &request).await.unwrap();

    assert_eq!(answer.hits.len(), 4);
    assert!(answer.hits.iter().all(|hit| !hit.path.to_str().unwrap().contains("/app/")));
    let ranks: Vec<u32> = answer.hits.iter().map(|h| h.rank).collect();
    assert_eq!(ranks, vec![0, 1, 2, 3]);
}
