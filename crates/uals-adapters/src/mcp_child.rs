//! A long-lived MCP server in a child process, driven with the rmcp client.
//! Findings, a working snippet, deadlines and the restart strategy: `docs/spikes/s1-rmcp.md`.
//!
//! Life cycle of an [`McpChild`]:
//! - [`McpChild::spawn`] starts the server in its own process group and completes the handshake.
//! - [`McpToolCaller::call_tool`] runs on that connection; several calls may be in flight at once.
//!   Dropping the future is safe: rmcp sends `notifications/cancelled` and the connection stays usable.
//! - A crashed server is detected (`ServiceError::TransportClosed`, or the process group is gone;
//!   rmcp's `is_closed()` alone is not trusted) and respawned by the next call. A call that hit a
//!   dead server is replayed once on the fresh one, never more; when the replay finds the server
//!   dead as well the call fails with `BackendError::Unavailable`.
//! - A failure is a failed start, or a call that found the server dead twice; a dead server alone
//!   is not one. After 3 consecutive failures the respawn backs off 1 s, 2 s, 4 s ... up to 60 s
//!   and calls fail fast with `BackendError::Unavailable` until the backoff ends. A server that
//!   exits before the handshake ends is `Unavailable` too.
//! - Dropping the `McpChild` sends SIGTERM to the whole process group and SIGKILL 200 ms later,
//!   unless the group is gone by then. [`McpChild::stop_and_wait`] does the same and waits.
//!   [`wait_for_stopped_groups`] waits for those SIGKILLs, which a process needs before it exits.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use process_wrap::tokio::{CommandWrap, KillOnDrop};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{ClientInitializeError, RoleClient, RunningService, ServiceError};
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
/// How often the process group is checked while waiting for it to end.
const GROUP_POLL: Duration = Duration::from_millis(10);
/// Consecutive failures after which respawning backs off.
const BACKOFF_AFTER_FAILURES: u32 = 3;
/// Delay after the first failure past the threshold; it doubles with every further failure.
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// After a failed handshake, how long to wait for the stderr drain task to read what the server
/// said before it died.
const STDERR_DRAIN_GRACE: Duration = Duration::from_millis(50);
/// Variables every child receives from the current environment, when set.
const BASE_ENV_VARS: [&str; 6] = ["PATH", "HOME", "USER", "TMPDIR", "LANG", "LC_ALL"];

type StderrTail = Arc<StdMutex<VecDeque<String>>>;

/// The threads that send SIGKILL to the stopped process groups still alive after SIGTERM.
static GROUP_KILLERS: StdMutex<Vec<std::thread::JoinHandle<()>>> = StdMutex::new(Vec::new());

/// Waits until every process group stopped so far is gone or got SIGKILL: at most [`KILL_GRACE`]
/// after its SIGTERM. The SIGKILL comes from a thread, and threads die with the process.
pub fn wait_for_stopped_groups() {
    let killers = std::mem::take(&mut *GROUP_KILLERS.lock().unwrap_or_else(PoisonError::into_inner));
    for killer in killers {
        // A killer that panicked has nothing more to send.
        let _ = killer.join();
    }
}

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
    /// Command-line arguments.
    pub args: Vec<String>,
    /// Literal environment variables.
    pub env: BTreeMap<String, String>,
    /// Names of variables copied from the current environment when set.
    pub pass_env: Vec<String>,
    /// Working directory of the child; the current one when `None`.
    pub cwd: Option<PathBuf>,
}

impl ChildSpec {
    /// The environment the child is started with (see the type documentation).
    pub fn child_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        for name in BASE_ENV_VARS.iter().copied().chain(self.pass_env.iter().map(String::as_str)) {
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
        shared.adopt(&mut *shared.slot.lock().await, conn);
        Ok(Self { shared })
    }

