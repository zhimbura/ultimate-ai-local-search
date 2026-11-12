//! `uals mcp` end to end: the real binary on stdin and stdout, on a copy of the public fixture
//! workspace. ast-index is not installed for the server, so `search` runs on the lexical backend
//! and a fake Semble: an MCP server in `sh` that finds `HostSelector.kt` lines 5-14 in
//! `sample-android`, the regions listed in a file in `sample-web` and nothing elsewhere. With
//! `FAKE_SEMBLE_MODE=hang` it never answers a search, with `die` it exits on one. It ignores
//! SIGTERM, so only SIGKILL stops it, and it logs every tool call.
//!
//! The tool tests talk to the server through `McpChild`, an rmcp client. The stop and roots tests
//! drive the server's pipes by hand, so that the client can close stdin or keep it open, and answer
//! the server's `roots/list` requests as they like.
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
use uals_core::types::{
    BackendId, BackendReport, BackendStatus, IndexState, LineRange, SearchResponse, UsagesResponse,
};

const FAKE_SEMBLE: &str = r#"#!/bin/sh
# Ignored signals stay ignored in children, so the helper below ignores SIGTERM too.
trap '' TERM
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
      printf '%s\n' "$line" >> "$FAKE_SEMBLE_CALLS"
      case "$FAKE_SEMBLE_MODE" in
        hang) continue ;;
        die) exit 1 ;;
      esac
      case "$line" in
        *sample-android*) results=$found ;;
        *sample-web*) results=$(cat "$FAKE_SEMBLE_WEB" 2>/dev/null || printf '[]') ;;
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
    /// `semble_mode`: `answer`, `hang` or `die`.
    fn new(semble_mode: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let workspace = fixture_copy(dir.path()).canonicalize().unwrap();
        let script = dir.path().join("fake-semble");
        std::fs::write(&script, FAKE_SEMBLE).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        let fixture = Self { dir, workspace };
        // A Semble that answers gets deadlines a busy machine does not miss; the others get short
        // ones, so that the tests see how a missing answer is handled.
        let (semantic_ms, semantic_cold_ms) = if semble_mode == "answer" { (10_000, 10_000) } else { (1000, 1500) };
        let config = format!(
            r#"
[semantic.semble]
command = [{script:?}]
env = {{ FAKE_SEMBLE_MODE = "{semble_mode}", FAKE_SEMBLE_PIDS = {pids:?}, FAKE_SEMBLE_CALLS = {calls:?}, FAKE_SEMBLE_WEB = {web:?} }}

[ast_index]
binary = "/nonexistent/ast-index"
update_on_start = false

[deadlines]
semantic_ms = {semantic_ms}
semantic_cold_ms = {semantic_cold_ms}
"#,
            script = script.display().to_string(),
            pids = fixture.pids_file().display().to_string(),
            calls = fixture.calls_file().display().to_string(),
            web = fixture.web_results_file().display().to_string(),
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

    fn calls_file(&self) -> PathBuf {
        self.dir.path().join("semble-calls")
    }

    /// The results the fake Semble gives for `sample-web`, escaped for its `text` field.
    fn web_results_file(&self) -> PathBuf {
        self.dir.path().join("semble-web-results")
    }

    /// Every tool call the fake Semble servers got so far, one JSON-RPC line each.
    fn semble_calls(&self) -> String {
        std::fs::read_to_string(self.calls_file()).unwrap_or_default()
    }

    /// Makes the fake Semble find `count` regions of 20 lines of 200 characters in `sample-web`,
    /// one per file.
    fn add_wide_web_results(&self, count: usize) {
        let folder = self.workspace.join("sample-web/src/wide");
        std::fs::create_dir_all(&folder).unwrap();
        let line = format!("const padding = \"{}\";", "x".repeat(178));
        let mut results = Vec::new();
        for n in 1..=count {
            std::fs::write(folder.join(format!("w{n}.ts")), vec![line.as_str(); 20].join("\n")).unwrap();
            results
                .push(json!({"file_path": format!("src/wide/w{n}.ts"), "start_line": 1, "end_line": 20, "score": 0.5}));
        }
        std::fs::write(self.web_results_file(), Value::Array(results).to_string().replace('"', "\\\"")).unwrap();
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
}

#[tokio::test]
async fn the_default_answer_stays_within_the_output_budget() {
    let fixture = Fixture::new("answer");
    // About 49 000 characters of snippets on the default page of 12 hits, twice the budget.
    fixture.add_wide_web_results(15);
    let server = fixture.server(&fixture.workspace, &[]).await;

    let concise = call(&server, "search", json!({"query": "wide padding"})).await;
    let json = call(&server, "search", json!({"query": "wide padding", "format": "json"})).await;

    let output = OutputConfig::default();
    assert!(concise.chars().count() <= output.max_output_chars, "{} characters", concise.chars().count());
    let response: SearchResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(response.hits.len(), output.default_limit);
    assert!(response.hits[0].snippet.is_some(), "the first hit keeps its snippet");
    assert!(response.hits.iter().any(|hit| hit.snippet.is_none()), "the budget dropped no snippet");
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
    server.stop_and_wait(PROCESS_BOUND);
    fixture.wait_for_semble_to_end();
}

#[tokio::test]
async fn a_semantic_backend_that_dies_leaves_a_partial_answer() {
    let fixture = Fixture::new("die");
    let server = fixture.server(&fixture.workspace, &[]).await;

    let json = call(&server, "search", json!({"query": "HostSelector", "format": "json"})).await;

    let response: SearchResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(response.hits[0].path, fixture.workspace.join(HOST_SELECTOR));
    let semble: Vec<&BackendReport> =
        response.backends.iter().filter(|report| report.backend == BackendId::Semble).collect();
    assert_eq!(semble.len(), REPOS.len());
    for report in semble {
        let found_dead = matches!(report.status, BackendStatus::Failed { .. } | BackendStatus::Unavailable { .. });
        assert!(found_dead, "{report:?}");
    }
    server.stop_and_wait(PROCESS_BOUND);
    fixture.wait_for_semble_to_end();
}

#[tokio::test]
async fn the_workspace_comes_from_the_parameter_else_the_variable() {
    let fixture = Fixture::new("answer");
    let empty = fixture.dir.path().join("empty");
    let server = fixture.server(&empty, &[("UALS_WORKSPACE", &fixture.workspace)]).await;

    let usages = call(&server, "usages", json!({"symbol": "slugify", "format": "json"})).await;
    let status = call(&server, "status", json!({})).await;
    let status_json = call(&server, "status", json!({"format": "json"})).await;
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
    let status_json: Value = serde_json::from_str(&status_json).unwrap();
    let listed: Vec<(String, Vec<BackendId>)> = status_json["repos"]
        .as_array()
        .expect("a list of repositories")
        .iter()
        .map(|repo| {
            let states: Vec<IndexState> = serde_json::from_value(repo["backends"].clone()).unwrap();
            (repo["name"].as_str().unwrap().to_owned(), states.iter().map(|state| state.backend).collect())
        })
        .collect();
    let every_backend = vec![BackendId::AstIndex, BackendId::Lexical, BackendId::Semble];
    assert_eq!(listed, REPOS.map(|repo| (repo.to_owned(), every_backend.clone())));
    assert!(web_status.starts_with(&format!("status of {} · 1 repository\n", web.display())), "{web_status}");
    assert!(unknown.is_error);
    assert!(unknown.texts.concat().starts_with("unknown repository \"nope\""), "{:?}", unknown.texts);
}

/// What the raw client says about roots.
#[derive(Clone)]
enum Roots {
    /// The client does not offer roots.
    NotOffered,
    /// The client offers roots and lists this folder.
    Listed(PathBuf),
    /// The client offers roots and never answers `roots/list`.
    Unanswered,
}

/// `uals mcp` on raw pipes.
struct RawServer {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    roots: Roots,
    /// How many `roots/list` requests the server has sent so far.
    roots_asked: usize,
}

impl RawServer {
    /// Starts the server in an empty folder with `$UALS_WORKSPACE` set to `workspace_var`, without
    /// the handshake.
    fn spawn(fixture: &Fixture, workspace_var: &Path, roots: Roots) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_uals"))
            .arg("mcp")
            .arg("--config")
            .arg(fixture.config())
            .current_dir(fixture.dir.path().join("empty"))
            .env("UALS_WORKSPACE", workspace_var)
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
        Self { stdin: child.stdin.take(), child, lines, roots, roots_asked: 0 }
    }

    /// `spawn`, then the handshake. The server then prepares the workspace of a call without
    /// `workspace`, which starts the fake Semble.
    fn start(fixture: &Fixture, workspace_var: &Path, roots: Roots) -> Self {
        let mut server = Self::spawn(fixture, workspace_var, roots);
        server.initialize();
        server.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        server
    }

    /// Sends `initialize` and waits for its answer. The client offers roots unless `NotOffered`.
    fn initialize(&mut self) {
        let capabilities = match self.roots {
            Roots::NotOffered => json!({}),
            Roots::Listed(_) | Roots::Unanswered => json!({"roots": {"listChanged": true}}),
        };
        self.request(
            0,
            "initialize",
            json!({"protocolVersion": "2025-11-25", "capabilities": capabilities, "clientInfo": {"name": "e2e", "version": "0"}}),
        );
    }

    fn pid(&self) -> Pid {
        Pid::from_raw(i32::try_from(self.child.id()).unwrap())
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{message}").expect("uals reads stdin");
    }

    /// The next message from the server within `wait`, or `None`. A `roots/list` request on the
    /// way is counted and answered as `roots` says.
    fn receive(&mut self, wait: Duration) -> Option<Value> {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).ok()?;
            let message: Value = serde_json::from_str(&line).expect("a JSON-RPC line");
            if message["method"] != "roots/list" {
                return Some(message);
            }
            self.roots_asked += 1;
            let roots = match &self.roots {
                Roots::Listed(root) => json!([{"uri": format!("file://{}", root.display())}]),
                Roots::NotOffered | Roots::Unanswered => continue,
            };
            self.send(&json!({"jsonrpc": "2.0", "id": message["id"], "result": {"roots": roots}}));
        }
    }

    /// Sends a request and returns the answer with the same id.
    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + PROCESS_BOUND;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = self.receive(left).unwrap_or_else(|| panic!("no answer to {method}"));
            if message["id"] == json!(id) {
                return message;
            }
        }
    }

    /// Answers the server's `roots/list` requests until `condition` holds.
    fn answer_roots_until(&mut self, what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + PROCESS_BOUND;
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            self.receive(Duration::from_millis(20));
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
    let mut server = RawServer::start(&fixture, &fixture.workspace, Roots::Listed(web.clone()));

    let status = server.call(1, "status", json!({}));

    assert!(status.starts_with(&format!("status of {} · 1 repository\n", web.display())), "{status}");
}

