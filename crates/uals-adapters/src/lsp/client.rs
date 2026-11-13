//! One language server process spoken to over stdio: JSON-RPC with `Content-Length` framing.
//!
//! A writer task owns stdin and writes whole messages in the order they were queued, so a call
//! that is cancelled never leaves half a message behind. A reader task matches responses to
//! requests, answers the requests the server sends (for configuration, progress tokens,
//! capability registration) and keeps the state readiness is judged by: active progress, load
//! failures, the quiescent flag of rust-analyzer, exit.

use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot};
use uals_core::backend::BackendError;

use crate::mcp_child::{ChildSpec, GroupGuard, StderrTail, drain_stderr, resolve_program, stderr_suffix};

/// Longest message body accepted from a server.
const MAX_BODY: usize = 256 * 1024 * 1024;
/// How long `shutdown` waits for its answer before the process group is stopped anyway.
const SHUTDOWN_WAIT: Duration = Duration::from_millis(500);
/// JSON-RPC "method not found".
const METHOD_NOT_FOUND: i64 = -32601;
/// How long `shutdown` waits for the process group to end, so the state folder is free after it.
const EXIT_WAIT: Duration = Duration::from_secs(2);
/// How often `shutdown` checks whether the process group ended.
const EXIT_POLL: Duration = Duration::from_millis(20);

type Pending = Arc<StdMutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;
/// Framed messages on their way to the server's stdin.
type Outbox = mpsc::UnboundedSender<Vec<u8>>;

/// What the reader task learned about the server.
#[derive(Debug)]
pub struct ServerState {
    /// Progress tokens begun and not ended.
    pub active_progress: HashSet<String>,
    /// Whether any progress has begun since the start.
    pub seen_progress: bool,
    /// Last progress begin, report or end; the start when there was none.
    pub last_progress: Instant,
    /// The first message that matched a load failure pattern.
    pub load_failure: Option<String>,
    /// `experimental/serverStatus` `quiescent`, when the server sent one.
    pub quiescent: Option<bool>,
    /// The server closed its stdout.
    pub exited: bool,
}

/// A running language server.
pub struct LspClient {
    name: String,
    outbox: Outbox,
    next_id: AtomicI64,
    pending: Pending,
    state: Arc<StdMutex<ServerState>>,
    stderr_tail: StderrTail,
    started: Instant,
    /// Open documents and how many requests use each.
    open: StdMutex<HashMap<String, usize>>,
    // Field order matters: the group is stopped before the child handle is dropped.
    group: GroupGuard,
    _child: Box<dyn ChildWrapper>,
    /// A lock file held while the server uses its state folder.
    _state_lock: Option<std::fs::File>,
}

/// How to start one server for one project.
pub struct Launch {
    pub child: ChildSpec,
    /// Messages that mean the project did not load.
    pub load_failures: Vec<String>,
    /// Sent as `initializationOptions` unless null.
    pub initialization_options: Value,
    pub root_uri: String,
    pub root_name: String,
    pub state_lock: Option<std::fs::File>,
}

