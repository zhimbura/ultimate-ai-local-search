//! claude-context adapter tests through the public trait methods. A fake `McpToolCaller` replays
//! recorded fixtures (`milvus_*`) and synthetic ones (`synthetic_*`, built from `handlers.js`).

use std::collections::{HashMap, VecDeque};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use uals_adapters::claude_context::{ClaudeContextBackend, probe_tcp};
use uals_core::backend::{BackendError, McpToolCaller, SearchBackend, ToolOutput};
use uals_core::config::ClaudeContextConfig;
use uals_core::types::{BackendId, QueryKind, Repo, SearchRequest};

const ROOT: &str = "/work/sample-android";

/// Replays queued replies per tool and records every call.
#[derive(Default)]
struct FakeCaller {
    replies: Mutex<HashMap<String, VecDeque<Result<ToolOutput, BackendError>>>>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl FakeCaller {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn reply(self: &Arc<Self>, tool: &str, fixture: &str) -> &Arc<Self> {
        self.reply_with(tool, load_fixture(fixture))
    }

    fn reply_with(self: &Arc<Self>, tool: &str, reply: Result<ToolOutput, BackendError>) -> &Arc<Self> {
        self.replies.lock().unwrap().entry(tool.to_string()).or_default().push_back(reply);
        self
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    fn tools_called(&self) -> Vec<String> {
        self.calls().into_iter().map(|(tool, _)| tool).collect()
    }
}

#[async_trait]
impl McpToolCaller for FakeCaller {
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolOutput, BackendError> {
        self.calls.lock().unwrap().push((name.to_string(), arguments));
        self.replies
            .lock()
            .unwrap()
            .get_mut(name)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| Err(BackendError::Failed(format!("test: no reply queued for {name}"))))
    }
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude_context")
}

/// A recorded `CallToolResult`, or a recorded failure (`{"debug", "service_error"}`), as a naive
/// caller would report it.
fn load_fixture(name: &str) -> Result<ToolOutput, BackendError> {
    let path = fixture_dir().join(format!("{name}.json"));
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let json: Value = serde_json::from_str(&raw).unwrap();
    if let Some(error) = json.get("service_error").and_then(Value::as_str) {
        return Err(BackendError::Failed(error.to_string()));
    }
    let texts = json["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["text"].as_str().map(str::to_string))
        .collect();
    Ok(ToolOutput { texts, structured: None, is_error: json["isError"].as_bool().unwrap_or(false) })
}

fn text_output(text: &str) -> Result<ToolOutput, BackendError> {
    Ok(ToolOutput { texts: vec![text.to_string()], structured: None, is_error: false })
}

fn repo() -> Repo {
    Repo { name: "android".into(), root: PathBuf::from(ROOT) }
}

fn request(fetch_limit: usize) -> SearchRequest {
    SearchRequest {
        query: "retry with exponential backoff".into(),
        kind: QueryKind::Natural,
        fetch_limit,
        paths: Vec::new(),
    }
}

/// A listening socket that stands for Milvus for as long as it lives.
struct FakeMilvus {
    _listener: TcpListener,
    address: String,
}

impl FakeMilvus {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        Self { _listener: listener, address }
    }
}

/// A local address nobody listens on.
fn closed_address() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    drop(listener);
    address
}

fn config_with(address: &str) -> ClaudeContextConfig {
    let mut config = ClaudeContextConfig::default();
    config.env.insert("MILVUS_ADDRESS".into(), address.to_string());
    config
}

fn backend(caller: &Arc<FakeCaller>, milvus: &FakeMilvus) -> ClaudeContextBackend {
    ClaudeContextBackend::new(caller.clone(), config_with(&milvus.address))
}

