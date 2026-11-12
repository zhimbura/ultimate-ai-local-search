//! A long-lived MCP server in a child process, driven with the rmcp client (track T4).
//! Findings, a working snippet, deadlines and the restart strategy: `docs/spikes/s1-rmcp.md`.
//!
//! Life cycle of an [`McpChild`]:
//! - [`McpChild::spawn`] starts the server in its own process group and completes the handshake.
//! - [`McpToolCaller::call_tool`] runs on that connection; several calls may be in flight at once.
//!   Dropping the future is safe: rmcp sends `notifications/cancelled` and the connection stays usable.
//! - A crashed server is detected (`ServiceError::TransportClosed`, or the process group is gone;
//!   rmcp's `is_closed()` alone is not trusted) and respawned by the next call. After 3 consecutive
//!   failures the respawn backs off 1 s, 2 s, 4 s ... up to 60 s and calls fail fast with
//!   `BackendError::Unavailable` until the backoff ends. A call that hit a dead server is replayed
//!   once on the fresh one, never more.
//! - Dropping the `McpChild` sends SIGTERM to the whole process group and SIGKILL 200 ms later.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use process_wrap::tokio::{CommandWrap, KillOnDrop};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::TokioChildProcess;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::Mutex;
use uals_core::backend::{BackendError, McpToolCaller, ToolOutput};

/// Deadline for the spawn plus the MCP `initialize` handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How many trailing stderr lines are kept for error messages.
const STDERR_TAIL_LINES: usize = 20;
/// Longest stderr line kept for error messages (longer lines are cut).
const STDERR_LINE_MAX: usize = 400;
/// Time between SIGTERM and SIGKILL when the process group is stopped.
const KILL_GRACE: Duration = Duration::from_millis(200);
/// Consecutive failures after which respawning backs off.
const BACKOFF_AFTER_FAILURES: u32 = 3;
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

type StderrTail = Arc<StdMutex<VecDeque<String>>>;

/// How to start an MCP server.
///
/// The child gets a **clean environment**, not a copy of ours: it starts empty, then receives
/// `PATH`, `HOME`, `USER`, `TMPDIR`, `LANG` and `LC_ALL` from the current environment (when set),
/// then the variables named in `pass_env` (when set), then the literals in `env`. Later steps
/// override earlier ones, so a literal in `env` wins over everything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSpec {
    /// Short name for logs and errors: "semble", "claude-context".
    pub name: String,
    /// Executable. A bare name is looked up in the child's `PATH`.
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Literal environment variables.
    pub env: BTreeMap<String, String>,
    /// Names of variables copied from the current environment when set.
    pub pass_env: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl ChildSpec {
    /// The environment the child is started with (see the type documentation).
    pub fn child_env(&self) -> BTreeMap<String, String> {
        const BASE: [&str; 6] = ["PATH", "HOME", "USER", "TMPDIR", "LANG", "LC_ALL"];
        let mut env = BTreeMap::new();
        for name in BASE.iter().copied().chain(self.pass_env.iter().map(String::as_str)) {
            if let Some(value) = std::env::var_os(name) {
                env.insert(name.to_owned(), value.to_string_lossy().into_owned());
            }
        }
        env.extend(self.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        env
    }
}

/// A running MCP server child process.
pub struct McpChild {
    shared: Arc<Shared>,
}

struct Shared {
    spec: ChildSpec,
    slot: Mutex<Slot>,
    /// Process group of the current connection, 0 when there is none.
    pgid: AtomicI32,
    generation: AtomicU64,
}

#[derive(Default)]
struct Slot {
    conn: Option<Arc<Connection>>,
    /// Failures since the last successful call.
    failures: u32,
    /// While set and in the future, respawning is refused.
    backoff_until: Option<Instant>,
}

impl Slot {
    fn note_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
        self.backoff_until = backoff_for(self.failures).map(|delay| Instant::now() + delay);
    }
}

/// One live server: the rmcp client plus the guard that stops its process group.
struct Connection {
    generation: u64,
    client: RunningService<RoleClient, ()>,
    group: GroupGuard,
    stderr_tail: StderrTail,
}