impl LspClient {
    /// Starts the server, sends `initialize` and `initialized`. Returns once the server answered
    /// `initialize`, within `timeout`; loading the project goes on in the server.
    pub async fn start(launch: Launch, timeout: Duration) -> Result<Arc<Self>, BackendError> {
        let spec = &launch.child;
        let name = spec.name.clone();
        let env = spec.child_env();
        let program = resolve_program(spec, &env)?;
        let mut cmd = CommandWrap::with_new(&program, |c| {
            c.args(&spec.args);
            c.env_clear();
            c.envs(&env);
            if let Some(cwd) = &spec.cwd {
                c.current_dir(cwd);
            }
            c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        });
        #[cfg(unix)]
        cmd.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        cmd.wrap(process_wrap::tokio::JobObject);
        cmd.wrap(KillOnDrop);
        let mut child = cmd
            .spawn()
            .map_err(|e| BackendError::Unavailable(format!("cannot start {name} ({}): {e}", program.display())))?;
        let pid = child.id().and_then(|pid| i32::try_from(pid).ok()).unwrap_or(0);
        let group = GroupGuard::new(pid);
        let (Some(stdin), Some(stdout)) = (child.stdin().take(), child.stdout().take()) else {
            return Err(BackendError::Failed(format!("{name}: no stdio pipes")));
        };
        let stderr_tail: StderrTail = Arc::new(StdMutex::new(VecDeque::new()));
        if let Some(stderr) = child.stderr().take() {
            tokio::spawn(drain_stderr(name.clone(), stderr, Arc::clone(&stderr_tail)));
        }
        let started = Instant::now();
        let (outbox, queued) = mpsc::unbounded_channel();
        tokio::spawn(write_messages(stdin, queued));
        let client = Arc::new(Self {
            name: name.clone(),
            outbox: outbox.clone(),
            next_id: AtomicI64::new(1),
            pending: Arc::default(),
            state: Arc::new(StdMutex::new(ServerState {
                active_progress: HashSet::new(),
                seen_progress: false,
                last_progress: started,
                load_failure: None,
                quiescent: None,
                exited: false,
            })),
            stderr_tail,
            started,
            open: StdMutex::default(),
            group,
            _child: child,
            _state_lock: launch.state_lock,
        });
        tokio::spawn(read_messages(
            outbox,
            stdout,
            Arc::clone(&client.pending),
            Arc::clone(&client.state),
            launch.load_failures,
            json!([{ "uri": launch.root_uri, "name": launch.root_name }]),
        ));
        tracing::debug!(server = %name, pid, "language server started");

        let params = initialize_params(&launch.root_uri, &launch.root_name, launch.initialization_options);
        client.request("initialize", params, timeout).await?;
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    /// When the process started.
    pub fn started(&self) -> Instant {
        self.started
    }

    /// A copy of what the reader task learned, through `read`.
    pub fn with_state<T>(&self, read: impl FnOnce(&ServerState) -> T) -> T {
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        read(&state)
    }

    /// Whether the process is still there.
    pub fn is_alive(&self) -> bool {
        !self.with_state(|state| state.exited) && self.group.exists()
    }

    /// The server's last stderr lines as an error-message suffix.
    pub fn stderr(&self) -> String {
        stderr_suffix(&self.stderr_tail)
    }

    /// Sends a request and waits for its result within `timeout`.
    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, BackendError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        // Forgets the request however this call ends, also when it is cancelled.
        let _pending = PendingGuard { pending: &self.pending, id };
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => Err(BackendError::Failed(format!("{}: {method} failed: {error}", self.name))),
            Ok(Err(_)) => Err(BackendError::Unavailable(format!("{} exited{}", self.name, self.stderr()))),
            Err(_) => {
                // The server may still work on it; it is told to stop.
                let _ = self.notify("$/cancelRequest", json!({ "id": id }));
                Err(BackendError::Unavailable(format!(
                    "{} did not answer {method} within {:.1}s",
                    self.name,
                    timeout.as_secs_f64()
                )))
            }
        }
    }

    /// Sends a notification.
    pub fn notify(&self, method: &str, params: Value) -> Result<(), BackendError> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Runs `request` with the document `uri` open in the server. Requests on the same document
    /// share one `didOpen`; the last one closes it, also when its call is cancelled.
    pub async fn with_document<T>(
        &self,
        uri: &str,
        language_id: &str,
        text: impl FnOnce() -> String,
        request: impl AsyncFnOnce() -> Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        {
            let mut open = lock(&self.open);
            if !open.contains_key(uri) {
                let params = json!({ "textDocument": {
                    "uri": uri, "languageId": language_id, "version": 1, "text": text() } });
                self.notify("textDocument/didOpen", params)?;
            }
            *open.entry(uri.to_owned()).or_insert(0) += 1;
        }
        let _open = OpenGuard { client: self, uri };
        request().await
    }

    /// Asks the server to shut down, stops its process group and waits up to [`EXIT_WAIT`] for it
    /// to end, so the next server of the project finds its state folder free.
    pub async fn shutdown(&self) {
        if self.is_alive() {
            let _ = self.request("shutdown", Value::Null, SHUTDOWN_WAIT).await;
            let _ = self.notify("exit", Value::Null);
        }
        self.group.terminate();
        let deadline = Instant::now() + EXIT_WAIT;
        while self.group.exists() && Instant::now() < deadline {
            tokio::time::sleep(EXIT_POLL).await;
        }
    }

    /// Queues one message for the writer task.
    fn send(&self, message: &Value) -> Result<(), BackendError> {
        self.outbox
            .send(frame(message))
            .map_err(|_| BackendError::Unavailable(format!("{} is down{}", self.name, self.stderr())))
    }
}