fn unavailable(error: BackendError) -> String {
    match error {
        BackendError::Unavailable(text) => text,
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

fn failed(error: BackendError) -> String {
    match error {
        BackendError::Failed(text) => text,
        other => panic!("expected Failed, got {other:?}"),
    }
}

// ---------------------------------------------------------------- identity

#[test]
fn identifies_itself_and_supports_prose_and_names() {
    let backend = ClaudeContextBackend::new(FakeCaller::new(), ClaudeContextConfig::default());
    assert_eq!(backend.id(), BackendId::ClaudeContext);
    assert!(backend.supports(QueryKind::Natural));
    assert!(backend.supports(QueryKind::Identifier));
    assert!(!backend.supports(QueryKind::Regex));
    assert!(!backend.supports(QueryKind::Literal));
}

// ---------------------------------------------------------------- search: parsing

#[tokio::test]
async fn search_parses_results_in_order_with_absolute_paths_and_snippets() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    caller.reply("search_code", "synthetic_search_code_three_results");

    let answer = backend(&caller, &milvus).search(&repo(), &request(10)).await.unwrap();

    assert_eq!(answer.partial, None);
    assert_eq!(answer.stale, None);
    let shape: Vec<_> = answer
        .hits
        .iter()
        .map(|h| (h.rank, h.path.to_str().unwrap().to_string(), h.lines.start, h.lines.end))
        .collect();
    assert_eq!(
        shape,
        vec![
            (0, format!("{ROOT}/core/src/main/kotlin/sample/core/RetryPolicy.kt"), 1, 18),
            (1, format!("{ROOT}/network/src/main/kotlin/sample/network/HostSelector.kt"), 5, 9),
            (2, format!("{ROOT}/network/src/main/kotlin/sample/network/ConfigLoader.kt"), 12, 12),
        ]
    );
    assert!(answer.hits.iter().all(|h| h.backend == BackendId::ClaudeContext && h.repo == "android"));
    assert!(answer.hits.iter().all(|h| h.symbol.is_none() && h.score.is_none() && !h.is_definition));
    // Multi-line code is kept whole, blank lines included.
    assert_eq!(
        answer.hits[0].snippet.as_deref(),
        Some(
            "package sample.core\n\nclass RetryPolicy(private val maxAttempts: Int) {\n    fun delayMs(attempt: Int): Long = 100L shl attempt\n}"
        )
    );
    assert_eq!(answer.hits[2].snippet.as_deref(), Some("fun load() = Unit"));
}

#[tokio::test]
async fn search_sends_the_repository_root_query_and_limit() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    caller.reply("search_code", "synthetic_search_code_three_results");
    backend(&caller, &milvus).search(&repo(), &request(10)).await.unwrap();

    assert_eq!(
        caller.calls(),
        vec![(
            "search_code".to_string(),
            json!({"path": ROOT, "query": "retry with exponential backoff", "limit": 10})
        )]
    );
}

#[tokio::test]
async fn search_caps_the_limit_at_50_and_reports_the_cap() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    let blocks: Vec<String> = (1..=50)
        .map(|i| format!("{i}. Code snippet (kotlin) [sample-android]\n   Location: src/F{i}.kt:{i}-{i}\n   Rank: {i}\n   Context: \n```kotlin\nval a{i} = {i}\n```\n"))
        .collect();
    let text = format!("Found 50 results for query: \"q\" in codebase '{ROOT}'\n\n{}", blocks.join("\n"));
    caller.reply_with("search_code", text_output(&text));

    let answer = backend(&caller, &milvus).search(&repo(), &request(120)).await.unwrap();

    assert_eq!(caller.calls()[0].1["limit"], json!(50));
    assert_eq!(answer.hits.len(), 50);
    assert_eq!(answer.hits[49].rank, 49);
    assert!(answer.partial.as_deref().unwrap().contains("at most 50"));
}

#[tokio::test]
async fn search_returns_no_more_hits_than_fetch_limit() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    caller.reply("search_code", "synthetic_search_code_three_results");
    let answer = backend(&caller, &milvus).search(&repo(), &request(2)).await.unwrap();
    assert_eq!(answer.hits.len(), 2);
    assert_eq!(caller.calls()[0].1["limit"], json!(2));
}

