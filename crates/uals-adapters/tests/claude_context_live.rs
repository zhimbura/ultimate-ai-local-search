//! Live test against a real claude-context server. Ignored by default (`cargo test -- --ignored`
//! runs it); it fails when Milvus does not answer, the entry point is not installed or no
//! embedding service indexes the fixture.
//!
//! The server runs with `HOME` set to a temporary folder, so the test never reads or rewrites the
//! user's `~/.context` (snapshot, sync lock) and cannot race another claude-context process. The
//! embedding service comes from the environment the test runs in (`EMBEDDING_PROVIDER`, API keys).
//! It indexes a temporary copy of the public fixture `tests/fixtures/workspace/sample-android`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use process_wrap::tokio::{CommandWrap, KillOnDrop};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use serde_json::{Value, json};
use uals_adapters::claude_context::{ClaudeContextBackend, MILVUS_PROBE_TIMEOUT, probe_tcp};
use uals_core::backend::{BackendError, McpToolCaller, SearchBackend, ToolOutput};
use uals_core::config::ClaudeContextConfig;
use uals_core::types::{QueryKind, Repo, SearchRequest};

/// A minimal caller over rmcp (the real `McpChild` belongs to another track).
struct RmcpCaller {
    client: RunningService<RoleClient, ()>,
}

#[async_trait]
impl McpToolCaller for RmcpCaller {
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolOutput, BackendError> {
        let mut params = CallToolRequestParams::new(name.to_string());
        params.arguments = arguments.as_object().cloned();
        let result: CallToolResult =
            self.client.call_tool(params).await.map_err(|e| BackendError::Failed(format!("{e}")))?;
        // Read the result through its JSON form: `content[].text`, `isError`, `structuredContent`.
        let json = serde_json::to_value(&result).map_err(|e| BackendError::Failed(e.to_string()))?;
        let texts = json["content"]
            .as_array()
            .map(|items| items.iter().filter_map(|item| item["text"].as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        Ok(ToolOutput {
            texts,
            structured: json.get("structuredContent").cloned(),
            is_error: json["isError"].as_bool().unwrap_or(false),
        })
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[tokio::test]
#[ignore = "live: needs Milvus, the claude-context entry point, node and an embedding service"]
async fn index_search_and_clear_against_a_live_server() {
    let mut config = ClaudeContextConfig::default();
    let address = std::env::var("MILVUS_ADDRESS").unwrap_or_else(|_| "127.0.0.1:19530".into());
    config.env.insert("MILVUS_ADDRESS".into(), address.clone());
    let probe_address = ClaudeContextBackend::new(Arc::new(NoCaller), config.clone()).milvus_address();
    if let Err(why) = probe_tcp(&probe_address, MILVUS_PROBE_TIMEOUT).await {
        panic!("live test needs Milvus: not reachable at {probe_address} ({why})");
    }
    let entry = config
        .entry_path()
        .filter(|entry| entry.exists())
        .expect("live test needs the claude-context entry point to be installed");
    let node = which::which(&config.node).unwrap_or_else(|_| panic!("live test needs `{}` on PATH", config.node));

    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace/sample-android");
    let root = workspace.path().canonicalize().unwrap().join("sample-android");
    copy_dir(&fixture, &root);

    let mut command = CommandWrap::with_new(node, |c| {
        c.arg(&entry);
        c.env("HOME", home.path());
        c.env("MILVUS_ADDRESS", &address);
    });
    command.wrap(KillOnDrop);
    let (transport, stderr) = TokioChildProcess::builder(command).stderr(std::process::Stdio::piped()).spawn().unwrap();
    if let Some(stderr) = stderr {
        // Always drain stderr: an unread pipe blocks the server.
        tokio::spawn(async move {
            let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stderr));
            while let Ok(Some(_)) = lines.next_line().await {}
        });
    }
    let client = tokio::time::timeout(Duration::from_secs(10), ().serve(transport))
        .await
        .expect("initialize timed out")
        .expect("initialize failed");
    let caller = Arc::new(RmcpCaller { client });
    let backend = ClaudeContextBackend::new(caller.clone(), config).with_context_dir(home.path().join(".context"));
    let repo = Repo { name: "sample-android".into(), root: root.clone() };

    backend.warm(std::slice::from_ref(&repo)).await.expect("warm");
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let state = backend.index_state(&repo).await;
        if state.indexed {
            break;
        }
        let detail = state.detail.unwrap_or_default();
        assert!(
            !detail.starts_with("indexing failed"),
            "indexing failed, is an embedding service configured? ({detail})"
        );
        assert!(Instant::now() < deadline, "indexing did not finish in 180 s (last state: {detail})");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let request = SearchRequest {
        query: "retry with exponential backoff".into(),
        kind: QueryKind::Natural,
        fetch_limit: 10,
        paths: Vec::new(),
    };
    let answer = backend.search(&repo, &request).await.expect("search");
    assert!(!answer.hits.is_empty(), "no hits for the query");
    for (position, hit) in (0u32..).zip(&answer.hits) {
        assert_eq!(hit.rank, position);
        assert!(hit.path.starts_with(&root) && hit.path.exists(), "bad path {}", hit.path.display());
        assert!(hit.lines.start >= 1 && hit.lines.start <= hit.lines.end);
    }

    // Always remove the collection: it lives in Milvus, outside the temporary folders.
    let cleared = caller.call_tool("clear_index", json!({"path": root})).await;
    eprintln!("clear_index: {cleared:?}");
}

/// A caller that is never used: only to read the Milvus address from a config.
struct NoCaller;

#[async_trait]
impl McpToolCaller for NoCaller {
    async fn call_tool(&self, _name: &str, _arguments: Value) -> Result<ToolOutput, BackendError> {
        Err(BackendError::Failed("not used".into()))
    }
}