/// Removes a request from the pending ones when its call ends.
struct PendingGuard<'a> {
    pending: &'a Pending,
    id: i64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        lock(self.pending).remove(&self.id);
    }
}

/// Gives back one use of an open document when its call ends; the last use closes it.
struct OpenGuard<'a> {
    client: &'a LspClient,
    uri: &'a str,
}

impl Drop for OpenGuard<'_> {
    fn drop(&mut self) {
        let mut open = lock(&self.client.open);
        let Some(users) = open.get_mut(self.uri) else { return };
        *users -= 1;
        if *users == 0 {
            open.remove(self.uri);
            let _ = self.client.notify("textDocument/didClose", json!({ "textDocument": { "uri": self.uri } }));
        }
    }
}

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `message` with its `Content-Length` header.
fn frame(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    framed.extend_from_slice(body.as_bytes());
    framed
}

/// Writes queued messages to the server until the queue closes or the pipe breaks.
async fn write_messages(mut stdin: ChildStdin, mut queued: mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Some(framed) = queued.recv().await {
        if stdin.write_all(&framed).await.is_err() || stdin.flush().await.is_err() {
            break;
        }
    }
}

/// Parameters of `initialize`: the client announces what the reader task answers.
fn initialize_params(root_uri: &str, root_name: &str, options: Value) -> Value {
    let mut params = json!({
        "processId": std::process::id(),
        "clientInfo": { "name": "uals" },
        "rootUri": root_uri,
        "workspaceFolders": [{ "uri": root_uri, "name": root_name }],
        "capabilities": {
            "workspace": { "configuration": true, "workspaceFolders": true },
            "textDocument": {
                "synchronization": { "dynamicRegistration": false },
                "references": { "dynamicRegistration": false }
            },
            "window": { "workDoneProgress": true },
            "general": { "positionEncodings": ["utf-16"] },
            "experimental": { "serverStatusNotification": true }
        }
    });
    if !options.is_null() {
        params["initializationOptions"] = options;
    }
    params
}

/// Reads messages until the server closes stdout, then fails every pending request.
async fn read_messages(
    outbox: Outbox,
    stdout: impl tokio::io::AsyncRead + Unpin,
    pending: Pending,
    state: Arc<StdMutex<ServerState>>,
    load_failures: Vec<String>,
    folders: Value,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let message = match read_message(&mut reader).await {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!("language server sent a bad message: {error}");
                break;
            }
        };
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        match (method, id) {
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { continue };
                let sender = pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
                if let Some(sender) = sender {
                    let result = match message.get("error") {
                        Some(error) => Err(error.get("message").and_then(Value::as_str).unwrap_or("error").to_owned()),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = sender.send(result);
                }
            }
            (Some(method), Some(id)) => {
                let reply = server_request_reply(method, message.get("params"), &folders);
                let mut answer = json!({ "jsonrpc": "2.0", "id": id });
                match reply {
                    Some(result) => answer["result"] = result,
                    None => answer["error"] = json!({ "code": METHOD_NOT_FOUND, "message": "not supported" }),
                }
                // Queued, never awaited: the reader must not wait for the server to read.
                let _ = outbox.send(frame(&answer));
            }
            (Some(method), None) => {
                let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                note_notification(&mut state, method, message.get("params"), &load_failures);
            }
            (None, None) => {}
        }
    }
    state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).exited = true;
    // Dropping the senders wakes every waiting request with "exited".
    pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
}

/// The result for a request the server sends, `None` for "method not found".
fn server_request_reply(method: &str, params: Option<&Value>, folders: &Value) -> Option<Value> {
    match method {
        // One `null` per asked section: every server falls back to its defaults.
        "workspace/configuration" => {
            let items = params.and_then(|p| p.get("items")).and_then(Value::as_array).map_or(0, Vec::len);
            Some(Value::Array(vec![Value::Null; items]))
        }
        "workspace/workspaceFolders" => Some(folders.clone()),
        "window/workDoneProgress/create"
        | "client/registerCapability"
        | "client/unregisterCapability"
        | "window/showMessageRequest"
        | "workspace/semanticTokens/refresh"
        | "workspace/inlayHint/refresh"
        | "workspace/codeLens/refresh"
        | "workspace/diagnostic/refresh" => Some(Value::Null),
        _ => None,
    }
}