#[tokio::test]
async fn search_handles_spaces_colons_windows_paths_fences_and_empty_code() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    caller.reply("search_code", "synthetic_search_code_special_paths");

    let answer = backend(&caller, &milvus).search(&repo(), &request(10)).await.unwrap();

    let paths: Vec<_> = answer.hits.iter().map(|h| h.path.to_str().unwrap().to_string()).collect();
    assert_eq!(
        paths,
        vec![
            format!("{ROOT}/docs/user guide/Retry Notes.md"),
            format!("{ROOT}/core/src/odd:name/Colon: File.kt"),
            format!("{ROOT}/core/src/main/kotlin/sample/core/Analytics.kt"),
            format!("{ROOT}/core/Empty.kt"),
        ]
    );
    let lines: Vec<_> = answer.hits.iter().map(|h| (h.lines.start, h.lines.end)).collect();
    assert_eq!(lines, vec![(3, 12), (7, 9), (20, 30), (1, 1)]);
    // A markdown block with its own fence and a line that looks like the next result stays whole.
    assert_eq!(
        answer.hits[0].snippet.as_deref(),
        Some("# Retry\n\n```kotlin\nval p = RetryPolicy(3)\n```\n\nSee also:\n\n2. Not a result block")
    );
    assert_eq!(answer.hits[3].snippet, None);
}

#[tokio::test]
async fn search_while_indexing_is_partial() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    caller.reply("search_code", "synthetic_search_code_indexing");
    let answer = backend(&caller, &milvus).search(&repo(), &request(10)).await.unwrap();
    assert_eq!(answer.hits.len(), 1);
    assert!(answer.partial.as_deref().unwrap().contains("still indexing"));
}

#[tokio::test]
async fn search_reads_a_success_whose_snippet_quotes_an_error_or_status_text() {
    let milvus = FakeMilvus::start();
    for quoted in [
        "is not indexed. Please index it first",
        "has been lost (collection not found in Milvus)",
        "This codebase is still being indexed",
    ] {
        let text = format!(
            "Found 1 results for query: \"q\" in codebase '{ROOT}'\n\n1. Code snippet (rust) [sample-android]\n   Location: src/parse.rs:1-3\n   Rank: 1\n   Context: \n```rust\nconst TEXT: &str = \"{quoted}\";\n```\n"
        );
        let caller = FakeCaller::new();
        caller.reply_with("search_code", text_output(&text));

        let answer = backend(&caller, &milvus).search(&repo(), &request(10)).await.unwrap();

        assert_eq!(answer.hits.len(), 1, "{quoted}");
        assert!(answer.hits[0].snippet.as_deref().unwrap().contains(quoted), "{quoted}");
        assert_eq!(answer.partial, None, "{quoted}");
    }
}

#[tokio::test]
async fn search_keeps_only_this_repository_when_a_parent_folder_is_indexed() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    caller.reply("search_code", "synthetic_search_code_parent_indexed");

    let answer = backend(&caller, &milvus).search(&repo(), &request(10)).await.unwrap();

    let shape: Vec<_> = answer.hits.iter().map(|h| (h.rank, h.path.to_str().unwrap().to_string())).collect();
    assert_eq!(
        shape,
        vec![
            (0, format!("{ROOT}/core/src/main/kotlin/sample/core/RetryPolicy.kt")),
            (1, format!("{ROOT}/network/src/main/kotlin/sample/network/HostSelector.kt")),
        ]
    );
}

#[tokio::test]
async fn search_without_results_is_an_empty_answer() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    caller.reply("search_code", "synthetic_search_code_no_results");
    caller.reply("search_code", "synthetic_search_code_no_results_indexing");
    let backend = backend(&caller, &milvus);

    let settled = backend.search(&repo(), &request(10)).await.unwrap();
    assert!(settled.hits.is_empty());
    assert_eq!(settled.partial, None);

    let indexing = backend.search(&repo(), &request(10)).await.unwrap();
    assert!(indexing.hits.is_empty());
    assert!(indexing.partial.is_some());
}

