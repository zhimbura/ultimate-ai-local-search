//! `uals mcp` end to end: the real binary on stdin and stdout, on a copy of the public fixture
//! workspace. ast-index is not installed for the server, so `search` runs on the lexical backend
//! and a fake Semble: an MCP server in `sh` that finds `HostSelector.kt` lines 5-14 in
//! `sample-android` and nothing elsewhere, or never answers a search with `FAKE_SEMBLE_MODE=hang`.
//!
//! The tool tests talk to the server through `McpChild`, an rmcp client. The stop tests drive the
//! server's pipes by hand, so that the client can close stdin or keep it open.
#![cfg(unix)]

#[path = "../../uals-adapters/tests/lexical_support/mod.rs"]
mod lexical_support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use lexical_support::fixture_copy;
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use tempfile::TempDir;
use uals_adapters::mcp_child::{ChildSpec, McpChild};
use uals_core::backend::McpToolCaller;
use uals_core::config::OutputConfig;
use uals_core::types::{BackendId, BackendStatus, LineRange, SearchResponse, UsagesResponse};

const FAKE_SEMBLE: &str = r#"#!/bin/sh
echo $$ >> "$FAKE_SEMBLE_PIDS"
# A helper in the same process group, like Semble's resource tracker.
sleep 300 >/dev/null 2>&1 </dev/null &
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
found='[{\"file_path\":\"network/src/main/kotlin/sample/network/HostSelector.kt\",\"start_line\":5,\"end_line\":14,\"score\":0.9}]'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      reply "$id" '{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fake-semble","version":"0"}}' ;;
    *'"method":"tools/call"'*)
      [ "$FAKE_SEMBLE_MODE" = hang ] && continue
      case "$line" in
        *sample-android*) results=$found ;;
        *) results='[]' ;;
      esac
      reply "$id" "{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"results\\\":$results}\"}],\"isError\":false}" ;;
  esac
done
"#;

const HOST_SELECTOR: &str = "sample-android/network/src/main/kotlin/sample/network/HostSelector.kt";
const REPOS: [&str; 3] = ["sample-android", "sample-tools", "sample-web"];
/// How long a test waits for a process to start or end.
const PROCESS_BOUND: Duration = Duration::from_secs(10);

/// A copy of the fixture workspace, the fake Semble and a config that uses it.
struct Fixture {
    dir: TempDir,
    /// The canonical path of the workspace copy, as the server reports it.
    workspace: PathBuf,
}

impl Fixture {
    /// `semble_mode`: `answer` or `hang`.
    fn new(semble_mode: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let workspace = fixture_copy(dir.path()).canonicalize().unwrap();
        let script = dir.path().join("fake-semble");
        std::fs::write(&script, FAKE_SEMBLE).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        let fixture = Self { dir, workspace };
        let config = format!(
            r#"
[semantic.semble]
command = [{script:?}]
env = {{ FAKE_SEMBLE_MODE = "{semble_mode}", FAKE_SEMBLE_PIDS = {pids:?} }}

[ast_index]
binary = "/nonexistent/ast-index"
update_on_start = false

[deadlines]
semantic_ms = 1000
semantic_cold_ms = 1500
"#,
            script = script.display().to_string(),
            pids = fixture.pids_file().display().to_string(),
        );
        std::fs::write(fixture.config(), config).unwrap();
        fixture
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("config.toml")
    }

    fn pids_file(&self) -> PathBuf {
        self.dir.path().join("semble-pids")
    }

    /// The process group of every fake Semble started so far; each one leads its own group.
    fn semble_groups(&self) -> Vec<Pid> {
        let pids = std::fs::read_to_string(self.pids_file()).unwrap_or_default();
        pids.lines().filter_map(|line| line.trim().parse().ok()).map(Pid::from_raw).collect()
    }

    /// `uals mcp` in `cwd` with the extra variables `env`, through an rmcp client.
    async fn server(&self, cwd: &Path, env: &[(&str, &Path)]) -> McpChild {
        McpChild::spawn(ChildSpec {
            name: "uals".into(),
            program: PathBuf::from(env!("CARGO_BIN_EXE_uals")),
            args: vec!["mcp".into(), "--config".into(), self.config().display().to_string()],
            env: env.iter().map(|(name, value)| ((*name).to_owned(), value.display().to_string())).collect(),
            pass_env: Vec::new(),
            cwd: Some(cwd.to_path_buf()),
        })
        .await
        .expect("uals mcp starts")
    }

    /// Waits until the server has started a fake Semble.
    fn wait_for_semble(&self) {
        wait_until("a fake Semble starts", || !self.semble_groups().is_empty());
    }

