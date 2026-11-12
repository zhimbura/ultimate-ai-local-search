//! `McpChild` against a tiny fake MCP server written in `sh` (no Semble needed, runs in CI).
//! The fake answers `initialize` and `tools/call`; its behaviour is chosen by the tool name:
//! `ping`, `slow` (0.4 s), `env`, `multi`, `fail`, `crash` (kills itself with SIGKILL).
//! Every start appends a line to `$FAKE_COUNT_FILE`; with `$FAKE_FAIL_AFTER=N` starts number N+1
//! and later print "boom" on stderr and exit.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use uals_adapters::mcp_child::{ChildSpec, McpChild};
use uals_core::backend::{BackendError, McpToolCaller};

const FAKE_SERVER: &str = r#"#!/bin/sh
echo start >> "$FAKE_COUNT_FILE"
n=$(( $(wc -l < "$FAKE_COUNT_FILE") ))
if [ -n "$FAKE_FAIL_AFTER" ] && [ "$n" -gt "$FAKE_FAIL_AFTER" ]; then
  echo "boom: start number $n" >&2
  exit 1
fi
echo "fake server $n is up" >&2
# A helper in the same process group, like Semble's resource tracker.
sleep 300 >/dev/null 2>&1 </dev/null &
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      reply "$id" '{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}' ;;
    *'"method":"tools/call"'*)
      tool=$(printf '%s' "$line" | sed -n 's/.*"name":"\([a-z_]*\)".*/\1/p')
      case "$tool" in
        ping) reply "$id" '{"content":[{"type":"text","text":"pong"}],"isError":false}' ;;
        slow) sleep 0.4; reply "$id" '{"content":[{"type":"text","text":"pong"}],"isError":false}' ;;
        multi) reply "$id" '{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}],"structuredContent":{"n":1},"isError":false}' ;;
        fail) reply "$id" '{"content":[{"type":"text","text":"bad"}],"isError":true}' ;;
        env) dump=$(env | LC_ALL=C sort | tr -d '"\\' | tr '\n' ';')
             reply "$id" "{\"content\":[{\"type\":\"text\",\"text\":\"$dump\"}],\"isError\":false}" ;;
        crash) kill -9 $$ ;;
        *) reply "$id" '{"content":[{"type":"text","text":"unknown tool"}],"isError":true}' ;;
      esac ;;
  esac