#[tokio::test]
async fn search_applies_the_path_globs_of_the_request() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    for _ in 0..3 {
        caller.reply("search_code", "synthetic_search_code_three_results");
    }
    let backend = backend(&caller, &milvus);
    let names = |answer: uals_core::types::BackendAnswer| -> Vec<String> {
        answer.hits.iter().map(|h| h.path.file_name().unwrap().to_str().unwrap().to_string()).collect()
    };

    let mut by_glob = request(10);
    by_glob.paths = vec!["network/**/*.kt".into()];
    let answer = backend.search(&repo(), &by_glob).await.unwrap();
    assert_eq!(names(answer.clone()), vec!["HostSelector.kt", "ConfigLoader.kt"]);
    // Ranks follow the filtered list.
    assert_eq!(answer.hits.iter().map(|h| h.rank).collect::<Vec<_>>(), vec![0, 1]);

    let mut by_folder = request(10);
    by_folder.paths = vec!["core/".into()];
    assert_eq!(names(backend.search(&repo(), &by_folder).await.unwrap()), vec!["RetryPolicy.kt"]);

    let mut bad = request(10);
    bad.paths = vec!["[".into()];
    let error = backend.search(&repo(), &bad).await.unwrap_err();
    assert!(failed(error).contains("invalid path glob"));
    // The bad glob is rejected before the server is asked.
    assert_eq!(caller.tools_called().len(), 2);
}

// ---------------------------------------------------------------- search: errors

#[tokio::test]
async fn search_maps_every_error_text() {
    let milvus = FakeMilvus::start();
    type Case = (&'static str, fn(BackendError) -> String, &'static str);
    let cases: [Case; 7] = [
        ("synthetic_search_code_not_indexed", unavailable, "uals refresh"),
        ("synthetic_search_code_index_lost", unavailable, "full `uals refresh`"),
        ("synthetic_search_code_milvus_unreachable", unavailable, "ECONNREFUSED"),
        ("synthetic_search_code_path_missing", failed, "does not exist"),
        ("synthetic_search_code_error", failed, "Embedding request failed"),
        ("synthetic_search_code_collection_limit", failed, "collection limit"),
        ("synthetic_search_code_unknown_text", failed, "Something the server never says"),
    ];
    for (fixture, expect, needle) in cases {
        let caller = FakeCaller::new();
        caller.reply("search_code", fixture);
        let error = backend(&caller, &milvus).search(&repo(), &request(10)).await.unwrap_err();
        let text = expect(error);
        assert!(text.contains(needle), "{fixture}: `{text}` lacks `{needle}`");
    }
}

#[tokio::test]
async fn not_indexed_names_the_repository() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    caller.reply("search_code", "synthetic_search_code_not_indexed");
    let error = backend(&caller, &milvus).search(&repo(), &request(10)).await.unwrap_err();
    assert!(unavailable(error).contains(ROOT));
}

#[tokio::test]
async fn an_unparsable_result_list_is_a_failure() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    caller.reply_with(
        "search_code",
        text_output(&format!("Found 2 results for query: \"q\" in codebase '{ROOT}'\n\n1. Code snippet (kotlin) [x]\n   Location: nonsense\n")),
    );
    let error = backend(&caller, &milvus).search(&repo(), &request(10)).await.unwrap_err();
    assert!(failed(error).contains("unparsable"));
}

#[tokio::test]
async fn a_reply_without_text_is_a_failure() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    caller.reply_with("search_code", Ok(ToolOutput::default()));
    let error = backend(&caller, &milvus).search(&repo(), &request(10)).await.unwrap_err();
    assert!(failed(error).contains("no text"));
}