    /// Stops the current server's process group and waits until it is gone: SIGTERM at once,
    /// SIGKILL when `bound` has passed. Blocks the calling thread.
    pub fn stop_and_wait(&self, bound: Duration) {
        let pgid = self.shared.pgid.load(Ordering::SeqCst);
        stop_group_and_wait(pgid, bound);
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
            self.forget_connection(&mut slot);
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
            Ok(conn) => Ok(self.adopt(&mut slot, conn)),
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
            self.forget_connection(&mut slot);
        }
        conn.group.terminate();
    }

    /// Makes `conn` the current connection and publishes its process group.
    fn adopt(&self, slot: &mut Slot, conn: Connection) -> Arc<Connection> {
        self.pgid.store(conn.group.pgid(), Ordering::SeqCst);
        let conn = Arc::new(conn);
        slot.conn = Some(Arc::clone(&conn));
        conn
    }

    /// Drops the current connection; its process group is stopped by the caller.
    fn forget_connection(&self, slot: &mut Slot) {
        slot.conn = None;
        self.pgid.store(0, Ordering::SeqCst);
    }

    /// Counts a call that found the server dead and could not be completed on a fresh one.
    async fn record_failure(&self) {
        self.slot.lock().await.note_failure();
    }

    /// Clears the failure count and the backoff after a call went through.
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
        let group = GroupGuard::new(pid);
        let stderr_tail: StderrTail = Arc::new(StdMutex::new(VecDeque::new()));
        if let Some(stderr) = stderr {
            tokio::spawn(drain_stderr(name.clone(), stderr, Arc::clone(&stderr_tail)));
        }

        let client = handshake(name, transport, &stderr_tail).await?;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        tracing::debug!(server = %name, pid, generation, "mcp server started");
        Ok(Connection { generation, client, group, stderr_tail })
    }
}

/// Runs the MCP `initialize` handshake within [`HANDSHAKE_TIMEOUT`]. A failure carries the
/// server's last stderr lines.
async fn handshake(
    name: &str,
    transport: TokioChildProcess,
    stderr_tail: &StdMutex<VecDeque<String>>,
) -> Result<RunningService<RoleClient, ()>, BackendError> {
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, ().serve(transport)).await {
        Ok(Ok(client)) => Ok(client),
        Ok(Err(error)) => {
            tokio::time::sleep(STDERR_DRAIN_GRACE).await;
            let message = format!("{name} handshake failed: {error}{}", stderr_suffix(stderr_tail));
            // A server that exits before it answers cannot start (a missing dependency, a model that
            // does not load): the same expected case as a missing binary. Anything else is a
            // protocol error.
            Err(if server_exited(&error) { BackendError::Unavailable(message) } else { BackendError::Failed(message) })
        }
        Err(_) => Err(BackendError::Unavailable(format!(
            "{name} did not answer the handshake within {}s{}",
            HANDSHAKE_TIMEOUT.as_secs(),
            stderr_suffix(stderr_tail)
        ))),
    }
}

/// Whether the handshake ended because the server's pipe closed.
fn server_exited(error: &ClientInitializeError) -> bool {
    match error {
        ClientInitializeError::ConnectionClosed(_) | ClientInitializeError::TransportError { .. } => true,
        ClientInitializeError::LegacyFallbackFailed { fallback, .. } => server_exited(fallback),
        _ => false,
    }
}

/// Delay before the next respawn after `failures` consecutive failures: none up to 2, then
/// 1 s, 2 s, 4 s ... capped at 60 s.
fn backoff_for(failures: u32) -> Option<Duration> {
    let steps = failures.checked_sub(BACKOFF_AFTER_FAILURES)?;
    let factor = 1u32.checked_shl(steps).unwrap_or(u32::MAX);
    Some(BACKOFF_BASE.saturating_mul(factor).min(BACKOFF_MAX))
}

/// The executable to start: a path with folders as is, a bare name looked up in the child's
/// `PATH`.
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
                    shared.record_failure().await;
                }
                Err(map_service_error(server, name, &error, &conn))
            }
        }
    }
}

/// Whether the error means the server process or its pipe is gone.
fn is_transport_failure(error: &ServiceError) -> bool {
    matches!(error, ServiceError::TransportClosed | ServiceError::TransportSend(_))
}