done
"#;

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let script = dir.path().join("fake-mcp-server");
        std::fs::write(&script, FAKE_SERVER).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn count_file(&self) -> PathBuf {
        self.dir.path().join("starts")
    }

    fn starts(&self) -> usize {
        std::fs::read_to_string(self.count_file()).map(|s| s.lines().count()).unwrap_or(0)
    }

    fn spec(&self, extra_env: &[(&str, &str)]) -> ChildSpec {
        let mut env = BTreeMap::from([("FAKE_COUNT_FILE".to_owned(), self.count_file().display().to_string())]);
        env.extend(extra_env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
        ChildSpec {
            name: "fake".into(),
            program: self.dir.path().join("fake-mcp-server"),
            args: vec![],
            env,
            pass_env: vec![],
            cwd: None,
        }
    }

    async fn spawn(&self, extra_env: &[(&str, &str)]) -> McpChild {
        McpChild::spawn(self.spec(extra_env)).await.unwrap()
    }
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

async fn ping(child: &McpChild) -> Result<Vec<String>, BackendError> {
    child.call_tool("ping", json!({})).await.map(|out| out.texts)
}

#[tokio::test]
async fn maps_results_to_tool_output() {
    let fx = Fixture::new();
    let child = fx.spawn(&[]).await;

    let pong = child.call_tool("ping", json!({})).await.unwrap();
    assert_eq!((pong.texts, pong.structured, pong.is_error), (vec!["pong".to_owned()], None, false));

    let multi = child.call_tool("multi", serde_json::Value::Null).await.unwrap();
    assert_eq!(multi.texts, vec!["a", "b"]);
    assert_eq!(multi.structured, Some(json!({"n": 1})));
    assert!(!multi.is_error);

    let fail = child.call_tool("fail", json!({"x": 1})).await.unwrap();
    assert!(fail.is_error);
    assert_eq!(fail.texts, vec!["bad"]);
    assert_eq!(fx.starts(), 1);
}

#[tokio::test]
async fn arguments_must_be_an_object_or_null() {
    let fx = Fixture::new();
    let child = fx.spawn(&[]).await;
    let error = child.call_tool("ping", json!([1, 2])).await.unwrap_err();
    assert!(matches!(error, BackendError::Failed(_)), "{error:?}");
    assert_eq!(ping(&child).await.unwrap(), vec!["pong"]);
}

#[tokio::test]
async fn child_gets_a_clean_environment() {
    let fx = Fixture::new();
    let mut spec = fx.spec(&[("UALS_LITERAL", "yes")]);
    // `CARGO_PKG_NAME` and `CARGO_MANIFEST_DIR` are set by cargo for every test run.
    spec.pass_env = vec!["CARGO_PKG_NAME".into(), "UALS_SURELY_UNSET".into()];
    let child = McpChild::spawn(spec).await.unwrap();

    let dump = child.call_tool("env", json!({})).await.unwrap().texts.remove(0);
    let vars: Vec<&str> = dump.split(';').filter(|v| !v.is_empty()).collect();
    let has = |prefix: &str| vars.iter().any(|v| v.starts_with(prefix));
    assert!(has("PATH="), "{dump}");
    assert!(has("HOME="), "{dump}");
    assert!(has("UALS_LITERAL=yes"), "{dump}");
    assert!(has("CARGO_PKG_NAME=uals-adapters"), "{dump}");
    assert!(has("FAKE_COUNT_FILE="), "{dump}");
    assert!(!has("CARGO_MANIFEST_DIR="), "the parent's environment leaked: {dump}");
    assert!(!has("UALS_SURELY_UNSET"), "{dump}");
}

#[tokio::test]
async fn concurrent_calls_share_one_connection() {
    let fx = Fixture::new();
    let child = Arc::new(fx.spawn(&[]).await);
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..8 {
        let child = Arc::clone(&child);
        tasks.spawn(async move {
            let tool = if i % 2 == 0 { "ping" } else { "multi" };
            child.call_tool(tool, json!({})).await
        });
    }
    let mut done = 0;
    while let Some(result) = tasks.join_next().await {
        assert!(!result.unwrap().unwrap().texts.is_empty());
        done += 1;
    }
    assert_eq!(done, 8);
    assert_eq!(fx.starts(), 1, "one server for all calls");
}

#[tokio::test]
async fn dropping_a_call_keeps_the_connection_usable() {
    let fx = Fixture::new();
    let child = fx.spawn(&[]).await;
    let pgid = child.process_group();

    let timed_out = tokio::time::timeout(Duration::from_millis(100), child.call_tool("slow", json!({}))).await;
    assert!(timed_out.is_err(), "the slow call should have hit the timeout");

    // The server is still busy with the abandoned call; its late answer must not confuse the next one.
    assert_eq!(ping(&child).await.unwrap(), vec!["pong"]);
    assert_eq!(ping(&child).await.unwrap(), vec!["pong"]);
    assert_eq!(fx.starts(), 1, "no respawn");
    assert_eq!(child.process_group(), pgid);
}

#[tokio::test]
async fn a_crash_during_a_call_is_replayed_once_then_reported() {
    let fx = Fixture::new();
    let child = fx.spawn(&[]).await;
    let first = child.process_group().unwrap();

    let error = child.call_tool("crash", json!({})).await.unwrap_err();
    let BackendError::Unavailable(message) = error else { panic!("expected Unavailable, got {error:?}") };
    assert!(message.contains("fake server 2 is up"), "stderr tail is part of the message: {message}");
    assert_eq!(fx.starts(), 2, "the call was replayed once on a fresh server and not more");

    // The next call respawns again and works.
    assert_eq!(ping(&child).await.unwrap(), vec!["pong"]);
    assert_eq!(fx.starts(), 3);
    assert_ne!(child.process_group(), Some(first));
    wait_until("the first group to be gone", Duration::from_secs(3), || group_is_gone(first)).await;
}

#[tokio::test]
async fn a_server_killed_from_outside_is_respawned_by_the_next_call() {
    let fx = Fixture::new();
    let child = fx.spawn(&[]).await;
    let first = child.process_group().unwrap();
    assert_eq!(ping(&child).await.unwrap(), vec!["pong"]);

    killpg(Pid::from_raw(first), Signal::SIGKILL).unwrap();
    assert_eq!(ping(&child).await.unwrap(), vec!["pong"]);
    assert_eq!(fx.starts(), 2);
    let second = child.process_group().unwrap();
    assert_ne!(second, first);
    wait_until("the first group to be gone", Duration::from_secs(3), || group_is_gone(first)).await;
}

#[tokio::test]
async fn failures_back_off_and_report_unavailable() {
    let fx = Fixture::new();
    // Only the first start works.
    let child = fx.spawn(&[("FAKE_FAIL_AFTER", "1")]).await;
    killpg(Pid::from_raw(child.process_group().unwrap()), Signal::SIGKILL).unwrap();

    // The dead server is not a failure by itself; each failed start is one. Three of them back off 1 s.
    for start in 2..=4 {
        let error = ping(&child).await.unwrap_err();
        let BackendError::Unavailable(message) = error else { panic!("expected Unavailable, got {error:?}") };
        assert!(message.contains(&format!("boom: start number {start}")), "{message}");
        assert!(!message.contains("next start in"), "start {start} must be attempted: {message}");
    }
    assert_eq!(fx.starts(), 4);
    assert_eq!(child.process_group(), None);

    // While backing off no server is started.
    for _ in 0..3 {
        let error = ping(&child).await.unwrap_err();
        let BackendError::Unavailable(message) = error else { panic!("expected Unavailable, got {error:?}") };
        assert!(message.contains("3 consecutive failures"), "{message}");
        assert!(message.contains("next start in"), "{message}");
    }
    assert_eq!(fx.starts(), 4);

    // After 1 s one more attempt is made (and fails), then the backoff doubles to 2 s.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let error = ping(&child).await.unwrap_err();
    let BackendError::Unavailable(message) = error else { panic!("expected Unavailable, got {error:?}") };
    assert!(message.contains("boom: start number 5"), "{message}");
    assert_eq!(fx.starts(), 5);
    let error = ping(&child).await.unwrap_err();
    let BackendError::Unavailable(message) = error else { panic!("expected Unavailable, got {error:?}") };
    assert!(message.contains("4 consecutive failures"), "{message}");
}

#[tokio::test]
async fn a_server_that_dies_on_every_call_backs_off_after_three_calls() {
    let fx = Fixture::new();
    let child = fx.spawn(&[]).await;

    // Each call hits a dead server, replays once on a fresh one that dies too: one failure per call.
    for _ in 0..3 {
        let error = child.call_tool("crash", json!({})).await.unwrap_err();
        assert!(matches!(error, BackendError::Unavailable(_)), "{error:?}");
    }
    assert_eq!(fx.starts(), 6, "the first call crashes the running server, the next two start two each");

    let error = ping(&child).await.unwrap_err();
    let BackendError::Unavailable(message) = error else { panic!("expected Unavailable, got {error:?}") };
    assert!(message.contains("3 consecutive failures"), "{message}");
    assert_eq!(fx.starts(), 6, "no server is started while backing off");
}

#[tokio::test]
async fn dropping_the_child_kills_the_whole_process_group() {
    let fx = Fixture::new();
    let child = fx.spawn(&[]).await;
    let pgid = child.process_group().unwrap();
    assert_eq!(ping(&child).await.unwrap(), vec!["pong"]);
    assert!(!group_is_gone(pgid));

    drop(child);
    // The helper `sleep 300` lives in the same group, so the group only disappears when all of it died.
    wait_until("the group to be gone", Duration::from_secs(3), || group_is_gone(pgid)).await;
}

#[tokio::test]
async fn spawn_errors_are_reported_without_hanging() {
    let fx = Fixture::new();

    let mut spec = fx.spec(&[]);
    spec.program = Path::new("/definitely/not/here/server").into();
    let error = McpChild::spawn(spec).await.err().unwrap();
    assert!(matches!(error, BackendError::Unavailable(_)), "{error:?}");

    let mut spec = fx.spec(&[]);
    spec.program = "uals-no-such-program-anywhere".into();
    let error = McpChild::spawn(spec).await.err().unwrap();
    let BackendError::Unavailable(message) = error else { panic!("expected Unavailable, got {error:?}") };
    assert!(message.contains("not installed"), "{message}");

    // Starts, complains on stderr and exits before the handshake.
    let mut spec = fx.spec(&[]);
    spec.program = "sh".into();
    spec.args = vec!["-c".into(), "echo 'cannot load model' >&2; exit 3".into()];
    let started = Instant::now();
    let error = McpChild::spawn(spec).await.err().unwrap();
    let BackendError::Unavailable(message) = error else { panic!("expected Unavailable, got {error:?}") };
    assert!(message.contains("cannot load model"), "{message}");
    assert!(started.elapsed() < Duration::from_secs(5), "must not wait for the 10 s handshake deadline");
}
