//! What the `uals doctor` tests replace: the home folder and the Claude Code config, the
//! executables on PATH (ast-index, node, semble) and an HTTP server that plays Milvus and the
//! embedder. Everything else, `uals` included, is real.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tempfile::TempDir;

/// A semble that speaks just enough MCP: it answers `initialize` and every `search`. The answer
/// follows `FAKE_SEMBLE_MODE` of the server's environment (`semantic.semble.env`): `found` (the
/// default), `empty`, or `broken` (Semble's text for a folder it cannot index). With
/// `FAKE_SEMBLE_PIDFILE` it writes its process id there, and its Semble cache folder next to it.
const FAKE_SEMBLE: &str = r#"#!/bin/sh
case "$1" in --version) echo "__VERSION__"; exit 0 ;; esac
if [ -n "$FAKE_SEMBLE_PIDFILE" ]; then echo $$ > "$FAKE_SEMBLE_PIDFILE"; echo "$SEMBLE_CACHE_LOCATION" > "$FAKE_SEMBLE_PIDFILE.cache"; fi
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      case "$FAKE_SEMBLE_MODE" in
        empty) text='{\"results\":[]}' ;;
        broken) text='Failed to index the folder: boom' ;;
        *) text='{\"results\":[{\"file_path\":\"greeting.rs\",\"start_line\":1,\"end_line\":4,\"score\":0.9}]}' ;;
      esac
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}],"isError":false}}\n' "$id" "$text" ;;
  esac
done
# Stdin is closed: stay until stopped, as a server that ignores the end of its input.
sleep 30
"#;

/// The boundaries of one test: a home folder, a PATH folder with the fake executables, the
/// Claude Code config and the config of uals.
pub struct Sandbox {
    dir: TempDir,
}

/// What `uals doctor` did.
pub struct Run {
    output: Output,
}

impl Run {
    pub fn code(&self) -> Option<i32> {
        self.output.status.code()
    }

    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    /// The line of the check `name`, with its hint (the line after it, when indented).
    pub fn check(&self, name: &str) -> String {
        let stdout = self.stdout();
        let starts_check =
            |line: &str| line.split_once(' ').is_some_and(|(_, rest)| rest.starts_with(&format!("{name}: ")));
        let mut lines = stdout.lines().skip_while(|line| !starts_check(line));
        let mut found = lines.next().unwrap_or_else(|| panic!("no check {name} in:\n{stdout}")).to_owned();
        if let Some(hint) = lines.next().filter(|line| line.starts_with("    hint: ")) {
            found.push('\n');
            found.push_str(hint);
        }
        found
    }
}

impl Sandbox {
    pub fn new() -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("bin")).unwrap();
        std::fs::create_dir(dir.path().join("home")).unwrap();
        Self { dir }
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    /// An executable shell script called `name` in the PATH folder.
    pub fn executable(&self, name: &str, script: &str) -> PathBuf {
        let path = self.path("bin").join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A program that prints `text` for `--version` and is `name`.
    pub fn versioned(&self, name: &str, text: &str) {
        self.executable(name, &format!("#!/bin/sh\necho '{text}'\n"));
    }

    pub fn fake_semble(&self, version: &str) {
        self.executable("semble", &FAKE_SEMBLE.replace("__VERSION__", version));
    }

    /// A claude-context install with `version` in its package.json; the entry point.
    pub fn package(&self, version: &str) -> PathBuf {
        let package = self.path("claude-context");
        std::fs::create_dir_all(package.join("dist")).unwrap();
        std::fs::write(
            package.join("package.json"),
            json!({"name": "claude-context-mcp", "version": version}).to_string(),
        )
        .unwrap();
        let entry = package.join("dist/index.js");
        std::fs::write(&entry, "").unwrap();
        entry
    }

    /// The Claude Code config with `value`.
    pub fn claude_json(&self, value: &Value) {
        std::fs::write(self.path("claude.json"), value.to_string()).unwrap();
    }

    /// Writes the config of uals and returns its path.
    pub fn config(&self, toml: &str) -> PathBuf {
        let path = self.path("config.toml");
        std::fs::write(&path, toml).unwrap();
        path
    }

    /// `uals doctor` with `args`. Without `config` no config file exists. The environment holds
    /// only the sandbox: no variable of the real one reaches uals except the proxy-free basics.
    pub fn doctor(&self, config: Option<&Path>, args: &[&str]) -> Run {
        self.doctor_with(config, args, &[])
    }

    pub fn doctor_with(&self, config: Option<&Path>, args: &[&str], env: &[(&str, &Path)]) -> Run {
        let mut command = Command::new(env!("CARGO_BIN_EXE_uals"));
        command.env_clear();
        command
            .env("HOME", self.path("home"))
            .env("PATH", format!("{}:/usr/bin:/bin", self.path("bin").display()))
            .env("CLAUDE_CONFIG", self.path("claude.json"))
            .env("UALS_CONFIG", self.path("no-such-config.toml"))
            .env("TMPDIR", self.dir.path());
        for (name, value) in env {
            command.env(name, value);
        }
        if let Some(config) = config {
            command.arg("--config").arg(config);
        }
        let output = command.arg("doctor").args(args).output().expect("uals runs");
        Run { output }
    }
}