/// Updates the state from one notification.
fn note_notification(state: &mut ServerState, method: &str, params: Option<&Value>, load_failures: &[String]) {
    let Some(params) = params else { return };
    match method {
        "$/progress" => {
            let token = match params.get("token") {
                Some(Value::String(token)) => token.clone(),
                Some(other) => other.to_string(),
                None => return,
            };
            match params.get("value").and_then(|v| v.get("kind")).and_then(Value::as_str) {
                Some("begin") => {
                    state.active_progress.insert(token);
                    state.seen_progress = true;
                }
                Some("end") => {
                    state.active_progress.remove(&token);
                }
                _ => {}
            }
            state.last_progress = Instant::now();
        }
        "window/logMessage" | "window/showMessage" => {
            let text = params.get("message").and_then(Value::as_str).unwrap_or("");
            if state.load_failure.is_none() && load_failures.iter().any(|pattern| text.contains(pattern.as_str())) {
                state.load_failure = Some(text.lines().next().unwrap_or(text).chars().take(300).collect());
            }
        }
        "experimental/serverStatus" => {
            state.quiescent = params.get("quiescent").and_then(Value::as_bool);
        }
        _ => {}
    }
}

/// Reads one framed message; `None` at a clean end of the stream.
async fn read_message(reader: &mut (impl tokio::io::AsyncBufRead + Unpin)) -> std::io::Result<Option<Value>> {
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let header = line.trim_end();
        if header.is_empty() {
            if length.is_some() {
                break;
            }
            // Stray empty lines before a header are skipped.
            continue;
        }
        if let Some((key, value)) = header.split_once(':')
            && key.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let length = length.unwrap_or(0);
    if length > MAX_BODY {
        return Err(std::io::Error::other(format!("message of {length} bytes")));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map(Some).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn framed_messages_are_read_one_by_one() {
        let first = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let second = r#"{"jsonrpc":"2.0","method":"x","params":{"a":"é"}}"#;
        let input = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc\r\n\r\n{first}Content-Length: {}\r\n\r\n{second}",
            first.len(),
            second.len()
        );
        let mut reader = BufReader::new(input.as_bytes());
        assert_eq!(read_message(&mut reader).await.unwrap().unwrap()["id"], 1);
        assert_eq!(read_message(&mut reader).await.unwrap().unwrap()["params"]["a"], "é");
        assert!(read_message(&mut reader).await.unwrap().is_none());
    }

    #[test]
    fn progress_and_failures_change_the_state() {
        let now = Instant::now();
        let mut state = ServerState {
            active_progress: HashSet::new(),
            seen_progress: false,
            last_progress: now,
            load_failure: None,
            quiescent: None,
            exited: false,
        };
        let failures = vec!["Unable to import".to_owned()];
        let begin = json!({ "token": 7, "value": { "kind": "begin", "title": "Importing" } });
        note_notification(&mut state, "$/progress", Some(&begin), &failures);
        assert!(state.seen_progress);
        assert_eq!(state.active_progress.len(), 1);
        let end = json!({ "token": 7, "value": { "kind": "end" } });
        note_notification(&mut state, "$/progress", Some(&end), &failures);
        assert!(state.active_progress.is_empty());

        let log = json!({ "type": 1, "message": "Unable to import project\nstack" });
        note_notification(&mut state, "window/logMessage", Some(&log), &failures);
        assert_eq!(state.load_failure.as_deref(), Some("Unable to import project"));

        let status = json!({ "health": "ok", "quiescent": true });
        note_notification(&mut state, "experimental/serverStatus", Some(&status), &failures);
        assert_eq!(state.quiescent, Some(true));
    }

    #[test]
    fn configuration_requests_get_one_null_per_item() {
        let folders = json!([]);
        let params = json!({ "items": [{ "section": "a" }, { "section": "b" }] });
        assert_eq!(server_request_reply("workspace/configuration", Some(&params), &folders), Some(json!([null, null])));
        assert_eq!(server_request_reply("window/workDoneProgress/create", None, &folders), Some(Value::Null));
        assert_eq!(server_request_reply("custom/thing", None, &folders), None);
    }
}