#[test]
fn a_client_that_never_lists_its_roots_is_asked_once() {
    let fixture = Fixture::new("answer");
    let started = Instant::now();
    let mut server = RawServer::start(&fixture, &fixture.workspace, Roots::Unanswered);

    let first = server.call(1, "status", json!({}));
    let first_answered = started.elapsed();
    let second = server.call(2, "status", json!({}));

    let expected = format!("status of {} · 3 repositories\n", fixture.workspace.display());
    assert!(first.starts_with(&expected), "{first}");
    assert!(second.starts_with(&expected), "{second}");
    // The variable answers once the question has waited `ROOTS_DEADLINE` (2 s).
    assert!(first_answered >= Duration::from_secs(2), "the first answer came after {first_answered:?}");
    assert_eq!(server.roots_asked, 1);
}

#[test]
fn the_client_roots_are_asked_again_after_they_change() {
    let fixture = Fixture::new("answer");
    let web = fixture.workspace.join("sample-web");
    let tools = fixture.workspace.join("sample-tools");
    let mut server = RawServer::start(&fixture, &fixture.workspace, Roots::Listed(web.clone()));
    let before = server.call(1, "status", json!({}));

    server.roots = Roots::Listed(tools.clone());
    server.send(&json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}));

    assert!(before.starts_with(&format!("status of {} · 1 repository\n", web.display())), "{before}");
    // The server may handle the notification after the next call, so the calls go on until the
    // new root shows.
    let mut id = 1;
    wait_until("the new root is used", || {
        id += 1;
        server.call(id, "status", json!({})).starts_with(&format!("status of {} · 1 repository\n", tools.display()))
    });
}