impl Connection {
    /// True while the server can still be talked to. `is_closed()` of rmcp alone does not notice
    /// a dead child, so the process group is checked as well.
    fn is_alive(&self) -> bool {
        !self.client.is_transport_closed() && !self.client.is_closed() && self.group.exists()
    }
}

impl McpChild {
    /// Starts the server and completes the MCP handshake.
    pub async fn spawn(spec: ChildSpec) -> Result<Self, BackendError> {
        let shared = Arc::new(Shared {
            spec,
            slot: Mutex::new(Slot::default()),
            pgid: AtomicI32::new(0),
            generation: AtomicU64::new(0),
        });
        let conn = shared.connect().await?;
        shared.pgid.store(conn.group.pgid(), Ordering::SeqCst);
        shared.slot.lock().await.conn = Some(Arc::new(conn));
        Ok(Self { shared })
    }

    /// Process group id of the current server process (equal to its pid), `None` while no server
    /// runs (before a respawn, or while backing off).
    pub fn process_group(&self) -> Option<i32> {
        match self.shared.pgid.load(Ordering::SeqCst) {
            0 => None,
            pgid => Some(pgid),
        }
    }
}

impl Shared {
    /// Returns the live connection, spawning a new server when there is none. A spawn only
    /// happens under the slot lock, so concurrent callers never start two servers.
    async fn connection(&self) -> Result<Arc<Connection>, BackendError> {
        let mut slot = self.slot.lock().await;
        if let Some(conn) = &slot.conn {
            if conn.is_alive() {
                return Ok(Arc::clone(conn));
            }
            tracing::warn!(server = %self.spec.name, "mcp server is gone, it will be respawned");
            conn.group.terminate();
            slot.conn = None;
            self.pgid.store(0, Ordering::SeqCst);
            slot.note_failure();
        }
        if let Some(until) = slot.backoff_until {
            let now = Instant::now();
            if until > now {
                return Err(BackendError::Unavailable(format!(
                    "{} is down ({} consecutive failures), next start in {:.1}s",
                    self.spec.name,
                    slot.failures,
                    (until - now).as_secs_f64()
                )));
            }
        }
        match self.connect().await {
            Ok(conn) => {
                self.pgid.store(conn.group.pgid(), Ordering::SeqCst);
                let conn = Arc::new(conn);
                slot.conn = Some(Arc::clone(&conn));
                Ok(conn)
            }
            Err(error) => {
                slot.note_failure();
                Err(error)
            }
        }
    }

    /// Forgets `conn` (and stops its process group) after a transport failure, unless a newer
    /// connection has replaced it already.
    async fn discard(&self, conn: &Arc<Connection>) {
        let mut slot = self.slot.lock().await;
        let is_current = slot.conn.as_ref().is_some_and(|current| current.generation == conn.generation);
        if is_current {
            slot.conn = None;
            self.pgid.store(0, Ordering::SeqCst);
            slot.note_failure();
        }
        conn.group.terminate();
    }

    async fn record_success(&self) {
        let mut slot = self.slot.lock().await;
        slot.failures = 0;
        slot.backoff_until = None;
    }

    /// Starts the process and runs the handshake, within [`HANDSHAKE_TIMEOUT`].
    async fn connect(&self) -> Result<Connection, BackendError> {
        let name = &self.spec.name;
        let env = self.spec.child_env();
        let program = resolve_program(&self.spec, &env)?;

        let mut cmd = CommandWrap::with_new(&program, |c| {
            c.args(&self.spec.args);
            c.env_clear();
            c.envs(&env);
            if let Some(cwd) = &self.spec.cwd {
                c.current_dir(cwd);
            }
        });
        #[cfg(unix)]
        cmd.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        cmd.wrap(process_wrap::tokio::JobObject);
        cmd.wrap(KillOnDrop);

        let (transport, stderr) = TokioChildProcess::builder(cmd)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| BackendError::Unavailable(format!("cannot start {name} ({}): {e}", program.display())))?;
        let pid = transport.id().and_then(|pid| i32::try_from(pid).ok()).unwrap_or(0);
        // From here on dropping `group` stops the whole process group, also when the handshake
        // fails or this future is dropped.
        let group = GroupGuard { pgid: pid };
        let stderr_tail: StderrTail = Arc::new(StdMutex::new(VecDeque::new()));
        if let Some(stderr) = stderr {
            tokio::spawn(drain_stderr(name.clone(), stderr, Arc::clone(&stderr_tail)));
        }