/// The `BackendError` for a failed call, with the server's last stderr lines: `Unavailable` when
/// the server is gone, `Failed` for everything else.
fn map_service_error(server: &str, tool: &str, error: &ServiceError, conn: &Connection) -> BackendError {
    let stderr = stderr_suffix(&conn.stderr_tail);
    if is_transport_failure(error) {
        BackendError::Unavailable(format!("{server} is down: call to '{tool}' failed: {error}{stderr}"))
    } else {
        BackendError::Failed(format!("{server}: call to '{tool}' failed: {error}{stderr}"))
    }
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

/// The server's last stderr lines as an error-message suffix, empty when it printed nothing.
fn stderr_suffix(tail: &StdMutex<VecDeque<String>>) -> String {
    let text = tail.lock().map(|tail| tail.iter().cloned().collect::<Vec<_>>().join("\n")).unwrap_or_default();
    if text.is_empty() { String::new() } else { format!("; server stderr (last lines):\n{text}") }
}

/// Stops the server's process group when dropped: SIGTERM at once, SIGKILL after [`KILL_GRACE`]
/// when the group is still there. The whole group is signalled, never only the child pid, so
/// helper processes die too.
struct GroupGuard {
    pgid: i32,
    terminated: AtomicBool,
}

impl GroupGuard {
    fn new(pgid: i32) -> Self {
        Self { pgid, terminated: AtomicBool::new(false) }
    }

    /// The process group id (equal to the child's pid).
    fn pgid(&self) -> i32 {
        self.pgid
    }

    /// Whether the process group still has a process.
    fn exists(&self) -> bool {
        group_exists(self.pgid)
    }

    /// SIGTERM now, SIGKILL later from a thread, so this never blocks the runtime and still
    /// happens while the runtime shuts down; [`wait_for_stopped_groups`] waits for that thread.
    /// Only the first call does anything.
    #[cfg(unix)]
    fn terminate(&self) {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        if self.pgid <= 0 || self.terminated.swap(true, Ordering::SeqCst) {
            return;
        }
        if killpg(Pid::from_raw(self.pgid), Signal::SIGTERM).is_err() {
            return;
        }
        let pgid = self.pgid;
        let killer = std::thread::spawn(move || {
            wait_for_group_to_end(pgid, KILL_GRACE);
            kill_group_if_present(pgid);
        });
        let mut killers = GROUP_KILLERS.lock().unwrap_or_else(PoisonError::into_inner);
        killers.retain(|killer| !killer.is_finished());
        killers.push(killer);
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

/// Whether process group `pgid` still has a process.
#[cfg(unix)]
fn group_exists(pgid: i32) -> bool {
    use nix::errno::Errno;
    if pgid <= 0 {
        return false;
    }
    !matches!(nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None), Err(Errno::ESRCH))
}

#[cfg(not(unix))]
fn group_exists(_pgid: i32) -> bool {
    true
}

/// Polls until process group `pgid` is gone or `bound` has passed.
fn wait_for_group_to_end(pgid: i32, bound: Duration) {
    let deadline = Instant::now() + bound;
    while group_exists(pgid) && Instant::now() < deadline {
        std::thread::sleep(GROUP_POLL);
    }
}

/// SIGKILL to process group `pgid`, only while the group still exists.
#[cfg(unix)]
fn kill_group_if_present(pgid: i32) {
    if group_exists(pgid) {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), nix::sys::signal::Signal::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group_if_present(_pgid: i32) {}

/// SIGTERM to process group `pgid`, then waits up to `bound` for it to end and sends SIGKILL if it
/// has not.
#[cfg(unix)]
fn stop_group_and_wait(pgid: i32, bound: Duration) {
    use nix::sys::signal::{Signal, killpg};
    if pgid <= 0 || killpg(nix::unistd::Pid::from_raw(pgid), Signal::SIGTERM).is_err() {
        return;
    }
    wait_for_group_to_end(pgid, bound);
    kill_group_if_present(pgid);
    wait_for_group_to_end(pgid, KILL_GRACE);
}

#[cfg(not(unix))]
fn stop_group_and_wait(_pgid: i32, _bound: Duration) {}

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
