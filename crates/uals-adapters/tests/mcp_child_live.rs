//! `McpChild` against the real `semble` on a temp copy of the public fixture workspace.
//! The tests that need Semble are `#[ignore]`d; run them with `-- --ignored`. They panic when
//! `semble` or `git` is not on PATH. Semble's cache is redirected to the temp folder with
//! `SEMBLE_CACHE_LOCATION`, so nothing is left in the user's cache.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tempfile::TempDir;
use uals_adapters::mcp_child::{ChildSpec, McpChild};
use uals_core::backend::{BackendError, McpToolCaller, ToolOutput};

const PROJECTS: [&str; 3] = ["sample-android", "sample-tools", "sample-web"];

struct Live {
    _dir: TempDir,
    workspace: PathBuf,
    spec: ChildSpec,
}

/// The fixture workspace and the spec of the real Semble; panics when `semble` is not on PATH.
fn live() -> Live {
    let semble =
        which::which("semble").expect("live test needs `semble` on PATH (install it, e.g. `uv tool install semble`)");
    let dir = TempDir::new().unwrap();
    let workspace = dir.path().join("workspace");
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace"), &workspace);
    for project in PROJECTS {
        let status = Command::new("git").args(["init", "-q"]).current_dir(workspace.join(project)).status().unwrap();
        assert!(status.success());
    }
    let cache = dir.path().join("semble-cache");
    std::fs::create_dir_all(&cache).unwrap();
    let spec = ChildSpec {
        name: "semble".into(),
        program: semble,
        args: vec![],
        env: BTreeMap::from([("SEMBLE_CACHE_LOCATION".to_owned(), cache.display().to_string())]),
        pass_env: ["HF_HOME", "HF_ENDPOINT", "HF_HUB_OFFLINE", "XDG_CACHE_HOME"].map(String::from).to_vec(),
        cwd: None,
    };
    Live { _dir: dir, workspace, spec }
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

fn search_args(live: &Live, project: &str, query: &str) -> Value {
    json!({
        "query": query,
        "repo": live.workspace.join(project).display().to_string(),
        "top_k": 3,
        "max_snippet_lines": 0,
    })
}

fn result_paths(output: &ToolOutput) -> Vec<String> {
    assert!(!output.is_error, "{output:?}");
    let inner = output.structured.as_ref().unwrap()["result"].as_str().unwrap();
    let value: Value = serde_json::from_str(inner).unwrap();
    value["results"].as_array().unwrap().iter().map(|r| r["file_path"].as_str().unwrap().to_owned()).collect()
}

fn group_is_gone(pgid: i32) -> bool {
    killpg(Pid::from_raw(pgid), None) == Err(Errno::ESRCH)
}

async fn wait_until(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs semble and git on PATH"]
async fn spawns_searches_cold_and_warm() {
    let live = live();
    let started = Instant::now();
    let child = McpChild::spawn(live.spec.clone()).await.unwrap();
    let spawn_time = started.elapsed();

    let started = Instant::now();
    let cold = child
        .call_tool("search", search_args(&live, "sample-android", "retry with exponential backoff"))
        .await
        .unwrap();
    let cold_time = started.elapsed();
    let paths = result_paths(&cold);
    assert_eq!(paths.first().map(String::as_str), Some("core/src/main/kotlin/sample/core/RetryPolicy.kt"), "{paths:?}");

    let started = Instant::now();
    let warm = child.call_tool("search", search_args(&live, "sample-android", "host selection")).await.unwrap();
    let warm_time = started.elapsed();
    assert!(!result_paths(&warm).is_empty());
    eprintln!("MEASURED spawn+handshake {spawn_time:?}, cold search {cold_time:?}, warm search {warm_time:?}");
    assert!(warm_time < cold_time || warm_time < Duration::from_millis(500));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs semble and git on PATH"]
async fn semble_failures_come_back_as_text_not_as_errors() {
    let live = live();
    let child = McpChild::spawn(live.spec.clone()).await.unwrap();
    let missing = live.workspace.join("does-not-exist");
    let output = child.call_tool("search", json!({"query": "x", "repo": missing.display().to_string()})).await.unwrap();
    assert!(!output.is_error);
    assert!(output.texts[0].starts_with("Failed to index"), "{:?}", output.texts);

    let output = child.call_tool("search", json!({"query": "x"})).await.unwrap();
    assert!(output.is_error, "missing `repo` is a validation error: {output:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs semble and git on PATH"]
async fn concurrent_calls_on_one_connection() {
    let live = live();
    let child = Arc::new(McpChild::spawn(live.spec.clone()).await.unwrap());
    let started = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    for (project, query) in [
        ("sample-android", "retry with exponential backoff"),
        ("sample-tools", "cache of computed values"),
        ("sample-web", "fetch with retry"),
        ("sample-android", "HostSelector"),
    ] {
        let child = Arc::clone(&child);
        let args = search_args(&live, project, query);
        tasks.spawn(async move { (project, child.call_tool("search", args).await) });
    }
    while let Some(joined) = tasks.join_next().await {
        let (project, result) = joined.unwrap();
        assert!(!result_paths(&result.unwrap()).is_empty(), "{project}");
    }
    eprintln!("MEASURED 4 concurrent cold searches {:?}", started.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs semble and git on PATH"]
async fn a_timed_out_call_leaves_the_connection_usable() {
    let live = live();
    let child = McpChild::spawn(live.spec.clone()).await.unwrap();
    let pgid = child.process_group();

    // The first search loads the model and builds the index: far slower than 20 ms.
    let args = search_args(&live, "sample-android", "retry with exponential backoff");
    let timed_out = tokio::time::timeout(Duration::from_millis(20), child.call_tool("search", args.clone())).await;
    assert!(timed_out.is_err());

    let started = Instant::now();
    let output = child.call_tool("search", args).await.unwrap();
    eprintln!("MEASURED retry after the dropped call {:?}", started.elapsed());
    assert!(!result_paths(&output).is_empty());
    assert_eq!(child.process_group(), pgid, "no respawn");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs semble and git on PATH"]
async fn dropping_kills_the_process_group() {
    let live = live();
    let child = McpChild::spawn(live.spec.clone()).await.unwrap();
    let pgid = child.process_group().unwrap();
    child.call_tool("search", search_args(&live, "sample-tools", "cache")).await.unwrap();
    assert!(!group_is_gone(pgid));

    let started = Instant::now();
    drop(child);
    wait_until("the process group to be gone", Duration::from_secs(5), || group_is_gone(pgid)).await;
    eprintln!("MEASURED drop to group gone {:?}", started.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs semble and git on PATH"]
async fn a_crashed_semble_is_respawned_and_finds_its_index_on_disk() {
    let live = live();
    let child = McpChild::spawn(live.spec.clone()).await.unwrap();
    let args = search_args(&live, "sample-android", "retry with exponential backoff");
    child.call_tool("search", args.clone()).await.unwrap();
    let first = child.process_group().unwrap();

    killpg(Pid::from_raw(first), Signal::SIGKILL).unwrap();
    let started = Instant::now();
    let output = child.call_tool("search", args).await.unwrap();
    eprintln!("MEASURED call after a crash (respawn + search) {:?}", started.elapsed());
    assert!(!result_paths(&output).is_empty());
    let second = child.process_group().unwrap();
    assert_ne!(first, second);
    wait_until("the crashed group to be gone", Duration::from_secs(5), || group_is_gone(first)).await;
}

#[tokio::test]
async fn a_missing_binary_is_unavailable() {
    let mut spec = ChildSpec {
        name: "semble".into(),
        program: "uals-no-such-semble".into(),
        args: vec![],
        env: BTreeMap::new(),
        pass_env: vec![],
        cwd: None,
    };
    let error = McpChild::spawn(spec.clone()).await.err().unwrap();
    assert!(matches!(error, BackendError::Unavailable(_)), "{error:?}");
    spec.program = "/no/such/dir/semble".into();
    let error = McpChild::spawn(spec).await.err().unwrap();
    assert!(matches!(error, BackendError::Unavailable(_)), "{error:?}");
}