        let client = match tokio::time::timeout(HANDSHAKE_TIMEOUT, ().serve(transport)).await {
            Ok(Ok(client)) => client,
            Ok(Err(error)) => {
                // Let the drain task read what the server said before it died.
                tokio::time::sleep(Duration::from_millis(50)).await;
                return Err(BackendError::Failed(format!(
                    "{name} handshake failed: {error}{}",
                    stderr_suffix(&stderr_tail)
                )));
            }
            Err(_) => {
                return Err(BackendError::Unavailable(format!(
                    "{name} did not answer the handshake within {}s{}",
                    HANDSHAKE_TIMEOUT.as_secs(),
                    stderr_suffix(&stderr_tail)
                )));
            }
        };
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        tracing::debug!(server = %name, pid, generation, "mcp server started");
        Ok(Connection { generation, client, group, stderr_tail })
    }
}

/// Delay before the next respawn after `failures` consecutive failures: none up to 2, then
/// 1 s, 2 s, 4 s ... capped at 60 s.
fn backoff_for(failures: u32) -> Option<Duration> {
    let steps = failures.checked_sub(BACKOFF_AFTER_FAILURES)?;
    let factor = 1u32.checked_shl(steps).unwrap_or(u32::MAX);
    Some(BACKOFF_BASE.saturating_mul(factor).min(BACKOFF_MAX))
}

fn resolve_program(spec: &ChildSpec, env: &BTreeMap<String, String>) -> Result<PathBuf, BackendError> {
    if spec.program.components().count() > 1 {
        return Ok(spec.program.clone());
    }
    let cwd = match &spec.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir().unwrap_or_default(),
    };
    which::which_in(&spec.program, env.get("PATH"), cwd).map_err(|error| {
        BackendError::Unavailable(format!("{} is not installed ({}: {error})", spec.name, spec.program.display()))
    })
}

#[async_trait]
impl McpToolCaller for McpChild {
    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<ToolOutput, BackendError> {
        let shared = &self.shared;
        let server = &shared.spec.name;
        let arguments = match arguments {
            serde_json::Value::Null => None,
            serde_json::Value::Object(map) => Some(map),
            other => {
                return Err(BackendError::Failed(format!(
                    "{server}: arguments of '{name}' must be a JSON object, got {other}"
                )));
            }
        };
        let request = || {
            let mut params = CallToolRequestParams::new(name.to_owned());
            params.arguments = arguments.clone();
            params
        };

        let conn = shared.connection().await?;
        match conn.client.call_tool(request()).await {
            Ok(result) => {
                shared.record_success().await;
                return Ok(to_output(&result));
            }
            Err(error) if !is_transport_failure(&error) => return Err(map_service_error(server, name, &error, &conn)),
            Err(error) => {
                tracing::warn!(server = %server, tool = name, %error, "mcp transport failed, restarting the server");
                shared.discard(&conn).await;
            }
        }

        // The server died. Replay the call once on a fresh server.
        let conn = shared.connection().await?;
        match conn.client.call_tool(request()).await {
            Ok(result) => {
                shared.record_success().await;
                Ok(to_output(&result))
            }
            Err(error) => {
                if is_transport_failure(&error) {
                    shared.discard(&conn).await;
                }
                Err(map_service_error(server, name, &error, &conn))
            }
        }
    }
}

fn is_transport_failure(error: &ServiceError) -> bool {
    matches!(error, ServiceError::TransportClosed | ServiceError::TransportSend(_))
}

fn map_service_error(server: &str, tool: &str, error: &ServiceError, conn: &Connection) -> BackendError {
    BackendError::Failed(format!("{server}: call to '{tool}' failed: {error}{}", stderr_suffix(&conn.stderr_tail)))
}