#[tokio::test]
async fn recorded_milvus_crashes_are_unavailable() {
    let milvus = FakeMilvus::start();
    // `TransportClosed` as rmcp reported it when claude-context died without Milvus.
    let caller = FakeCaller::new();
    caller.reply("search_code", "milvus_down_search_code_after_1s");
    let error = backend(&caller, &milvus).search(&repo(), &request(10)).await.unwrap_err();
    assert!(unavailable(error).contains("Transport closed"));

    let caller = FakeCaller::new();
    caller.reply("get_indexing_status", "milvus_down_first_call_get_indexing_status");
    let state = backend(&caller, &milvus).index_state(&repo()).await;
    assert!(!state.indexed);
    assert!(state.detail.unwrap().contains("not running"));

    let caller = FakeCaller::new();
    caller.reply("get_indexing_status", "milvus_hung_first_call_get_indexing_status");
    let error = backend(&caller, &milvus).refresh(&repo(), false).await.unwrap_err();
    assert!(unavailable(error).contains("not running"));
}

#[tokio::test]
async fn errors_from_the_caller_pass_through() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    caller.reply_with("search_code", Err(BackendError::Unavailable("server gone".into())));
    caller.reply_with("search_code", Err(BackendError::Failed("protocol error".into())));
    let backend = backend(&caller, &milvus);
    assert_eq!(unavailable(backend.search(&repo(), &request(10)).await.unwrap_err()), "server gone");
    assert_eq!(failed(backend.search(&repo(), &request(10)).await.unwrap_err()), "protocol error");
}

// ---------------------------------------------------------------- Milvus probe

#[tokio::test]
async fn probe_reaches_a_listening_port_and_not_a_closed_one() {
    let milvus = FakeMilvus::start();
    let timeout = std::time::Duration::from_millis(300);
    assert!(probe_tcp(&milvus.address, timeout).await.is_ok());
    assert!(probe_tcp(&closed_address(), timeout).await.is_err());
}

#[tokio::test]
async fn nothing_is_called_while_milvus_is_down() {
    let address = closed_address();
    let caller = FakeCaller::new();
    let backend = ClaudeContextBackend::new(caller.clone(), config_with(&address));
    let expected = format!("Milvus is not reachable at {address}");

    assert!(unavailable(backend.search(&repo(), &request(10)).await.unwrap_err()).starts_with(&expected));
    let state = backend.index_state(&repo()).await;
    assert!(!state.indexed);
    assert!(state.detail.unwrap().starts_with(&expected));
    assert!(unavailable(backend.warm(&[repo()]).await.unwrap_err()).starts_with(&expected));
    assert!(unavailable(backend.refresh(&repo(), true).await.unwrap_err()).starts_with(&expected));
    assert!(unavailable(backend.refresh(&repo(), false).await.unwrap_err()).starts_with(&expected));

    assert_eq!(caller.calls(), Vec::new());
}

#[test]
fn milvus_address_comes_from_the_config_with_a_default() {
    let address = |value: Option<&str>| {
        let mut config = ClaudeContextConfig::default();
        if let Some(value) = value {
            config.env.insert("MILVUS_ADDRESS".into(), value.into());
        }
        ClaudeContextBackend::new(FakeCaller::new(), config).milvus_address()
    };
    assert_eq!(address(None), "127.0.0.1:19530");
    assert_eq!(address(Some("")), "127.0.0.1:19530");
    assert_eq!(address(Some("10.0.0.5:19531")), "10.0.0.5:19531");
    assert_eq!(address(Some("milvus.local")), "milvus.local:19530");
    assert_eq!(address(Some("http://milvus.local:19530/path")), "milvus.local:19530");
    assert_eq!(address(Some("https://cluster.example")), "cluster.example:443");
    assert_eq!(address(Some("[::1]:19530")), "[::1]:19530");
}

// ---------------------------------------------------------------- index_state