#[test]
fn at_start_the_server_prepares_the_folder_the_client_lists() {
    let fixture = Fixture::new("answer");
    let web = fixture.workspace.join("sample-web");
    let tools = fixture.workspace.join("sample-tools");
    let mut server = RawServer::start(&fixture, &tools, Roots::Listed(web.clone()));

    server.answer_roots_until("the fake Semble warms a repository", || !fixture.semble_calls().is_empty());

    let calls = fixture.semble_calls();
    assert!(calls.contains(&web.display().to_string()), "{calls}");
    assert!(!calls.contains(&tools.display().to_string()), "{calls}");
}

#[test]
fn the_server_starts_the_backends_of_its_workspace_before_the_first_call() {
    let fixture = Fixture::new("answer");
    let _server = RawServer::start(&fixture, &fixture.workspace, Roots::NotOffered);

    fixture.wait_for_semble();
}

#[test]
fn closing_stdin_stops_the_server_and_its_backends() {
    let fixture = Fixture::new("answer");
    let mut server = RawServer::start(&fixture, &fixture.workspace, Roots::NotOffered);
    server.search();
    fixture.wait_for_semble();

    drop(server.stdin.take());

    assert!(server.wait().success());
    fixture.wait_for_semble_to_end();
}

#[test]
fn a_signal_stops_the_server_and_its_backends_while_stdin_is_open() {
    for signal in [Signal::SIGTERM, Signal::SIGINT] {
        let fixture = Fixture::new("answer");
        let mut server = RawServer::start(&fixture, &fixture.workspace, Roots::NotOffered);
        server.search();
        fixture.wait_for_semble();

        let started = Instant::now();
        kill(server.pid(), signal).unwrap();
        let status = server.wait();

        // A blocked read of stdin holds the exit for at most `SHUTDOWN_BOUND` (0.5 s).
        assert!(started.elapsed() < Duration::from_secs(3), "{signal:?}: uals took {:?} to exit", started.elapsed());
        assert!(status.success(), "{signal:?}: {status}");
        fixture.wait_for_semble_to_end();
    }
}

#[test]
fn a_signal_stops_the_server_during_the_handshake() {
    let fixture = Fixture::new("answer");
    let mut server = RawServer::spawn(&fixture, &fixture.workspace, Roots::NotOffered);
    // The handshake answers a ping and goes on waiting for `initialize`.
    server.request(0, "ping", json!({}));

    let started = Instant::now();
    kill(server.pid(), Signal::SIGTERM).unwrap();
    let status = server.wait();

    assert!(started.elapsed() < Duration::from_secs(3), "uals took {:?} to exit", started.elapsed());
    assert!(status.success(), "{status}");
}