/// The config of a claude-context setup whose embedder and Milvus are `http`. `overrides` replace
/// entries of `semantic.claude_context.env`; an empty value removes the entry.
pub fn claude_context_config(entry: &Path, http: &FakeHttp, overrides: &[(&str, &str)]) -> String {
    let address = http.address();
    let mut env: BTreeMap<&str, String> = BTreeMap::from([
        ("MILVUS_ADDRESS", address.clone()),
        ("EMBEDDING_PROVIDER", "OpenAI".to_owned()),
        ("OPENAI_BASE_URL", format!("http://{address}/v1")),
        ("EMBEDDING_MODEL", "test-model".to_owned()),
        ("EMBEDDING_DIMENSION", "768".to_owned()),
    ]);
    for (name, value) in overrides {
        env.insert(name, (*value).to_owned());
    }
    let table: Vec<String> =
        env.iter().filter(|(_, value)| !value.is_empty()).map(|(name, value)| format!("{name} = {value:?}")).collect();
    format!(
        "[semantic]\ndefault = \"claude-context\"\n\n[semantic.claude_context]\nentry = {:?}\nenv = {{ {} }}\n",
        entry.display().to_string(),
        table.join(", ")
    )
}

/// One request the fake server got.
#[derive(Debug, Clone)]
pub struct Request {
    pub path: String,
    pub authorization: Option<String>,
    pub body: Value,
}

/// An HTTP server on 127.0.0.1 that answers with what `handler` returns (status, JSON text) and
/// records every request.
pub struct FakeHttp {
    port: u16,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
}

impl FakeHttp {
    pub fn start(handler: impl Fn(&Request) -> (u16, String) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(handler);
        {
            let (requests, stop) = (requests.clone(), stop.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let (requests, handler) = (requests.clone(), handler.clone());
                    if let Ok(stream) = stream {
                        std::thread::spawn(move || serve(stream, &requests, &*handler));
                    }
                }
            });
        }
        Self { port, requests, stop }
    }

    /// A server that plays Milvus and an OpenAI-compatible embedder with `dimension` values.
    pub fn healthy(dimension: usize) -> Self {
        Self::start(move |request| healthy_answer(request, dimension))
    }

    pub fn address(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// The paths of the requests, in order.
    pub fn paths(&self) -> Vec<String> {
        self.requests().into_iter().map(|request| request.path).collect()
    }
}

impl Drop for FakeHttp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept loop so that it sees the flag.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// What a healthy Milvus and embedder answer.
pub fn healthy_answer(request: &Request, dimension: usize) -> (u16, String) {
    if request.path.ends_with("/embeddings") {
        let vector = vec![0.5; dimension];
        (200, json!({"data": [{"embedding": vector}]}).to_string())
    } else if request.path.ends_with("/entities/search") {
        (200, json!({"code": 0, "data": [{"id": 1, "distance": 1.0}]}).to_string())
    } else {
        (200, json!({"code": 0, "data": {}}).to_string())
    }
}

fn serve(
    mut stream: TcpStream,
    requests: &Mutex<Vec<Request>>,
    handler: &(dyn Fn(&Request) -> (u16, String) + Send + Sync),
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("").to_owned();
    let mut length = 0;
    let mut authorization = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap_or(0),
                "authorization" => authorization = Some(value.trim().to_owned()),
                _ => {}
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let request = Request { path, authorization, body: serde_json::from_slice(&body).unwrap_or(Value::Null) };
    requests.lock().unwrap().push(request.clone());
    let (status, text) = handler(&request);
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}