#[tokio::test]
async fn index_state_maps_every_status_text() {
    let milvus = FakeMilvus::start();
    struct Case {
        fixture: &'static str,
        indexed: bool,
        detail: &'static str,
    }
    let cases = [
        Case { fixture: "synthetic_get_indexing_status_indexed", indexed: true, detail: "7 files, 12 chunks" },
        Case { fixture: "synthetic_get_indexing_status_indexing", indexed: false, detail: "indexing 40%" },
        Case {
            fixture: "synthetic_get_indexing_status_failed",
            indexed: false,
            detail: "indexing failed: Embedding provider unreachable",
        },
        Case { fixture: "synthetic_get_indexing_status_not_indexed", indexed: false, detail: "not indexed" },
        Case {
            fixture: "synthetic_get_indexing_status_covered",
            indexed: true,
            detail: "21 files, 40 chunks; covered by the index of /work",
        },
        Case {
            fixture: "synthetic_get_indexing_status_error",
            indexed: false,
            detail: "claude-context status failed: Error getting indexing status",
        },
    ];
    for case in cases {
        let caller = FakeCaller::new();
        caller.reply("get_indexing_status", case.fixture);
        let state = backend(&caller, &milvus).index_state(&repo()).await;
        assert_eq!(state.backend, BackendId::ClaudeContext, "{}", case.fixture);
        assert_eq!(state.repo, "android");
        assert_eq!(state.indexed, case.indexed, "{}", case.fixture);
        assert_eq!(state.stale, None);
        assert!(state.detail.as_deref().unwrap().starts_with(case.detail), "{}: {:?}", case.fixture, state.detail);
        assert_eq!(caller.calls(), vec![("get_indexing_status".to_string(), json!({"path": ROOT}))]);
    }
}

// ---------------------------------------------------------------- warm and refresh

#[tokio::test]
async fn warm_indexes_only_repositories_without_an_index() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    // android: not indexed -> index; web: indexed; tools: being indexed; broken: failed before -> retry.
    caller.reply("get_indexing_status", "synthetic_get_indexing_status_not_indexed");
    caller.reply("get_indexing_status", "synthetic_get_indexing_status_indexed");
    caller.reply("get_indexing_status", "synthetic_get_indexing_status_indexing");
    caller.reply("get_indexing_status", "synthetic_get_indexing_status_failed");
    caller.reply("index_codebase", "synthetic_index_codebase_started");
    caller.reply("index_codebase", "synthetic_index_codebase_started");
    let repos: Vec<Repo> = ["android", "web", "tools", "broken"]
        .iter()
        .map(|name| Repo { name: (*name).into(), root: PathBuf::from(format!("/work/{name}")) })
        .collect();

    backend(&caller, &milvus).warm(&repos).await.unwrap();

    let index_calls: Vec<_> = caller.calls().into_iter().filter(|(tool, _)| tool == "index_codebase").collect();
    assert_eq!(index_calls.len(), 2);
    let config = ClaudeContextConfig::default();
    assert_eq!(
        index_calls[0].1,
        json!({
            "path": "/work/android",
            "force": false,
            "splitter": "ast",
            "ignorePatterns": config.ignore_patterns,
            "customExtensions": config.custom_extensions,
        })
    );
    assert_eq!(index_calls[1].1["path"], json!("/work/broken"));
}

#[tokio::test]
async fn warm_treats_already_indexed_and_already_indexing_as_success() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    caller.reply("get_indexing_status", "synthetic_get_indexing_status_not_indexed");
    caller.reply("index_codebase", "synthetic_index_codebase_already_indexing");
    caller.reply("get_indexing_status", "synthetic_get_indexing_status_not_indexed");
    caller.reply("index_codebase", "synthetic_index_codebase_already_indexed");
    let repos = [repo(), Repo { name: "web".into(), root: PathBuf::from("/work/web") }];
    backend(&caller, &milvus).warm(&repos).await.unwrap();
}