/// Converts the rmcp result through its JSON form, so only the wire shape matters.
fn to_output(result: &rmcp::model::CallToolResult) -> ToolOutput {
    let value = serde_json::to_value(result).unwrap_or_default();
    let texts = value
        .get("content")
        .and_then(|content| content.as_array())
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    ToolOutput { texts, structured: result.structured_content.clone(), is_error: result.is_error.unwrap_or(false) }
}

/// Reads the child's stderr to the end: debug log lines plus a ring of the last few for errors.
async fn drain_stderr(name: String, stderr: impl AsyncRead + Unpin, tail: StderrTail) {
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf).trim_end().to_owned();
        if line.is_empty() {
            continue;
        }
        tracing::debug!(server = %name, "stderr: {line}");
        let line: String = line.chars().take(STDERR_LINE_MAX).collect();
        if let Ok(mut tail) = tail.lock() {
            if tail.len() == STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    }
}

fn stderr_suffix(tail: &StdMutex<VecDeque<String>>) -> String {
    let text = tail.lock().map(|tail| tail.iter().cloned().collect::<Vec<_>>().join("\n")).unwrap_or_default();
    if text.is_empty() { String::new() } else { format!("; server stderr (last lines):\n{text}") }
}

/// Stops the server's process group when dropped: SIGTERM at once, SIGKILL after [`KILL_GRACE`].
/// The whole group is signalled, never only the child pid, so helper processes die too.
struct GroupGuard {
    pgid: i32,
}

impl GroupGuard {
    fn pgid(&self) -> i32 {
        self.pgid
    }

    #[cfg(unix)]
    fn exists(&self) -> bool {
        use nix::errno::Errno;
        if self.pgid <= 0 {
            return false;
        }
        !matches!(nix::sys::signal::killpg(nix::unistd::Pid::from_raw(self.pgid), None), Err(Errno::ESRCH))
    }

    #[cfg(not(unix))]
    fn exists(&self) -> bool {
        true
    }

    /// SIGTERM now, SIGKILL later from a detached thread, so this never blocks the runtime and
    /// still happens while the runtime shuts down. Repeating it is harmless.
    #[cfg(unix)]
    fn terminate(&self) {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        if self.pgid <= 0 {
            return;
        }
        let pgid = Pid::from_raw(self.pgid);
        if killpg(pgid, Signal::SIGTERM).is_err() {
            return;
        }
        std::thread::spawn(move || {
            std::thread::sleep(KILL_GRACE);
            let _ = killpg(pgid, Signal::SIGKILL);
        });
    }

    /// Windows: the job object and `KillOnDrop` stop the tree when the transport is dropped.
    #[cfg(not(unix))]
    fn terminate(&self) {}
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_starts_after_three_failures_and_is_capped() {
        let secs = |n| backoff_for(n).map(|d| d.as_secs());
        assert_eq!(secs(0), None);
        assert_eq!(secs(2), None);
        assert_eq!(secs(3), Some(1));
        assert_eq!(secs(4), Some(2));
        assert_eq!(secs(5), Some(4));
        assert_eq!(secs(9), Some(60));
        assert_eq!(secs(1000), Some(60));
    }

    #[test]
    fn child_env_is_clean_and_literals_win() {
        let spec = ChildSpec {
            name: "x".into(),
            program: "x".into(),
            args: vec![],
            env: BTreeMap::from([("PATH".to_owned(), "/literal".to_owned()), ("EXTRA".to_owned(), "1".to_owned())]),
            pass_env: vec!["UALS_TEST_SURELY_UNSET_VARIABLE".into()],
            cwd: None,
        };
        let env = spec.child_env();
        assert_eq!(env.get("PATH").map(String::as_str), Some("/literal"));
        assert_eq!(env.get("EXTRA").map(String::as_str), Some("1"));
        let allowed = ["PATH", "HOME", "USER", "TMPDIR", "LANG", "LC_ALL", "EXTRA"];
        assert!(env.keys().all(|k| allowed.contains(&k.as_str())), "unexpected variables: {env:?}");
    }
}