    /// Waits until every fake Semble process group is gone.
    fn wait_for_semble_to_end(&self) {
        let groups = self.semble_groups();
        assert!(!groups.is_empty(), "no fake Semble was started");
        wait_until("every fake Semble ends", || groups.iter().all(|group| killpg(*group, None) == Err(Errno::ESRCH)));
    }
}

impl Drop for Fixture {
    /// Kills the fake Semble servers a failed test leaves behind.
    fn drop(&mut self) {
        for group in self.semble_groups() {
            let _ = killpg(group, Signal::SIGKILL);
        }
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + PROCESS_BOUND;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The text of a tool answer that is not an error.
async fn call(server: &McpChild, tool: &str, arguments: Value) -> String {
    let output = server.call_tool(tool, arguments).await.expect("the call reaches uals");
    assert!(!output.is_error, "{tool} failed: {:?}", output.texts);
    output.texts.concat()
}

#[tokio::test]
async fn search_fuses_text_and_semantic_hits_from_every_repository() {
    let fixture = Fixture::new("answer");
    let server = fixture.server(&fixture.workspace, &[]).await;

    let json = call(&server, "search", json!({"query": "HostSelector", "format": "json"})).await;
    let concise = call(&server, "search", json!({"query": "HostSelector"})).await;

    let response: SearchResponse = serde_json::from_str(&json).unwrap();
    let top = &response.hits[0];
    assert_eq!(top.path, fixture.workspace.join(HOST_SELECTOR));
    // The lexical hit on the class line (10) and the semantic hit on lines 5-14 are one hit.
    assert_eq!(top.lines, LineRange::new(5, 14));
    let sources: Vec<BackendId> = top.sources.iter().map(|source| source.backend).collect();
    assert_eq!(sources, [BackendId::Lexical, BackendId::Semble]);
    let mut statuses: Vec<(String, BackendId, &str)> =
        response.backends.iter().map(|report| (report.repo.clone(), report.backend, report.status.state())).collect();
    statuses.sort();
    let expected: Vec<(String, BackendId, &str)> = REPOS
        .iter()
        .flat_map(|repo| {
            [(BackendId::AstIndex, "unavailable"), (BackendId::Lexical, "ok"), (BackendId::Semble, "ok")]
                .map(|(backend, state)| ((*repo).to_owned(), backend, state))
        })
        .collect();
    assert_eq!(statuses, expected);
    assert!(concise.chars().count() <= OutputConfig::default().max_output_chars, "{concise}");
}

#[tokio::test]
async fn a_semantic_backend_that_never_answers_leaves_a_partial_answer_in_time() {
    let fixture = Fixture::new("hang");
    let server = fixture.server(&fixture.workspace, &[]).await;

    let started = Instant::now();
    let json = call(&server, "search", json!({"query": "HostSelector", "format": "json"})).await;
    let elapsed = started.elapsed();

    // The first semantic call gets `semantic_cold_ms` (1.5 s).
    assert!(elapsed < Duration::from_secs(5), "the answer took {elapsed:?}");
    let response: SearchResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(response.hits[0].path, fixture.workspace.join(HOST_SELECTOR));
    let semble: Vec<(&str, &BackendStatus)> = response
        .backends
        .iter()
        .filter(|report| report.backend == BackendId::Semble)
        .map(|report| (report.repo.as_str(), &report.status))
        .collect();
    assert_eq!(semble, REPOS.map(|repo| (repo, &BackendStatus::Timeout)));
}

#[tokio::test]
async fn the_workspace_comes_from_the_parameter_else_the_variable() {
    let fixture = Fixture::new("answer");
    let empty = fixture.dir.path().join("empty");
    let server = fixture.server(&empty, &[("UALS_WORKSPACE", &fixture.workspace)]).await;

    let usages = call(&server, "usages", json!({"symbol": "slugify", "format": "json"})).await;
    let status = call(&server, "status", json!({})).await;
    let web = fixture.workspace.join("sample-web");
    let web_status = call(&server, "status", json!({"workspace": web})).await;
    let unknown = server.call_tool("search", json!({"query": "x", "repos": ["nope"]})).await.unwrap();

    let usages: UsagesResponse = serde_json::from_str(&usages).unwrap();
    let tools = usages.repos.iter().find(|repo| repo.repo == "sample-tools").expect("sample-tools answered");
    // ast-index has no index there, so the whole-word text search answers; the declaration
    // `def slugify` in text.py is not a usage.
    assert_eq!(tools.source, BackendId::Lexical);
    let found: Vec<(String, u32)> = tools
        .usages
        .iter()
        .map(|usage| (usage.path.strip_prefix(&fixture.workspace).unwrap().display().to_string(), usage.line))
        .collect();
    assert_eq!(
        found,
        [
            ("sample-tools/sample_tools/__init__.py".to_owned(), 2),
            ("sample-tools/sample_tools/__init__.py".to_owned(), 4),
        ]
    );
    assert!(status.starts_with(&format!("status of {} · 3 repositories\n", fixture.workspace.display())), "{status}");
    assert!(web_status.starts_with(&format!("status of {} · 1 repository\n", web.display())), "{web_status}");
    assert!(unknown.is_error);
    assert!(unknown.texts.concat().starts_with("unknown repository \"nope\""), "{:?}", unknown.texts);
}

/// `uals mcp` on raw pipes, after the MCP handshake.
struct RawServer {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    /// The folder the client lists as its root; `None` when the client does not offer roots.
    root: Option<PathBuf>,
}

impl RawServer {
    /// Starts the server in an empty folder with `$UALS_WORKSPACE` set to the fixture workspace,
    /// and completes the handshake; with `root`, the client offers roots and lists that folder.
    /// The server then prepares the workspace, which starts the fake Semble.
    fn start(fixture: &Fixture, root: Option<&Path>) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_uals"))
            .arg("mcp")
            .arg("--config")
            .arg(fixture.config())
            .current_dir(fixture.dir.path().join("empty"))
            .env("UALS_WORKSPACE", &fixture.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("uals starts");
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut server = Self { stdin: child.stdin.take(), child, lines, root: root.map(Path::to_path_buf) };
        let capabilities = if root.is_some() { json!({"roots": {}}) } else { json!({}) };
        server.request(
            0,
            "initialize",
            json!({"protocolVersion": "2025-11-25", "capabilities": capabilities, "clientInfo": {"name": "e2e", "version": "0"}}),
        );
        server.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        server
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{message}").expect("uals reads stdin");
    }

    /// Sends a request and returns the answer with the same id. Answers the server's `roots/list`
    /// requests on the way.
    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + PROCESS_BOUND;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).unwrap_or_else(|_| panic!("no answer to {method}"));
            let message: Value = serde_json::from_str(&line).expect("a JSON-RPC line");
            if message["method"] == "roots/list" {
                let roots: Vec<Value> =
                    self.root.iter().map(|root| json!({"uri": format!("file://{}", root.display())})).collect();
                self.send(&json!({"jsonrpc": "2.0", "id": message["id"], "result": {"roots": roots}}));
            } else if message["id"] == json!(id) {
                return message;
            }
        }
    }

    /// Calls `tool` and returns the text of its answer.
    fn call(&mut self, id: u64, tool: &str, arguments: Value) -> String {
        let answer = self.request(id, "tools/call", json!({"name": tool, "arguments": arguments}));
        assert_eq!(answer["result"]["isError"], json!(false), "{answer}");
        answer["result"]["content"][0]["text"].as_str().expect("a text answer").to_owned()
    }

    /// Runs a search, so that the fake Semble has answered once.
    fn search(&mut self) {
        self.call(1, "search", json!({"query": "HostSelector"}));
    }

    /// Waits for the server to exit.
    fn wait(&mut self) -> ExitStatus {
        let mut status = None;
        wait_until("uals mcp exits", || {
            status = self.child.try_wait().expect("uals can be waited for");
            status.is_some()
        });
        status.expect("an exit status")
    }
}