#[tokio::test]
async fn warm_fails_only_when_every_repository_fails() {
    let milvus = FakeMilvus::start();
    let repos = [repo(), Repo { name: "web".into(), root: PathBuf::from("/work/web") }];

    let caller = FakeCaller::new();
    caller.reply("get_indexing_status", "synthetic_get_indexing_status_not_indexed");
    caller.reply("index_codebase", "synthetic_index_codebase_failed");
    caller.reply("get_indexing_status", "synthetic_get_indexing_status_indexed");
    assert!(backend(&caller, &milvus).warm(&repos).await.is_ok());

    let caller = FakeCaller::new();
    caller.reply("get_indexing_status", "synthetic_get_indexing_status_not_indexed");
    caller.reply("index_codebase", "synthetic_index_codebase_failed");
    caller.reply("get_indexing_status", "synthetic_get_indexing_status_not_indexed");
    caller.reply("index_codebase", "synthetic_index_codebase_milvus_unreachable");
    let error = backend(&caller, &milvus).warm(&repos).await.unwrap_err();
    assert!(failed(error).contains("android"));
}

#[tokio::test]
async fn warm_without_repositories_calls_nothing() {
    let caller = FakeCaller::new();
    let backend = ClaudeContextBackend::new(caller.clone(), config_with(&closed_address()));
    backend.warm(&[]).await.unwrap();
    assert_eq!(caller.calls(), Vec::new());
}

#[tokio::test]
async fn full_refresh_reindexes_with_force() {
    let milvus = FakeMilvus::start();
    let caller = FakeCaller::new();
    caller.reply("index_codebase", "synthetic_index_codebase_started");

    backend(&caller, &milvus).refresh(&repo(), true).await.unwrap();

    let calls = caller.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "index_codebase");
    assert_eq!(calls[0].1["force"], json!(true));
    assert_eq!(calls[0].1["path"], json!(ROOT));
    assert_eq!(calls[0].1["splitter"], json!("ast"));
}

#[tokio::test]
async fn incremental_refresh_of_an_indexed_repository_touches_the_sync_trigger() {
    let milvus = FakeMilvus::start();
    let context = tempfile::tempdir().unwrap();
    let caller = FakeCaller::new();
    caller.reply("get_indexing_status", "synthetic_get_indexing_status_indexed");
    let backend = backend(&caller, &milvus).with_context_dir(context.path());

    backend.refresh(&repo(), false).await.unwrap();

    assert_eq!(caller.tools_called(), vec!["get_indexing_status"]);
    let trigger = std::fs::read_to_string(context.path().join(".sync-trigger")).unwrap();
    assert!(trigger.parse::<u128>().unwrap() > 0);
}

#[tokio::test]
async fn incremental_refresh_indexes_a_repository_that_has_no_index() {
    let milvus = FakeMilvus::start();
    let context = tempfile::tempdir().unwrap();
    let caller = FakeCaller::new();
    caller.reply("get_indexing_status", "synthetic_get_indexing_status_not_indexed");
    caller.reply("index_codebase", "synthetic_index_codebase_started");

    backend(&caller, &milvus).with_context_dir(context.path()).refresh(&repo(), false).await.unwrap();

    let calls = caller.calls();
    assert_eq!(calls[1].0, "index_codebase");
    assert_eq!(calls[1].1["force"], json!(false));
    assert!(!context.path().join(".sync-trigger").exists());
}

#[tokio::test]
async fn incremental_refresh_leaves_a_running_index_alone() {
    let milvus = FakeMilvus::start();
    let context = tempfile::tempdir().unwrap();
    let caller = FakeCaller::new();
    caller.reply("get_indexing_status", "synthetic_get_indexing_status_indexing");

    backend(&caller, &milvus).with_context_dir(context.path()).refresh(&repo(), false).await.unwrap();

    assert_eq!(caller.tools_called(), vec!["get_indexing_status"]);
    assert!(!context.path().join(".sync-trigger").exists());
}