impl Drop for RawServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_client_root_comes_before_the_variable() {
    let fixture = Fixture::new("answer");
    let web = fixture.workspace.join("sample-web");
    let mut server = RawServer::start(&fixture, Some(&web));

    let status = server.call(1, "status", json!({}));

    assert!(status.starts_with(&format!("status of {} · 1 repository\n", web.display())), "{status}");
}

#[test]
fn the_server_starts_the_backends_of_its_workspace_before_the_first_call() {
    let fixture = Fixture::new("answer");
    let _server = RawServer::start(&fixture, None);

    fixture.wait_for_semble();
}

#[test]
fn closing_stdin_stops_the_server_and_its_backends() {
    let fixture = Fixture::new("answer");
    let mut server = RawServer::start(&fixture, None);
    server.search();
    fixture.wait_for_semble();

    drop(server.stdin.take());

    assert!(server.wait().success());
    fixture.wait_for_semble_to_end();
}

#[test]
fn sigterm_stops_the_server_and_its_backends_while_stdin_is_open() {
    let fixture = Fixture::new("answer");
    let mut server = RawServer::start(&fixture, None);
    server.search();
    fixture.wait_for_semble();

    let started = Instant::now();
    kill(Pid::from_raw(i32::try_from(server.child.id()).unwrap()), Signal::SIGTERM).unwrap();
    let status = server.wait();

    // A blocked read of stdin holds the exit for at most `SHUTDOWN_BOUND` (0.5 s).
    assert!(started.elapsed() < Duration::from_secs(3), "uals took {:?} to exit", started.elapsed());
    assert!(status.success(), "{status}");
    fixture.wait_for_semble_to_end();
}
